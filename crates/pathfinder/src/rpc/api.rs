//! Implementation of JSON-RPC endpoints.
use crate::{
    cairo::ext_py,
    core::{
        CallResultValue, CallSignatureElem, ClassHash, ConstructorParam, ContractAddress,
        ContractAddressSalt, ContractClass, ContractCode, Fee, GasPrice, GlobalRoot,
        SequencerAddress, StarknetBlockHash, StarknetBlockNumber, StarknetBlockTimestamp,
        StarknetTransactionHash, StarknetTransactionIndex, StorageValue, TransactionNonce,
        TransactionVersion,
    },
    ethereum::Chain,
    rpc::types::{
        reply::{
            Block, BlockStatus, ErrorCode, FeeEstimate, GetEventsResult, Syncing, Transaction,
            TransactionReceipt,
        },
        request::{BlockResponseScope, Call, EventFilter, OverflowingStorageAddress},
        BlockHashOrTag, BlockNumberOrTag, Tag,
    },
    sequencer::{self, request::add_transaction::ContractDefinition, ClientApi},
    state::SyncState,
    storage::{
        ContractsTable, EventFilterError, RefsTable, StarknetBlocksBlockId, StarknetBlocksTable,
        StarknetEventsTable, StarknetTransactionsTable, Storage,
    },
};
use anyhow::Context;
use jsonrpsee::{
    core::{error::Error, RpcResult},
    types::{error::CallError, ErrorObject},
};
use stark_hash::StarkHash;
use std::convert::TryInto;
use std::sync::Arc;

use super::types::reply::{
    DeclareTransactionResult, DeployTransactionResult, InvokeTransactionResult,
};

/// Implements JSON-RPC endpoints.
pub struct RpcApi {
    storage: Storage,
    sequencer: sequencer::Client,
    chain_id: &'static str,
    call_handle: Option<ext_py::Handle>,
    shared_gas_price: Option<Cached>,
    sync_state: Arc<SyncState>,
}

#[derive(Debug)]
pub struct RawBlock {
    pub number: StarknetBlockNumber,
    pub hash: StarknetBlockHash,
    pub root: GlobalRoot,
    pub parent_hash: StarknetBlockHash,
    pub parent_root: GlobalRoot,
    pub timestamp: StarknetBlockTimestamp,
    pub status: BlockStatus,
    pub sequencer: SequencerAddress,
    pub gas_price: GasPrice,
}

/// Based on [the Starknet operator API spec](https://github.com/starkware-libs/starknet-specs/blob/master/api/starknet_api_openrpc.json).
impl RpcApi {
    pub fn new(
        storage: Storage,
        sequencer: sequencer::Client,
        chain: Chain,
        sync_state: Arc<SyncState>,
    ) -> Self {
        Self {
            storage,
            sequencer,
            chain_id: match chain {
                // Hex str for b"SN_GOERLI"
                Chain::Goerli => "0x534e5f474f45524c49",
                // Hex str for b"SN_MAIN"
                Chain::Mainnet => "0x534e5f4d41494e",
            },
            call_handle: None,
            shared_gas_price: None,
            sync_state,
        }
    }

    pub fn with_call_handling(self, call_handle: ext_py::Handle) -> Self {
        Self {
            call_handle: Some(call_handle),
            ..self
        }
    }

    pub fn with_eth_gas_price(self, shared: Cached) -> Self {
        Self {
            shared_gas_price: Some(shared),
            ..self
        }
    }

    /// Get block information given the block hash.
    pub async fn get_block_by_hash(
        &self,
        block_hash: BlockHashOrTag,
        requested_scope: Option<BlockResponseScope>,
    ) -> RpcResult<Block> {
        let block_id = match block_hash {
            BlockHashOrTag::Tag(Tag::Pending) => {
                let block = self
                    .sequencer
                    .block(block_hash.into())
                    .await
                    .map_err(internal_server_error)?;

                let scope = requested_scope.unwrap_or_default();

                return Ok(Block::from_sequencer_scoped(block, scope));
            }
            BlockHashOrTag::Hash(hash) => hash.into(),
            BlockHashOrTag::Tag(Tag::Latest) => StarknetBlocksBlockId::Latest,
        };

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut connection = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let transaction = connection
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            // Need to get the block status. This also tests that the block hash is valid.
            let block = Self::get_raw_block_by_hash(&transaction, block_id)?;
            let scope = requested_scope.unwrap_or_default();

            let transactions = Self::get_block_transactions(&transaction, block.number, scope)?;

            Ok(Block::from_raw(block, transactions))
        })
        .await
        .context("Database read panic or shutting down")
        .map_err(internal_server_error)
        .and_then(|x| x)
    }

    /// This function assumes that the block ID is valid i.e. it won't check if the block hash or number exist.
    pub fn get_block_transactions(
        db_tx: &rusqlite::Transaction<'_>,
        block_number: StarknetBlockNumber,
        scope: BlockResponseScope,
    ) -> RpcResult<super::types::reply::Transactions> {
        let transactions_receipts =
            StarknetTransactionsTable::get_transaction_data_for_block(db_tx, block_number.into())
                .context("Reading transactions from database")
                .map_err(internal_server_error)?;

        // All our data is L2 accepted, check our L1-L2 head to see if this block has been accepted on L1.
        let l1_l2_head = RefsTable::get_l1_l2_head(db_tx)
            .context("Read latest L1 head from database")
            .map_err(internal_server_error)?;
        let block_status = match l1_l2_head {
            Some(number) if number >= block_number => BlockStatus::AcceptedOnL1,
            _ => BlockStatus::AcceptedOnL2,
        };

        use super::types::reply;
        let transactions = match scope {
            BlockResponseScope::TransactionHashes => reply::Transactions::HashesOnly(
                transactions_receipts
                    .into_iter()
                    .map(|(t, _)| t.transaction_hash)
                    .collect(),
            ),
            BlockResponseScope::FullTransactions => reply::Transactions::Full(
                transactions_receipts
                    .into_iter()
                    .map(|(t, _)| t.into())
                    .collect(),
            ),
            BlockResponseScope::FullTransactionsAndReceipts => {
                reply::Transactions::FullWithReceipts(
                    transactions_receipts
                        .into_iter()
                        .map(|(t, r)| {
                            let t: Transaction = t.into();
                            let r = TransactionReceipt::with_status(r, block_status);

                            reply::TransactionAndReceipt {
                                txn_hash: t.txn_hash,
                                contract_address: t.contract_address,
                                entry_point_selector: t.entry_point_selector,
                                calldata: t.calldata,
                                max_fee: t.max_fee,
                                actual_fee: r.actual_fee,
                                status: r.status,
                                status_data: r.status_data,
                                messages_sent: r.messages_sent,
                                l1_origin_message: r.l1_origin_message,
                                events: r.events,
                            }
                        })
                        .collect(),
                )
            }
        };

        Ok(transactions)
    }

    /// Get block information given the block number (its height).
    pub async fn get_block_by_number(
        &self,
        block_number: BlockNumberOrTag,
        requested_scope: Option<BlockResponseScope>,
    ) -> RpcResult<Block> {
        let block_id = match block_number {
            BlockNumberOrTag::Number(number) => number.into(),
            BlockNumberOrTag::Tag(Tag::Latest) => StarknetBlocksBlockId::Latest,
            BlockNumberOrTag::Tag(Tag::Pending) => {
                let block = self
                    .sequencer
                    .block(block_number.into())
                    .await
                    .context("Fetch block from sequencer")
                    .map_err(internal_server_error)?;

                let scope = requested_scope.unwrap_or_default();

                return Ok(Block::from_sequencer_scoped(block, scope));
            }
        };

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut connection = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let transaction = connection
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            // Need to get the block status. This also tests that the block hash is valid.
            let block = Self::get_raw_block_by_number(&transaction, block_id)?;
            let scope = requested_scope.unwrap_or_default();

            let transactions = Self::get_block_transactions(&transaction, block.number, scope)?;

            Ok(Block::from_raw(block, transactions))
        })
        .await
        .context("Database read panic or shutting down")
        .map_err(internal_server_error)
        .and_then(|x| x)
    }

    fn get_raw_block_by_hash(
        tx: &rusqlite::Transaction<'_>,
        block_id: StarknetBlocksBlockId,
    ) -> RpcResult<RawBlock> {
        Self::get_raw_block(tx, block_id, ErrorCode::InvalidBlockHash)
    }

    fn get_raw_block_by_number(
        tx: &rusqlite::Transaction<'_>,
        block_id: StarknetBlocksBlockId,
    ) -> RpcResult<RawBlock> {
        Self::get_raw_block(tx, block_id, ErrorCode::InvalidBlockNumber)
    }

    /// Fetches a [RawBlock] from storage.
    ///
    /// `error_code_for_latest` is the error code when the `latest` block is missing,
    /// ie. when the storage is empty, wrapped by [`Self::get_raw_block_by_number`] and
    /// [`Self::get_raw_block_by_number`] for the two different usecases.
    fn get_raw_block(
        transaction: &rusqlite::Transaction<'_>,
        block_id: StarknetBlocksBlockId,
        error_code_for_latest: ErrorCode,
    ) -> RpcResult<RawBlock> {
        let block = StarknetBlocksTable::get(transaction, block_id)
            .context("Read block from database")
            .map_err(internal_server_error)?
            .ok_or_else(|| Error::from(error_code_for_latest))?;

        // All our data is L2 accepted, check our L1-L2 head to see if this block has been accepted on L1.
        let l1_l2_head = RefsTable::get_l1_l2_head(transaction)
            .context("Read latest L1 head from database")
            .map_err(internal_server_error)?;
        let block_status = match l1_l2_head {
            Some(number) if number >= block.number => BlockStatus::AcceptedOnL1,
            _ => BlockStatus::AcceptedOnL2,
        };

        let (parent_hash, parent_root) = match block.number {
            StarknetBlockNumber::GENESIS => (
                StarknetBlockHash(StarkHash::ZERO),
                GlobalRoot(StarkHash::ZERO),
            ),
            other => {
                let parent_block = StarknetBlocksTable::get(transaction, (other - 1).into())
                    .context("Read parent block from database")
                    .map_err(internal_server_error)?
                    .context("Parent block missing")?;

                (parent_block.hash, parent_block.root)
            }
        };

        let block = RawBlock {
            number: block.number,
            hash: block.hash,
            root: block.root,
            parent_hash,
            parent_root,
            timestamp: block.timestamp,
            status: block_status,
            gas_price: block.gas_price,
            sequencer: block.sequencer_address,
        };

        Ok(block)
    }

    // /// Get the information about the result of executing the requested block.
    // pub async fn get_state_update_by_hash(
    //     &self,
    //     block_hash: BlockHashOrTag,
    // ) -> RpcResult<StateUpdate> {
    //     // TODO get this from storage or directly from L1
    //     match block_hash {
    //         BlockHashOrTag::Tag(Tag::Latest) => todo!("Implement L1 state diff retrieval."),
    //         BlockHashOrTag::Tag(Tag::Pending) => {
    //             todo!("Implement when sequencer support for pending tag available.")
    //         }
    //         BlockHashOrTag::Hash(_) => todo!("Implement L1 state diff retrieval."),
    //     }
    // }

    /// Get the value of the storage at the given address and key.
    ///
    /// We are using overflowing type for `key` to be able to correctly report `INVALID_STORAGE_KEY` as per
    /// [StarkNet RPC spec](https://github.com/starkware-libs/starknet-specs/blob/master/api/starknet_api_openrpc.json),
    /// otherwise we would report [-32602](https://www.jsonrpc.org/specification#error_object).
    pub async fn get_storage_at(
        &self,
        contract_address: ContractAddress,
        key: OverflowingStorageAddress,
        block_hash: BlockHashOrTag,
    ) -> RpcResult<StorageValue> {
        use crate::{
            core::StorageAddress,
            state::state_tree::{ContractsStateTree, GlobalStateTree},
            storage::ContractsStateTable,
        };
        use stark_hash::OverflowError;

        let key = StorageAddress(StarkHash::from_be_bytes(key.0.to_fixed_bytes()).map_err(
            // Report that the value is >= than the field modulus
            // Use explicit typing in closure arg to force compiler error should error variants ever be expanded
            |_e: OverflowError| Error::from(ErrorCode::InvalidStorageKey),
        )?);

        if key.0.has_more_than_251_bits() {
            // Report that the value is more than 251 bits
            return Err(Error::from(ErrorCode::InvalidStorageKey));
        }

        let block_id = match block_hash {
            BlockHashOrTag::Hash(hash) => hash.into(),
            BlockHashOrTag::Tag(Tag::Latest) => StarknetBlocksBlockId::Latest,
            BlockHashOrTag::Tag(Tag::Pending) => {
                return Ok(self
                    .sequencer
                    .storage(contract_address, key, block_hash)
                    .await?);
            }
        };

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            // Use internal_server_error to indicate that the process of querying for a particular block failed,
            // which is not the same as being sure that the block is not in the db.
            let global_root = StarknetBlocksTable::get_root(&tx, block_id)
                .map_err(internal_server_error)?
                // Since the db query succeeded in execution, we can now report if the block hash was indeed not found
                // by using a dedicated error code from the RPC API spec
                .ok_or_else(|| Error::from(ErrorCode::InvalidBlockHash))?;

            let global_state_tree = GlobalStateTree::load(&tx, global_root)
                .context("Global state tree")
                .map_err(internal_server_error)?;

            let contract_state_hash = global_state_tree
                .get(contract_address)
                .context("Get contract state hash from global state tree")
                .map_err(internal_server_error)?;

            // There is a dedicated error code for a non-existent contract in the RPC API spec, so use it.
            if contract_state_hash.0 == StarkHash::ZERO {
                return Err(Error::from(ErrorCode::ContractNotFound));
            }

            let contract_state_root = ContractsStateTable::get_root(&tx, contract_state_hash)
                .context("Get contract state root")
                .map_err(internal_server_error)?
                .ok_or_else(|| {
                    internal_server_error(anyhow::anyhow!(
                        "Contract state root not found for contract state hash {}",
                        contract_state_hash.0
                    ))
                })?;

            let contract_state_tree = ContractsStateTree::load(&tx, contract_state_root)
                .context("Load contract state tree")
                .map_err(internal_server_error)?;

            // ContractsStateTree::get() will return zero if the value is still not found (and we know the key is valid),
            // which is consistent with the specification.
            let storage_val = contract_state_tree
                .get(key)
                .context("Get value from contract state tree")
                .map_err(internal_server_error)?;

            Ok(storage_val)
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            // flatten is unstable
            .and_then(|x| x)
    }

    /// Get the details and status of a submitted transaction.
    pub async fn get_transaction_by_hash(
        &self,
        transaction_hash: StarknetTransactionHash,
    ) -> RpcResult<Transaction> {
        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let db_tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            // Get the transaction from storage.
            StarknetTransactionsTable::get_transaction(&db_tx, transaction_hash)
                .context("Reading transaction from database")?
                .ok_or_else(|| ErrorCode::InvalidTransactionHash.into())
                .map(|tx| tx.into())
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the details of a transaction by a given block hash and index.
    pub async fn get_transaction_by_block_hash_and_index(
        &self,
        block_hash: BlockHashOrTag,
        index: StarknetTransactionIndex,
    ) -> RpcResult<Transaction> {
        let index: usize = index
            .0
            .try_into()
            .map_err(|e| Error::Call(CallError::InvalidParams(anyhow::Error::new(e))))?;

        let block_id = match block_hash {
            BlockHashOrTag::Hash(hash) => StarknetBlocksBlockId::Hash(hash),
            BlockHashOrTag::Tag(Tag::Latest) => StarknetBlocksBlockId::Latest,
            BlockHashOrTag::Tag(Tag::Pending) => {
                let block = self
                    .sequencer
                    .block(block_hash.into())
                    .await
                    .context("Fetch block from sequencer")
                    .map_err(internal_server_error)?;

                return block
                    .transactions
                    .into_iter()
                    .nth(index)
                    .map_or(Err(ErrorCode::InvalidTransactionIndex.into()), |txn| {
                        Ok(txn.into())
                    });
            }
        };

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let db_tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            // Get the transaction from storage.
            match StarknetTransactionsTable::get_transaction_at_block(&db_tx, block_id, index)
                .context("Reading transaction from database")?
            {
                Some(transaction) => Ok(transaction.into()),
                None => {
                    // We now need to check whether it was the block hash or transaction index which were invalid. We do this by checking if the block exists
                    // at all. If no, then the block hash is invalid. If yes, then the index is invalid.
                    //
                    // get_root is cheaper than querying the full block.
                    match StarknetBlocksTable::get_root(&db_tx, block_id)
                        .context("Reading block from database")?
                    {
                        Some(_) => Err(ErrorCode::InvalidTransactionIndex.into()),
                        None => Err(ErrorCode::InvalidBlockHash.into()),
                    }
                }
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the details of a transaction by a given block number and index.
    pub async fn get_transaction_by_block_number_and_index(
        &self,
        block_number: BlockNumberOrTag,
        index: StarknetTransactionIndex,
    ) -> RpcResult<Transaction> {
        let index: usize = index
            .0
            .try_into()
            .map_err(|e| Error::Call(CallError::InvalidParams(anyhow::Error::new(e))))?;

        let block_id = match block_number {
            BlockNumberOrTag::Number(number) => StarknetBlocksBlockId::Number(number),
            BlockNumberOrTag::Tag(Tag::Latest) => StarknetBlocksBlockId::Latest,
            BlockNumberOrTag::Tag(Tag::Pending) => {
                let block = self
                    .sequencer
                    .block(block_number.into())
                    .await
                    .context("Fetch block from sequencer")
                    .map_err(internal_server_error)?;

                return block
                    .transactions
                    .into_iter()
                    .nth(index)
                    .map_or(Err(ErrorCode::InvalidTransactionIndex.into()), |txn| {
                        Ok(txn.into())
                    });
            }
        };

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let db_tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            // Get the transaction from storage.
            match StarknetTransactionsTable::get_transaction_at_block(&db_tx, block_id, index)
                .context("Reading transaction from database")?
            {
                Some(transaction) => Ok(transaction.into()),
                None => {
                    // We now need to check whether it was the block number or transaction index which were invalid. We do this by checking if the block exists
                    // at all. If no, then the block number is invalid. If yes, then the index is invalid.
                    //
                    // get_root is cheaper than querying the full block.
                    match StarknetBlocksTable::get_root(&db_tx, block_id)
                        .context("Reading block from database")?
                    {
                        Some(_) => Err(ErrorCode::InvalidTransactionIndex.into()),
                        None => Err(ErrorCode::InvalidBlockNumber.into()),
                    }
                }
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the transaction receipt by the transaction hash.
    pub async fn get_transaction_receipt(
        &self,
        transaction_hash: StarknetTransactionHash,
    ) -> RpcResult<TransactionReceipt> {
        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let db_tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            match StarknetTransactionsTable::get_receipt(&db_tx, transaction_hash)
                .context("Reading transaction receipt from database")
                .map_err(internal_server_error)?
            {
                Some((receipt, block_hash)) => {
                    // We require the block status here as well..
                    let block = StarknetBlocksTable::get(&db_tx, block_hash.into())
                        .context("Reading block from database")
                        .map_err(internal_server_error)?
                        .context("Block missing from database")
                        .map_err(internal_server_error)?;

                    // All our data is L2 accepted, check our L1-L2 head to see if this block has been accepted on L1.
                    let l1_l2_head = RefsTable::get_l1_l2_head(&db_tx)
                        .context("Read latest L1 head from database")
                        .map_err(internal_server_error)?;
                    let block_status = match l1_l2_head {
                        Some(number) if number >= block.number => BlockStatus::AcceptedOnL1,
                        _ => BlockStatus::AcceptedOnL2,
                    };

                    Ok(TransactionReceipt::with_status(receipt, block_status))
                }
                None => Err(ErrorCode::InvalidTransactionHash.into()),
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the class based on its hash.
    ///
    /// This is for the deprecated starknet_getCode API that requires returning the
    /// contract bytecode and abi.
    pub async fn get_code(&self, contract_address: ContractAddress) -> RpcResult<ContractCode> {
        use crate::storage::ContractCodeTable;

        let storage = self.storage.clone();

        let jh = tokio::task::spawn_blocking(move || {
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;
            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            let class_hash = ContractsTable::get_hash(&tx, contract_address)
                .context("Fetching class hash from database")
                .map_err(internal_server_error)?;

            let class_hash = match class_hash {
                Some(hash) => hash,
                None => return Err(ErrorCode::ContractNotFound.into()),
            };

            let code = ContractCodeTable::get_code(&tx, class_hash)
                .context("Fetching code from database")
                .map_err(internal_server_error)?;

            match code {
                Some(code) => Ok(code),
                None => Err(ErrorCode::InvalidContractClassHash.into()),
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the class based on its hash.
    pub async fn get_class(&self, class_hash: ClassHash) -> RpcResult<ContractClass> {
        use crate::storage::ContractCodeTable;

        let storage = self.storage.clone();

        let jh = tokio::task::spawn_blocking(move || {
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;
            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            let class = ContractCodeTable::get_class(&tx, class_hash)
                .context("Fetching code from database")
                .map_err(internal_server_error)?;

            match class {
                Some(class) => Ok(class),
                None => Err(ErrorCode::InvalidContractClassHash.into()),
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the class hash of a specific contract.
    pub async fn get_class_hash_at(
        &self,
        contract_address: ContractAddress,
    ) -> RpcResult<ClassHash> {
        let storage = self.storage.clone();

        let jh = tokio::task::spawn_blocking(move || {
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;
            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            let class_hash = ContractsTable::get_hash(&tx, contract_address)
                .context("Fetching class hash from database")
                .map_err(internal_server_error)?;

            match class_hash {
                Some(hash) => Ok(hash),
                None => Err(ErrorCode::ContractNotFound.into()),
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the class of a specific contract.
    /// `contract_address` is the address of the contract to read from.
    pub async fn get_class_at(
        &self,
        contract_address: ContractAddress,
    ) -> RpcResult<ContractClass> {
        use crate::storage::ContractCodeTable;

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;
            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            let class_hash = ContractsTable::get_hash(&tx, contract_address)
                .context("Fetching class hash from database")
                .map_err(internal_server_error)?;

            let class_hash = match class_hash {
                Some(hash) => hash,
                None => return Err(ErrorCode::ContractNotFound.into()),
            };

            let code = ContractCodeTable::get_class(&tx, class_hash)
                .context("Fetching code from database")
                .map_err(internal_server_error)?;

            match code {
                Some(code) => Ok(code),
                None => Err(ErrorCode::ContractNotFound.into()),
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the number of transactions in a block given a block hash.
    pub async fn get_block_transaction_count_by_hash(
        &self,
        block_hash: BlockHashOrTag,
    ) -> RpcResult<u64> {
        let block_id = match block_hash {
            BlockHashOrTag::Hash(hash) => hash.into(),
            BlockHashOrTag::Tag(Tag::Latest) => StarknetBlocksBlockId::Latest,
            BlockHashOrTag::Tag(Tag::Pending) => {
                let block = self
                    .sequencer
                    .block(block_hash.into())
                    .await
                    .context("Fetch block from sequencer")
                    .map_err(internal_server_error)?;

                let len: u64 =
                    block.transactions.len().try_into().map_err(|e| {
                        Error::Call(CallError::InvalidParams(anyhow::Error::new(e)))
                    })?;

                return Ok(len);
            }
        };

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;
            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            match StarknetTransactionsTable::get_transaction_count(&tx, block_id)
                .context("Reading transaction count from database")
                .map_err(internal_server_error)?
            {
                0 => {
                    // We need to check if the value was 0 because there were no transactions, or because the block hash
                    // is invalid.
                    //
                    // get_root is cheaper than querying the full block.
                    match StarknetBlocksTable::get_root(&tx, block_id)
                        .context("Reading block from database")?
                    {
                        Some(_) => Ok(0),
                        None => Err(ErrorCode::InvalidBlockHash.into()),
                    }
                }
                other => Ok(other as u64),
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Get the number of transactions in a block given a block hash.
    pub async fn get_block_transaction_count_by_number(
        &self,
        block_number: BlockNumberOrTag,
    ) -> RpcResult<u64> {
        let block_id = match block_number {
            BlockNumberOrTag::Number(number) => number.into(),
            BlockNumberOrTag::Tag(Tag::Latest) => StarknetBlocksBlockId::Latest,
            BlockNumberOrTag::Tag(Tag::Pending) => {
                let block = self
                    .sequencer
                    .block(block_number.into())
                    .await
                    .context("Fetch block from sequencer")
                    .map_err(internal_server_error)?;

                let len: u64 =
                    block.transactions.len().try_into().map_err(|e| {
                        Error::Call(CallError::InvalidParams(anyhow::Error::new(e)))
                    })?;

                return Ok(len);
            }
        };

        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;
            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            match StarknetTransactionsTable::get_transaction_count(&tx, block_id)
                .context("Reading transaction count from database")
                .map_err(internal_server_error)?
            {
                0 => {
                    // We need to check if the value was 0 because there were no transactions, or because the block number
                    // is invalid.
                    //
                    // get_root is cheaper than querying the full block.
                    match StarknetBlocksTable::get_root(&tx, block_id)
                        .context("Reading block from database")?
                    {
                        Some(_) => Ok(0),
                        None => Err(ErrorCode::InvalidBlockNumber.into()),
                    }
                }
                other => Ok(other as u64),
            }
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Call a starknet function without creating a StarkNet transaction.
    pub async fn call(
        &self,
        request: Call,
        block_hash: BlockHashOrTag,
    ) -> RpcResult<Vec<CallResultValue>> {
        use futures::future::TryFutureExt;

        match (self.call_handle.as_ref(), &block_hash) {
            (Some(h), &BlockHashOrTag::Hash(_) | &BlockHashOrTag::Tag(Tag::Latest)) => {
                // we don't yet handle pending at all, and latest has been decided to be whatever
                // block we have, which is exactly how the py/src/call.py handles it.
                h.call(request, block_hash).map_err(Error::from).await
            }
            (Some(_), _) | (None, _) => {
                // just forward it to the sequencer for now.
                self.sequencer
                    .call(request.into(), block_hash)
                    .map_ok(|x| x.result)
                    .map_err(Error::from)
                    .await
            }
        }
    }

    /// Get the most recent accepted block number.
    pub async fn block_number(&self) -> RpcResult<u64> {
        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let mut db = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;
            let tx = db
                .transaction()
                .context("Creating database transaction")
                .map_err(internal_server_error)?;

            StarknetBlocksTable::get_latest_number(&tx)
                .context("Reading latest block number from database")
                .map_err(internal_server_error)?
                .context("Database is empty")
                .map_err(internal_server_error)
                .map(|number| number.0)
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            .and_then(|x| x)
    }

    /// Return the currently configured StarkNet chain id.
    pub async fn chain_id(&self) -> RpcResult<&'static str> {
        Ok(self.chain_id)
    }

    // /// Returns the transactions in the transaction pool, recognized by this sequencer.
    // pub async fn pending_transactions(&self) -> RpcResult<Vec<Transaction>> {
    //     todo!("Figure out where to take them from.")
    // }

    // /// Returns the current starknet protocol version identifier, as supported by this node.
    // pub async fn protocol_version(&self) -> RpcResult<StarknetProtocolVersion> {
    //     todo!("Figure out where to take it from.")
    // }

    /// Returns an object about the sync status, or false if the node is not synching.
    pub async fn syncing(&self) -> RpcResult<Syncing> {
        // Scoped so I don't have to think too hard about mutex guard drop semantics.
        let value = { self.sync_state.status.read().await.clone() };
        Ok(value)
    }

    /// Returns events matching the specified filter
    pub async fn get_events(&self, request: EventFilter) -> RpcResult<GetEventsResult> {
        let storage = self.storage.clone();
        let span = tracing::Span::current();

        let jh = tokio::task::spawn_blocking(move || {
            let _g = span.enter();
            let connection = storage
                .connection()
                .context("Opening database connection")
                .map_err(internal_server_error)?;

            let filter = request.into();
            // We don't add context here, because [StarknetEventsTable::get_events] adds its
            // own context to the errors. This way we get meaningful error information
            // for errors related to query parameters.
            let page = StarknetEventsTable::get_events(&connection, &filter).map_err(|e| {
                if let Some(e) = e.downcast_ref::<EventFilterError>() {
                    Error::from(*e)
                } else {
                    internal_server_error(e)
                }
            })?;

            Ok(GetEventsResult {
                events: page.events.into_iter().map(|e| e.into()).collect(),
                page_number: filter.page_number,
                is_last_page: page.is_last_page,
            })
        });

        jh.await
            .context("Database read panic or shutting down")
            .map_err(internal_server_error)
            // flatten is unstable
            .and_then(|x| x)
    }

    /// Submit a new transaction to be added to the chain.
    ///
    /// This method just forwards the request received over the JSON-RPC
    /// interface to the sequencer.
    pub async fn add_invoke_transaction(
        &self,
        call: Call,
        signature: Vec<CallSignatureElem>,
        max_fee: Fee,
        version: TransactionVersion,
    ) -> RpcResult<InvokeTransactionResult> {
        let mut call: sequencer::request::Call = call.into();
        call.signature = signature;

        let result = self
            .sequencer
            .add_invoke_transaction(call, max_fee, version)
            .await?;
        Ok(InvokeTransactionResult {
            transaction_hash: result.transaction_hash,
        })
    }

    /// Submit a new declare transaction.
    ///
    /// "Similarly to deploy, declare transactions will contain the contract class.
    /// The state of StarkNet will contain a list of declared classes, that can
    /// be appended to via the new declare transaction."
    ///
    /// This method just forwards the request received over the JSON-RPC
    /// interface to the sequencer.
    pub async fn add_declare_transaction(
        &self,
        contract_class: ContractDefinition,
        version: TransactionVersion,
        token: Option<String>,
    ) -> RpcResult<DeclareTransactionResult> {
        let result = self
            .sequencer
            .add_declare_transaction(
                contract_class,
                // actual address dumped from a `starknet declare` call
                ContractAddress(StarkHash::from_hex_str("0x1").unwrap()),
                Fee(0u128.to_be_bytes().into()),
                vec![],
                TransactionNonce(StarkHash::ZERO),
                version,
                token,
            )
            .await?;
        Ok(DeclareTransactionResult {
            transaction_hash: result.transaction_hash,
            class_hash: result.class_hash,
        })
    }

    /// Submit a new deploy contract transaction.
    ///
    /// This method just forwards the request received over the JSON-RPC
    /// interface to the sequencer.
    pub async fn add_deploy_transaction(
        &self,
        contract_address_salt: ContractAddressSalt,
        constructor_calldata: Vec<ConstructorParam>,
        contract_definition: ContractDefinition,
        token: Option<String>,
    ) -> RpcResult<DeployTransactionResult> {
        let result = self
            .sequencer
            .add_deploy_transaction(
                contract_address_salt,
                constructor_calldata,
                contract_definition,
                token,
            )
            .await?;
        Ok(DeployTransactionResult {
            transaction_hash: result.transaction_hash,
            contract_address: result.address,
        })
    }

    pub async fn estimate_fee(
        &self,
        request: Call,
        block_hash: BlockHashOrTag,
    ) -> RpcResult<FeeEstimate> {
        use crate::cairo::ext_py::GasPriceSource;

        match (
            self.call_handle.as_ref(),
            self.shared_gas_price.as_ref(),
            &block_hash,
        ) {
            (Some(h), _, &BlockHashOrTag::Hash(_)) => {
                // discussed during estimateFee work: when using block_hash use the gasPrice from
                // the starknet_blocks::gas_price column, otherwise (tags) get the latest eth_gasPrice.
                h.estimate_fee(request, block_hash, GasPriceSource::PastBlock)
                    .await
                    .map_err(Error::from)
            }
            (Some(h), Some(g), &BlockHashOrTag::Tag(Tag::Latest)) => {
                // now we want the gas_price from our hopefully pre-fetched source, it will be the
                // same when we finally have pending support

                let gas_price = g
                    .get()
                    .await
                    .ok_or_else(|| internal_server_error("eth_gasPrice unavailable"))?;

                h.estimate_fee(request, block_hash, GasPriceSource::Current(gas_price))
                    .await
                    .map_err(Error::from)
            }
            (Some(_), Some(_), &BlockHashOrTag::Tag(Tag::Pending)) => {
                Err(internal_server_error("Unimplemented"))
            }
            _ => {
                // sequencer return type is incompatible with jsonrpc api return type
                Err(internal_server_error("Unsupported configuration"))
            }
        }
    }
}

impl From<ext_py::CallFailure> for jsonrpsee::core::Error {
    fn from(e: ext_py::CallFailure) -> Self {
        match e {
            ext_py::CallFailure::NoSuchBlock => Error::from(ErrorCode::InvalidBlockHash),
            ext_py::CallFailure::NoSuchContract => Error::from(ErrorCode::ContractNotFound),
            ext_py::CallFailure::ExecutionFailed(e) => internal_server_error(e),
            // Intentionally hide the message under Internal
            ext_py::CallFailure::Internal(_) | ext_py::CallFailure::Shutdown => {
                static_internal_server_error()
            }
        }
    }
}

impl From<EventFilterError> for jsonrpsee::core::Error {
    fn from(e: EventFilterError) -> Self {
        match e {
            EventFilterError::PageSizeTooBig(max_size) => {
                let error = ErrorCode::PageSizeTooBig as i32;
                Error::Call(CallError::Custom(ErrorObject::owned(
                    error,
                    ErrorCode::PageSizeTooBig.to_string(),
                    Some(serde_json::json!({ "max_page_size": max_size })),
                )))
            }
        }
    }
}

// We cannot just return Error::Internal (-32003) in cases which are not covered by starknet RPC API spec
// as jsonrpsee reserved it for internal subscription related errors only, so we resort to
// CallError::Custom with the same code value and message as Error::Internal. This way we can still provide
// an "Internal server error" but with additional context.
//
// This error is used for all instances of operations that are not explicitly specified in the StarkNet spec.
// See <https://github.com/starkware-libs/starknet-specs/blob/master/api/starknet_api_openrpc.json>
fn internal_server_error(e: impl std::fmt::Display) -> jsonrpsee::core::Error {
    Error::Call(CallError::Custom(ErrorObject::owned(
        jsonrpsee::types::error::ErrorCode::InternalError.code(),
        format!("{}: {}", jsonrpsee::types::error::INTERNAL_ERROR_MSG, e),
        None::<()>,
    )))
}

fn static_internal_server_error() -> jsonrpsee::core::Error {
    Error::Call(CallError::Custom(ErrorObject::from(
        jsonrpsee::types::error::ErrorCode::InternalError,
    )))
}

/// Background task updated latest `eth_gasPrice`.
///
/// The value is used for [`RpcApi::estimate_fee`] when user requests for [`BlockHashOrTag::Tag`].
#[derive(Default, Clone)]
pub struct Cached(std::sync::Arc<std::sync::Mutex<Inner>>);

impl Cached {
    /// Returns either a fast fresh value, slower a periodically polled value or fails because
    /// polling has stopped.
    async fn get(&self) -> Option<web3::types::H256> {
        let mut rx = {
            let mut g = self.0.lock().unwrap_or_else(|e| e.into_inner());

            let stale_limit = std::time::Duration::from_secs(10);

            if let Some((fetched_at, gas_price)) = g.latest.as_ref() {
                if fetched_at.elapsed() < stale_limit {
                    // fresh
                    let accepted = *gas_price;
                    g.cache_hits += 1;
                    return Some(accepted);
                }
            }
            g.next.subscribe()
        };

        // periodically polled or failure
        rx.recv().await.ok()
    }
}

struct Inner {
    latest: Option<(std::time::Instant, web3::types::H256)>,
    cache_hits: usize,
    next: tokio::sync::broadcast::Sender<web3::types::H256>,
}

impl Default for Inner {
    fn default() -> Self {
        let (tx, _) = tokio::sync::broadcast::channel(1);
        Inner {
            latest: None,
            cache_hits: 0,
            next: tx,
        }
    }
}

#[tracing::instrument(name = "fetch_eth_gasPrice", skip_all)]
pub async fn fetch_eth_gas_price_periodically(
    client: reqwest::Client,
    url: reqwest::Url,
    shared: Cached,
) {
    #[serde_with::serde_as]
    #[derive(serde::Deserialize, Debug)]
    struct Response {
        // internal of H256 required full width hex strings, so specify our own
        #[serde_as(as = "crate::rpc::serde::H256AsHexStr")]
        result: web3::types::H256,
    }

    let tx = shared
        .0
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .next
        .clone();

    let fetch_every = std::time::Duration::from_secs(5);
    let request_timeout = fetch_every;
    let minimum_sleep = std::time::Duration::from_secs(1);

    let retry_base = std::time::Duration::from_secs_f64(0.5);
    let mut retry_sleep = retry_base;
    let max_retry = std::time::Duration::from_secs(20);
    let retry_coefficient = 1.5;

    loop {
        let builder = client.post(url.clone());
        let result = builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(reqwest::header::USER_AGENT, crate::consts::USER_AGENT)
            .body(r#"{"jsonrpc":"2.0","id":"1","method":"eth_gasPrice"}"#)
            // rationale for 5s is around 10s block time on mainnet, and 15s on goerli
            .timeout(request_timeout)
            .send()
            .await
            .context("Failed to send request")
            .and_then(|r| r.error_for_status().context("Non-ok response"));

        let result = if let Ok(r) = result {
            let full = r.bytes().await.context("Failed to receive full response");
            full.and_then(|full| {
                serde_json::from_slice::<Response>(&full).with_context(|| {
                    format!(
                        "Failed to deserialize: {:?}",
                        String::from_utf8_lossy(&full)
                    )
                })
            })
        } else {
            Err(result.unwrap_err())
        };

        let gas_price = match result {
            Ok(x) => x.result,
            Err(e) => {
                retry_sleep = retry_sleep.mul_f64(retry_coefficient).min(max_retry);
                tracing::debug!(error=%e, wait=?retry_sleep, "Retrying after error with eth_getPrice");
                tokio::time::sleep(retry_sleep).await;
                continue;
            }
        };

        // reset retry as we succeed
        retry_sleep = retry_base;
        let fetched_at = std::time::Instant::now();

        // first send it out to any receivers
        // success or failure, we don't really care, sadly this cannot be used as a shutdown
        // mechanism either because we will always hold the tx alive.
        let _ = tx.send(gas_price);

        let (delay, last_hits) = {
            // then cache it
            let mut g = shared.0.lock().unwrap_or_else(|e| e.into_inner());
            let prev = g.latest.replace((fetched_at, gas_price));
            let last_hits = std::mem::take(&mut g.cache_hits);
            (prev.map(|(last_at, _)| (fetched_at - last_at)), last_hits)
        };

        tracing::debug!(between=?delay, cache_hits=last_hits, "Fetched eth_gasPrice");

        tokio::time::sleep(
            fetch_every
                .saturating_sub(fetched_at.elapsed())
                .max(minimum_sleep),
        )
        .await;
    }
}
