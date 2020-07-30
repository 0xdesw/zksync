// External imports
// Workspace imports
use models::ActionType;
// Local imports
use crate::tests::db_test;
use crate::{
    chain::{
        block::BlockSchema,
        operations::{
            records::{NewExecutedPriorityOperation, NewExecutedTransaction, NewOperation},
            OperationsSchema,
        },
    },
    StorageProcessor,
};

/// Checks the save&load routine for unconfirmed operations.
#[test]
#[cfg_attr(not(feature = "db_test"), ignore)]
fn operations() {
    let conn = StorageProcessor::establish_connection().unwrap();
    db_test(conn.conn(), || {
        let block_number = 1;
        let action_type = ActionType::COMMIT;
        OperationsSchema(&conn).store_operation(NewOperation {
            block_number,
            action_type: action_type.to_string(),
        })?;

        let stored_operation = OperationsSchema(&conn)
            .get_operation(block_number as u32, action_type)
            .expect("Can't get the operation");

        assert_eq!(stored_operation.block_number, 1);
        assert_eq!(stored_operation.action_type, action_type.to_string());
        assert_eq!(stored_operation.confirmed, false);

        Ok(())
    });
}

/// Checks the save&load routine for executed operations.
#[test]
#[cfg_attr(not(feature = "db_test"), ignore)]
fn executed_operations() {
    let conn = StorageProcessor::establish_connection().unwrap();
    db_test(conn.conn(), || {
        let executed_tx = NewExecutedTransaction {
            block_number: 1,
            tx_hash: vec![0xDE, 0xAD, 0xBE, 0xEF],
            tx: Default::default(),
            operation: Default::default(),
            from_account: Default::default(),
            to_account: None,
            success: true,
            fail_reason: None,
            block_index: None,
            primary_account_address: Default::default(),
            nonce: Default::default(),
            created_at: chrono::Utc::now(),
            eth_sign_data: None,
        };

        OperationsSchema(&conn).store_executed_operation(executed_tx.clone())?;

        let stored_operation = OperationsSchema(&conn)
            .get_executed_operation(executed_tx.tx_hash.as_ref())?
            .expect("No operation was found for a valid hash");

        assert_eq!(stored_operation.block_number, executed_tx.block_number);
        assert_eq!(stored_operation.tx_hash, executed_tx.tx_hash);
        assert_eq!(stored_operation.tx, executed_tx.tx);
        assert_eq!(stored_operation.operation, executed_tx.operation);
        assert_eq!(stored_operation.from_account, executed_tx.from_account);
        assert_eq!(stored_operation.to_account, executed_tx.to_account);
        assert_eq!(stored_operation.success, executed_tx.success);
        assert_eq!(stored_operation.fail_reason, executed_tx.fail_reason);
        assert_eq!(stored_operation.block_index, executed_tx.block_index);
        assert_eq!(stored_operation.nonce, executed_tx.nonce);
        assert_eq!(
            stored_operation.primary_account_address,
            executed_tx.primary_account_address
        );

        Ok(())
    });
}

/// Checks the save&load routine for executed priority operations.
#[test]
#[cfg_attr(not(feature = "db_test"), ignore)]
fn executed_priority_operations() {
    let conn = StorageProcessor::establish_connection().unwrap();
    db_test(conn.conn(), || {
        let executed_tx = NewExecutedPriorityOperation {
            block_number: 1,
            block_index: 1,
            operation: Default::default(),
            from_account: Default::default(),
            to_account: Default::default(),
            priority_op_serialid: 0,
            deadline_block: 100,
            eth_hash: vec![0xDE, 0xAD, 0xBE, 0xEF],
            eth_block: 10,
            created_at: chrono::Utc::now(),
        };
        OperationsSchema(&conn).store_executed_priority_operation(executed_tx.clone())?;

        let stored_operation = OperationsSchema(&conn)
            .get_executed_priority_operation(executed_tx.priority_op_serialid as u32)?
            .expect("No operation was found for a valid hash");

        assert_eq!(stored_operation.block_number, executed_tx.block_number);
        assert_eq!(stored_operation.block_index, executed_tx.block_index);
        assert_eq!(stored_operation.operation, executed_tx.operation);
        assert_eq!(stored_operation.from_account, executed_tx.from_account);
        assert_eq!(stored_operation.to_account, executed_tx.to_account);
        assert_eq!(
            stored_operation.priority_op_serialid,
            executed_tx.priority_op_serialid
        );
        assert_eq!(stored_operation.deadline_block, executed_tx.deadline_block);
        assert_eq!(stored_operation.eth_hash, executed_tx.eth_hash);

        Ok(())
    });
}

/// Checks that attempt to save the duplicate txs is ignored by the DB.
#[test]
#[cfg_attr(not(feature = "db_test"), ignore)]
fn duplicated_operations() {
    const BLOCK_NUMBER: i64 = 1;

    let executed_tx = NewExecutedTransaction {
        block_number: BLOCK_NUMBER,
        tx_hash: vec![0x12, 0xAD, 0xBE, 0xEF],
        tx: Default::default(),
        operation: Default::default(),
        from_account: Default::default(),
        to_account: None,
        success: true,
        fail_reason: None,
        block_index: None,
        primary_account_address: Default::default(),
        nonce: Default::default(),
        created_at: chrono::Utc::now(),
        eth_sign_data: None,
    };

    let executed_priority_op = NewExecutedPriorityOperation {
        block_number: BLOCK_NUMBER,
        block_index: 1,
        operation: Default::default(),
        from_account: Default::default(),
        to_account: Default::default(),
        priority_op_serialid: 0,
        deadline_block: 100,
        eth_hash: vec![0xDE, 0xAD, 0xBE, 0xEF],
        eth_block: 10,
        created_at: chrono::Utc::now(),
    };

    let conn = StorageProcessor::establish_connection().unwrap();
    db_test(conn.conn(), || {
        // Save the same operations twice.
        OperationsSchema(&conn).store_executed_operation(executed_tx.clone())?;
        OperationsSchema(&conn).store_executed_operation(executed_tx.clone())?;
        OperationsSchema(&conn).store_executed_priority_operation(executed_priority_op.clone())?;
        OperationsSchema(&conn).store_executed_priority_operation(executed_priority_op.clone())?;

        // Check that we can still load it.
        assert!(OperationsSchema(&conn)
            .get_executed_operation(executed_tx.tx_hash.as_ref())?
            .is_some());
        assert!(OperationsSchema(&conn)
            .get_executed_priority_operation(executed_priority_op.priority_op_serialid as u32)?
            .is_some());

        // Get the block transactions and check if there are exactly 2 txs.
        let block_txs = BlockSchema(&conn).get_block_transactions(BLOCK_NUMBER as u32)?;

        assert_eq!(block_txs.len(), 2);

        Ok(())
    });
}
