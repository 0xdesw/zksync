//use log::*;

use crate::eth_account::{get_executed_tx_fee, parse_ether, ETHExecResult, EthereumAccount};
use crate::external_commands::{deploy_test_contracts, get_test_accounts, Contracts};
use crate::zksync_account::ZksyncAccount;
use failure::bail;
use futures::{
    channel::{mpsc, oneshot},
    executor::block_on,
    SinkExt, StreamExt,
};
use models::config_options::ConfigurationOptions;
use models::node::{
    Account, AccountId, AccountMap, Address, BlockTimestamp, DepositOp, FranklinTx, FullExitOp,
    Nonce, PriorityOp, TokenId, TransferOp, TransferToNewOp, WithdrawOp,
};
use models::{BlockCommitRequest, CommitRequest};
use num::BigUint;
use server::mempool::ProposedBlock;
use server::state_keeper::{
    start_state_keeper, PlasmaStateInitParams, PlasmaStateKeeper, StateKeeperRequest,
    MAX_WITHDRAWALS_PER_BLOCK,
};
use std::collections::HashMap;
use std::thread::JoinHandle;
use std::time::{Instant, SystemTime};
use tokio::runtime::Runtime;
use web3::transports::Http;
use web3::Transport;

pub mod eth_account;
pub mod external_commands;
pub mod zksync_account;
use crypto_exports::rand::Rng;
use itertools::Itertools;
use models::prover_utils::EncodedProofPlonk;
use web3::types::{TransactionReceipt, U64};

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct ETHAccountId(pub usize);
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct ZKSyncAccountId(pub usize);
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Token(pub TokenId);

#[derive(Debug, Clone)]
pub struct BlockExecutionResult {
    pub commit_result: TransactionReceipt,
    pub verify_result: TransactionReceipt,
    pub withdrawals_result: TransactionReceipt,
    pub block_size_chunks: usize,
}

impl BlockExecutionResult {
    pub fn new(
        commit_result: TransactionReceipt,
        verify_result: TransactionReceipt,
        withdrawals_result: TransactionReceipt,
        block_size_chunks: usize,
    ) -> Self {
        Self {
            commit_result,
            verify_result,
            withdrawals_result,
            block_size_chunks,
        }
    }
}

/// Account set is used to create transactions using stored account
/// in a convenient way
pub struct AccountSet<T: Transport> {
    pub eth_accounts: Vec<EthereumAccount<T>>,
    pub zksync_accounts: Vec<ZksyncAccount>,
    pub fee_account_id: ZKSyncAccountId,
}
impl<T: Transport> AccountSet<T> {
    /// Create deposit from eth account to zksync account
    pub fn deposit(
        &self,
        from: ETHAccountId,
        to: ZKSyncAccountId,
        token: Option<Address>, // None for ETH
        amount: BigUint,
    ) -> (Vec<TransactionReceipt>, PriorityOp) {
        let from = &self.eth_accounts[from.0];
        let to = &self.zksync_accounts[to.0];

        if let Some(address) = token {
            block_on(from.deposit_erc20(address, amount, &to.address))
                .expect("erc20 deposit should not fail")
        } else {
            block_on(from.deposit_eth(amount, &to.address, None))
                .expect("eth deposit should not fail")
        }
    }

    pub fn deposit_to_random(
        &self,
        from: ETHAccountId,
        token: Option<Address>, // None for ETH
        amount: BigUint,
        rng: &mut impl Rng,
    ) -> (Vec<TransactionReceipt>, PriorityOp) {
        let from = &self.eth_accounts[from.0];
        let to_address = Address::from_slice(&rng.gen::<[u8; 20]>());

        if let Some(address) = token {
            block_on(from.deposit_erc20(address, amount, &to_address))
                .expect("erc20 deposit should not fail")
        } else {
            block_on(from.deposit_eth(amount, &to_address, None))
                .expect("eth deposit should not fail")
        }
    }

    /// Create signed transfer between zksync accounts
    /// `nonce` optional nonce override
    /// `increment_nonce` - flag for `from` account nonce increment
    #[allow(clippy::too_many_arguments)]
    pub fn transfer(
        &self,
        from: ZKSyncAccountId,
        to: ZKSyncAccountId,
        token_id: Token,
        amount: BigUint,
        fee: BigUint,
        nonce: Option<Nonce>,
        increment_nonce: bool,
    ) -> FranklinTx {
        let from = &self.zksync_accounts[from.0];
        let to = &self.zksync_accounts[to.0];

        FranklinTx::Transfer(Box::new(
            from.sign_transfer(
                token_id.0,
                "",
                amount,
                fee,
                &to.address,
                nonce,
                increment_nonce,
            )
            .0,
        ))
    }

    /// Create signed transfer between zksync accounts
    /// `nonce` optional nonce override
    /// `increment_nonce` - flag for `from` account nonce increment
    #[allow(clippy::too_many_arguments)]
    pub fn transfer_to_new_random(
        &self,
        from: ZKSyncAccountId,
        token_id: Token,
        amount: BigUint,
        fee: BigUint,
        nonce: Option<Nonce>,
        increment_nonce: bool,
        rng: &mut impl Rng,
    ) -> FranklinTx {
        let from = &self.zksync_accounts[from.0];

        let to_address = Address::from_slice(&rng.gen::<[u8; 20]>());

        FranklinTx::Transfer(Box::new(
            from.sign_transfer(
                token_id.0,
                "",
                amount,
                fee,
                &to_address,
                nonce,
                increment_nonce,
            )
            .0,
        ))
    }

    /// Create withdraw from zksync account to eth account
    /// `nonce` optional nonce override
    /// `increment_nonce` - flag for `from` account nonce increment
    #[allow(clippy::too_many_arguments)]
    fn withdraw(
        &self,
        from: ZKSyncAccountId,
        to: ETHAccountId,
        token_id: Token,
        amount: BigUint,
        fee: BigUint,
        nonce: Option<Nonce>,
        increment_nonce: bool,
    ) -> FranklinTx {
        let from = &self.zksync_accounts[from.0];
        let to = &self.eth_accounts[to.0];

        FranklinTx::Withdraw(Box::new(
            from.sign_withdraw(
                token_id.0,
                "",
                amount,
                fee,
                &to.address,
                nonce,
                increment_nonce,
            )
            .0,
        ))
    }

    /// Create withdraw from zksync account to random eth account
    /// `nonce` optional nonce override
    /// `increment_nonce` - flag for `from` account nonce increment
    #[allow(clippy::too_many_arguments)]
    fn withdraw_to_random(
        &self,
        from: ZKSyncAccountId,
        token_id: Token,
        amount: BigUint,
        fee: BigUint,
        nonce: Option<Nonce>,
        increment_nonce: bool,
        rng: &mut impl Rng,
    ) -> FranklinTx {
        let from = &self.zksync_accounts[from.0];
        let to_address = Address::from_slice(&rng.gen::<[u8; 20]>());

        FranklinTx::Withdraw(Box::new(
            from.sign_withdraw(
                token_id.0,
                "",
                amount,
                fee,
                &to_address,
                nonce,
                increment_nonce,
            )
            .0,
        ))
    }

    /// Create full exit from zksync account to eth account
    /// `nonce` optional nonce override
    /// `increment_nonce` - flag for `from` account nonce increment
    #[allow(clippy::too_many_arguments)]
    fn full_exit(
        &self,
        post_by: ETHAccountId,
        token_address: Address,
        account_id: AccountId,
    ) -> (TransactionReceipt, PriorityOp) {
        block_on(self.eth_accounts[post_by.0].full_exit(account_id, token_address))
            .expect("FullExit eth call failed")
    }

    fn change_pubkey_with_onchain_auth(
        &self,
        eth_account: ETHAccountId,
        zksync_signer: ZKSyncAccountId,
        nonce: Option<Nonce>,
        increment_nonce: bool,
    ) -> FranklinTx {
        let zksync_account = &self.zksync_accounts[zksync_signer.0];
        let auth_nonce = nonce.unwrap_or_else(|| zksync_account.nonce());

        let eth_account = &self.eth_accounts[eth_account.0];
        let tx_receipt =
            block_on(eth_account.auth_fact(&zksync_account.pubkey_hash.data, auth_nonce))
                .expect("Auth pubkey fail");
        assert_eq!(tx_receipt.status, Some(U64::from(1)), "Auth pubkey fail");
        FranklinTx::ChangePubKey(Box::new(zksync_account.create_change_pubkey_tx(
            nonce,
            increment_nonce,
            true,
        )))
    }

    fn change_pubkey_with_tx(
        &self,
        zksync_signer: ZKSyncAccountId,
        nonce: Option<Nonce>,
        increment_nonce: bool,
    ) -> FranklinTx {
        let zksync_account = &self.zksync_accounts[zksync_signer.0];
        FranklinTx::ChangePubKey(Box::new(zksync_account.create_change_pubkey_tx(
            nonce,
            increment_nonce,
            false,
        )))
    }
}

/// Initialize plasma state with one account - fee account.
pub fn genesis_state(fee_account_address: &Address) -> PlasmaStateInitParams {
    let operator_account = Account::default_with_address(fee_account_address);
    let mut params = PlasmaStateInitParams::new();
    params.insert_account(0, operator_account);
    params
}

pub async fn state_keeper_get_account(
    mut sender: mpsc::Sender<StateKeeperRequest>,
    address: &Address,
) -> Option<(AccountId, Account)> {
    let resp = oneshot::channel();
    sender
        .send(StateKeeperRequest::GetAccount(*address, resp.0))
        .await
        .expect("sk request send");
    resp.1.await.expect("sk account resp recv")
}

pub struct StateKeeperChannels {
    requests: mpsc::Sender<StateKeeperRequest>,
    new_blocks: mpsc::Receiver<CommitRequest>,
}

// Thread join handle and stop channel sender.
pub fn spawn_state_keeper(
    fee_account: &Address,
) -> (JoinHandle<()>, oneshot::Sender<()>, StateKeeperChannels) {
    let (proposed_blocks_sender, proposed_blocks_receiver) = mpsc::channel(256);
    let (state_keeper_req_sender, state_keeper_req_receiver) = mpsc::channel(256);
    let (executed_tx_notify_sender, _executed_tx_notify_receiver) = mpsc::channel(256);

    let max_ops_in_block = 1000;
    let ops_chunks = vec![
        TransferToNewOp::CHUNKS,
        TransferOp::CHUNKS,
        DepositOp::CHUNKS,
        FullExitOp::CHUNKS,
        WithdrawOp::CHUNKS,
    ];
    let mut block_chunks_sizes = (0..max_ops_in_block)
        .cartesian_product(ops_chunks)
        .map(|(x, y)| x * y)
        .collect::<Vec<_>>();
    block_chunks_sizes.sort();
    block_chunks_sizes.dedup();

    let max_miniblock_iterations = *block_chunks_sizes.iter().max().unwrap();
    let state_keeper = PlasmaStateKeeper::new(
        genesis_state(fee_account),
        *fee_account,
        state_keeper_req_receiver,
        proposed_blocks_sender,
        executed_tx_notify_sender,
        block_chunks_sizes,
        max_miniblock_iterations,
        max_miniblock_iterations,
    );

    let (stop_state_keeper_sender, stop_state_keeper_receiver) = oneshot::channel::<()>();
    let sk_thread_handle = std::thread::spawn(move || {
        let mut main_runtime = Runtime::new().expect("main runtime start");
        let state_keeper_task = start_state_keeper(state_keeper, None, &main_runtime);
        main_runtime.block_on(async move {
            tokio::select! {
                _ = stop_state_keeper_receiver => {},
                _ = state_keeper_task => {},
            }
        })
    });

    (
        sk_thread_handle,
        stop_state_keeper_sender,
        StateKeeperChannels {
            requests: state_keeper_req_sender,
            new_blocks: proposed_blocks_receiver,
        },
    )
}

#[derive(PartialEq, Eq, Clone, Copy, Debug)]
pub enum BlockProcessing {
    CommitAndVerify,
    NoVerify,
}

pub fn perform_basic_operations(
    token: u16,
    test_setup: &mut TestSetup,
    deposit_amount: BigUint,
    blocks_processing: BlockProcessing,
) {
    // test deposit to other account
    test_setup.start_block();
    test_setup.deposit(
        ETHAccountId(0),
        ZKSyncAccountId(2),
        Token(token),
        deposit_amount.clone(),
    );
    if blocks_processing == BlockProcessing::CommitAndVerify {
        test_setup
            .execute_commit_and_verify_block()
            .expect("Block execution failed");
        println!("Deposit to other account test success, token_id: {}", token);
    } else {
        test_setup.execute_commit_block().expect_success();
    }

    // test two deposits
    test_setup.start_block();
    test_setup.deposit(
        ETHAccountId(0),
        ZKSyncAccountId(1),
        Token(token),
        deposit_amount.clone(),
    );
    test_setup.deposit(
        ETHAccountId(0),
        ZKSyncAccountId(1),
        Token(token),
        deposit_amount.clone(),
    );
    if blocks_processing == BlockProcessing::CommitAndVerify {
        test_setup
            .execute_commit_and_verify_block()
            .expect("Block execution failed");
        println!("Deposit test success, token_id: {}", token);
    } else {
        test_setup.execute_commit_block().expect_success();
    }

    // test transfers
    test_setup.start_block();

    if blocks_processing == BlockProcessing::CommitAndVerify {
        test_setup.change_pubkey_with_onchain_auth(ETHAccountId(0), ZKSyncAccountId(1));
    } else {
        test_setup.change_pubkey_with_tx(ZKSyncAccountId(1));
    }

    //transfer to self should work
    test_setup.transfer(
        ZKSyncAccountId(1),
        ZKSyncAccountId(1),
        Token(token),
        &deposit_amount / BigUint::from(8u32),
        &deposit_amount / BigUint::from(8u32),
    );

    //should be executed as a transfer
    test_setup.transfer(
        ZKSyncAccountId(1),
        ZKSyncAccountId(2),
        Token(token),
        &deposit_amount / BigUint::from(8u32),
        &deposit_amount / BigUint::from(8u32),
    );

    let nonce = test_setup.accounts.zksync_accounts[1].nonce();
    let incorrect_nonce_transfer = test_setup.accounts.transfer(
        ZKSyncAccountId(1),
        ZKSyncAccountId(0),
        Token(token),
        deposit_amount.clone(),
        BigUint::from(0u32),
        Some(nonce + 1),
        false,
    );
    test_setup.execute_incorrect_tx(incorrect_nonce_transfer);

    //should be executed as a transfer to new
    test_setup.transfer(
        ZKSyncAccountId(1),
        ZKSyncAccountId(2),
        Token(token),
        &deposit_amount / BigUint::from(4u32),
        &deposit_amount / BigUint::from(4u32),
    );

    test_setup.change_pubkey_with_tx(ZKSyncAccountId(2));

    test_setup.withdraw(
        ZKSyncAccountId(2),
        ETHAccountId(0),
        Token(token),
        &deposit_amount / BigUint::from(4u32),
        &deposit_amount / BigUint::from(4u32),
    );
    if blocks_processing == BlockProcessing::CommitAndVerify {
        test_setup
            .execute_commit_and_verify_block()
            .expect("Block execution failed");
        println!("Transfer test success, token_id: {}", token);
    } else {
        test_setup.execute_commit_block().expect_success();
    }

    test_setup.start_block();
    test_setup.full_exit(ETHAccountId(0), ZKSyncAccountId(1), Token(token));
    if blocks_processing == BlockProcessing::CommitAndVerify {
        test_setup
            .execute_commit_and_verify_block()
            .expect("Block execution failed");
        println!("Full exit test success, token_id: {}", token);
    } else {
        test_setup.execute_commit_block().expect_success();
    }
}

pub struct TestkitConfig {
    pub chain_id: u8,
    pub gas_price_factor: f64,
    pub web3_url: String,
}

pub fn get_testkit_config_from_env() -> TestkitConfig {
    let env_config = ConfigurationOptions::from_env();
    TestkitConfig {
        chain_id: env_config.chain_id,
        gas_price_factor: env_config.gas_price_factor,
        web3_url: env_config.web3_url,
    }
}

pub fn perform_basic_tests() {
    let testkit_config = get_testkit_config_from_env();

    let fee_account = ZksyncAccount::rand();
    let (sk_thread_handle, stop_state_keeper_sender, sk_channels) =
        spawn_state_keeper(&fee_account.address);

    let deploy_timer = Instant::now();
    println!("deploying contracts");
    let contracts = deploy_test_contracts();
    println!(
        "contracts deployed {:#?}, {} secs",
        contracts,
        deploy_timer.elapsed().as_secs()
    );

    let (_el, transport) = Http::new(&testkit_config.web3_url).expect("http transport start");
    let (test_accounts_info, commit_account_info) = get_test_accounts();
    let commit_account = EthereumAccount::new(
        commit_account_info.private_key,
        commit_account_info.address,
        transport.clone(),
        contracts.contract,
        testkit_config.chain_id,
        testkit_config.gas_price_factor,
    );
    let eth_accounts = test_accounts_info
        .into_iter()
        .map(|test_eth_account| {
            EthereumAccount::new(
                test_eth_account.private_key,
                test_eth_account.address,
                transport.clone(),
                contracts.contract,
                testkit_config.chain_id,
                testkit_config.gas_price_factor,
            )
        })
        .collect::<Vec<_>>();

    let zksync_accounts = {
        let mut zksync_accounts = Vec::new();
        zksync_accounts.push(fee_account);
        zksync_accounts.extend(eth_accounts.iter().map(|eth_account| {
            let rng_zksync_key = ZksyncAccount::rand().private_key;
            ZksyncAccount::new(
                rng_zksync_key,
                0,
                eth_account.address,
                eth_account.private_key,
            )
        }));
        zksync_accounts
    };

    let accounts = AccountSet {
        eth_accounts,
        zksync_accounts,
        fee_account_id: ZKSyncAccountId(0),
    };

    let mut test_setup = TestSetup::new(sk_channels, accounts, &contracts, commit_account);

    let deposit_amount = parse_ether("1.0").unwrap();

    for token in 0..=1 {
        perform_basic_operations(
            token,
            &mut test_setup,
            deposit_amount.clone(),
            BlockProcessing::CommitAndVerify,
        );
    }

    stop_state_keeper_sender.send(()).expect("sk stop send");
    sk_thread_handle.join().expect("sk thread join");
}

// Struct used to keep expected balance changes after transactions execution.
#[derive(Default)]
pub struct ExpectedAccountState {
    eth_accounts_state: HashMap<(ETHAccountId, TokenId), BigUint>,
    sync_accounts_state: HashMap<(ZKSyncAccountId, TokenId), BigUint>,

    // Amount of withdraw operations performed in block.
    withdraw_ops: usize,
}

/// Used to create transactions between accounts and check for their validity.
/// Every new block should start with `.start_block()`
/// and end with `execute_commit_and_verify_block()`
/// with desired transactions in between.
///
/// Transactions balance side effects are checked,
/// in order to execute unusual/failed transactions one should create it separately and commit to block
/// using `execute_incorrect_tx`
pub struct TestSetup {
    pub state_keeper_request_sender: mpsc::Sender<StateKeeperRequest>,
    pub proposed_blocks_receiver: mpsc::Receiver<CommitRequest>,

    pub accounts: AccountSet<Http>,
    pub tokens: HashMap<TokenId, Address>,

    pub expected_changes_for_current_block: ExpectedAccountState,

    pub commit_account: EthereumAccount<Http>,
}

impl TestSetup {
    pub fn new(
        sk_channels: StateKeeperChannels,
        accounts: AccountSet<Http>,
        deployed_contracts: &Contracts,
        commit_account: EthereumAccount<Http>,
    ) -> Self {
        let mut tokens = HashMap::new();
        tokens.insert(1, deployed_contracts.test_erc20_address.clone());
        tokens.insert(0, Address::default());
        Self {
            state_keeper_request_sender: sk_channels.requests,
            proposed_blocks_receiver: sk_channels.new_blocks,
            accounts,
            tokens,
            expected_changes_for_current_block: ExpectedAccountState::default(),
            commit_account,
        }
    }

    pub fn get_expected_eth_account_balance(
        &self,
        account: ETHAccountId,
        token: TokenId,
    ) -> BigUint {
        self.expected_changes_for_current_block
            .eth_accounts_state
            .get(&(account, token))
            .cloned()
            .unwrap_or_else(|| self.get_eth_balance(account, token))
    }

    pub fn get_expected_zksync_account_balance(
        &self,
        account: ZKSyncAccountId,
        token: TokenId,
    ) -> BigUint {
        self.expected_changes_for_current_block
            .sync_accounts_state
            .get(&(account, token))
            .cloned()
            .unwrap_or_else(|| self.get_zksync_balance(account, token))
    }

    pub fn start_block(&mut self) {
        self.expected_changes_for_current_block = ExpectedAccountState::default();
    }

    pub fn execute_incorrect_tx(&mut self, tx: FranklinTx) {
        self.execute_tx(tx);
    }

    pub fn deposit(
        &mut self,
        from: ETHAccountId,
        to: ZKSyncAccountId,
        token: Token,
        amount: BigUint,
    ) -> Vec<TransactionReceipt> {
        let mut from_eth_balance = self.get_expected_eth_account_balance(from, token.0);
        from_eth_balance -= &amount;

        self.expected_changes_for_current_block
            .eth_accounts_state
            .insert((from, token.0), from_eth_balance);

        let mut zksync0_old = self.get_expected_zksync_account_balance(to, token.0);
        zksync0_old += &amount;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((to, token.0), zksync0_old);

        let token_address = if token.0 == 0 {
            None
        } else {
            Some(
                self.tokens
                    .get(&token.0)
                    .cloned()
                    .expect("Token with token id does not exist"),
            )
        };
        let mut eth_balance = self.get_expected_eth_account_balance(from, 0);

        let (receipts, deposit_op) = self.accounts.deposit(from, to, token_address, amount);

        let gas_fee = receipts
            .iter()
            .map(|r| {
                block_on(get_executed_tx_fee(
                    self.commit_account.main_contract_eth_client.web3.eth(),
                    &r,
                ))
                .expect("Failed to get transaction fee")
            })
            .sum::<BigUint>();
        eth_balance -= gas_fee;
        self.expected_changes_for_current_block
            .eth_accounts_state
            .insert((from, 0), eth_balance);

        self.execute_priority_op(deposit_op);
        receipts
    }

    fn execute_tx(&mut self, tx: FranklinTx) {
        let block = ProposedBlock {
            priority_ops: Vec::new(),
            txs: vec![tx],
        };
        let block_sender = async {
            self.state_keeper_request_sender
                .clone()
                .send(StateKeeperRequest::ExecuteMiniBlock(block))
                .await
                .expect("sk receiver dropped");
        };
        // Request miniblock execution.
        block_on(block_sender);
        // Receive the pending block processing request from state keeper.
        block_on(self.await_for_pending_block_request());
    }

    pub fn deposit_to_random(
        &mut self,
        from: ETHAccountId,
        token: Token,
        amount: BigUint,
        rng: &mut impl Rng,
    ) -> Vec<TransactionReceipt> {
        let mut from_eth_balance = self.get_expected_eth_account_balance(from, token.0);
        from_eth_balance -= &amount;

        self.expected_changes_for_current_block
            .eth_accounts_state
            .insert((from, token.0), from_eth_balance);

        let token_address = if token.0 == 0 {
            None
        } else {
            Some(
                self.tokens
                    .get(&token.0)
                    .cloned()
                    .expect("Token with token id does not exist"),
            )
        };
        let mut eth_balance = self.get_expected_eth_account_balance(from, 0);

        let (receipts, deposit_op) =
            self.accounts
                .deposit_to_random(from, token_address, amount, rng);

        let gas_fee = receipts
            .iter()
            .map(|r| {
                block_on(get_executed_tx_fee(
                    self.commit_account.main_contract_eth_client.web3.eth(),
                    &r,
                ))
                .expect("Failed to get transaction fee")
            })
            .sum::<BigUint>();
        eth_balance -= gas_fee;
        self.expected_changes_for_current_block
            .eth_accounts_state
            .insert((from, 0), eth_balance);

        self.execute_priority_op(deposit_op);
        receipts
    }

    fn execute_priority_op(&mut self, op: PriorityOp) {
        let block = ProposedBlock {
            priority_ops: vec![op],
            txs: Vec::new(),
        };
        let block_sender = async {
            self.state_keeper_request_sender
                .clone()
                .send(StateKeeperRequest::ExecuteMiniBlock(block))
                .await
                .expect("sk receiver dropped");
        };
        // Request miniblock execution.
        block_on(block_sender);
        // Receive the pending block processing request from state keeper.
        block_on(self.await_for_pending_block_request());
    }

    pub fn exit(
        &mut self,
        sending_account: ETHAccountId,
        account_id: AccountId,
        token_id: Token,
        amount: &BigUint,
        proof: EncodedProofPlonk,
    ) -> ETHExecResult {
        block_on(
            self.accounts.eth_accounts[sending_account.0]
                .exit(account_id, token_id.0, amount, proof),
        )
        .expect("Failed to post exit tx")
    }

    pub fn full_exit(
        &mut self,
        post_by: ETHAccountId,
        from: ZKSyncAccountId,
        token: Token,
    ) -> TransactionReceipt {
        let account_id = self
            .get_zksync_account_committed_state(from)
            .map(|(id, _)| id)
            .expect("Account should be in the map");
        let token_address = if token.0 == 0 {
            Address::zero()
        } else {
            *self.tokens.get(&token.0).expect("Token does not exist")
        };

        let zksync0_old = self.get_expected_zksync_account_balance(from, token.0);
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((from, token.0), BigUint::from(0u32));

        let mut post_by_eth_balance = self.get_expected_eth_account_balance(post_by, token.0);
        post_by_eth_balance += zksync0_old;
        self.expected_changes_for_current_block
            .eth_accounts_state
            .insert((post_by, token.0), post_by_eth_balance);

        let mut eth_balance = self.get_expected_eth_account_balance(post_by, 0);

        let (receipt, full_exit_op) = self.accounts.full_exit(post_by, token_address, account_id);

        let gas_fee = block_on(get_executed_tx_fee(
            self.commit_account.main_contract_eth_client.web3.eth(),
            &receipt,
        ))
        .expect("Failed to get transaction fee");
        eth_balance -= gas_fee;
        self.expected_changes_for_current_block
            .eth_accounts_state
            .insert((post_by, 0), eth_balance);

        self.execute_priority_op(full_exit_op);
        receipt
    }

    pub fn change_pubkey_with_tx(&mut self, zksync_signer: ZKSyncAccountId) {
        let account_id = self
            .get_zksync_account_committed_state(zksync_signer)
            .expect("can't change pubkey, account does not exist")
            .0;
        self.accounts.zksync_accounts[zksync_signer.0].set_account_id(Some(account_id));

        let tx = self
            .accounts
            .change_pubkey_with_tx(zksync_signer, None, true);

        self.execute_tx(tx);
    }

    pub fn change_pubkey_with_onchain_auth(
        &mut self,
        eth_account: ETHAccountId,
        zksync_signer: ZKSyncAccountId,
    ) {
        let account_id = self
            .get_zksync_account_committed_state(zksync_signer)
            .expect("can't change pubkey, account does not exist")
            .0;
        self.accounts.zksync_accounts[zksync_signer.0].set_account_id(Some(account_id));

        let tx =
            self.accounts
                .change_pubkey_with_onchain_auth(eth_account, zksync_signer, None, true);

        self.execute_tx(tx);
    }

    pub fn transfer(
        &mut self,
        from: ZKSyncAccountId,
        to: ZKSyncAccountId,
        token: Token,
        amount: BigUint,
        fee: BigUint,
    ) {
        let mut zksync0_old = self.get_expected_zksync_account_balance(from, token.0);
        zksync0_old -= &amount;
        zksync0_old -= &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((from, token.0), zksync0_old);

        let mut zksync0_old = self.get_expected_zksync_account_balance(to, token.0);
        zksync0_old += &amount;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((to, token.0), zksync0_old);

        let mut zksync0_old =
            self.get_expected_zksync_account_balance(self.accounts.fee_account_id, token.0);
        zksync0_old += &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((self.accounts.fee_account_id, token.0), zksync0_old);

        let transfer = self
            .accounts
            .transfer(from, to, token, amount, fee, None, true);

        self.execute_tx(transfer)
    }

    pub fn transfer_to_new_random(
        &mut self,
        from: ZKSyncAccountId,
        token: Token,
        amount: BigUint,
        fee: BigUint,
        rng: &mut impl Rng,
    ) {
        let mut zksync0_old = self.get_expected_zksync_account_balance(from, token.0);
        zksync0_old -= &amount;
        zksync0_old -= &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((from, token.0), zksync0_old);

        let mut zksync0_old =
            self.get_expected_zksync_account_balance(self.accounts.fee_account_id, token.0);
        zksync0_old += &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((self.accounts.fee_account_id, token.0), zksync0_old);

        let transfer = self
            .accounts
            .transfer_to_new_random(from, token, amount, fee, None, true, rng);

        self.execute_tx(transfer)
    }

    fn increase_block_withdraws_amount(&mut self) {
        self.expected_changes_for_current_block.withdraw_ops += 1;

        if self.expected_changes_for_current_block.withdraw_ops > MAX_WITHDRAWALS_PER_BLOCK as usize
        {
            panic!(
                "Attempt to perform too many withdraw operations in one block. \
                Maximum amount of withdraw operations in one block: {}. \
                You have to commit block if it has this amount of withdraws.",
                MAX_WITHDRAWALS_PER_BLOCK
            )
        }
    }

    pub fn withdraw(
        &mut self,
        from: ZKSyncAccountId,
        to: ETHAccountId,
        token: Token,
        amount: BigUint,
        fee: BigUint,
    ) {
        self.increase_block_withdraws_amount();

        let mut zksync0_old = self.get_expected_zksync_account_balance(from, token.0);
        zksync0_old -= &amount;
        zksync0_old -= &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((from, token.0), zksync0_old);

        let mut to_eth_balance = self.get_expected_eth_account_balance(to, token.0);
        to_eth_balance += &amount;
        self.expected_changes_for_current_block
            .eth_accounts_state
            .insert((to, token.0), to_eth_balance);

        let mut zksync0_old =
            self.get_expected_zksync_account_balance(self.accounts.fee_account_id, token.0);
        zksync0_old += &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((self.accounts.fee_account_id, token.0), zksync0_old);

        let withdraw = self
            .accounts
            .withdraw(from, to, token, amount, fee, None, true);

        self.execute_tx(withdraw);
    }

    pub fn withdraw_to_random_account(
        &mut self,
        from: ZKSyncAccountId,
        token: Token,
        amount: BigUint,
        fee: BigUint,
        rng: &mut impl Rng,
    ) {
        self.increase_block_withdraws_amount();

        let mut zksync0_old = self.get_expected_zksync_account_balance(from, token.0);
        zksync0_old -= &amount;
        zksync0_old -= &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((from, token.0), zksync0_old);

        let mut zksync0_old =
            self.get_expected_zksync_account_balance(self.accounts.fee_account_id, token.0);
        zksync0_old += &fee;
        self.expected_changes_for_current_block
            .sync_accounts_state
            .insert((self.accounts.fee_account_id, token.0), zksync0_old);

        let withdraw = self
            .accounts
            .withdraw_to_random(from, token, amount, fee, None, true, rng);

        self.execute_tx(withdraw);
    }

    /// Waits for `CommitRequest::Block` to appear on proposed blocks receiver, ignoring
    /// the pending blocks.
    async fn await_for_block_commit_request(&mut self) -> BlockCommitRequest {
        while let Some(new_block_event) = self.proposed_blocks_receiver.next().await {
            match new_block_event {
                CommitRequest::Block(new_block, receiver) => {
                    receiver.send(()).unwrap();
                    return new_block;
                }
                CommitRequest::PendingBlock(_, receiver) => {
                    // Pending blocks are ignored.
                    receiver.send(()).unwrap();
                }
            }
        }
        panic!("Proposed blocks receiver dropped");
    }

    /// Takes the next `CommitRequest` from the proposed blocks receiver and expects
    /// it to be `PendingBlock`. Panics otherwise.
    async fn await_for_pending_block_request(&mut self) {
        let new_block_event = self
            .proposed_blocks_receiver
            .next()
            .await
            .expect("StateKeeper sender dropped");
        match new_block_event {
            CommitRequest::Block(new_block, _) => {
                panic!(
                    "Expected pending block, got full block proposed. Block: {:?}",
                    new_block
                );
            }
            CommitRequest::PendingBlock(_, receiver) => {
                // Notify state keeper that we've processed the request.
                receiver.send(()).unwrap();
            }
        }
    }

    /// Should not be used execept special cases(when we want to commit but don't want to verify block)
    pub fn execute_commit_block(&mut self) -> ETHExecResult {
        let block_sender = async {
            self.state_keeper_request_sender
                .clone()
                .send(StateKeeperRequest::SealBlock)
                .await
                .expect("sk receiver dropped");
        };
        block_on(block_sender);

        let mut new_block = block_on(self.await_for_block_commit_request());
        new_block.block.block_timestamp = Some(BlockTimestamp::from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("unix timestamp calculation failed")
                .as_secs(),
        ));

        block_on(self.commit_account.commit_block(&new_block.block)).expect("block commit fail")
    }

    /// Analog of `execute_commit_block`, but uses defined block timestamp value
    pub fn execute_commit_block_with_defined_timestamp(
        &mut self,
        block_timestamp: BlockTimestamp,
    ) -> ETHExecResult {
        let block_sender = async {
            self.state_keeper_request_sender
                .clone()
                .send(StateKeeperRequest::SealBlock)
                .await
                .expect("sk receiver dropped");
        };
        block_on(block_sender);

        let mut new_block = block_on(self.await_for_block_commit_request());
        new_block.block.block_timestamp = Some(block_timestamp);

        block_on(self.commit_account.commit_block(&new_block.block)).expect("block commit fail")
    }

    pub fn execute_commit_and_verify_block(
        &mut self,
    ) -> Result<BlockExecutionResult, failure::Error> {
        let block_sender = async {
            self.state_keeper_request_sender
                .clone()
                .send(StateKeeperRequest::SealBlock)
                .await
                .expect("sk receiver dropped");
        };
        block_on(block_sender);
        let mut new_block = block_on(self.await_for_block_commit_request());
        new_block.block.block_timestamp = Some(BlockTimestamp::from(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("unix timestamp calculation failed")
                .as_secs(),
        ));

        let commit_result = block_on(self.commit_account.commit_block(&new_block.block))
            .expect("block commit send tx")
            .expect_success();
        let verify_result = block_on(self.commit_account.verify_block(&new_block.block))
            .expect("block verify send tx")
            .expect_success();
        let withdrawals_result = block_on(self.commit_account.complete_withdrawals())
            .expect("complete withdrawal send tx")
            .expect_success();
        let block_chunks = new_block.block.block_chunks_size;

        let mut block_checks_failed = false;
        for ((eth_account, token), expeted_balance) in
            &self.expected_changes_for_current_block.eth_accounts_state
        {
            let real_balance = self.get_eth_balance(*eth_account, *token);
            if expeted_balance != &real_balance {
                println!("eth acc: {}, token: {}", eth_account.0, token);
                println!("expected: {}", expeted_balance);
                println!("real:     {}", real_balance);
                block_checks_failed = true;
            }
        }

        for ((zksync_account, token), balance) in
            &self.expected_changes_for_current_block.sync_accounts_state
        {
            let real = self.get_zksync_balance(*zksync_account, *token);
            let is_diff_valid = real.clone() - balance == BigUint::from(0u32);
            if !is_diff_valid {
                println!(
                    "zksync acc {} diff {}, real: {}",
                    zksync_account.0,
                    real.clone() - balance,
                    real.clone()
                );
                block_checks_failed = true;
            }
        }

        if block_checks_failed {
            println!(
                "Failed block exec_operations: {:#?}",
                new_block.block.block_transactions
            );
            bail!("Block checks failed")
        }

        for zk_id in 0..self.accounts.zksync_accounts.len() {
            self.accounts.zksync_accounts[zk_id]
                .set_account_id(self.get_zksync_account_id(ZKSyncAccountId(zk_id)));
        }

        Ok(BlockExecutionResult::new(
            commit_result,
            verify_result,
            withdrawals_result,
            block_chunks,
        ))
    }

    pub fn get_zksync_account_committed_state(
        &self,
        zksync_id: ZKSyncAccountId,
    ) -> Option<(AccountId, Account)> {
        let address = &self.accounts.zksync_accounts[zksync_id.0].address;
        block_on(state_keeper_get_account(
            self.state_keeper_request_sender.clone(),
            address,
        ))
    }

    pub fn get_zksync_account_id(&self, zksync_id: ZKSyncAccountId) -> Option<AccountId> {
        self.get_zksync_account_committed_state(zksync_id)
            .map(|a| a.0)
    }

    fn get_zksync_balance(&self, zksync_id: ZKSyncAccountId, token: TokenId) -> BigUint {
        self.get_zksync_account_committed_state(zksync_id)
            .map(|(_, acc)| acc.get_balance(token))
            .unwrap_or_default()
    }

    fn get_eth_balance(&self, eth_account_id: ETHAccountId, token: TokenId) -> BigUint {
        let account = &self.accounts.eth_accounts[eth_account_id.0];
        let result = if token == 0 {
            block_on(account.eth_balance()).expect("Failed to get eth balance")
        } else {
            block_on(account.erc20_balance(&self.tokens[&token]))
                .expect("Failed to get erc20 balance")
        };
        result + self.get_balance_to_withdraw(eth_account_id, Token(token))
    }

    pub fn get_balance_to_withdraw(&self, eth_account_id: ETHAccountId, token: Token) -> BigUint {
        block_on(self.accounts.eth_accounts[eth_account_id.0].balances_to_withdraw(token.0))
            .expect("failed to query balance to withdraws")
    }

    pub fn is_exodus(&self) -> bool {
        block_on(self.commit_account.is_exodus()).expect("Exodus query")
    }

    pub fn total_blocks_committed(&self) -> Result<u64, failure::Error> {
        block_on(self.accounts.eth_accounts[0].total_blocks_committed())
    }

    pub fn total_blocks_verified(&self) -> Result<u64, failure::Error> {
        block_on(self.accounts.eth_accounts[0].total_blocks_verified())
    }

    pub fn revert_blocks(&self, blocks_to_revert: u64) -> Result<(), failure::Error> {
        block_on(self.commit_account.revert_blocks(blocks_to_revert))?;
        Ok(())
    }

    pub fn eth_block_number(&self) -> u64 {
        block_on(self.commit_account.eth_block_number()).expect("Block number query")
    }

    pub fn get_tokens(&self) -> Vec<Token> {
        self.tokens.iter().map(|(id, _)| Token(*id)).collect()
    }

    pub fn trigger_exodus_if_needed(&self, eth_account: ETHAccountId) {
        block_on(self.accounts.eth_accounts[eth_account.0].trigger_exodus_if_needed())
            .expect("Trigger exodus if needed call");
    }

    pub fn cancel_outstanding_deposits(&self, eth_account: ETHAccountId) {
        const DEPOSITS_TO_CANCEL: u64 = 100;
        block_on(
            self.accounts.eth_accounts[eth_account.0]
                .cancel_outstanding_deposits_for_exodus_mode(DEPOSITS_TO_CANCEL),
        )
        .expect("Failed to cancel outstanding deposits");
    }

    pub fn get_accounts_state(&self) -> AccountMap {
        let mut account_map = AccountMap::default();
        for id in 0..self.accounts.zksync_accounts.len() {
            if let Some((id, account)) =
                self.get_zksync_account_committed_state(ZKSyncAccountId(id))
            {
                account_map.insert(id, account);
            }
        }
        account_map
    }

    pub fn gen_exit_proof(
        &self,
        accounts: AccountMap,
        fund_owner: ZKSyncAccountId,
        token: Token,
    ) -> (EncodedProofPlonk, BigUint) {
        let owner = &self.accounts.zksync_accounts[fund_owner.0];
        let owner_id = owner
            .get_account_id()
            .expect("Account should have id to exit");
        // restore account state
        prover::exit_proof::create_exit_proof(accounts, owner_id, owner.address, token.0)
            .expect("Failed to generate exit proof")
    }
}
