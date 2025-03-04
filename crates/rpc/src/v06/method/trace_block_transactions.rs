use anyhow::Context;
use pathfinder_common::{BlockId, TransactionHash};
use pathfinder_executor::{ExecutionState, TransactionExecutionError};
use serde::{Deserialize, Serialize};
use starknet_gateway_client::GatewayApi;
use starknet_gateway_types::trace::TransactionTrace as GatewayTxTrace;

use crate::executor::VERSIONS_LOWER_THAN_THIS_SHOULD_FALL_BACK_TO_FETCHING_TRACE_FROM_GATEWAY;
use crate::v06::method::simulate_transactions::dto::{
    DeclareTxnTrace, DeployAccountTxnTrace, ExecuteInvocation, InvokeTxnTrace, L1HandlerTxnTrace,
};
use crate::{compose_executor_transaction, context::RpcContext, executor::ExecutionStateError};

use starknet_gateway_types::reply::transaction::Transaction as GatewayTransaction;

use super::simulate_transactions::dto::TransactionTrace;

#[derive(Deserialize, Debug)]
#[serde(deny_unknown_fields)]
pub struct TraceBlockTransactionsInput {
    block_id: BlockId,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
pub struct Trace {
    pub transaction_hash: TransactionHash,
    pub trace_root: TransactionTrace,
}

#[derive(Debug, Serialize, Eq, PartialEq)]
pub struct TraceBlockTransactionsOutput(pub Vec<Trace>);

#[derive(Debug)]
pub enum TraceBlockTransactionsError {
    Internal(anyhow::Error),
    Custom(anyhow::Error),
    BlockNotFound,
}

impl From<anyhow::Error> for TraceBlockTransactionsError {
    fn from(value: anyhow::Error) -> Self {
        Self::Internal(value)
    }
}

impl From<TraceBlockTransactionsError> for crate::error::ApplicationError {
    fn from(value: TraceBlockTransactionsError) -> Self {
        match value {
            TraceBlockTransactionsError::Internal(e) => Self::Internal(e),
            TraceBlockTransactionsError::BlockNotFound => Self::BlockNotFound,
            TraceBlockTransactionsError::Custom(e) => Self::Custom(e),
        }
    }
}

impl From<ExecutionStateError> for TraceBlockTransactionsError {
    fn from(value: ExecutionStateError) -> Self {
        match value {
            ExecutionStateError::BlockNotFound => Self::BlockNotFound,
            ExecutionStateError::Internal(e) => Self::Internal(e),
        }
    }
}

impl From<TransactionExecutionError> for TraceBlockTransactionsError {
    fn from(value: TransactionExecutionError) -> Self {
        use TransactionExecutionError::*;
        match value {
            ExecutionError {
                transaction_index,
                error,
            } => Self::Custom(anyhow::anyhow!(
                "Transaction execution failed at index {}: {}",
                transaction_index,
                error
            )),
            Internal(e) => Self::Internal(e),
            Custom(e) => Self::Custom(e),
        }
    }
}

impl From<TraceConversionError> for TraceBlockTransactionsError {
    fn from(value: TraceConversionError) -> Self {
        Self::Custom(anyhow::anyhow!(value.0))
    }
}

pub(super) struct TraceConversionError(pub &'static str);

pub(crate) fn map_gateway_trace(
    transaction: GatewayTransaction,
    trace: GatewayTxTrace,
) -> Result<TransactionTrace, TraceConversionError> {
    Ok(match transaction {
        GatewayTransaction::Declare(_) => TransactionTrace::Declare(DeclareTxnTrace {
            fee_transfer_invocation: trace.fee_transfer_invocation.map(Into::into),
            validate_invocation: trace.validate_invocation.map(Into::into),
            state_diff: None,
        }),
        GatewayTransaction::Deploy(_) => TransactionTrace::DeployAccount(DeployAccountTxnTrace {
            constructor_invocation: trace
                .function_invocation
                .ok_or(TraceConversionError(
                    "Constructor_invocation is missing from trace response",
                ))?
                .into(),
            fee_transfer_invocation: trace.fee_transfer_invocation.map(Into::into),
            validate_invocation: trace.validate_invocation.map(Into::into),
            state_diff: None,
        }),
        GatewayTransaction::DeployAccount(_) => {
            TransactionTrace::DeployAccount(DeployAccountTxnTrace {
                constructor_invocation: trace
                    .function_invocation
                    .ok_or(TraceConversionError(
                        "constructor_invocation is missing from trace response",
                    ))?
                    .into(),
                fee_transfer_invocation: trace.fee_transfer_invocation.map(Into::into),
                validate_invocation: trace.validate_invocation.map(Into::into),
                state_diff: None,
            })
        }
        GatewayTransaction::Invoke(_) => TransactionTrace::Invoke(InvokeTxnTrace {
            execute_invocation: if let Some(revert_reason) = trace.revert_error {
                ExecuteInvocation::RevertedReason { revert_reason }
            } else {
                trace
                    .function_invocation
                    .map(|invocation| ExecuteInvocation::FunctionInvocation(invocation.into()))
                    .unwrap_or_else(|| ExecuteInvocation::Empty)
            },
            fee_transfer_invocation: trace.fee_transfer_invocation.map(Into::into),
            validate_invocation: trace.validate_invocation.map(Into::into),
            state_diff: None,
        }),
        GatewayTransaction::L1Handler(_) => TransactionTrace::L1Handler(L1HandlerTxnTrace {
            function_invocation: trace
                .function_invocation
                .ok_or(TraceConversionError(
                    "function_invocation is missing from trace response",
                ))?
                .into(),
            state_diff: None,
        }),
    })
}

pub async fn trace_block_transactions(
    context: RpcContext,
    input: TraceBlockTransactionsInput,
) -> Result<TraceBlockTransactionsOutput, TraceBlockTransactionsError> {
    enum LocalExecution {
        Success(Vec<Trace>),
        Unsupported(Vec<GatewayTransaction>),
    }

    let span = tracing::Span::current();

    let storage = context.storage.clone();
    let traces = tokio::task::spawn_blocking(move || {
        let _g = span.enter();

        let mut db = storage.connection()?;
        let db = db.transaction()?;

        let (header, transactions) = match input.block_id {
            BlockId::Pending => {
                let pending = context
                    .pending_data
                    .get(&db)
                    .context("Querying pending data")?;

                let header = pending.header();
                let transactions = pending.block.transactions.clone();

                (header, transactions)
            }
            other => {
                let block_id = other.try_into().expect("Only pending should fail");
                let header = db
                    .block_header(block_id)?
                    .ok_or(TraceBlockTransactionsError::BlockNotFound)?;

                let transactions = db
                    .transactions_for_block(block_id)?
                    .context("Transaction data missing")?;

                (header, transactions)
            }
        };

        let starknet_version = header
            .starknet_version
            .parse_as_semver()
            .context("Parsing starknet version")?
            .unwrap_or(semver::Version::new(0, 0, 0));
        if starknet_version
            < VERSIONS_LOWER_THAN_THIS_SHOULD_FALL_BACK_TO_FETCHING_TRACE_FROM_GATEWAY
        {
            match input.block_id {
                BlockId::Pending => {
                    return Err(TraceBlockTransactionsError::Internal(anyhow::anyhow!(
                        "Traces are not supported for pending blocks by the feeder gateway"
                    )))
                }
                _ => {
                    return Ok::<_, TraceBlockTransactionsError>(LocalExecution::Unsupported(
                        transactions,
                    ))
                }
            }
        }

        let transactions = transactions
            .iter()
            .map(|transaction| compose_executor_transaction(transaction, &db))
            .collect::<Result<Vec<_>, _>>()?;

        let state = ExecutionState::trace(&db, context.chain_id, header, None);
        let traces = pathfinder_executor::trace_all(state, transactions, true, true)?;

        let result = traces
            .into_iter()
            .map(|(hash, trace)| {
                Ok(Trace {
                    transaction_hash: hash,
                    trace_root: trace.try_into()?,
                })
            })
            .collect::<Result<Vec<_>, TraceBlockTransactionsError>>()?;

        Ok(LocalExecution::Success(result))
    })
    .await
    .context("trace_block_transactions: fetch block & transactions")??;

    let transactions = match traces {
        LocalExecution::Success(traces) => return Ok(TraceBlockTransactionsOutput(traces)),
        LocalExecution::Unsupported(transactions) => transactions,
    };

    context
        .sequencer
        .block_traces(input.block_id)
        .await
        .context("Forwarding to feeder gateway")
        .map_err(TraceBlockTransactionsError::from)
        .map(|trace| {
            Ok(TraceBlockTransactionsOutput(
                trace
                    .traces
                    .into_iter()
                    .zip(transactions.into_iter())
                    .map(|(trace, tx)| {
                        let transaction_hash = tx.hash();
                        let trace_root = map_gateway_trace(tx, trace)?;

                        Ok(Trace {
                            transaction_hash,
                            trace_root,
                        })
                    })
                    .collect::<Result<Vec<_>, TraceBlockTransactionsError>>()?,
            ))
        })?
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;

    use pathfinder_common::{
        block_hash, felt, BlockHeader, ChainId, GasPrice, SierraHash, StateUpdate, TransactionIndex,
    };
    use starknet_gateway_types::reply::transaction::{ExecutionStatus, Receipt};

    use super::*;

    pub(crate) async fn setup_multi_tx_trace_test(
    ) -> anyhow::Result<(RpcContext, BlockHeader, Vec<Trace>)> {
        use super::super::simulate_transactions::tests::fixtures;
        use super::super::simulate_transactions::tests::setup_storage;

        let (
            storage,
            last_block_header,
            account_contract_address,
            universal_deployer_address,
            test_storage_value,
        ) = setup_storage().await;
        let context = RpcContext::for_tests().with_storage(storage.clone());

        let transactions = vec![
            fixtures::input::declare(account_contract_address),
            fixtures::input::universal_deployer(
                account_contract_address,
                universal_deployer_address,
            ),
            fixtures::input::invoke(account_contract_address),
        ];

        let traces = vec![
            fixtures::expected_output::declare(account_contract_address, &last_block_header)
                .transaction_trace,
            fixtures::expected_output::universal_deployer(
                account_contract_address,
                &last_block_header,
                universal_deployer_address,
            )
            .transaction_trace,
            fixtures::expected_output::invoke(
                account_contract_address,
                &last_block_header,
                test_storage_value,
            )
            .transaction_trace,
        ];

        let next_block_header = {
            let mut db = storage.connection()?;
            let tx = db.transaction()?;

            tx.insert_sierra_class(
                &SierraHash(fixtures::SIERRA_HASH.0),
                fixtures::SIERRA_DEFINITION,
                &fixtures::CASM_HASH,
                fixtures::CASM_DEFINITION,
                "compiler version",
            )?;

            let next_block_header = BlockHeader::builder()
                .with_number(last_block_header.number + 1)
                .with_eth_l1_gas_price(GasPrice(1))
                .with_parent_hash(last_block_header.hash)
                .with_starknet_version(last_block_header.starknet_version)
                .with_sequencer_address(last_block_header.sequencer_address)
                .with_timestamp(last_block_header.timestamp)
                .finalize_with_hash(block_hash!("0x1"));
            tx.insert_block_header(&next_block_header)?;

            let dummy_receipt: Receipt = Receipt {
                actual_fee: None,
                events: vec![],
                execution_resources: None,
                l1_to_l2_consumed_message: None,
                l2_to_l1_messages: vec![],
                transaction_hash: TransactionHash(felt!("0x1")),
                transaction_index: TransactionIndex::new_or_panic(0),
                execution_status: ExecutionStatus::default(),
                revert_error: None,
            };
            tx.insert_transaction_data(
                next_block_header.hash,
                next_block_header.number,
                &[
                    (transactions[0].clone().into(), dummy_receipt.clone()),
                    (transactions[1].clone().into(), dummy_receipt.clone()),
                    (transactions[2].clone().into(), dummy_receipt.clone()),
                ],
            )?;
            tx.commit()?;

            next_block_header
        };

        let traces = vec![
            Trace {
                transaction_hash: transactions[0]
                    .transaction_hash(ChainId::GOERLI_TESTNET, Some(fixtures::SIERRA_HASH)),
                trace_root: traces[0].clone(),
            },
            Trace {
                transaction_hash: transactions[1].transaction_hash(ChainId::GOERLI_TESTNET, None),
                trace_root: traces[1].clone(),
            },
            Trace {
                transaction_hash: transactions[2].transaction_hash(ChainId::GOERLI_TESTNET, None),
                trace_root: traces[2].clone(),
            },
        ];

        Ok((context, next_block_header, traces))
    }

    #[tokio::test]
    async fn test_multiple_transactions() -> anyhow::Result<()> {
        let (context, next_block_header, traces) = setup_multi_tx_trace_test().await?;

        let input = TraceBlockTransactionsInput {
            block_id: next_block_header.hash.into(),
        };
        let output = trace_block_transactions(context, input).await.unwrap();
        let expected = TraceBlockTransactionsOutput(traces);

        pretty_assertions::assert_eq!(output, expected);
        Ok(())
    }

    pub(crate) async fn setup_multi_tx_trace_pending_test(
    ) -> anyhow::Result<(RpcContext, Vec<Trace>)> {
        use super::super::simulate_transactions::tests::fixtures;
        use super::super::simulate_transactions::tests::setup_storage;

        let (
            storage,
            last_block_header,
            account_contract_address,
            universal_deployer_address,
            test_storage_value,
        ) = setup_storage().await;
        let context = RpcContext::for_tests().with_storage(storage.clone());

        let transactions = vec![
            fixtures::input::declare(account_contract_address),
            fixtures::input::universal_deployer(
                account_contract_address,
                universal_deployer_address,
            ),
            fixtures::input::invoke(account_contract_address),
        ];

        let traces = vec![
            fixtures::expected_output::declare(account_contract_address, &last_block_header)
                .transaction_trace,
            fixtures::expected_output::universal_deployer(
                account_contract_address,
                &last_block_header,
                universal_deployer_address,
            )
            .transaction_trace,
            fixtures::expected_output::invoke(
                account_contract_address,
                &last_block_header,
                test_storage_value,
            )
            .transaction_trace,
        ];

        let pending_block = {
            let mut db = storage.connection()?;
            let tx = db.transaction()?;

            tx.insert_sierra_class(
                &SierraHash(fixtures::SIERRA_HASH.0),
                fixtures::SIERRA_DEFINITION,
                &fixtures::CASM_HASH,
                fixtures::CASM_DEFINITION,
                "compiler version",
            )?;

            let dummy_receipt: Receipt = Receipt {
                actual_fee: None,
                events: vec![],
                execution_resources: None,
                l1_to_l2_consumed_message: None,
                l2_to_l1_messages: vec![],
                transaction_hash: TransactionHash(felt!("0x1")),
                transaction_index: TransactionIndex::new_or_panic(0),
                execution_status: ExecutionStatus::default(),
                revert_error: None,
            };

            let transaction_receipts =
                vec![dummy_receipt.clone(), dummy_receipt.clone(), dummy_receipt];

            let pending_block = starknet_gateway_types::reply::PendingBlock {
                eth_l1_gas_price: GasPrice(1),
                strk_l1_gas_price: Some(GasPrice(1)),
                parent_hash: last_block_header.hash,
                sequencer_address: last_block_header.sequencer_address,
                status: starknet_gateway_types::reply::Status::Pending,
                timestamp: last_block_header.timestamp,
                transaction_receipts,
                transactions: transactions.iter().cloned().map(Into::into).collect(),
                starknet_version: last_block_header.starknet_version,
            };

            tx.commit()?;

            pending_block
        };

        let pending_data = crate::pending::PendingData {
            block: pending_block,
            state_update: StateUpdate::default(),
            number: last_block_header.number + 1,
        };

        let (tx, rx) = tokio::sync::watch::channel(Default::default());
        tx.send(Arc::new(pending_data)).unwrap();

        let context = context.with_pending_data(rx);

        let traces = vec![
            Trace {
                transaction_hash: transactions[0]
                    .transaction_hash(ChainId::GOERLI_TESTNET, Some(fixtures::SIERRA_HASH)),
                trace_root: traces[0].clone(),
            },
            Trace {
                transaction_hash: transactions[1].transaction_hash(ChainId::GOERLI_TESTNET, None),
                trace_root: traces[1].clone(),
            },
            Trace {
                transaction_hash: transactions[2].transaction_hash(ChainId::GOERLI_TESTNET, None),
                trace_root: traces[2].clone(),
            },
        ];

        Ok((context, traces))
    }

    #[tokio::test]
    async fn test_multiple_pending_transactions() -> anyhow::Result<()> {
        let (context, traces) = setup_multi_tx_trace_pending_test().await?;

        let input = TraceBlockTransactionsInput {
            block_id: BlockId::Pending,
        };
        let output = trace_block_transactions(context, input).await.unwrap();
        let expected = TraceBlockTransactionsOutput(traces);

        pretty_assertions::assert_eq!(output, expected);
        Ok(())
    }
}
