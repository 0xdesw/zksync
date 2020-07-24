use super::FranklinOp;
use super::FranklinTx;
use super::PriorityOp;
use super::{AccountId, BlockNumber, Fr};
use crate::franklin_crypto::bellman::pairing::ff::{PrimeField, PrimeFieldRepr};
use crate::node::BlockTimestamp;
use crate::params::CHUNK_BIT_WIDTH;
use crate::serialization::*;
use chrono::DateTime;
use chrono::Utc;
use web3::types::{H256, U256};

#[derive(Clone, Debug)]
pub struct PendingBlock {
    pub number: u32,
    pub chunks_left: usize,
    pub unprocessed_priority_op_before: u64,
    pub pending_block_iteration: usize,
    pub success_operations: Vec<ExecutedOperations>,
    pub block_timestamp: BlockTimestamp,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutedTx {
    pub tx: FranklinTx,
    pub success: bool,
    pub op: Option<FranklinOp>,
    pub fail_reason: Option<String>,
    pub block_index: Option<u32>,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExecutedPriorityOp {
    pub priority_op: PriorityOp,
    pub op: FranklinOp,
    pub block_index: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ExecutedOperations {
    Tx(Box<ExecutedTx>),
    PriorityOp(Box<ExecutedPriorityOp>),
}

impl ExecutedOperations {
    pub fn get_executed_op(&self) -> Option<&FranklinOp> {
        match self {
            ExecutedOperations::Tx(exec_tx) => exec_tx.op.as_ref(),
            ExecutedOperations::PriorityOp(exec_op) => Some(&exec_op.op),
        }
    }

    pub fn get_executed_tx(&self) -> Option<&ExecutedTx> {
        match self {
            ExecutedOperations::Tx(exec_tx) => Some(exec_tx),
            ExecutedOperations::PriorityOp(_) => None,
        }
    }

    pub fn get_eth_public_data(&self) -> Vec<u8> {
        self.get_executed_op()
            .map(FranklinOp::public_data)
            .unwrap_or_default()
    }

    pub fn get_eth_witness_bytes(&self) -> Option<Vec<u8>> {
        self.get_executed_op()
            .map(|op| op.eth_witness().unwrap_or_else(Vec::new))
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Block {
    pub block_number: BlockNumber,
    #[serde(with = "FrSerde")]
    pub new_root_hash: Fr,
    pub fee_account: AccountId,
    // Can be None if the timestamp is not known at some moment of processing
    // or block has been created before this field is added
    pub block_timestamp: Option<BlockTimestamp>,
    pub block_transactions: Vec<ExecutedOperations>,
    /// (unprocessed prior op id before block, unprocessed prior op id after block)
    pub processed_priority_ops: (u64, u64),
    // actual block chunks sizes that will be used on contract, `block_chunks_sizes >= block.chunks_used()`
    pub block_chunks_size: usize,

    /// Gas limit to be set for the Commit Ethereum transaction.
    pub commit_gas_limit: U256,
    /// Gas limit to be set for the Verify Ethereum transaction.
    pub verify_gas_limit: U256,
}

impl Block {
    // Constructor
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        block_number: BlockNumber,
        new_root_hash: Fr,
        fee_account: AccountId,
        block_timestamp: Option<BlockTimestamp>,
        block_transactions: Vec<ExecutedOperations>,
        processed_priority_ops: (u64, u64),
        block_chunks_size: usize,
        commit_gas_limit: U256,
        verify_gas_limit: U256,
    ) -> Self {
        Self {
            block_number,
            new_root_hash,
            fee_account,
            block_timestamp,
            block_transactions,
            processed_priority_ops,
            block_chunks_size,
            commit_gas_limit,
            verify_gas_limit,
        }
    }

    /// Constructor that determines smallest block size for the given block
    #[allow(clippy::too_many_arguments)]
    pub fn new_from_availabe_block_sizes(
        block_number: BlockNumber,
        new_root_hash: Fr,
        fee_account: AccountId,
        block_timestamp: Option<BlockTimestamp>,
        block_transactions: Vec<ExecutedOperations>,
        processed_priority_ops: (u64, u64),
        available_block_chunks_sizes: &[usize],
        commit_gas_limit: U256,
        verify_gas_limit: U256,
    ) -> Self {
        let mut block = Self {
            block_number,
            new_root_hash,
            fee_account,
            block_timestamp,
            block_transactions,
            processed_priority_ops,
            block_chunks_size: 0,
            commit_gas_limit,
            verify_gas_limit,
        };
        block.block_chunks_size = block.smallest_block_size(available_block_chunks_sizes);
        block
    }

    pub fn get_eth_encoded_root(&self) -> H256 {
        let mut be_bytes = [0u8; 32];
        self.new_root_hash
            .into_repr()
            .write_be(be_bytes.as_mut())
            .expect("Write commit bytes");
        H256::from(be_bytes)
    }
    pub fn get_eth_public_data(&self) -> Vec<u8> {
        let mut executed_tx_pub_data = self
            .block_transactions
            .iter()
            .filter_map(ExecutedOperations::get_executed_op)
            .flat_map(FranklinOp::public_data)
            .collect::<Vec<_>>();

        // Pad block with noops.
        executed_tx_pub_data.resize(self.block_chunks_size * CHUNK_BIT_WIDTH / 8, 0x00);

        executed_tx_pub_data
    }

    /// Returns eth_witness data and bytes used by each operation which needed them
    pub fn get_eth_witness_data(&self) -> (Vec<u8>, Vec<u64>) {
        let mut eth_witness = Vec::new();
        let mut used_bytes = Vec::new();

        for block_tx in &self.block_transactions {
            if let Some(franklin_op) = block_tx.get_executed_op() {
                if let Some(witness_bytes) = franklin_op.eth_witness() {
                    used_bytes.push(witness_bytes.len() as u64);
                    eth_witness.extend(witness_bytes.into_iter());
                }
            }
        }

        (eth_witness, used_bytes)
    }

    pub fn number_of_processed_prior_ops(&self) -> u64 {
        self.processed_priority_ops.1 - self.processed_priority_ops.0
    }

    fn chunks_used(&self) -> usize {
        self.block_transactions
            .iter()
            .filter_map(ExecutedOperations::get_executed_op)
            .map(FranklinOp::chunks)
            .sum()
    }

    fn smallest_block_size(&self, available_block_sizes: &[usize]) -> usize {
        let chunks_used = self.chunks_used();
        smallest_block_size_for_chunks(chunks_used, available_block_sizes)
    }

    pub fn get_withdrawals_data(&self) -> Vec<u8> {
        let mut withdrawals_data = Vec::new();

        for block_tx in &self.block_transactions {
            if let Some(franklin_op) = block_tx.get_executed_op() {
                if let Some(withdrawal_data) = franklin_op.withdrawal_data() {
                    withdrawals_data.extend(&withdrawal_data);
                }
            }
        }

        withdrawals_data
    }
}

// Get smallest block size given
pub fn smallest_block_size_for_chunks(
    chunks_used: usize,
    available_block_sizes: &[usize],
) -> usize {
    for &block_size in available_block_sizes {
        if block_size >= chunks_used {
            return block_size;
        }
    }
    panic!(
        "Provided chunks amount ({}) cannot fit in one block, maximum available size is {}",
        chunks_used,
        available_block_sizes.last().unwrap()
    );
}
