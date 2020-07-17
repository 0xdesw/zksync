//! Loadtest scenario for the Reddit PoC.
//!
//! This test runs the following operations:
//!
//! - 100,000 point claims (minting & distributing points) (i.e. transfers — AG)
//! - 25,000 subscriptions (i.e. creating subscriptions; this can be done fully offchain — AG)
//! - 75,000 one-off points burning (i.e. subscription redemptions: — AG)
//! - 100,000 transfers

// Built-in deps
use std::{
    iter::Iterator,
    time::{Duration, Instant},
};
// External deps
use chrono::Utc;
use futures::future::try_join_all;
use num::BigUint;
use tokio::{fs, time};
use web3::transports::{EventLoopHandle, Http};
// Workspace deps
use models::{
    config_options::ConfigurationOptions,
    misc::utils::format_ether,
    node::{
        closest_packable_fee_amount, closest_packable_token_amount, tx::PackedEthSignature,
        FranklinTx,
    },
};
use testkit::zksync_account::ZksyncAccount;
// Local deps
use crate::{
    rpc_client::RpcClient,
    scenarios::{
        configs::RealLifeConfig,
        utils::{deposit_single, wait_for_verify, DynamicChunks},
        ScenarioContext,
    },
    sent_transactions::SentTransactions,
    test_accounts::TestAccount,
};

#[derive(Debug)]
struct ScenarioExecutor {
    rpc_client: RpcClient,

    /// Main account to deposit ETH from / return ETH back to.
    main_account: TestAccount,

    /// Intermediate account to rotate funds within.
    accounts: Vec<ZksyncAccount>,

    /// Amount of intermediate accounts.
    n_accounts: usize,
    /// Transfer amount per accounts (in wei).
    transfer_size: BigUint,
    /// Amount of cycles for funds rotation.
    cycles_amount: u32,

    /// Block sizes supported by server and suitable to use in this test
    /// (to not overload the node with too many txs at the moment)
    block_sizes: Vec<usize>,

    /// Amount of time to wait for one zkSync block to be verified.
    verify_timeout: Duration,

    /// Estimated fee amount for any zkSync operation. It is used to deposit
    /// funds initially and transfer the funds for intermediate accounts to
    /// operate.
    estimated_fee_for_op: BigUint,

    /// Event loop handle so transport for Eth account won't be invalidated.
    _event_loop_handle: EventLoopHandle,
}

impl ScenarioExecutor {
    /// Creates a real-life scenario executor.
    pub fn new(ctx: &ScenarioContext, rpc_client: RpcClient) -> Self {
        // Load the config for the test from JSON file.
        let config = RealLifeConfig::load(&ctx.config_path);

        // Generate random accounts to rotate funds within.
        let accounts = (0..config.n_accounts)
            .map(|_| ZksyncAccount::rand())
            .collect();

        // Create a transport for Ethereum account.
        let (_event_loop_handle, transport) =
            Http::new(&ctx.options.web3_url).expect("http transport start");

        // Create main account to deposit money from and to return money back later.
        let main_account = TestAccount::from_info(&config.input_account, &transport, &ctx.options);

        let block_sizes = Self::get_block_sizes(config.use_all_block_sizes);

        if config.use_all_block_sizes {
            log::info!(
                "Following block sizes will be used in test: {:?}",
                block_sizes
            );
        }

        let transfer_size = closest_packable_token_amount(&BigUint::from(config.transfer_size));
        let verify_timeout = Duration::from_secs(config.block_timeout);

        Self {
            rpc_client,

            main_account,
            accounts,

            n_accounts: config.n_accounts,
            transfer_size,
            cycles_amount: config.cycles_amount,

            block_sizes,

            verify_timeout,

            estimated_fee_for_op: 0u32.into(),

            _event_loop_handle,
        }
    }

    /// Infallible test runner which performs the emergency exit if any step of the test
    /// fails.
    pub async fn run(&mut self) {
        if let Err(error) = self.run_test().await {
            log::error!("Loadtest erred with the following error: {}", error);
        } else {
            log::info!("Loadtest completed successfully");
        }
    }

    /// Method to be used before the scenario.
    /// It stores all the zkSync account keys into a file named
    /// like "loadtest_accounts_2020_05_05_12_23_55.txt"
    /// so the funds left on accounts will not be lost.
    ///
    /// If saving the file fails, the accounts are printed to the log.
    async fn save_accounts(&self) {
        // Timestamp is used to generate unique file name postfix.
        let timestamp = Utc::now();
        let timestamp_str = timestamp.format("%Y_%m_%d_%H_%M_%S").to_string();

        let output_file_name = format!("loadtest_accounts_{}.txt", timestamp_str);

        let mut account_list = String::new();

        // Add all the accounts to the string.
        // Debug representations of account contains both zkSync and Ethereum private keys.
        account_list += &format!("{:?}\n", self.main_account.zk_acc);
        for account in self.accounts.iter() {
            account_list += &format!("{:?}\n", account);
        }

        // If we're unable to save the file, print its contents to the console at least.
        if let Err(error) = fs::write(&output_file_name, &account_list).await {
            log::error!(
                "Storing the account list erred with the following error: {}",
                error
            );
            log::warn!(
                "Printing the account list to the log instead: \n{}",
                account_list
            )
        } else {
            log::info!(
                "Accounts used in this test are saved to the file '{}'",
                &output_file_name
            );
        }
    }

    /// Runs the test step-by-step. Every test step is encapsulated into its own function.
    pub async fn run_test(&mut self) -> Result<(), failure::Error> {
        self.save_accounts().await;

        self.initialize().await?;
        self.deposit().await?;
        self.initial_transfer().await?;
        self.funds_rotation().await?;
        self.collect_funds().await?;
        self.withdraw().await?;
        self.finish().await?;

        Ok(())
    }

    /// Initializes the test, preparing the main account for the interaction.
    async fn initialize(&mut self) -> Result<(), failure::Error> {
        // First of all, we have to update both the Ethereum and ZKSync accounts nonce values.
        self.main_account
            .update_nonce_values(&self.rpc_client)
            .await?;

        // Then, we have to get the fee value (assuming that dev-ticker is used, we estimate
        // the fee in such a way that it will always be sufficient).
        // Withdraw operation has more chunks, so we estimate fee for it.
        let mut fee = self.withdraw_fee(&self.main_account.zk_acc).await;

        // To be sure that we will have enough funds for all the transfers,
        // we will request 1.2x of the suggested fees. All the unspent funds
        // will be withdrawn later.
        fee = fee * BigUint::from(120u32) / BigUint::from(100u32);

        // And after that we have to make the fee packable.
        fee = closest_packable_fee_amount(&fee);

        self.estimated_fee_for_op = fee.clone();

        Ok(())
    }

    /// Runs the initial deposit of the money onto the main account.
    async fn deposit(&mut self) -> Result<(), failure::Error> {
        // Amount of money we need to deposit.
        // Initialize it with the raw amount: only sum of transfers per account.
        // Fees are taken into account below.
        let mut amount_to_deposit =
            self.transfer_size.clone() * BigUint::from(self.n_accounts as u64);

        // Count the fees: we need to provide fee for each of initial transfer transactions,
        // for each funds rotating transaction, and for each withdraw transaction.

        // Sum of fees for one tx per every account.
        let fee_for_all_accounts =
            self.estimated_fee_for_op.clone() * BigUint::from(self.n_accounts as u64);
        // Total amount of cycles is amount of funds rotation cycles + one for initial transfers +
        // one for collecting funds back to the main account.
        amount_to_deposit += fee_for_all_accounts * (self.cycles_amount + 2);
        // Also the fee is required to perform a final withdraw
        amount_to_deposit += self.estimated_fee_for_op.clone();

        let account_balance = self.main_account.eth_acc.eth_balance().await?;
        log::info!(
            "Main account ETH balance: {} ETH",
            format_ether(&account_balance)
        );

        log::info!(
            "Starting depositing phase. Depositing {} ETH to the main account",
            format_ether(&amount_to_deposit)
        );

        // Ensure that account does have enough money.
        if amount_to_deposit > account_balance {
            panic!("Main ETH account does not have enough balance to run the test with the provided config");
        }

        // Deposit funds and wait for operation to be executed.
        deposit_single(&self.main_account, amount_to_deposit, &self.rpc_client).await?;

        log::info!("Deposit sent and verified");

        // Now when deposits are done it is time to update account id.
        self.main_account
            .update_account_id(&self.rpc_client)
            .await?;

        log::info!("Main account ID set");

        // ...and change the main account pubkey.
        // We have to change pubkey after the deposit so we'll be able to use corresponding
        // `zkSync` account.
        let (change_pubkey_tx, eth_sign) = (self.main_account.sign_change_pubkey(), None);
        let mut sent_txs = SentTransactions::new();
        let tx_hash = self.rpc_client.send_tx(change_pubkey_tx, eth_sign).await?;
        sent_txs.add_tx_hash(tx_hash);
        wait_for_verify(sent_txs, self.verify_timeout, &self.rpc_client).await?;

        log::info!("Main account pubkey changed");

        log::info!("Deposit phase completed");

        Ok(())
    }

    /// Splits the money from the main account between the intermediate accounts
    /// with the `TransferToNew` operations.
    async fn initial_transfer(&mut self) -> Result<(), failure::Error> {
        log::info!(
            "Starting initial transfer. {} ETH will be send to each of {} new accounts",
            format_ether(&self.transfer_size),
            self.n_accounts
        );

        let mut signed_transfers = Vec::with_capacity(self.n_accounts);

        for to_idx in 0..self.n_accounts {
            let from_acc = &self.main_account.zk_acc;
            let to_acc = &self.accounts[to_idx];

            // Transfer size is (transfer_amount) + (fee for every tx to be sent) + (fee for final transfer
            // back to the main account).
            let transfer_amount = self.transfer_size.clone()
                + self.estimated_fee_for_op.clone() * (self.cycles_amount + 1);

            // Make amount packable.
            let packable_transfer_amount = closest_packable_fee_amount(&transfer_amount);

            // Fee for the transfer itself differs from the estimated fee.
            let fee = self.transfer_fee(&to_acc).await;
            let transfer = self.sign_transfer(from_acc, to_acc, packable_transfer_amount, fee);

            signed_transfers.push(transfer);
        }

        log::info!("Signed all the initial transfer transactions, sending");

        // Send txs by batches that can fit in one block.
        let to_verify = signed_transfers.len();
        let mut verified = 0;
        let txs_chunks = DynamicChunks::new(signed_transfers, &self.block_sizes);
        for tx_batch in txs_chunks {
            let mut sent_txs = SentTransactions::new();
            // Send each tx.
            // This has to be done synchronously, since we're sending from the same account
            // and truly async sending will result in a nonce mismatch errors.
            for (tx, eth_sign) in tx_batch {
                let tx_hash = self
                    .rpc_client
                    .send_tx(tx.clone(), eth_sign.clone())
                    .await?;
                sent_txs.add_tx_hash(tx_hash);
            }

            let sent_txs_amount = sent_txs.len();
            verified += sent_txs_amount;

            // Wait until all the transactions are verified.
            wait_for_verify(sent_txs, self.verify_timeout, &self.rpc_client).await?;

            log::info!(
                "Sent and verified {}/{} txs ({} on this iteration)",
                verified,
                to_verify,
                sent_txs_amount
            );
        }

        log::info!("All the initial transfers are completed");
        log::info!("Updating the accounts info and changing their public keys");

        // After all the initial transfer completed, we have to update new account IDs
        // and change public keys of accounts (so we'll be able to send transfers from them).
        let mut tx_futures = vec![];
        for account in self.accounts.iter() {
            let resp = self
                .rpc_client
                .account_state_info(account.address)
                .await
                .expect("rpc error");
            assert!(resp.id.is_some(), "Account ID is none for new account");
            account.set_account_id(resp.id);

            let change_pubkey_tx = FranklinTx::ChangePubKey(Box::new(
                account.create_change_pubkey_tx(None, true, false),
            ));

            let tx_future = self.rpc_client.send_tx(change_pubkey_tx, None);

            tx_futures.push(tx_future);
        }
        let mut sent_txs = SentTransactions::new();
        sent_txs.tx_hashes = try_join_all(tx_futures).await?;

        // Calculate the estimated amount of blocks for all the txs to be processed.
        let max_block_size = *self.block_sizes.iter().max().unwrap();
        let n_blocks = (self.accounts.len() / max_block_size + 1) as u32;
        wait_for_verify(sent_txs, self.verify_timeout * n_blocks, &self.rpc_client).await?;

        log::info!("All the accounts are prepared");

        log::info!("Initial transfers are sent and verified");

        Ok(())
    }

    /// Performs the funds rotation phase: transfers the money between intermediate
    /// accounts multiple times.
    /// Sine the money amount is always the same, after execution of this step every
    /// intermediate account should have the same balance as it has before.
    async fn funds_rotation(&mut self) -> Result<(), failure::Error> {
        for step_number in 1..=self.cycles_amount {
            log::info!("Starting funds rotation cycle {}", step_number);

            self.funds_rotation_step().await?;
        }

        Ok(())
    }

    /// Transfers the money between intermediate accounts. For each account with
    /// ID `N`, money are transferred to the account with ID `N + 1`.
    async fn funds_rotation_step(&mut self) -> Result<(), failure::Error> {
        let mut signed_transfers = Vec::with_capacity(self.n_accounts);

        for from_id in 0..self.n_accounts {
            let from_acc = &self.accounts[from_id];
            let to_id = self.acc_for_transfer(from_id);
            let to_acc = &self.accounts[to_id];

            let fee = self.transfer_fee(&to_acc).await;
            let transfer = self.sign_transfer(from_acc, to_acc, self.transfer_size.clone(), fee);

            signed_transfers.push(transfer);
        }

        log::info!("Signed transfers, sending");

        // Send txs by batches that can fit in one block.
        let to_verify = signed_transfers.len();
        let mut verified = 0;
        let txs_chunks = DynamicChunks::new(signed_transfers, &self.block_sizes);
        for tx_batch in txs_chunks {
            let mut tx_futures = vec![];
            // Send each tx.
            for (tx, eth_sign) in tx_batch {
                let tx_future = self.rpc_client.send_tx(tx.clone(), eth_sign.clone());

                tx_futures.push(tx_future);
            }
            let mut sent_txs = SentTransactions::new();
            sent_txs.tx_hashes = try_join_all(tx_futures).await?;

            let sent_txs_amount = sent_txs.len();
            verified += sent_txs_amount;

            // Wait until all the transactions are verified.
            wait_for_verify(sent_txs, self.verify_timeout, &self.rpc_client).await?;

            log::info!(
                "Sent and verified {}/{} txs ({} on this iteration)",
                verified,
                to_verify,
                sent_txs_amount
            );
        }

        log::info!("Transfers are sent and verified");

        Ok(())
    }

    /// Transfers all the money from the intermediate accounts back to the main account.
    async fn collect_funds(&mut self) -> Result<(), failure::Error> {
        log::info!("Starting collecting funds back to the main account");

        let mut signed_transfers = Vec::with_capacity(self.n_accounts);

        for from_id in 0..self.n_accounts {
            let from_acc = &self.accounts[from_id];
            let to_acc = &self.main_account.zk_acc;

            let fee = self.transfer_fee(&to_acc).await;

            let comitted_account_state = self
                .rpc_client
                .account_state_info(from_acc.address)
                .await?
                .committed;
            let account_balance = comitted_account_state.balances["ETH"].0.clone();
            let transfer_amount = &account_balance - &fee;
            let transfer_amount = closest_packable_token_amount(&transfer_amount);
            let transfer = self.sign_transfer(from_acc, to_acc, transfer_amount, fee);

            signed_transfers.push(transfer);
        }

        log::info!("Signed transfers, sending");

        // Send txs by batches that can fit in one block.
        let to_verify = signed_transfers.len();
        let mut verified = 0;
        let txs_chunks = DynamicChunks::new(signed_transfers, &self.block_sizes);
        for tx_batch in txs_chunks {
            let mut sent_txs = SentTransactions::new();
            // Send each tx.
            for (tx, eth_sign) in tx_batch {
                let tx_hash = self
                    .rpc_client
                    .send_tx(tx.clone(), eth_sign.clone())
                    .await?;
                sent_txs.add_tx_hash(tx_hash);
            }

            let sent_txs_amount = sent_txs.len();
            verified += sent_txs_amount;

            // Wait until all the transactions are verified.
            wait_for_verify(sent_txs, self.verify_timeout, &self.rpc_client).await?;

            log::info!(
                "Sent and verified {}/{} txs ({} on this iteration)",
                verified,
                to_verify,
                sent_txs_amount
            );
        }

        log::info!("Collecting funds completed");
        Ok(())
    }

    /// Withdraws the money from the main account back to the Ethereum.
    async fn withdraw(&mut self) -> Result<(), failure::Error> {
        let current_balance = self.main_account.eth_acc.eth_balance().await?;

        let fee = self.withdraw_fee(&self.main_account.zk_acc).await;

        let comitted_account_state = self
            .rpc_client
            .account_state_info(self.main_account.zk_acc.address)
            .await?
            .committed;
        let account_balance = comitted_account_state.balances["ETH"].0.clone();
        let withdraw_amount = &account_balance - &fee;
        let withdraw_amount = closest_packable_token_amount(&withdraw_amount);

        log::info!(
            "Starting withdrawing phase. Withdrawing {} ETH back to the Ethereum",
            format_ether(&withdraw_amount)
        );

        let (tx, eth_sign) = self
            .main_account
            .sign_withdraw(withdraw_amount.clone(), fee);
        let tx_hash = self
            .rpc_client
            .send_tx(tx.clone(), eth_sign.clone())
            .await?;
        let mut sent_txs = SentTransactions::new();
        sent_txs.add_tx_hash(tx_hash);

        wait_for_verify(sent_txs, self.verify_timeout, &self.rpc_client).await?;

        log::info!("Withdrawing funds completed");

        self.wait_for_eth_balance(current_balance, withdraw_amount)
            .await?;

        Ok(())
    }

    async fn finish(&mut self) -> Result<(), failure::Error> {
        Ok(())
    }

    /// Waits for main ETH account to receive funds on its balance.
    /// Returns an error if funds are not received within a reasonable amount of time.
    async fn wait_for_eth_balance(
        &self,
        current_balance: BigUint,
        withdraw_amount: BigUint,
    ) -> Result<(), failure::Error> {
        log::info!("Awaiting for ETH funds to be received");

        let expected_balance = current_balance + withdraw_amount;

        let timeout_minutes = 10;
        let timeout = Duration::from_secs(timeout_minutes * 60);
        let start = Instant::now();

        let polling_interval = Duration::from_millis(250);
        let mut timer = time::interval(polling_interval);

        loop {
            let current_balance = self.main_account.eth_acc.eth_balance().await?;
            if current_balance == expected_balance {
                break;
            }
            if start.elapsed() > timeout {
                failure::bail!(
                    "ETH funds were not received for {} minutes",
                    timeout_minutes
                );
            }
            timer.tick().await;
        }

        log::info!("ETH funds received");
        Ok(())
    }

    /// Obtains a fee required for the transfer operation.
    async fn transfer_fee(&self, to_acc: &ZksyncAccount) -> BigUint {
        let fee = self
            .rpc_client
            .get_tx_fee("Transfer", to_acc.address, "ETH")
            .await
            .expect("Can't get tx fee");

        closest_packable_fee_amount(&fee)
    }

    /// Obtains a fee required for the withdraw operation.
    async fn withdraw_fee(&self, to_acc: &ZksyncAccount) -> BigUint {
        let fee = self
            .rpc_client
            .get_tx_fee("Withdraw", to_acc.address, "ETH")
            .await
            .expect("Can't get tx fee");

        closest_packable_fee_amount(&fee)
    }

    /// Creates a signed transfer transaction.
    /// Sender and receiver are chosen from the generated
    /// accounts, determined by its indices.
    fn sign_transfer(
        &self,
        from: &ZksyncAccount,
        to: &ZksyncAccount,
        amount: impl Into<BigUint>,
        fee: impl Into<BigUint>,
    ) -> (FranklinTx, Option<PackedEthSignature>) {
        let (tx, eth_signature) = from.sign_transfer(
            0, // ETH
            "ETH",
            amount.into(),
            fee.into(),
            &to.address,
            None,
            true,
        );

        (FranklinTx::Transfer(Box::new(tx)), Some(eth_signature))
    }

    /// Generates an ID for funds transfer. The ID is the ID of the next
    /// account, treating the accounts array like a circle buffer:
    /// given 3 accounts, IDs returned for queries (0, 1, 2) will be
    /// (1, 2, 0) correspondingly.
    fn acc_for_transfer(&self, from_idx: usize) -> usize {
        (from_idx + 1) % self.accounts.len()
    }

    /// Load block sizes to use in test for generated blocks.
    /// This method assumes that loadtest and server share the same env config,
    /// since the value is loaded from the env.
    fn get_block_sizes(use_all_block_sizes: bool) -> Vec<usize> {
        let options = ConfigurationOptions::from_env();
        if use_all_block_sizes {
            // Load all the supported block sizes.
            options.available_block_chunk_sizes
        } else {
            // Use only the max block size (for more quick execution).
            let max_size = *options.available_block_chunk_sizes.iter().max().unwrap();

            vec![max_size]
        }
    }
}

/// Runs the real-life test scenario.
/// For description, see the module doc-comment.
pub fn run_scenario(mut ctx: ScenarioContext) {
    let rpc_addr = ctx.rpc_addr.clone();
    let rpc_client = RpcClient::new(&rpc_addr);

    let mut scenario = ScenarioExecutor::new(&ctx, rpc_client);

    // Run the scenario.
    log::info!("Starting the real-life test");
    ctx.rt.block_on(scenario.run());
}
