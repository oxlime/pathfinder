use anyhow::Context;
use rusqlite::Transaction;

/// Adds `starknet_state_updates` table.
pub(crate) fn migrate(transaction: &Transaction<'_>) -> anyhow::Result<()> {
    transaction
        .execute_batch(
            r"
            CREATE TABLE starknet_state_updates (
                block_hash BLOB PRIMARY KEY NOT NULL,
                data BLOB NOT NULL,
                FOREIGN KEY(block_hash) REFERENCES starknet_blocks(hash)
                ON DELETE CASCADE
            );",
        )
        .context("Creating starknet_state_updates table")?;

    Ok(())
}
