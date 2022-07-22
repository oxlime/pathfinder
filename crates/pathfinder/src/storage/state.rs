use anyhow::Context;
use rusqlite::{named_params, params, OptionalExtension, Transaction};
use stark_hash::StarkHash;
use web3::types::H256;

use crate::{
    consts::{GOERLI_GENESIS_HASH, MAINNET_GENESIS_HASH},
    core::{
        Chain, ClassHash, ContractAddress, ContractRoot, ContractStateHash, EthereumBlockHash,
        EthereumBlockNumber, EthereumLogIndex, EthereumTransactionHash, EthereumTransactionIndex,
        EventData, EventKey, GasPrice, GlobalRoot, SequencerAddress, StarknetBlockHash,
        StarknetBlockNumber, StarknetBlockTimestamp, StarknetTransactionHash,
    },
    ethereum::{log::StateUpdateLog, BlockOrigin, EthOrigin, TransactionOrigin},
    rpc::types::reply::StateUpdate,
    sequencer::reply::transaction,
};

/// Contains the [L1 Starknet update logs](StateUpdateLog).
pub struct L1StateTable {}

/// Identifies block in some [L1StateTable] queries.
pub enum L1TableBlockId {
    Number(StarknetBlockNumber),
    Latest,
}

impl From<StarknetBlockNumber> for L1TableBlockId {
    fn from(number: StarknetBlockNumber) -> Self {
        L1TableBlockId::Number(number)
    }
}

impl L1StateTable {
    /// Inserts a new [update](StateUpdateLog), replaces if it already exists.
    pub fn upsert(tx: &Transaction<'_>, update: &StateUpdateLog) -> anyhow::Result<()> {
        tx.execute(
            r"INSERT OR REPLACE INTO l1_state (
                        starknet_block_number,
                        starknet_global_root,
                        ethereum_block_hash,
                        ethereum_block_number,
                        ethereum_transaction_hash,
                        ethereum_transaction_index,
                        ethereum_log_index
                    ) VALUES (
                        :starknet_block_number,
                        :starknet_global_root,
                        :ethereum_block_hash,
                        :ethereum_block_number,
                        :ethereum_transaction_hash,
                        :ethereum_transaction_index,
                        :ethereum_log_index
                    )",
            named_params! {
                ":starknet_block_number": update.block_number.0,
                ":starknet_global_root": update.global_root.0.as_be_bytes(),
                ":ethereum_block_hash": &update.origin.block.hash.0[..],
                ":ethereum_block_number": update.origin.block.number.0,
                ":ethereum_transaction_hash": &update.origin.transaction.hash.0[..],
                ":ethereum_transaction_index": update.origin.transaction.index.0,
                ":ethereum_log_index": update.origin.log_index.0,
            },
        )?;

        Ok(())
    }

    /// Deletes all rows from __head down-to reorg_tail__
    /// i.e. it deletes all rows where `block number >= reorg_tail`.
    pub fn reorg(tx: &Transaction<'_>, reorg_tail: StarknetBlockNumber) -> anyhow::Result<()> {
        tx.execute(
            "DELETE FROM l1_state WHERE starknet_block_number >= ?",
            params![reorg_tail.0],
        )?;
        Ok(())
    }

    /// Returns the [root](GlobalRoot) of the given block.
    pub fn get_root(
        tx: &Transaction<'_>,
        block: L1TableBlockId,
    ) -> anyhow::Result<Option<GlobalRoot>> {
        let mut statement = match block {
            L1TableBlockId::Number(_) => {
                tx.prepare("SELECT starknet_global_root FROM l1_state WHERE starknet_block_number = ?")
            }
            L1TableBlockId::Latest => tx
                .prepare("SELECT starknet_global_root FROM l1_state ORDER BY starknet_block_number DESC LIMIT 1"),
        }?;

        let mut rows = match block {
            L1TableBlockId::Number(number) => statement.query(params![number.0]),
            L1TableBlockId::Latest => statement.query([]),
        }?;

        let row = rows.next()?;
        let row = match row {
            Some(row) => row,
            None => return Ok(None),
        };

        let starknet_global_root = row
            .get_ref_unwrap("starknet_global_root")
            .as_blob()
            .unwrap();
        let starknet_global_root = StarkHash::from_be_slice(starknet_global_root).unwrap();
        let starknet_global_root = GlobalRoot(starknet_global_root);

        Ok(Some(starknet_global_root))
    }

    /// Returns the [update](StateUpdateLog) of the given block.
    pub fn get(
        tx: &Transaction<'_>,
        block: L1TableBlockId,
    ) -> anyhow::Result<Option<StateUpdateLog>> {
        let mut statement = match block {
            L1TableBlockId::Number(_) => tx.prepare(
                r"SELECT starknet_block_number,
                    starknet_global_root,
                    ethereum_block_hash,
                    ethereum_block_number,
                    ethereum_transaction_hash,
                    ethereum_transaction_index,
                    ethereum_log_index
                FROM l1_state WHERE starknet_block_number = ?",
            ),
            L1TableBlockId::Latest => tx.prepare(
                r"SELECT starknet_block_number,
                    starknet_global_root,
                    ethereum_block_hash,
                    ethereum_block_number,
                    ethereum_transaction_hash,
                    ethereum_transaction_index,
                    ethereum_log_index
                FROM l1_state ORDER BY starknet_block_number DESC LIMIT 1",
            ),
        }?;

        let mut rows = match block {
            L1TableBlockId::Number(number) => statement.query(params![number.0]),
            L1TableBlockId::Latest => statement.query([]),
        }?;

        let row = rows.next()?;
        let row = match row {
            Some(row) => row,
            None => return Ok(None),
        };

        let starknet_block_number = row
            .get_ref_unwrap("starknet_block_number")
            .as_i64()
            .unwrap() as u64;
        let starknet_block_number = StarknetBlockNumber(starknet_block_number);

        let starknet_global_root = row
            .get_ref_unwrap("starknet_global_root")
            .as_blob()
            .unwrap();
        let starknet_global_root = StarkHash::from_be_slice(starknet_global_root).unwrap();
        let starknet_global_root = GlobalRoot(starknet_global_root);

        let ethereum_block_hash = row.get_ref_unwrap("ethereum_block_hash").as_blob().unwrap();
        let ethereum_block_hash = EthereumBlockHash(H256(ethereum_block_hash.try_into().unwrap()));

        let ethereum_block_number = row
            .get_ref_unwrap("ethereum_block_number")
            .as_i64()
            .unwrap() as u64;
        let ethereum_block_number = EthereumBlockNumber(ethereum_block_number);

        let ethereum_transaction_hash = row
            .get_ref_unwrap("ethereum_transaction_hash")
            .as_blob()
            .unwrap();
        let ethereum_transaction_hash =
            EthereumTransactionHash(H256(ethereum_transaction_hash.try_into().unwrap()));

        let ethereum_transaction_index = row
            .get_ref_unwrap("ethereum_transaction_index")
            .as_i64()
            .unwrap() as u64;
        let ethereum_transaction_index = EthereumTransactionIndex(ethereum_transaction_index);

        let ethereum_log_index = row.get_ref_unwrap("ethereum_log_index").as_i64().unwrap() as u64;
        let ethereum_log_index = EthereumLogIndex(ethereum_log_index);

        Ok(Some(StateUpdateLog {
            origin: EthOrigin {
                block: BlockOrigin {
                    hash: ethereum_block_hash,
                    number: ethereum_block_number,
                },
                transaction: TransactionOrigin {
                    hash: ethereum_transaction_hash,
                    index: ethereum_transaction_index,
                },
                log_index: ethereum_log_index,
            },
            global_root: starknet_global_root,
            block_number: starknet_block_number,
        }))
    }
}

pub struct RefsTable {}
impl RefsTable {
    /// Returns the current L1-L2 head. This indicates the latest block for which L1 and L2 agree.
    pub fn get_l1_l2_head(tx: &Transaction<'_>) -> anyhow::Result<Option<StarknetBlockNumber>> {
        // This table always contains exactly one row.
        let block_number =
            tx.query_row("SELECT l1_l2_head FROM refs WHERE idx = 1", [], |row| {
                let block_number = row
                    .get_ref_unwrap(0)
                    .as_i64_or_null()
                    .unwrap()
                    .map(|x| StarknetBlockNumber(x as u64));

                Ok(block_number)
            })?;

        Ok(block_number)
    }

    /// Sets the current L1-L2 head. This should indicate the latest block for which L1 and L2 agree.
    pub fn set_l1_l2_head(
        tx: &Transaction<'_>,
        head: Option<StarknetBlockNumber>,
    ) -> anyhow::Result<()> {
        match head {
            Some(number) => tx.execute("UPDATE refs SET l1_l2_head = ? WHERE idx = 1", [number.0]),
            None => tx.execute("UPDATE refs SET l1_l2_head = NULL WHERE idx = 1", []),
        }?;

        Ok(())
    }
}
/// Stores all known [StarknetBlocks][StarknetBlock].
pub struct StarknetBlocksTable {}
impl StarknetBlocksTable {
    /// Insert a new [StarknetBlock]. Fails if the block number is not unique.
    ///
    /// Version is the [`crate::sequencer::reply::Block::starknet_version`].
    pub fn insert(
        tx: &Transaction<'_>,
        block: &StarknetBlock,
        version: Option<&str>,
    ) -> anyhow::Result<()> {
        let version_id = if let Some(version) = version {
            Some(StarknetVersionsTable::intern(tx, version)?)
        } else {
            None
        };

        tx.execute(
            r"INSERT INTO starknet_blocks ( number,  hash,  root,  timestamp,  gas_price,  sequencer_address,  version_id)
                                   VALUES (:number, :hash, :root, :timestamp, :gas_price, :sequencer_address, :version_id)",
            named_params! {
                ":number": block.number.0,
                ":hash": block.hash.0.as_be_bytes(),
                ":root": block.root.0.as_be_bytes(),
                ":timestamp": block.timestamp.0,
                ":gas_price": &block.gas_price.to_be_bytes(),
                ":sequencer_address": block.sequencer_address.0.as_be_bytes(),
                ":version_id": version_id,
            },
        )?;

        Ok(())
    }

    /// Returns the requested [StarknetBlock].
    pub fn get(
        tx: &Transaction<'_>,
        block: StarknetBlocksBlockId,
    ) -> anyhow::Result<Option<StarknetBlock>> {
        let mut statement = match block {
            StarknetBlocksBlockId::Number(_) => tx.prepare(
                "SELECT hash, number, root, timestamp, gas_price, sequencer_address
                    FROM starknet_blocks WHERE number = ?",
            ),
            StarknetBlocksBlockId::Hash(_) => tx.prepare(
                "SELECT hash, number, root, timestamp, gas_price, sequencer_address
                    FROM starknet_blocks WHERE hash = ?",
            ),
            StarknetBlocksBlockId::Latest => tx.prepare(
                "SELECT hash, number, root, timestamp, gas_price, sequencer_address
                    FROM starknet_blocks ORDER BY number DESC LIMIT 1",
            ),
        }?;

        let mut rows = match block {
            StarknetBlocksBlockId::Number(number) => statement.query(params![number.0]),
            StarknetBlocksBlockId::Hash(hash) => statement.query(params![hash.0.as_be_bytes()]),
            StarknetBlocksBlockId::Latest => statement.query([]),
        }?;

        let row = rows.next().context("Iterate rows")?;

        match row {
            Some(row) => {
                let number = row.get_ref_unwrap("number").as_i64().unwrap() as u64;
                let number = StarknetBlockNumber(number);

                let hash = row.get_ref_unwrap("hash").as_blob().unwrap();
                let hash = StarkHash::from_be_slice(hash).unwrap();
                let hash = StarknetBlockHash(hash);

                let root = row.get_ref_unwrap("root").as_blob().unwrap();
                let root = StarkHash::from_be_slice(root).unwrap();
                let root = GlobalRoot(root);

                let timestamp = row.get_ref_unwrap("timestamp").as_i64().unwrap() as u64;
                let timestamp = StarknetBlockTimestamp(timestamp);

                let gas_price = row.get_ref_unwrap("gas_price").as_blob().unwrap();
                let gas_price = GasPrice::from_be_slice(gas_price).unwrap();

                let sequencer_address = row.get_ref_unwrap("sequencer_address").as_blob().unwrap();
                let sequencer_address = StarkHash::from_be_slice(sequencer_address).unwrap();
                let sequencer_address = SequencerAddress(sequencer_address);

                let block = StarknetBlock {
                    number,
                    hash,
                    root,
                    timestamp,
                    gas_price,
                    sequencer_address,
                };

                Ok(Some(block))
            }
            None => Ok(None),
        }
    }

    /// Returns the [root](GlobalRoot) of the given block.
    pub fn get_root(
        tx: &Transaction<'_>,
        block: StarknetBlocksBlockId,
    ) -> anyhow::Result<Option<GlobalRoot>> {
        let mut statement = match block {
            StarknetBlocksBlockId::Number(_) => {
                tx.prepare("SELECT root FROM starknet_blocks WHERE number = ?")
            }
            StarknetBlocksBlockId::Hash(_) => {
                tx.prepare("SELECT root FROM starknet_blocks WHERE hash = ?")
            }
            StarknetBlocksBlockId::Latest => {
                tx.prepare("SELECT root FROM starknet_blocks ORDER BY number DESC LIMIT 1")
            }
        }?;

        let mut rows = match block {
            StarknetBlocksBlockId::Number(number) => statement.query(params![number.0]),
            StarknetBlocksBlockId::Hash(hash) => statement.query(params![hash.0.as_be_bytes()]),
            StarknetBlocksBlockId::Latest => statement.query([]),
        }?;

        let row = rows.next().context("Iterate rows")?;
        match row {
            Some(row) => {
                let root = row.get_ref_unwrap("root").as_blob().unwrap();
                let root = StarkHash::from_be_slice(root).unwrap();
                let root = GlobalRoot(root);
                Ok(Some(root))
            }
            None => Ok(None),
        }
    }

    /// Deletes all rows from __head down-to reorg_tail__
    /// i.e. it deletes all rows where `block number >= reorg_tail`.
    pub fn reorg(tx: &Transaction<'_>, reorg_tail: StarknetBlockNumber) -> anyhow::Result<()> {
        tx.execute(
            "DELETE FROM starknet_blocks WHERE number >= ?",
            params![reorg_tail.0],
        )?;
        Ok(())
    }

    /// Returns the [number](StarknetBlockNumber) of the latest block.
    pub fn get_latest_number(tx: &Transaction<'_>) -> anyhow::Result<Option<StarknetBlockNumber>> {
        let mut statement =
            tx.prepare("SELECT number FROM starknet_blocks ORDER BY number DESC LIMIT 1")?;
        let mut rows = statement.query([])?;
        let row = rows.next().context("Iterate rows")?;
        match row {
            Some(row) => {
                let number = row.get_ref_unwrap("number").as_i64().unwrap() as u64;
                let number = StarknetBlockNumber(number);
                Ok(Some(number))
            }
            None => Ok(None),
        }
    }

    /// Returns the [chain](crate::core::Chain) based on genesis block hash stored in the DB.
    pub fn determine_chain(tx: &Transaction<'_>) -> anyhow::Result<Option<Chain>> {
        let genesis = Self::get(tx, StarknetBlockNumber(0).into())
            .context("Read genesis block from database")?;

        match genesis {
            None => Ok(None),
            Some(x) if x.hash == *GOERLI_GENESIS_HASH => Ok(Some(Chain::Goerli)),
            Some(x) if x.hash == *MAINNET_GENESIS_HASH => Ok(Some(Chain::Mainnet)),
            Some(x) => Err(anyhow::anyhow!("Unknown genesis block hash {}", x.hash.0)),
        }
    }
}

/// Identifies block in some [StarknetBlocksTable] queries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StarknetBlocksBlockId {
    Number(StarknetBlockNumber),
    Hash(StarknetBlockHash),
    Latest,
}

impl From<StarknetBlockNumber> for StarknetBlocksBlockId {
    fn from(number: StarknetBlockNumber) -> Self {
        StarknetBlocksBlockId::Number(number)
    }
}

impl From<StarknetBlockHash> for StarknetBlocksBlockId {
    fn from(hash: StarknetBlockHash) -> Self {
        StarknetBlocksBlockId::Hash(hash)
    }
}

/// Stores all known starknet transactions
pub struct StarknetTransactionsTable {}
impl StarknetTransactionsTable {
    /// Inserts a Starknet block's transactions and transaction receipts into the [StarknetTransactionsTable].
    ///
    /// overwrites existing data if the transaction hash already exists.
    pub fn upsert(
        tx: &Transaction<'_>,
        block_hash: StarknetBlockHash,
        block_number: StarknetBlockNumber,
        transaction_data: &[(transaction::Transaction, transaction::Receipt)],
    ) -> anyhow::Result<()> {
        if transaction_data.is_empty() {
            return Ok(());
        }

        let mut compressor = zstd::bulk::Compressor::new(10).context("Create zstd compressor")?;
        for (i, (transaction, receipt)) in transaction_data.iter().enumerate() {
            // Serialize and compress transaction data.
            let tx_data =
                serde_json::ser::to_vec(&transaction).context("Serialize Starknet transaction")?;
            let tx_data = compressor
                .compress(&tx_data)
                .context("Compress Starknet transaction")?;

            let serialized_receipt = serde_json::ser::to_vec(&receipt)
                .context("Serialize Starknet transaction receipt")?;
            let serialized_receipt = compressor
                .compress(&serialized_receipt)
                .context("Compress Starknet transaction receipt")?;

            tx.execute(r"INSERT OR REPLACE INTO starknet_transactions (hash, idx, block_hash, tx, receipt) VALUES (:hash, :idx, :block_hash, :tx, :receipt)",
        named_params![
                    ":hash": transaction.hash().0.as_be_bytes(),
                    ":idx": i,
                    ":block_hash": block_hash.0.as_be_bytes(),
                    ":tx": &tx_data,
                    ":receipt": &serialized_receipt,
                ]).context("Insert transaction data into transactions table")?;

            // insert events from receipt
            StarknetEventsTable::insert_events(tx, block_number, transaction, &receipt.events)?;
        }

        Ok(())
    }

    pub fn get_transaction_data_for_block(
        tx: &Transaction<'_>,
        block: StarknetBlocksBlockId,
    ) -> anyhow::Result<Vec<(transaction::Transaction, transaction::Receipt)>> {
        // Identify block hash
        let block_hash = match block {
            StarknetBlocksBlockId::Number(number) => {
                match StarknetBlocksTable::get(tx, number.into())? {
                    Some(block) => block.hash,
                    None => return Ok(Vec::new()),
                }
            }
            StarknetBlocksBlockId::Hash(hash) => hash,
            StarknetBlocksBlockId::Latest => {
                match StarknetBlocksTable::get(tx, StarknetBlocksBlockId::Latest)? {
                    Some(block) => block.hash,
                    None => return Ok(Vec::new()),
                }
            }
        };

        let mut stmt = tx
            .prepare(
                "SELECT tx, receipt FROM starknet_transactions WHERE block_hash = ? ORDER BY idx ASC",
            )
            .context("Preparing statement")?;

        let mut rows = stmt
            .query(params![block_hash.0.as_be_bytes()])
            .context("Executing query")?;

        let mut data = Vec::new();
        while let Some(row) = rows.next()? {
            let receipt = row
                .get_ref_unwrap("receipt")
                .as_blob_or_null()?
                .context("Receipt data missing")?;
            let receipt = zstd::decode_all(receipt).context("Decompressing transaction receipt")?;
            let receipt =
                serde_json::from_slice(&receipt).context("Deserializing transaction receipt")?;

            let transaction = row
                .get_ref_unwrap("tx")
                .as_blob_or_null()?
                .context("Transaction data missing")?;
            let transaction = zstd::decode_all(transaction).context("Decompressing transaction")?;
            let transaction =
                serde_json::from_slice(&transaction).context("Deserializing transaction")?;

            data.push((transaction, receipt));
        }

        Ok(data)
    }

    pub fn get_transaction_at_block(
        tx: &Transaction<'_>,
        block: StarknetBlocksBlockId,
        index: usize,
    ) -> anyhow::Result<Option<transaction::Transaction>> {
        // Identify block hash
        let block_hash = match block {
            StarknetBlocksBlockId::Number(number) => {
                match StarknetBlocksTable::get(tx, number.into())? {
                    Some(block) => block.hash,
                    None => return Ok(None),
                }
            }
            StarknetBlocksBlockId::Hash(hash) => hash,
            StarknetBlocksBlockId::Latest => {
                match StarknetBlocksTable::get(tx, StarknetBlocksBlockId::Latest)? {
                    Some(block) => block.hash,
                    None => return Ok(None),
                }
            }
        };

        let mut stmt = tx
            .prepare("SELECT tx FROM starknet_transactions WHERE block_hash = ? AND idx = ?")
            .context("Preparing statement")?;

        let mut rows = stmt
            .query(params![block_hash.0.as_be_bytes(), index])
            .context("Executing query")?;

        let row = match rows.next()? {
            Some(row) => row,
            None => return Ok(None),
        };

        let transaction = match row.get_ref_unwrap(0).as_blob_or_null()? {
            Some(data) => data,
            None => return Ok(None),
        };

        let transaction = zstd::decode_all(transaction).context("Decompressing transaction")?;
        let transaction =
            serde_json::from_slice(&transaction).context("Deserializing transaction")?;

        Ok(Some(transaction))
    }

    pub fn get_receipt(
        tx: &Transaction<'_>,
        transaction: StarknetTransactionHash,
    ) -> anyhow::Result<Option<(transaction::Receipt, StarknetBlockHash)>> {
        let mut stmt = tx
            .prepare("SELECT receipt, block_hash FROM starknet_transactions WHERE hash = ?1")
            .context("Preparing statement")?;

        let mut rows = stmt
            .query(params![transaction.0.as_be_bytes()])
            .context("Executing query")?;

        let row = match rows.next()? {
            Some(row) => row,
            None => return Ok(None),
        };

        let receipt = match row.get_ref_unwrap("receipt").as_blob_or_null()? {
            Some(data) => data,
            None => return Ok(None),
        };
        let receipt = zstd::decode_all(receipt).context("Decompressing transaction")?;
        let receipt = serde_json::from_slice(&receipt).context("Deserializing transaction")?;

        let block_hash = row.get_ref_unwrap("block_hash").as_blob()?;
        let block_hash =
            StarkHash::from_be_slice(block_hash).context("Deserializing block hash")?;
        let block_hash = StarknetBlockHash(block_hash);

        Ok(Some((receipt, block_hash)))
    }

    pub fn get_transaction(
        tx: &Transaction<'_>,
        transaction: StarknetTransactionHash,
    ) -> anyhow::Result<Option<transaction::Transaction>> {
        let mut stmt = tx
            .prepare("SELECT tx FROM starknet_transactions WHERE hash = ?1")
            .context("Preparing statement")?;

        let mut rows = stmt
            .query(params![transaction.0.as_be_bytes()])
            .context("Executing query")?;

        let row = match rows.next()? {
            Some(row) => row,
            None => return Ok(None),
        };

        let transaction = match row.get_ref_unwrap(0).as_blob_or_null()? {
            Some(data) => data,
            None => return Ok(None),
        };

        let transaction = zstd::decode_all(transaction).context("Decompressing transaction")?;
        let transaction =
            serde_json::from_slice(&transaction).context("Deserializing transaction")?;

        Ok(Some(transaction))
    }

    pub fn get_transaction_count(
        tx: &Transaction<'_>,
        block: StarknetBlocksBlockId,
    ) -> anyhow::Result<usize> {
        match block {
            StarknetBlocksBlockId::Number(number) => tx
                .query_row(
                    "SELECT COUNT(*) FROM starknet_transactions
                    JOIN starknet_blocks ON starknet_transactions.block_hash = starknet_blocks.hash
                    WHERE number = ?1",
                    params![number.0],
                    |row| row.get(0),
                )
                .context("Counting transactions"),
            StarknetBlocksBlockId::Hash(hash) => tx
                .query_row(
                    "SELECT COUNT(*) FROM starknet_transactions WHERE block_hash = ?1",
                    params![hash.0.as_be_bytes()],
                    |row| row.get(0),
                )
                .context("Counting transactions"),
            StarknetBlocksBlockId::Latest => {
                // First get the latest block
                let block = match StarknetBlocksTable::get(tx, StarknetBlocksBlockId::Latest)? {
                    Some(block) => block.number,
                    None => return Ok(0),
                };

                Self::get_transaction_count(tx, block.into())
            }
        }
    }
}

pub struct StarknetEventFilter {
    pub from_block: Option<StarknetBlockNumber>,
    pub to_block: Option<StarknetBlockNumber>,
    pub contract_address: Option<ContractAddress>,
    pub keys: Vec<EventKey>,
    pub page_size: usize,
    pub page_number: usize,
}

impl From<crate::rpc::types::request::EventFilter> for StarknetEventFilter {
    fn from(filter: crate::rpc::types::request::EventFilter) -> Self {
        Self {
            from_block: filter.from_block,
            to_block: filter.to_block,
            contract_address: filter.address,
            keys: filter.keys,
            page_size: filter.page_size,
            page_number: filter.page_number,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct StarknetEmittedEvent {
    pub from_address: ContractAddress,
    pub data: Vec<EventData>,
    pub keys: Vec<EventKey>,
    pub block_hash: StarknetBlockHash,
    pub block_number: StarknetBlockNumber,
    pub transaction_hash: StarknetTransactionHash,
}

#[derive(Copy, Clone, Debug, thiserror::Error, PartialEq)]
pub enum EventFilterError {
    #[error("requested page size is too big, supported maximum is {0}")]
    PageSizeTooBig(usize),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PageOfEvents {
    pub events: Vec<StarknetEmittedEvent>,
    pub is_last_page: bool,
}

pub struct StarknetEventsTable {}
impl StarknetEventsTable {
    pub fn event_data_to_bytes(data: &[EventData]) -> Vec<u8> {
        data.iter()
            .flat_map(|e| (*e.0.as_be_bytes()).into_iter())
            .collect()
    }

    fn event_key_to_base64_string(key: &EventKey) -> String {
        base64::encode(key.0.as_be_bytes())
    }

    pub fn event_keys_to_base64_strings(keys: &[EventKey]) -> String {
        // TODO: we really should be using Iterator::intersperse() here once it's stabilized.
        let keys: Vec<String> = keys.iter().map(Self::event_key_to_base64_string).collect();
        keys.join(" ")
    }

    pub fn insert_events(
        tx: &Transaction<'_>,
        block_number: StarknetBlockNumber,
        transaction: &transaction::Transaction,
        events: &[transaction::Event],
    ) -> anyhow::Result<()> {
        match transaction {
            transaction::Transaction::Declare(_) => {
                anyhow::ensure!(
                    events.is_empty(),
                    "Declare transactions cannot emit events: block {}, transaction {}",
                    block_number,
                    transaction.hash().0
                );
                Ok(())
            }
            transaction::Transaction::Deploy(transaction::DeployTransaction {
                transaction_hash,
                ..
            })
            | transaction::Transaction::Invoke(transaction::InvokeTransaction {
                transaction_hash,
                ..
            }) => {
                for (idx, event) in events.iter().enumerate() {
                    tx
                        .execute(
                            r"INSERT INTO starknet_events ( block_number,  idx,  transaction_hash,  from_address,  keys,  data)
                                                   VALUES (:block_number, :idx, :transaction_hash, :from_address, :keys, :data)",
                            named_params![
                                ":block_number": block_number.0,
                                ":idx": idx,
                                ":transaction_hash": &transaction_hash.0.as_be_bytes()[..],
                                ":from_address": &event.from_address.0.as_be_bytes()[..],
                                ":keys": Self::event_keys_to_base64_strings(&event.keys),
                                ":data": Self::event_data_to_bytes(&event.data),
                            ],
                        )
                        .context("Insert events into events table")?;
                }
                Ok(())
            }
        }
    }

    pub(crate) const PAGE_SIZE_LIMIT: usize = 1024;

    pub fn get_events(
        tx: &Transaction<'_>,
        filter: &StarknetEventFilter,
    ) -> anyhow::Result<PageOfEvents> {
        let mut base_query =
            r#"SELECT
                  block_number,
                  starknet_blocks.hash as block_hash,
                  transaction_hash,
                  starknet_transactions.idx as transaction_idx,
                  from_address,
                  data,
                  starknet_events.keys as keys
               FROM starknet_events
               INNER JOIN starknet_transactions ON (starknet_transactions.hash = starknet_events.transaction_hash)
               INNER JOIN starknet_blocks ON (starknet_blocks.number = starknet_events.block_number)"#
                .to_string();
        let mut where_statement_parts: Vec<&'static str> = Vec::new();
        let mut params: Vec<(&str, &dyn rusqlite::ToSql)> = Vec::new();

        // filter on block range
        match (&filter.from_block, &filter.to_block) {
            (Some(from_block), Some(to_block)) => {
                where_statement_parts.push("block_number BETWEEN :from_block AND :to_block");
                params.push((":from_block", &from_block.0));
                params.push((":to_block", &to_block.0));
            }
            (Some(from_block), None) => {
                where_statement_parts.push("block_number >= :from_block");
                params.push((":from_block", &from_block.0));
            }
            (None, Some(to_block)) => {
                where_statement_parts.push("block_number <= :to_block");
                params.push((":to_block", &to_block.0));
            }
            (None, None) => {}
        }

        // filter on contract address
        if let Some(contract_address) = &filter.contract_address {
            where_statement_parts.push("from_address = :contract_address");
            params.push((":contract_address", contract_address.0.as_be_bytes()))
        }

        // Filter on keys: this is using an FTS5 full-text index (virtual table) on the keys.
        // The idea is that we convert keys to a space-separated list of Bas64 encoded string
        // representation and then use the full-text index to find events matching the events.
        // HACK: make sure key_fts_expression lives long enough
        let key_fts_expression;
        if !filter.keys.is_empty() {
            let base64_keys: Vec<String> = filter
                .keys
                .iter()
                .map(|key| format!("\"{}\"", Self::event_key_to_base64_string(key)))
                .collect();
            key_fts_expression = base64_keys.join(" OR ");

            base_query.push_str("INNER JOIN starknet_events_keys ON starknet_events.rowid = starknet_events_keys.rowid");
            where_statement_parts.push("starknet_events_keys.keys MATCH :events_match");
            params.push((":events_match", &key_fts_expression));
        }

        // Paging
        if filter.page_size > Self::PAGE_SIZE_LIMIT {
            return Err(EventFilterError::PageSizeTooBig(Self::PAGE_SIZE_LIMIT).into());
        }
        if filter.page_size < 1 {
            anyhow::bail!("Invalid page size");
        }
        let offset = filter.page_number * filter.page_size;
        // We have to be able to decide if there are more events. We request one extra event
        // above the requested page size, so that we can decide.
        let limit = filter.page_size + 1;
        params.push((":limit", &limit));
        params.push((":offset", &offset));

        let query = if where_statement_parts.is_empty() {
            format!(
                "{} ORDER BY block_number, transaction_idx, starknet_events.idx LIMIT :limit OFFSET :offset",
                base_query
            )
        } else {
            format!(
                "{} WHERE {} ORDER BY block_number, transaction_idx, starknet_events.idx LIMIT :limit OFFSET :offset",
                base_query,
                where_statement_parts.join(" AND "),
            )
        };

        let mut statement = tx.prepare(&query).context("Preparing SQL query")?;
        let mut rows = statement
            .query(params.as_slice())
            .context("Executing SQL query")?;

        let mut is_last_page = true;
        let mut emitted_events = Vec::new();
        while let Some(row) = rows.next().context("Fetching next event")? {
            let block_number = row.get_ref_unwrap("block_number").as_i64().unwrap() as u64;
            let block_number = StarknetBlockNumber(block_number);

            let block_hash = row.get_ref_unwrap("block_hash").as_blob().unwrap();
            let block_hash = StarkHash::from_be_slice(block_hash).unwrap();
            let block_hash = StarknetBlockHash(block_hash);

            let transaction_hash = row.get_ref_unwrap("transaction_hash").as_blob().unwrap();
            let transaction_hash = StarkHash::from_be_slice(transaction_hash).unwrap();
            let transaction_hash = StarknetTransactionHash(transaction_hash);

            let from_address = row.get_ref_unwrap("from_address").as_blob().unwrap();
            let from_address = StarkHash::from_be_slice(from_address).unwrap();
            let from_address = ContractAddress(from_address);

            let data = row.get_ref_unwrap("data").as_blob().unwrap();
            let data: Vec<_> = data
                .chunks_exact(32)
                .map(|data| {
                    let data = StarkHash::from_be_slice(data).unwrap();
                    EventData(data)
                })
                .collect();

            let keys = row.get_ref_unwrap("keys").as_str().unwrap();
            let keys: Vec<_> = keys
                .split(' ')
                .map(|key| {
                    let key = StarkHash::from_be_slice(&base64::decode(key).unwrap()).unwrap();
                    EventKey(key)
                })
                .collect();

            if emitted_events.len() == filter.page_size {
                // We already have a full page, and are just fetching the extra event
                // This means that there are more pages.
                is_last_page = false;
            } else {
                let event = StarknetEmittedEvent {
                    data,
                    from_address,
                    keys,
                    block_hash,
                    block_number,
                    transaction_hash,
                };
                emitted_events.push(event);
            }
        }

        Ok(PageOfEvents {
            events: emitted_events,
            is_last_page,
        })
    }
}

/// Describes a Starknet block.
///
/// While the sequencer version on each block (when present) is stored since starknet 0.9.1, it is
/// not yet read.
#[derive(Debug, Clone, PartialEq)]
pub struct StarknetBlock {
    pub number: StarknetBlockNumber,
    pub hash: StarknetBlockHash,
    pub root: GlobalRoot,
    pub timestamp: StarknetBlockTimestamp,
    pub gas_price: GasPrice,
    pub sequencer_address: SequencerAddress,
}

/// StarknetVersionsTable tracks `starknet_versions` table, which just interns the version
/// metadata on each block.
///
/// It was decided to go with interned approach, as we couldn't be sure that a semantic version
/// string format is followed. Semantic version strings may have been cheaper to just store
/// in-line.
///
/// Introduced in [`super::schema::revision_0014::migrate`].
struct StarknetVersionsTable;

impl StarknetVersionsTable {
    /// Interns, or makes sure there's a unique row for each version.
    ///
    /// These are not deleted automatically nor is a need expected due to multiple blocks being
    /// generated with a single starknet version.
    fn intern(transaction: &Transaction<'_>, version: &str) -> anyhow::Result<i64> {
        let id: Option<i64> = transaction
            .query_row(
                "SELECT id FROM starknet_versions WHERE version = ?",
                &[version],
                |r| Ok(r.get_unwrap(0)),
            )
            .optional()
            .context("Querying for an existing starknet_version")?;

        let id = if let Some(id) = id {
            id
        } else {
            // sqlite "autoincrement" for integer primary keys works like this: we leave it out of
            // the insert, even though it's not null, it will get max(id)+1 assigned, which we can
            // read back with last_insert_rowid
            let rows = transaction
                .execute(
                    "INSERT INTO starknet_versions(version) VALUES (?)",
                    &[version],
                )
                .context("Inserting unique starknet_version")?;

            anyhow::ensure!(rows == 1, "Unexpected number of rows inserted: {rows}");

            transaction.last_insert_rowid()
        };

        Ok(id)
    }
}

/// Stores the contract state hash along with its preimage. This is useful to
/// map between the global state tree and the contracts tree.
///
/// Specifically it stores
///
/// - [contract state hash](ContractStateHash)
/// - [class hash](ClassHash)
/// - [contract root](ContractRoot)
pub struct ContractsStateTable {}

impl ContractsStateTable {
    /// Insert a state hash into the table, overwrites the data if the hash already exists.
    pub fn upsert(
        transaction: &Transaction<'_>,
        state_hash: ContractStateHash,
        hash: ClassHash,
        root: ContractRoot,
    ) -> anyhow::Result<()> {
        transaction.execute(
            "INSERT OR IGNORE INTO contract_states (state_hash, hash, root) VALUES (:state_hash, :hash, :root)",
            named_params! {
                ":state_hash": state_hash.0.to_be_bytes(),
                ":hash": hash.0.to_be_bytes(),
                ":root": root.0.to_be_bytes(),
            },
        )?;
        Ok(())
    }

    /// Gets the root associated with the given state hash, or [None]
    /// if it does not exist.
    pub fn get_root(
        transaction: &Transaction<'_>,
        state_hash: ContractStateHash,
    ) -> anyhow::Result<Option<ContractRoot>> {
        let bytes: Option<Vec<u8>> = transaction
            .query_row(
                "SELECT root FROM contract_states WHERE state_hash = :state_hash",
                named_params! {
                    ":state_hash": state_hash.0.to_be_bytes()
                },
                |row| row.get("root"),
            )
            .optional()?;

        let bytes = match bytes {
            Some(bytes) => bytes,
            None => return Ok(None),
        };

        let bytes: [u8; 32] = match bytes.try_into() {
            Ok(bytes) => bytes,
            Err(bytes) => anyhow::bail!("Bad contract root length: {}", bytes.len()),
        };

        let root = StarkHash::from_be_bytes(bytes)?;
        let root = ContractRoot(root);

        Ok(Some(root))
    }
}

/// Stores all known [Starknet state updates][crate::rpc::types::reply::StateUpdate].
pub struct StarknetStateUpdatesTable {}
impl StarknetStateUpdatesTable {
    /// Inserts a StarkNet state update accociated with a particular block into the [StarknetStateUpdatesTable].
    ///
    /// Overwrites existing data if the block hash already exists.
    // FIXME change to an insert
    pub fn upsert(
        tx: &Transaction<'_>,
        block_hash: StarknetBlockHash,
        state_update: &StateUpdate,
    ) -> anyhow::Result<()> {
        let serialized =
            serde_json::to_vec(&state_update).context("Serialize Starknet state update")?;

        let mut compressor = zstd::bulk::Compressor::new(10).context("Create zstd compressor")?;
        let compressed = compressor
            .compress(&serialized)
            .context("Compress Starknet state update")?;

        tx.execute(r"INSERT OR REPLACE INTO starknet_state_updates (block_hash, data) VALUES (:block_hash, :data)",
        named_params![
            ":block_hash": block_hash.0.as_be_bytes(),
            ":data": &compressed,
        ]).context("Insert state update data into state updates table")?;

        Ok(())
    }

    /// Gets a StarkNet state update for block.
    pub fn get(
        tx: &Transaction<'_>,
        block_hash: StarknetBlockHash,
    ) -> anyhow::Result<Option<transaction::Transaction>> {
        let mut stmt = tx
            .prepare("SELECT data FROM starknet_state_updates WHERE block_hash = ?1")
            .context("Preparing statement")?;

        let mut rows = stmt
            .query(params![block_hash.0.as_be_bytes()])
            .context("Executing query")?;

        let row = match rows.next()? {
            Some(row) => row,
            None => return Ok(None),
        };

        let state_update = row.get_ref_unwrap(0).as_blob()?;
        let state_update = zstd::decode_all(state_update).context("Decompressing state update")?;
        let state_update =
            serde_json::from_slice(&state_update).context("Deserializing state update")?;

        Ok(Some(state_update))
    }

    /// Deletes all rows from __head down-to reorg_tail__
    /// i.e. it deletes all rows where `block number >= reorg_tail`.
    pub fn reorg(tx: &Transaction<'_>, reorg_tail: StarknetBlockNumber) -> anyhow::Result<()> {
        tx.execute(
            r"DELETE FROM starknet_state_updates
            WHERE ROWID IN (
                SELECT updates.ROWID FROM starknet_state_updates updates
                INNER JOIN starknet_blocks blocks
                    ON (updates.block_hash = blocks.hash)
                WHERE blocks.number >= ?
            );",
            params![reorg_tail.0],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::Storage;

    mod contracts {
        use super::*;

        #[test]
        fn get_root() {
            let storage = Storage::in_memory().unwrap();
            let mut connection = storage.connection().unwrap();
            let transaction = connection.transaction().unwrap();

            let state_hash = ContractStateHash(StarkHash::from_hex_str("abc").unwrap());
            let hash = ClassHash(StarkHash::from_hex_str("123").unwrap());
            let root = ContractRoot(StarkHash::from_hex_str("def").unwrap());

            ContractsStateTable::upsert(&transaction, state_hash, hash, root).unwrap();

            let result = ContractsStateTable::get_root(&transaction, state_hash).unwrap();

            assert_eq!(result, Some(root));
        }
    }

    mod refs {
        use super::*;

        mod l1_l2_head {
            use super::*;

            #[test]
            fn fresh_is_none() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let l1_l2_head = RefsTable::get_l1_l2_head(&tx).unwrap();
                assert_eq!(l1_l2_head, None);
            }

            #[test]
            fn set_get() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let expected = Some(StarknetBlockNumber(22));
                RefsTable::set_l1_l2_head(&tx, expected).unwrap();
                assert_eq!(expected, RefsTable::get_l1_l2_head(&tx).unwrap());

                let expected = Some(StarknetBlockNumber(25));
                RefsTable::set_l1_l2_head(&tx, expected).unwrap();
                assert_eq!(expected, RefsTable::get_l1_l2_head(&tx).unwrap());

                RefsTable::set_l1_l2_head(&tx, None).unwrap();
                assert_eq!(None, RefsTable::get_l1_l2_head(&tx).unwrap());
            }
        }
    }

    mod l1_state_table {
        use super::*;

        /// Creates a set of consecutive [StateUpdateLog]s starting from L2 genesis,
        /// with arbitrary other values.
        fn create_updates() -> [StateUpdateLog; 3] {
            (0..3)
                .map(|i| StateUpdateLog {
                    origin: EthOrigin {
                        block: BlockOrigin {
                            hash: EthereumBlockHash(H256::from_low_u64_le(i + 33)),
                            number: EthereumBlockNumber(i + 12_000),
                        },
                        transaction: TransactionOrigin {
                            hash: EthereumTransactionHash(H256::from_low_u64_le(i + 999)),
                            index: EthereumTransactionIndex(i + 20_000),
                        },
                        log_index: EthereumLogIndex(i + 500),
                    },
                    global_root: GlobalRoot(
                        StarkHash::from_hex_str(&"3".repeat(i as usize + 1)).unwrap(),
                    ),
                    block_number: StarknetBlockNumber::GENESIS + i,
                })
                .collect::<Vec<_>>()
                .try_into()
                .unwrap()
        }

        mod get {
            use super::*;

            #[test]
            fn none() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let updates = create_updates();
                for update in &updates {
                    L1StateTable::upsert(&tx, update).unwrap();
                }

                let non_existent = updates.last().unwrap().block_number + 1;
                assert_eq!(L1StateTable::get(&tx, non_existent.into()).unwrap(), None);
            }

            #[test]
            fn some() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let updates = create_updates();
                for update in &updates {
                    L1StateTable::upsert(&tx, update).unwrap();
                }

                for (idx, update) in updates.iter().enumerate() {
                    assert_eq!(
                        L1StateTable::get(&tx, update.block_number.into())
                            .unwrap()
                            .as_ref(),
                        Some(update),
                        "Update {}",
                        idx
                    );
                }
            }

            mod latest {
                use super::*;

                #[test]
                fn none() {
                    let storage = Storage::in_memory().unwrap();
                    let mut connection = storage.connection().unwrap();
                    let tx = connection.transaction().unwrap();

                    assert_eq!(
                        L1StateTable::get(&tx, L1TableBlockId::Latest).unwrap(),
                        None
                    );
                }

                #[test]
                fn some() {
                    let storage = Storage::in_memory().unwrap();
                    let mut connection = storage.connection().unwrap();
                    let tx = connection.transaction().unwrap();

                    let updates = create_updates();
                    for update in &updates {
                        L1StateTable::upsert(&tx, update).unwrap();
                    }

                    assert_eq!(
                        L1StateTable::get(&tx, L1TableBlockId::Latest)
                            .unwrap()
                            .as_ref(),
                        updates.last()
                    );
                }
            }
        }

        mod get_root {
            use super::*;

            #[test]
            fn none() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let updates = create_updates();
                for update in &updates {
                    L1StateTable::upsert(&tx, update).unwrap();
                }

                let non_existent = updates.last().unwrap().block_number + 1;
                assert_eq!(
                    L1StateTable::get_root(&tx, non_existent.into()).unwrap(),
                    None
                );
            }

            #[test]
            fn some() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let updates = create_updates();
                for update in &updates {
                    L1StateTable::upsert(&tx, update).unwrap();
                }

                for (idx, update) in updates.iter().enumerate() {
                    assert_eq!(
                        L1StateTable::get_root(&tx, update.block_number.into()).unwrap(),
                        Some(update.global_root),
                        "Update {}",
                        idx
                    );
                }
            }

            mod latest {
                use super::*;

                #[test]
                fn none() {
                    let storage = Storage::in_memory().unwrap();
                    let mut connection = storage.connection().unwrap();
                    let tx = connection.transaction().unwrap();

                    assert_eq!(
                        L1StateTable::get_root(&tx, L1TableBlockId::Latest).unwrap(),
                        None
                    );
                }

                #[test]
                fn some() {
                    let storage = Storage::in_memory().unwrap();
                    let mut connection = storage.connection().unwrap();
                    let tx = connection.transaction().unwrap();

                    let updates = create_updates();
                    for update in &updates {
                        L1StateTable::upsert(&tx, update).unwrap();
                    }

                    assert_eq!(
                        L1StateTable::get_root(&tx, L1TableBlockId::Latest).unwrap(),
                        Some(updates.last().unwrap().global_root)
                    );
                }
            }
        }

        mod reorg {
            use super::*;

            #[test]
            fn full() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let updates = create_updates();
                for update in &updates {
                    L1StateTable::upsert(&tx, update).unwrap();
                }

                L1StateTable::reorg(&tx, StarknetBlockNumber::GENESIS).unwrap();

                assert_eq!(
                    L1StateTable::get(&tx, L1TableBlockId::Latest).unwrap(),
                    None
                );
            }

            #[test]
            fn partial() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let updates = create_updates();
                for update in &updates {
                    L1StateTable::upsert(&tx, update).unwrap();
                }

                let reorg_tail = updates[1].block_number;
                L1StateTable::reorg(&tx, reorg_tail).unwrap();

                assert_eq!(
                    L1StateTable::get(&tx, L1TableBlockId::Latest)
                        .unwrap()
                        .as_ref(),
                    Some(&updates[0])
                );
            }
        }
    }

    mod starknet_blocks {
        use super::*;
        use crate::storage::test_utils;

        fn create_blocks() -> [StarknetBlock; test_utils::NUM_BLOCKS] {
            test_utils::create_blocks()
        }

        fn with_default_blocks<F>(f: F)
        where
            F: FnOnce(&Transaction<'_>, [StarknetBlock; test_utils::NUM_BLOCKS]),
        {
            let storage = Storage::in_memory().unwrap();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let blocks = create_blocks();
            for block in &blocks {
                StarknetBlocksTable::insert(&tx, block, None).unwrap();
            }

            f(&tx, blocks)
        }

        mod get {
            use super::*;

            mod by_number {
                use super::*;

                #[test]
                fn some() {
                    with_default_blocks(|tx, blocks| {
                        for block in blocks {
                            let result = StarknetBlocksTable::get(tx, block.number.into())
                                .unwrap()
                                .unwrap();

                            assert_eq!(result, block);
                        }
                    })
                }

                #[test]
                fn none() {
                    with_default_blocks(|tx, blocks| {
                        let non_existent = blocks.last().unwrap().number + 1;
                        assert_eq!(
                            StarknetBlocksTable::get(tx, non_existent.into()).unwrap(),
                            None
                        );
                    });
                }
            }

            mod by_hash {
                use super::*;

                #[test]
                fn some() {
                    with_default_blocks(|tx, blocks| {
                        for block in blocks {
                            let result = StarknetBlocksTable::get(tx, block.hash.into())
                                .unwrap()
                                .unwrap();

                            assert_eq!(result, block);
                        }
                    });
                }

                #[test]
                fn none() {
                    with_default_blocks(|tx, _blocks| {
                        let non_existent =
                            StarknetBlockHash(StarkHash::from_hex_str(&"b".repeat(10)).unwrap());
                        assert_eq!(
                            StarknetBlocksTable::get(tx, non_existent.into()).unwrap(),
                            None
                        );
                    });
                }
            }

            mod latest {
                use super::*;

                #[test]
                fn some() {
                    with_default_blocks(|tx, blocks| {
                        let expected = blocks.last().unwrap();

                        let latest = StarknetBlocksTable::get(tx, StarknetBlocksBlockId::Latest)
                            .unwrap()
                            .unwrap();
                        assert_eq!(&latest, expected);
                    })
                }

                #[test]
                fn none() {
                    let storage = Storage::in_memory().unwrap();
                    let mut connection = storage.connection().unwrap();
                    let tx = connection.transaction().unwrap();

                    let latest =
                        StarknetBlocksTable::get(&tx, StarknetBlocksBlockId::Latest).unwrap();
                    assert_eq!(latest, None);
                }
            }
        }

        mod get_root {
            use super::*;

            mod by_number {
                use super::*;

                #[test]
                fn some() {
                    with_default_blocks(|tx, blocks| {
                        for block in blocks {
                            let root = StarknetBlocksTable::get_root(tx, block.number.into())
                                .unwrap()
                                .unwrap();

                            assert_eq!(root, block.root);
                        }
                    })
                }

                #[test]
                fn none() {
                    with_default_blocks(|tx, blocks| {
                        let non_existent = blocks.last().unwrap().number + 1;
                        assert_eq!(
                            StarknetBlocksTable::get_root(tx, non_existent.into()).unwrap(),
                            None
                        );
                    })
                }
            }

            mod by_hash {
                use super::*;

                #[test]
                fn some() {
                    with_default_blocks(|tx, blocks| {
                        for block in blocks {
                            let root = StarknetBlocksTable::get_root(tx, block.hash.into())
                                .unwrap()
                                .unwrap();

                            assert_eq!(root, block.root);
                        }
                    })
                }

                #[test]
                fn none() {
                    with_default_blocks(|tx, _blocks| {
                        let non_existent =
                            StarknetBlockHash(StarkHash::from_hex_str(&"b".repeat(10)).unwrap());
                        assert_eq!(
                            StarknetBlocksTable::get_root(tx, non_existent.into()).unwrap(),
                            None
                        );
                    })
                }
            }

            mod latest {
                use super::*;

                #[test]
                fn some() {
                    with_default_blocks(|tx, blocks| {
                        let expected = blocks.last().map(|block| block.root).unwrap();

                        let latest =
                            StarknetBlocksTable::get_root(tx, StarknetBlocksBlockId::Latest)
                                .unwrap()
                                .unwrap();
                        assert_eq!(latest, expected);
                    })
                }

                #[test]
                fn none() {
                    let storage = Storage::in_memory().unwrap();
                    let mut connection = storage.connection().unwrap();
                    let tx = connection.transaction().unwrap();

                    let latest_root =
                        StarknetBlocksTable::get_root(&tx, StarknetBlocksBlockId::Latest).unwrap();
                    assert_eq!(latest_root, None);
                }
            }
        }

        mod reorg {
            use super::*;

            #[test]
            fn full() {
                with_default_blocks(|tx, _blocks| {
                    // reorg to genesis expected to wipe the blocks
                    StarknetBlocksTable::reorg(tx, StarknetBlockNumber::GENESIS).unwrap();

                    assert_eq!(
                        StarknetBlocksTable::get(tx, StarknetBlocksBlockId::Latest).unwrap(),
                        None
                    );
                })
            }

            #[test]
            fn partial() {
                with_default_blocks(|tx, blocks| {
                    let reorg_tail = blocks[1].number;
                    StarknetBlocksTable::reorg(tx, reorg_tail).unwrap();

                    let expected = StarknetBlock {
                        number: blocks[0].number,
                        hash: blocks[0].hash,
                        root: blocks[0].root,
                        timestamp: blocks[0].timestamp,
                        gas_price: blocks[0].gas_price,
                        sequencer_address: blocks[0].sequencer_address,
                    };

                    assert_eq!(
                        StarknetBlocksTable::get(tx, StarknetBlocksBlockId::Latest).unwrap(),
                        Some(expected)
                    );
                })
            }
        }

        mod interned_version {
            use super::super::Storage;
            use super::StarknetBlocksTable;

            #[test]
            fn duplicate_versions_interned() {
                let storage = Storage::in_memory().unwrap();
                let mut connection = storage.connection().unwrap();
                let tx = connection.transaction().unwrap();

                let blocks = super::create_blocks();
                let versions = ["0.9.1", "0.9.1"]
                    .into_iter()
                    .chain(std::iter::repeat("0.9.2"));

                let mut inserted = 0;

                for (block, version) in blocks.iter().zip(versions) {
                    StarknetBlocksTable::insert(&tx, block, Some(version)).unwrap();
                    inserted += 1;
                }

                let rows = tx.prepare("select version_id, count(1) from starknet_blocks group by version_id order by version_id")
                    .unwrap()
                    .query([])
                    .unwrap()
                    .mapped(|r| Ok((r.get::<_, Option<i64>>(0)?, r.get::<_, i64>(1)?)))
                    .collect::<Result<Vec<(Option<i64>, i64)>, _>>()
                    .unwrap();

                // there should be two of 0.9.1
                assert_eq!(rows.first(), Some(&(Some(1), 2)));

                // there should be a few for 0.9.2 (initially the create_rows returned 3 => 1)
                assert_eq!(rows.last(), Some(&(Some(2), inserted - 2)));

                // we should not have any nulls
                assert_eq!(rows.len(), 2, "nulls were not expected in {rows:?}");
            }
        }
    }

    mod starknet_events {
        use web3::types::H128;

        use super::*;

        use crate::core::{EntryPoint, EventData, Fee};
        use crate::sequencer::reply::transaction;
        use crate::storage::test_utils;

        #[test]
        fn event_data_serialization() {
            let data = vec![
                EventData(StarkHash::from_hex_str("0x1").unwrap()),
                EventData(StarkHash::from_hex_str("0x2").unwrap()),
                EventData(StarkHash::from_hex_str("0x3").unwrap()),
            ];
            assert_eq!(
                &StarknetEventsTable::event_data_to_bytes(&data),
                &[
                    0u8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
                    0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3
                ]
            );
        }

        #[test]
        fn event_keys_to_base64_strings() {
            let event = transaction::Event {
                from_address: ContractAddress::from_hex_str(
                    "0x06fbd460228d843b7fbef670ff15607bf72e19fa94de21e29811ada167b4ca39",
                )
                .unwrap(),
                data: vec![],
                keys: vec![
                    EventKey(StarkHash::from_hex_str("0x901823").unwrap()),
                    EventKey(StarkHash::from_hex_str("0x901824").unwrap()),
                    EventKey(StarkHash::from_hex_str("0x901825").unwrap()),
                ],
            };
            assert_eq!(
                StarknetEventsTable::event_keys_to_base64_strings(&event.keys),
                "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACQGCM= AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACQGCQ= AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAACQGCU="
            );
        }

        #[test]
        fn get_events_with_fully_specified_filter() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let expected_event = &emitted_events[1];
            let filter = StarknetEventFilter {
                from_block: Some(expected_event.block_number),
                to_block: Some(expected_event.block_number),
                contract_address: Some(expected_event.from_address),
                // we're using a key which is present in _all_ events
                keys: vec![EventKey(StarkHash::from_hex_str("deadbeef").unwrap())],
                page_size: test_utils::NUM_EVENTS,
                page_number: 0,
            };

            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: vec![expected_event.clone()],
                    is_last_page: true
                }
            );
        }

        #[test]
        fn events_are_ordered() {
            // This is a regression test where events were incorrectly ordered by transaction hash
            // instead of transaction index.
            //
            // Events should be ordered by block number, transaction index, event index.
            use crate::core::StarknetTransactionHash;
            use crate::sequencer::reply::transaction::Event;

            // All events we are storing, arbitrarily use from_address to distinguish them.
            let expected_events = (0u8..5)
                .map(|idx| Event {
                    data: Vec::new(),
                    keys: Vec::new(),
                    from_address: ContractAddress(
                        StarkHash::from_be_slice(&idx.to_be_bytes()).unwrap(),
                    ),
                })
                .collect::<Vec<_>>();

            let block = StarknetBlock {
                number: StarknetBlockNumber(0),
                hash: StarknetBlockHash::from_hex_str("0x1234").unwrap(),
                root: GlobalRoot(StarkHash::from_hex_str("0x1234").unwrap()),
                timestamp: StarknetBlockTimestamp(0),
                gas_price: GasPrice(0),
                sequencer_address: SequencerAddress(StarkHash::from_hex_str("0x1234").unwrap()),
            };

            // Note: hashes are reverse ordered to trigger the sorting bug.
            let transactions = vec![
                transaction::Transaction::Invoke(transaction::InvokeTransaction {
                    calldata: vec![],
                    // Only required because event insert rejects if this is None
                    contract_address: ContractAddress(StarkHash::ZERO),
                    entry_point_type: transaction::EntryPointType::External,
                    entry_point_selector: EntryPoint(StarkHash::ZERO),
                    max_fee: Fee(H128::zero()),
                    signature: vec![],
                    transaction_hash: StarknetTransactionHash(
                        StarkHash::from_hex_str("0xF").unwrap(),
                    ),
                }),
                transaction::Transaction::Invoke(transaction::InvokeTransaction {
                    calldata: vec![],
                    // Only required because event insert rejects if this is None
                    contract_address: ContractAddress(StarkHash::ZERO),
                    entry_point_type: transaction::EntryPointType::External,
                    entry_point_selector: EntryPoint(StarkHash::ZERO),
                    max_fee: Fee(H128::zero()),
                    signature: vec![],
                    transaction_hash: StarknetTransactionHash(
                        StarkHash::from_hex_str("0x1").unwrap(),
                    ),
                }),
            ];

            let receipts = vec![
                transaction::Receipt {
                    actual_fee: None,
                    events: expected_events[..3].to_vec(),
                    execution_resources: transaction::ExecutionResources {
                        builtin_instance_counter:
                            transaction::execution_resources::BuiltinInstanceCounter::Empty(
                                transaction::execution_resources::EmptyBuiltinInstanceCounter {},
                            ),
                        n_steps: 0,
                        n_memory_holes: 0,
                    },
                    l1_to_l2_consumed_message: None,
                    l2_to_l1_messages: Vec::new(),
                    transaction_hash: transactions[0].hash(),
                    transaction_index: crate::core::StarknetTransactionIndex(0),
                },
                transaction::Receipt {
                    actual_fee: None,
                    events: expected_events[3..].to_vec(),
                    execution_resources: transaction::ExecutionResources {
                        builtin_instance_counter:
                            transaction::execution_resources::BuiltinInstanceCounter::Empty(
                                transaction::execution_resources::EmptyBuiltinInstanceCounter {},
                            ),
                        n_steps: 0,
                        n_memory_holes: 0,
                    },
                    l1_to_l2_consumed_message: None,
                    l2_to_l1_messages: Vec::new(),
                    transaction_hash: transactions[1].hash(),
                    transaction_index: crate::core::StarknetTransactionIndex(1),
                },
            ];

            let storage = Storage::in_memory().unwrap();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            StarknetBlocksTable::insert(&tx, &block, None).unwrap();
            StarknetTransactionsTable::upsert(
                &tx,
                block.hash,
                block.number,
                &vec![
                    (transactions[0].clone(), receipts[0].clone()),
                    (transactions[1].clone(), receipts[1].clone()),
                ],
            )
            .unwrap();

            let addresses = StarknetEventsTable::get_events(
                &tx,
                &StarknetEventFilter {
                    from_block: None,
                    to_block: None,
                    contract_address: None,
                    keys: vec![],
                    page_size: 1024,
                    page_number: 0,
                },
            )
            .unwrap()
            .events
            .iter()
            .map(|e| e.from_address)
            .collect::<Vec<_>>();

            let expected = expected_events
                .iter()
                .map(|e| e.from_address)
                .collect::<Vec<_>>();

            assert_eq!(addresses, expected);
        }

        #[test]
        fn get_events_by_block() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            const BLOCK_NUMBER: usize = 2;
            let filter = StarknetEventFilter {
                from_block: Some(StarknetBlockNumber(BLOCK_NUMBER as u64)),
                to_block: Some(StarknetBlockNumber(BLOCK_NUMBER as u64)),
                contract_address: None,
                keys: vec![],
                page_size: test_utils::NUM_EVENTS,
                page_number: 0,
            };

            let expected_events = &emitted_events[test_utils::EVENTS_PER_BLOCK * BLOCK_NUMBER
                ..test_utils::EVENTS_PER_BLOCK * (BLOCK_NUMBER + 1)];
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: expected_events.to_vec(),
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_up_to_block() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            const UNTIL_BLOCK_NUMBER: usize = 2;
            let filter = StarknetEventFilter {
                from_block: None,
                to_block: Some(StarknetBlockNumber(UNTIL_BLOCK_NUMBER as u64)),
                contract_address: None,
                keys: vec![],
                page_size: test_utils::NUM_EVENTS,
                page_number: 0,
            };

            let expected_events =
                &emitted_events[..test_utils::EVENTS_PER_BLOCK * (UNTIL_BLOCK_NUMBER + 1)];
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: expected_events.to_vec(),
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_from_block_onwards() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            const FROM_BLOCK_NUMBER: usize = 2;
            let filter = StarknetEventFilter {
                from_block: Some(StarknetBlockNumber(FROM_BLOCK_NUMBER as u64)),
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: test_utils::NUM_EVENTS,
                page_number: 0,
            };

            let expected_events =
                &emitted_events[test_utils::EVENTS_PER_BLOCK * FROM_BLOCK_NUMBER..];
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: expected_events.to_vec(),
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_from_contract() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let expected_event = &emitted_events[33];

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: Some(expected_event.from_address),
                keys: vec![],
                page_size: test_utils::NUM_EVENTS,
                page_number: 0,
            };

            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: vec![expected_event.clone()],
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_by_key() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let expected_event = &emitted_events[27];
            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![expected_event.keys[0]],
                page_size: test_utils::NUM_EVENTS,
                page_number: 0,
            };

            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: vec![expected_event.clone()],
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_with_no_filter() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: test_utils::NUM_EVENTS,
                page_number: 0,
            };

            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: emitted_events,
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_with_no_filter_and_paging() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: 10,
                page_number: 0,
            };
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: emitted_events[..10].to_vec(),
                    is_last_page: false
                }
            );

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: 10,
                page_number: 1,
            };
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: emitted_events[10..20].to_vec(),
                    is_last_page: false
                }
            );

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: 10,
                page_number: 3,
            };
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: emitted_events[30..40].to_vec(),
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_with_no_filter_and_nonexistent_page() {
            let (storage, _) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            const PAGE_SIZE: usize = 10;
            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: PAGE_SIZE,
                // one page _after_ the last one
                page_number: test_utils::NUM_BLOCKS * test_utils::EVENTS_PER_BLOCK / PAGE_SIZE,
            };
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: vec![],
                    is_last_page: true
                }
            );
        }

        #[test]
        fn get_events_with_invalid_page_size() {
            let (storage, _) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: 0,
                page_number: 0,
            };
            let result = StarknetEventsTable::get_events(&tx, &filter);
            assert!(result.is_err());
            assert_eq!(result.unwrap_err().to_string(), "Invalid page size");

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: vec![],
                page_size: StarknetEventsTable::PAGE_SIZE_LIMIT + 1,
                page_number: 0,
            };
            let result = StarknetEventsTable::get_events(&tx, &filter);
            assert!(result.is_err());
            assert_eq!(
                result.unwrap_err().downcast::<EventFilterError>().unwrap(),
                EventFilterError::PageSizeTooBig(StarknetEventsTable::PAGE_SIZE_LIMIT)
            );
        }

        #[test]
        fn get_events_by_key_with_paging() {
            let (storage, emitted_events) = test_utils::setup_test_storage();
            let mut connection = storage.connection().unwrap();
            let tx = connection.transaction().unwrap();

            let expected_events = &emitted_events[27..32];
            let keys_for_expected_events: Vec<_> =
                expected_events.iter().map(|e| e.keys[0]).collect();

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: keys_for_expected_events.clone(),
                page_size: 2,
                page_number: 0,
            };
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: expected_events[..2].to_vec(),
                    is_last_page: false
                }
            );

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: keys_for_expected_events.clone(),
                page_size: 2,
                page_number: 1,
            };
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: expected_events[2..4].to_vec(),
                    is_last_page: false
                }
            );

            let filter = StarknetEventFilter {
                from_block: None,
                to_block: None,
                contract_address: None,
                keys: keys_for_expected_events,
                page_size: 2,
                page_number: 2,
            };
            let events = StarknetEventsTable::get_events(&tx, &filter).unwrap();
            assert_eq!(
                events,
                PageOfEvents {
                    events: expected_events[4..].to_vec(),
                    is_last_page: true
                }
            );
        }
    }
}
