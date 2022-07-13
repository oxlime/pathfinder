#![deny(rust_2018_idioms)]

use anyhow::Context;
use pathfinder_lib::{
    cairo, config, core,
    ethereum::transport::{EthereumTransport, HttpTransport},
    rpc, sequencer, state,
    storage::{JournalMode, Storage},
};
use std::sync::Arc;
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::var_os("RUST_LOG").is_none() {
        std::env::set_var("RUST_LOG", "info");
    }

    setup_tracing();

    let config =
        config::Configuration::parse_cmd_line_and_cfg_file().context("Parsing configuration")?;

    info!(
        // this is expected to be $(last_git_tag)-$(commits_since)-$(commit_hash)
        version = env!("VERGEN_GIT_SEMVER_LIGHTWEIGHT"),
        "🏁 Starting node."
    );

    permission_check(&config.data_directory)?;

    let eth_transport = match config
        .ethereum
        .map(|config| HttpTransport::from_config(config).context("Creating Ethereum Transport"))
    {
        Some(Ok(ts)) => {
            let chain = ts.chain().await.context(
                "Determine Ethereum chain.

Hint: Make sure the provided ethereum.url and ethereum.password are good.",
            )?;
            Some((ts, chain))
        }
        Some(Err(e)) => {
            return Err(e);
        }
        None if std::env::var_os("PATHFINDER_USE_INTEGRATION").is_some() => None,
        None => anyhow::bail!("TODO better message about forgotten parameter"),
    };

    let database_path = config.data_directory.join(match eth_transport {
        Some((_, core::Chain::Mainnet)) => "mainnet.sqlite",
        Some((_, core::Chain::Goerli)) => "goerli.sqlite",
        None => "integration.sqlite",
    });
    let journal_mode = match config.sqlite_wal {
        false => JournalMode::Rollback,
        true => JournalMode::WAL,
    };
    let storage = Storage::migrate(database_path.clone(), journal_mode).unwrap();
    info!(location=?database_path, "Database migrated.");

    if let Some((_, ethereum_chain)) = eth_transport {
        verify_database_chain(&storage, ethereum_chain).context("Verifying database")?;
    }

    let sequencer = match config.sequencer_url {
        Some(url) => {
            info!(?url, "Using custom Sequencer address");
            let client = sequencer::Client::with_url(url).unwrap();
            let sequencer_chain = client.chain().await.unwrap();
            if let Some((_, ethereum_chain)) = eth_transport.as_ref() {
                if sequencer_chain != *ethereum_chain {
                    tracing::error!(sequencer=%sequencer_chain, ethereum=%ethereum_chain, "Sequencer and Ethereum network mismatch");
                    anyhow::bail!("Sequencer and Ethereum network mismatch. Sequencer is on {sequencer_chain} but Ethereum is on {ethereum_chain}");
                }
            }
            client
        }
        None if std::env::var_os("PATHFINDER_USE_INTEGRATION").is_some() => {
            sequencer::Client::with_url(
                reqwest::Url::parse("https://external.integration.starknet.io").unwrap(),
            )
            .unwrap()
        }
        None => sequencer::Client::new(
            eth_transport
                .as_ref()
                .map(|(_, chain)| *chain)
                .expect("should be some ethereum_chain when PATHFINDER_USE_INTEGRATION is not set"),
        )
        .unwrap(),
    };
    let sync_state = Arc::new(state::SyncState::default());

    // how to make l1 and l2 syncs into closures who own required resouces
    // TODO: maybe add a feature guarded Chain::Integration

    let l1_sync = {
        let eth_transport = eth_transport.clone();

        move |tx, maybe_head| {
            let eth_transport = eth_transport.clone();
            async move {
                if let Some((ts, chain)) = eth_transport {
                    pathfinder_lib::state::l1::sync(tx, ts, chain, maybe_head).await
                } else {
                    futures::future::pending().await
                }
            }
        }
    };

    let l2_sync = {
        let sequencer = sequencer.clone();
        let ethereum_chain = eth_transport.as_ref().map(|(_, chain)| *chain);

        move |tx, maybe_head| {
            // FIXME: needs only chain for polling delay and block verification
            pathfinder_lib::state::l2::sync(tx, sequencer.clone(), maybe_head, ethereum_chain)
        }
    };

    let sync_handle = tokio::spawn(state::sync(
        storage.clone(),
        sync_state.clone(),
        // FIXME: needed to update the highest block
        sequencer.clone(),
        // FIXME: needed for polling delay
        eth_transport.as_ref().map(|(_, chain)| *chain),
        l1_sync,
        l2_sync,
    ));

    let (api, cairo_handle) = if let Some((eth_transport, ethereum_chain)) = eth_transport.clone() {
        // TODO: the error could be recovered, but currently it's required for startup. There should
        // not be other reason for the start to fail than python script not firing up.
        let (call_handle, cairo_handle) = cairo::ext_py::start(
            storage.path().into(),
            config.python_subprocesses,
            futures::future::pending(),
            ethereum_chain,
        )
        .await
        .context(
            "Creating python process for call handling. Have you setup our Python dependencies?",
        )?;

        let shared = rpc::api::Cached::new(Arc::new(eth_transport));

        (
            rpc::api::RpcApi::new(storage, sequencer, Some(ethereum_chain), sync_state)
                .with_call_handling(call_handle)
                .with_eth_gas_price(shared),
            futures::future::Either::Left(cairo_handle),
        )
    } else {
        (
            rpc::api::RpcApi::new(storage, sequencer, None, sync_state),
            futures::future::Either::Right(futures::future::pending()),
        )
    };

    let (rpc_handle, local_addr) = rpc::run_server(config.http_rpc_addr, api)
        .await
        .context("Starting the RPC server")?;
    info!("📡 HTTP-RPC server started on: {}", local_addr);

    let update_handle = tokio::spawn(pathfinder_lib::update::poll_github_for_releases());

    // Monitor our spawned process tasks.
    tokio::select! {
        result = sync_handle => {
            match result {
                Ok(task_result) => tracing::error!("Sync process ended unexpected with: {:?}", task_result),
                Err(err) => tracing::error!("Sync process ended unexpected; failed to join task handle: {:?}", err),
            }
        }
        result = cairo_handle => {
            match result {
                Ok(task_result) => tracing::error!("Cairo process ended unexpected with: {:?}", task_result),
                Err(err) => tracing::error!("Cairo process ended unexpected; failed to join task handle: {:?}", err),
            }
        }
        _result = rpc_handle => {
            // This handle returns () so its not very useful.
            tracing::error!("RPC server process ended unexpected");
        }
        result = update_handle => {
            match result {
                Ok(_) => tracing::error!("Release monitoring process ended unexpectedly"),
                Err(err) => tracing::error!(error=%err, "Release monitoring process ended unexpectedly"),
            }
        }
    }

    Ok(())
}

/// Verifies that the database matches the expected chain; throws an error if it does not.
fn verify_database_chain(storage: &Storage, expected: core::Chain) -> anyhow::Result<()> {
    use pathfinder_lib::consts::{GOERLI_GENESIS_HASH, MAINNET_GENESIS_HASH};
    use pathfinder_lib::core::StarknetBlockNumber;

    let mut connection = storage.connection().context("Create database connection")?;
    let transaction = connection
        .transaction()
        .context("Create database transaction")?;
    let genesis = pathfinder_lib::storage::StarknetBlocksTable::get(
        &transaction,
        StarknetBlockNumber(0).into(),
    )
    .context("Read genesis block from database")?;

    let db_chain = match genesis {
        None => return Ok(()),
        Some(genesis) if genesis.hash == *GOERLI_GENESIS_HASH => core::Chain::Goerli,
        Some(genesis) if genesis.hash == *MAINNET_GENESIS_HASH => core::Chain::Mainnet,
        Some(genesis) => {
            anyhow::bail!("Unknown genesis block hash {}", genesis.hash.0)
        }
    };

    anyhow::ensure!(
        db_chain == expected,
        "Database ({}) does not much the expected network ({})",
        db_chain,
        expected
    );

    Ok(())
}

#[cfg(feature = "tokio-console")]
fn setup_tracing() {
    use tracing_subscriber::prelude::*;

    // EnvFilter isn't really a Filter, so this we need this ugly workaround for filtering with it.
    // See https://github.com/tokio-rs/tracing/issues/1868 for more details.
    let env_filter = Arc::new(tracing_subscriber::EnvFilter::from_default_env());
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .compact()
        .with_filter(tracing_subscriber::filter::dynamic_filter_fn(
            move |m, c| env_filter.enabled(m, c.clone()),
        ));
    let console_layer = console_subscriber::spawn();
    tracing_subscriber::registry()
        .with(fmt_layer)
        .with(console_layer)
        .init();
}

#[cfg(not(feature = "tokio-console"))]
fn setup_tracing() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_target(false)
        .compact()
        .init();
}

fn permission_check(base: &std::path::Path) -> Result<(), anyhow::Error> {
    tempfile::tempfile_in(base)
        .with_context(|| format!("Failed to create a file in {}. Make sure the directory is writable by the user running pathfinder.", base.display()))?;

    // well, don't really know what else to check

    Ok(())
}
