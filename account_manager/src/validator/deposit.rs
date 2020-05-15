use clap::{App, Arg, ArgMatches};
use clap_utils;
use deposit_contract::DEPOSIT_GAS;
use environment::Environment;
use futures::{compat::Future01CompatExt, future::Future};
use slog::{info, Logger};
use std::path::PathBuf;
use tokio::time::{delay_until, Duration, Instant};
use types::EthSpec;
use validator_client::validator_directory::ValidatorDirectoryBuilder;
use validator_dir::{Manager as ValidatorManager, ValidatorDir};
use web3::{
    transports::Ipc,
    types::{Address, SyncInfo, SyncState, TransactionRequest, U256},
    Transport, Web3,
};

pub const CMD: &str = "deposit";
const GWEI: u64 = 1_000_000_000;

const SYNCING_STATE_RETRY_DELAY: Duration = Duration::from_secs(2);

pub fn cli_app<'a, 'b>() -> App<'a, 'b> {
    App::new("deposit")
        .about(
            "Submits a deposit to an Eth1 validator registration contract via IPC endpoint \
            of an Eth1 client (e.g., Geth, OpenEthereum, etc.). The validators must already \
            have been created and exist on the filesystem. The process will exit immediately \
            with an error if any error occurs. After each deposit is submitted to the Eth1 \
            node a file will be saved in the validator directory with the transaction hash. \
            The application does not wait for confirmations so there is not guarantee that \
            the transaction is included in the Eth1 chain, use a block explorer and your \
            transaction hash to check for confirmations. The deposit contract address will \
            be determined by the --testnet-dir flag on the primary Lighthouse binary.",
        )
        .arg(
            Arg::with_name("data-dir")
                .long("data-dir")
                .value_name("DATA_DIRECTORY")
                .help(
                    "The path the validator client data directory. \
                    Defaults to ~/.lighthouse/validators",
                )
                .takes_value(true),
        )
        .arg(
            Arg::with_name("validator")
                .long("validator")
                .value_name("VALIDATOR_NAME")
                .help(
                    "The name of the directory in --data-dir for which to deposit. \
                    Set to 'all' to deposit all validators in the --data-dir.",
                )
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("eth1-ipc")
                .long("eth1-ipc")
                .value_name("ETH1_IPC_PATH")
                .help("Path to an Eth1 JSON-RPC IPC endpoint")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("from-address")
                .long("from-address")
                .value_name("FROM_ETH1_ADDRESS")
                .help(
                    "The address that will submit the eth1 deposit. \
                    Must be unlocked on the node at --eth1-ipc.",
                )
                .takes_value(true)
                .required(true),
        )
}

pub fn cli_run<T: EthSpec>(
    matches: &ArgMatches<'_>,
    mut env: Environment<T>,
) -> Result<(), String> {
    let spec = env.core_context().eth2_config.spec;
    let log = env.core_context().log;

    let data_dir = clap_utils::parse_path_with_default_in_home_dir(
        matches,
        "data_dir",
        PathBuf::new().join(".lighthouse").join("validators"),
    )?;
    let validator: String = clap_utils::parse_required(matches, "validator")?;
    let eth1_ipc_path: PathBuf = clap_utils::parse_required(matches, "eth1-ipc")?;
    let from_address: Address = clap_utils::parse_required(matches, "from-address")?;

    let manager = ValidatorManager::open(&data_dir)
        .map_err(|e| format!("Unable to read --datadir: {:?}", e))?;

    let validators = match validator.as_ref() {
        "all" => manager
            .open_all_validators()
            .map_err(|e| format!("Unable to read all validators: {:?}", e)),
        name => {
            let path = manager
                .directory_names()
                .map_err(|e| format!("Unable to read --datadir directory names: {:?}", e))?
                .get(name)
                .ok_or_else(|| format!("Unknown validator:  {}", name))?
                .clone();

            manager
                .open_validator(&path)
                .map_err(|e| format!("Unable to open {}: {:?}", name, e))
                .map(|v| vec![v])
        }
    }?;

    let eth1_deposit_datas = validators
        .into_iter()
        .filter(|v| !v.eth1_deposit_tx_hash_exists())
        .map(|v| match v.eth1_deposit_data() {
            Ok(Some(data)) => Ok((v, data)),
            Ok(None) => Err(format!(
                "Validator is missing deposit data file: {:?}",
                v.dir()
            )),
            Err(e) => Err(format!(
                "Unable to read deposit data for {:?}: {:?}",
                v.dir(),
                e
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;

    let total_gwei: u64 = eth1_deposit_datas
        .iter()
        .map(|(_, d)| d.deposit_data.amount)
        .sum();

    info!(
        log,
        "Starting deposits";
        "deposit_count" => eth1_deposit_datas.len(),
        "total_eth" => total_gwei / GWEI,
    );

    let deposit_contract = env
        .testnet
        .as_ref()
        .ok_or_else(|| "Unable to run account manager without a testnet dir".to_string())?
        .deposit_contract_address()
        .map_err(|e| format!("Unable to parse deposit contract address: {}", e))?;

    if deposit_contract == Address::zero() {
        return Err("Refusing to deposit to the zero address. Check testnet configuration.".into());
    }

    let (_event_loop_handle, transport) =
        Ipc::new(eth1_ipc_path).map_err(|e| format!("Unable to connect to eth1 IPC: {:?}", e))?;
    let web3 = Web3::new(transport);

    let deposits_fut = async {
        poll_until_synced(web3.clone(), log.clone()).await?;

        for (valdiator_dir, eth1_deposit_data) in eth1_deposit_datas {
            let result = web3
                .eth()
                .send_transaction(TransactionRequest {
                    from: from_address,
                    to: Some(deposit_contract),
                    gas: Some(DEPOSIT_GAS.into()),
                    gas_price: None,
                    value: Some(from_gwei(eth1_deposit_data.deposit_data.amount)),
                    data: Some(eth1_deposit_data.rlp.into()),
                    nonce: None,
                    condition: None,
                })
                .compat()
                .await
                .map_err(|e| format!("Failed to send transaction: {:?}", e))?;
        }

        Ok(())
    };

    env.runtime().block_on(deposits_fut)?;

    Ok(())
}

/// Converts gwei to wei.
fn from_gwei(gwei: u64) -> U256 {
    U256::from(gwei) * U256::exp10(9)
}

/// Run a poll on the `eth_syncing` endpoint, blocking until the node is synced.
async fn poll_until_synced<T>(web3: Web3<T>, log: Logger) -> Result<(), String>
where
    T: Transport + Send + 'static,
    <T as Transport>::Out: Send,
{
    loop {
        let sync_state = web3
            .clone()
            .eth()
            .syncing()
            .compat()
            .await
            .map_err(|e| format!("Unable to read syncing state from eth1 node: {:?}", e))?;

        match sync_state {
            SyncState::Syncing(SyncInfo {
                current_block,
                highest_block,
                ..
            }) => {
                info!(
                    log,
                    "Waiting for eth1 node to sync";
                    "est_highest_block" => format!("{}", highest_block),
                    "current_block" => format!("{}", current_block),
                );

                delay_until(Instant::now() + SYNCING_STATE_RETRY_DELAY).await;
            }
            SyncState::NotSyncing => {
                let block_number = web3
                    .clone()
                    .eth()
                    .block_number()
                    .compat()
                    .await
                    .map_err(|e| format!("Unable to read block number from eth1 node: {:?}", e))?;

                if block_number > 0.into() {
                    info!(
                        log,
                        "Eth1 node is synced";
                        "head_block" => format!("{}", block_number),
                    );
                    break;
                } else {
                    delay_until(Instant::now() + SYNCING_STATE_RETRY_DELAY).await;
                    info!(
                        log,
                        "Waiting for eth1 node to sync";
                        "current_block" => 0,
                    );
                }
            }
        }
    }
    Ok(())
}
