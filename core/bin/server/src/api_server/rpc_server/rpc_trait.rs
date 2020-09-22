use std::collections::HashMap;
// External uses
use futures::{FutureExt, TryFutureExt};
use jsonrpc_core::Error;
use jsonrpc_derive::rpc;
// Workspace uses
use models::node::{
    tx::{TxEthSignature, TxHash},
    Address, FranklinTx, Token, TokenLike, TxFeeTypes,
};
// use storage::{
//     chain::{
//         block::records::BlockDetails, operations::records::StoredExecutedPriorityOperation,
//         operations_ext::records::TxReceiptResponse,
//     },
//     ConnectionPool, StorageProcessor,
// };

// Local uses
use crate::fee_ticker::{BatchFee, Fee};
use bigdecimal::BigDecimal;

use super::{types::*, RpcApp};
use std::time::Instant;

pub type FutureResp<T> = Box<dyn futures01::Future<Item = T, Error = Error> + Send>;

#[rpc]
pub trait Rpc {
    #[rpc(name = "account_info", returns = "AccountInfoResp")]
    fn account_info(&self, addr: Address) -> FutureResp<AccountInfoResp>;

    #[rpc(name = "ethop_info", returns = "ETHOpInfoResp")]
    fn ethop_info(&self, serial_id: u32) -> FutureResp<ETHOpInfoResp>;

    #[rpc(name = "tx_info", returns = "ETHOpInfoResp")]
    fn tx_info(&self, hash: TxHash) -> FutureResp<TransactionInfoResp>;

    #[rpc(name = "tx_submit", returns = "TxHash")]
    fn tx_submit(
        &self,
        tx: Box<FranklinTx>,
        signature: Box<Option<TxEthSignature>>,
        fast_processing: Option<bool>,
    ) -> FutureResp<TxHash>;

    #[rpc(name = "submit_txs_batch", returns = "Vec<TxHash>")]
    fn submit_txs_batch(&self, txs: Vec<TxWithSignature>) -> FutureResp<Vec<TxHash>>;

    #[rpc(name = "contract_address", returns = "ContractAddressResp")]
    fn contract_address(&self) -> FutureResp<ContractAddressResp>;

    /// "ETH" | #ERC20_ADDRESS => {Token}
    #[rpc(name = "tokens", returns = "Token")]
    fn tokens(&self) -> FutureResp<HashMap<String, Token>>;

    #[rpc(name = "get_tx_fee", returns = "Fee")]
    fn get_tx_fee(
        &self,
        tx_type: TxFeeTypes,
        address: Address,
        token_like: TokenLike,
    ) -> FutureResp<Fee>;

    #[rpc(name = "get_txs_batch_fee_in_wei", returns = "BatchFee")]
    fn get_txs_batch_fee_in_wei(
        &self,
        tx_types: Vec<TxFeeTypes>,
        addresses: Vec<Address>,
        token_like: TokenLike,
    ) -> FutureResp<BatchFee>;

    #[rpc(name = "get_token_price", returns = "BigDecimal")]
    fn get_token_price(&self, token_like: TokenLike) -> FutureResp<BigDecimal>;

    #[rpc(name = "get_confirmations_for_eth_op_amount", returns = "u64")]
    fn get_confirmations_for_eth_op_amount(&self) -> FutureResp<u64>;
}

impl Rpc for RpcApp {
    fn account_info(&self, addr: Address) -> FutureResp<AccountInfoResp> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle.spawn(self_._impl_account_info(addr)).await.unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn ethop_info(&self, serial_id: u32) -> FutureResp<ETHOpInfoResp> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle
                .spawn(self_._impl_ethop_info(serial_id))
                .await
                .unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn tx_info(&self, hash: TxHash) -> FutureResp<TransactionInfoResp> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle.spawn(self_._impl_tx_info(hash)).await.unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn tx_submit(
        &self,
        tx: Box<FranklinTx>,
        signature: Box<Option<TxEthSignature>>,
        fast_processing: Option<bool>,
    ) -> FutureResp<TxHash> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle
                .spawn(self_._impl_tx_submit(tx, signature, fast_processing))
                .await
                .unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn submit_txs_batch(&self, txs: Vec<TxWithSignature>) -> FutureResp<Vec<TxHash>> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle
                .spawn(self_._impl_submit_txs_batch(txs))
                .await
                .unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn contract_address(&self) -> FutureResp<ContractAddressResp> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle.spawn(self_._impl_contract_address()).await.unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn tokens(&self) -> FutureResp<HashMap<String, Token>> {
        let timer = Instant::now();
        let self_ = self.clone();
        log::trace!("Clone timer: {} ms", timer.elapsed().as_millis());
        let resp = async move {
            let timer = Instant::now();
            let handle = self_.tokio_runtime.clone();
            let res = handle.spawn(self_._impl_tokens()).await.unwrap();
            log::trace!("Token impl timer: {} ms", timer.elapsed().as_millis());
            res
        };
        Box::new(resp.boxed().compat())
    }

    fn get_tx_fee(
        &self,
        tx_type: TxFeeTypes,
        address: Address,
        token_like: TokenLike,
    ) -> FutureResp<Fee> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle
                .spawn(self_._impl_get_tx_fee(tx_type, address, token_like))
                .await
                .unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn get_txs_batch_fee_in_wei(
        &self,
        tx_types: Vec<TxFeeTypes>,
        addresses: Vec<Address>,
        token_like: TokenLike,
    ) -> FutureResp<BatchFee> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle
                .spawn(self_._impl_get_txs_batch_fee_in_wei(tx_types, addresses, token_like))
                .await
                .unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn get_token_price(&self, token_like: TokenLike) -> FutureResp<BigDecimal> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle
                .spawn(self_._impl_get_token_price(token_like))
                .await
                .unwrap()
        };
        Box::new(resp.boxed().compat())
    }

    fn get_confirmations_for_eth_op_amount(&self) -> FutureResp<u64> {
        let self_ = self.clone();
        let resp = async move {
            let handle = self_.tokio_runtime.clone();
            handle
                .spawn(self_._impl_get_confirmations_for_eth_op_amount())
                .await
                .unwrap()
        };
        Box::new(resp.boxed().compat())
    }
}
