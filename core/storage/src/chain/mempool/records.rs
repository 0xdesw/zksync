// External imports
// Workspace imports
// Local imports
use crate::schema::*;
use chrono::{DateTime, Utc};

#[derive(Debug, Queryable, QueryableByName)]
#[table_name = "mempool_txs"]
pub struct MempoolTx {
    pub id: i64,
    pub tx_hash: String,
    pub tx: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub eth_sign_data: Option<serde_json::Value>,
    pub batch_id: Option<i64>,
}

#[derive(Debug, Insertable)]
#[table_name = "mempool_txs"]
pub struct NewMempoolTx {
    pub tx_hash: String,
    pub tx: serde_json::Value,
    pub created_at: DateTime<Utc>,
    pub eth_sign_data: Option<serde_json::Value>,
    pub batch_id: Option<i64>,
}
