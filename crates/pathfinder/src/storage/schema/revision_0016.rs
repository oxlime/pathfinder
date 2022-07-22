use anyhow::Context;
use rusqlite::Transaction;

use crate::core::StarknetBlockNumber;
use crate::storage::StarknetBlocksTable;

/// Adds `starknet_state_updates` table.
pub(crate) fn migrate(transaction: &Transaction<'_>) -> anyhow::Result<()> {
    transaction
        .execute_batch(
            r"
            CREATE TABLE starknet_state_updates (
                block_hash BLOB PRIMARY KEY NOT NULL,
                data BLOB NOT NULL
            );",
        )
        .context("Creating starknet_state_updates table")?;

    let chain = match StarknetBlocksTable::determine_chain(&transaction)? {
        Some(x) => x,
        None => return Ok(()),
    };

    let latest_block_number = match StarknetBlocksTable::get_latest_number(&transaction)? {
        Some(x) => x.0,
        None => unreachable!(
            "If there is a genesis block in the DB then there must be a latest block too."
        ),
    };

    tracing::info!("Downloading state updates for all blocks, this can take a while...");

    let handle = tokio::runtime::Handle::current();

    let (downloaded_tx, downloaded_rx) = std::sync::mpsc::sync_channel(1);
    let (compressed_tx, compressed_rx) = std::sync::mpsc::sync_channel(1);

    let downloader = std::thread::spawn(move || {
        use crate::sequencer::ClientApi;

        let client = crate::sequencer::Client::new(chain).unwrap();

        for block_number in (0..=latest_block_number).rev() {
            let state_update =
                handle
                    .block_on(client.state_update(crate::core::BlockId::Number(
                        StarknetBlockNumber(block_number),
                    )))
                    .unwrap();

            downloaded_tx.send(state_update).unwrap();
        }
    });

    let compressor = std::thread::spawn(move || {
        let mut compressor = zstd::bulk::Compressor::new(10).unwrap();

        for sequencer_state_upate in downloaded_rx.iter() {
            use crate::rpc::types::reply::StateUpdate;

            // Unwrap is safe because all non-pending state updates contain a block hash
            let block_hash = sequencer_state_upate.block_hash.unwrap();
            let rpc_state_update: StateUpdate = sequencer_state_upate.into();
            let rpc_state_update = serde_json::to_vec(&rpc_state_update).unwrap();
            let rpc_state_update = compressor.compress(&rpc_state_update).unwrap();

            compressed_tx.send((block_hash, rpc_state_update)).unwrap();
        }
    });

    let mut checkpoint = std::time::Instant::now();
    let mut block_cnt = 0;

    for (block_hash, compressed_state_update) in compressed_rx.iter() {
        transaction
            .execute(
                r"INSERT INTO starknet_state_updates (block_hash, data) VALUES(?1, ?2);",
                rusqlite::params![block_hash.0.as_be_bytes(), &compressed_state_update],
            )
            .with_context(|| format!("Inserting state update for block {block_hash}"))?;

        block_cnt += 1;

        if checkpoint.elapsed() >= std::time::Duration::from_secs(10) {
            tracing::info!("Downloaded {block_cnt}/{}", latest_block_number + 1);
            checkpoint = std::time::Instant::now();
        }
    }

    downloader.join().unwrap();
    compressor.join().unwrap();

    Ok(())
}
