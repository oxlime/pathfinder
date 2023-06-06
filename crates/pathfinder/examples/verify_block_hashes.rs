use anyhow::Context;
use pathfinder_common::{BlockHash, BlockNumber, Chain, ChainId, StarknetVersion};
use pathfinder_lib::state::block_hash::{verify_block_hash, VerifyResult};
use pathfinder_storage::{JournalMode, StarknetBlocksTable, Storage};
use stark_hash::Felt;
use starknet_gateway_types::reply::{Block, Status};

/// Verify block hashes in a pathfinder database.
///
/// Iterates over all blocks in the database and verifies if the computed block hash matches
/// values we store for the block.
///
/// Usage:
/// `cargo run --release -p pathfinder --example verify_block_hashes mainnet ./mainnet.sqlite`
fn main() -> anyhow::Result<()> {
    let chain_name = std::env::args().nth(1).unwrap();
    let (chain, chain_id) = match chain_name.as_str() {
        "mainnet" => (Chain::Mainnet, ChainId::MAINNET),
        "goerli" => (Chain::Testnet, ChainId::TESTNET),
        "testnet2" => (Chain::Testnet2, ChainId::TESTNET2),
        "integration" => (Chain::Integration, ChainId::INTEGRATION),
        _ => panic!("Expected chain name: mainnet/goerli/testnet2/integration"),
    };

    let database_path = std::env::args().nth(2).unwrap();
    let storage = Storage::migrate(database_path.into(), JournalMode::WAL)?;
    let mut db = storage
        .connection()
        .context("Opening database connection")?;

    let mut parent_block_hash = BlockHash(Felt::ZERO);

    let latest_block_number = {
        let tx = db.transaction().unwrap();
        StarknetBlocksTable::get_latest_number(&tx)?.unwrap()
    };

    for block_number in 0..latest_block_number.get() {
        let tx = db.transaction().unwrap();
        let block_id = pathfinder_storage::BlockId::Number(BlockNumber::new_or_panic(block_number));
        let block = StarknetBlocksTable::get(&tx, block_id)?.unwrap();
        let transactions_and_receipts = tx
            .transaction_data_for_block(block_id)?
            .context("Transaction data missing")?;
        drop(tx);

        let block_hash = block.hash;
        let (transactions, receipts): (Vec<_>, Vec<_>) =
            transactions_and_receipts.into_iter().unzip();

        let block = Block {
            block_hash: block.hash,
            block_number: block.number,
            gas_price: Some(block.gas_price),
            parent_block_hash,
            sequencer_address: Some(block.sequencer_address),
            state_commitment: block.state_commmitment,
            status: Status::AcceptedOnL1,
            timestamp: block.timestamp,
            transaction_receipts: receipts,
            transactions,
            starknet_version: StarknetVersion::default(),
        };
        parent_block_hash = block_hash;

        let result = verify_block_hash(&block, chain, chain_id, block_hash)?;
        match result {
            VerifyResult::Match(_) => {}
            VerifyResult::NotVerifiable => println!(
                "Block hash cannot be verified for block number {block_number} hash {block_hash:?}"
            ),
            VerifyResult::Mismatch => {
                println!("Block hash mismatch at block number {block_number} hash {block_hash:?}")
            }
        }
    }

    Ok(())
}
