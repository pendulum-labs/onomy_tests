use std::{env, time::Duration};

use clap::Parser;
use common::{
    cosmovisor::{cosmovisor, cosmovisor_start, onomyd_setup, wait_for_height},
    hermes::{create_channel_pair, create_client_pair, create_connection_pair, hermes},
    Args, TIMEOUT,
};
use lazy_static::lazy_static;
use log::info;
use serde_json::Value;
use stacked_errors::{MapAddError, Result};
use super_orchestrator::{
    docker::{Container, ContainerNetwork},
    get_separated_val,
    net_message::NetMessenger,
    remove_files_in_dir, sh, std_init, Command, FileOptions, STD_DELAY, STD_TRIES,
};
use tokio::time::sleep;

lazy_static! {
    static ref DAEMON_NAME: String = env::var("DAEMON_NAME").unwrap();
    static ref DAEMON_HOME: String = env::var("DAEMON_HOME").unwrap();
}

#[tokio::main]
async fn main() -> Result<()> {
    std_init()?;
    let args = Args::parse();

    if let Some(ref s) = args.entrypoint {
        match s.as_str() {
            "onomyd" => onomyd_runner().await,
            "marketd" => marketd_runner().await,
            "hermes" => hermes_runner().await,
            _ => format!("entrypoint \"{s}\" is not recognized").map_add_err(|| ()),
        }
    } else {
        container_runner().await
    }
}

async fn container_runner() -> Result<()> {
    let container_target = "x86_64-unknown-linux-gnu";
    let logs_dir = "./logs";
    let this_bin = "ics_basic";

    // build internal runner with `--release`
    sh("cargo build --release --bin", &[
        this_bin,
        "--target",
        container_target,
    ])
    .await?;

    // build binaries

    sh("make --directory ./../onomy/ build", &[]).await?;
    sh("make --directory ./../market/ build", &[]).await?;
    // copy to dockerfile resources (docker cannot use files from outside cwd)
    sh(
        "cp ./../onomy/onomyd ./dockerfiles/dockerfile_resources/onomyd",
        &[],
    )
    .await?;
    sh(
        "cp ./../market/marketd ./dockerfiles/dockerfile_resources/marketd",
        &[],
    )
    .await?;

    // prepare volumed resources
    remove_files_in_dir("./resources/keyring-test/", &["address", "info"]).await?;

    let entrypoint = &format!("./target/{container_target}/release/{this_bin}");
    let volumes = vec![("./logs", "/logs")];
    let mut onomyd_volumes = volumes.clone();
    onomyd_volumes.push(("./resources/keyring-test", "/root/.onomy/keyring-test"));
    let mut marketd_volumes = volumes.clone();
    marketd_volumes.push((
        "./resources/keyring-test",
        "/root/.onomy_market/keyring-test",
    ));
    let mut cn = ContainerNetwork::new(
        "test",
        vec![
            Container::new(
                "hermes",
                Some("./dockerfiles/hermes.dockerfile"),
                None,
                &[],
                &volumes,
                entrypoint,
                &["--entrypoint", "hermes"],
            ),
            Container::new(
                "onomyd",
                Some("./dockerfiles/onomyd.dockerfile"),
                None,
                &[],
                &onomyd_volumes,
                entrypoint,
                &["--entrypoint", "onomyd"],
            ),
            Container::new(
                "marketd",
                Some("./dockerfiles/marketd.dockerfile"),
                None,
                &[],
                &marketd_volumes,
                entrypoint,
                &["--entrypoint", "marketd"],
            ),
        ],
        true,
        logs_dir,
    )?;
    cn.run_all(true).await?;
    cn.wait_with_timeout_all(true, TIMEOUT).await?;
    Ok(())
}

async fn hermes_runner() -> Result<()> {
    let mut nm_onomyd = NetMessenger::listen_single_connect("0.0.0.0:26000", TIMEOUT).await?;

    let mnemonic: String = nm_onomyd.recv().await?;
    // set keys for our chains
    FileOptions::write_str("/root/.hermes/mnemonic.txt", &mnemonic).await?;
    hermes(
        "keys add --chain onomy --mnemonic-file /root/.hermes/mnemonic.txt",
        &[],
    )
    .await?;
    hermes(
        "keys add --chain market --mnemonic-file /root/.hermes/mnemonic.txt",
        &[],
    )
    .await?;

    nm_onomyd.recv::<()>().await?;

    // https://hermes.informal.systems/tutorials/local-chains/add-a-new-relay-path.html

    // Note: For ICS, there is a point where a handshake must be initiated by the
    // consumer chain, so we must make the consumer chain the "a-chain" and the
    // producer chain the "b-chain"

    let b_chain = "onomy";
    let a_chain = "market";
    // a client is already created because of the ICS setup
    //let _market_client_pair = create_client_pair(a_chain, b_chain).await?;
    // create one client and connection pair that will be used for IBC transfer and
    // ICS communication
    let market_connection_pair = create_connection_pair(a_chain, b_chain).await?;

    // market<->onomy transfer<->transfer
    let market_transfer_channel_pair = create_channel_pair(
        a_chain,
        &market_connection_pair.0,
        "transfer",
        "transfer",
        false,
    )
    .await?;

    // market<->onomy consumer<->provider
    let market_ics_channel_pair = create_channel_pair(
        a_chain,
        &market_connection_pair.0,
        "consumer",
        "provider",
        true,
    )
    .await?;

    let hermes_log = FileOptions::write2("/logs", "hermes_runner.log");
    let mut hermes_runner = Command::new("hermes start", &[])
        .stderr_log(&hermes_log)
        .stdout_log(&hermes_log)
        .run()
        .await?;

    info!("Onomy Network has been setup");

    sleep(Duration::from_secs(5)).await;

    hermes(
        "query packet acks --chain onomy --port transfer --channel",
        &[&market_transfer_channel_pair.0],
    )
    .await?;
    hermes(
        "query packet acks --chain market --port transfer --channel",
        &[&market_transfer_channel_pair.1],
    )
    .await?;
    hermes(
        "query packet acks --chain onomy --port provider --channel",
        &[&market_ics_channel_pair.0],
    )
    .await?;
    hermes(
        "query packet acks --chain market --port consumer --channel",
        &[&market_ics_channel_pair.1],
    )
    .await?;

    //hermes tx ft-transfer --timeout-seconds 10 --dst-chain market --src-chain
    // onomy --src-port transfer --src-channel channel-0 --amount 1337 --denom anom

    nm_onomyd.send::<()>(&()).await?;

    sleep(TIMEOUT).await;
    hermes_runner.terminate().await?;
    Ok(())
}

async fn onomyd_runner() -> Result<()> {
    let mut nm_hermes = NetMessenger::connect(STD_TRIES, STD_DELAY, "hermes:26000")
        .await
        .map_add_err(|| ())?;
    let mut nm_marketd = NetMessenger::connect(STD_TRIES, STD_DELAY, "marketd:26001")
        .await
        .map_add_err(|| ())?;

    let daemon_home = DAEMON_HOME.as_str();
    let mnemonic = onomyd_setup(daemon_home).await?;

    let mut cosmovisor_runner = cosmovisor_start("onomyd_runner.log", true, None).await?;

    let proposal_id = "1";

    // `json!` doesn't like large literals beyond i32
    let proposal_s = r#"{
        "title": "Propose the addition of a new chain",
        "description": "add consumer chain market",
        "chain_id": "market",
        "initial_height": {
            "revision_number": 0,
            "revision_height": 1
        },
        "genesis_hash": "Z2VuX2hhc2g=",
        "binary_hash": "YmluX2hhc2g=",
        "spawn_time": "2023-05-18T01:15:49.83019476-05:00",
        "consumer_redistribution_fraction": "0.75",
        "blocks_per_distribution_transmission": 1000,
        "historical_entries": 10000,
        "ccv_timeout_period": 2419200000000000,
        "transfer_timeout_period": 3600000000000,
        "unbonding_period": 1728000000000000,
        "deposit": "2000000000000000000000anom"
    }"#;
    // we will just place the file under the config folder
    let proposal_file_path = format!("{daemon_home}/config/consumer_add_proposal.json");
    FileOptions::write_str(&proposal_file_path, proposal_s)
        .await
        .map_add_err(|| ())?;

    let gas_args = [
        "--gas",
        "auto",
        "--gas-adjustment",
        "1.3",
        "-y",
        "-b",
        "block",
        "--from",
        "validator",
    ]
    .as_slice();
    cosmovisor(
        "tx gov submit-proposal consumer-addition",
        &[&[proposal_file_path.as_str()], gas_args].concat(),
    )
    .await?;
    // the deposit is done as part of the chain addition proposal
    cosmovisor(
        "tx gov vote",
        &[[proposal_id, "yes"].as_slice(), gas_args].concat(),
    )
    .await?;

    // In the mean time get consensus key assignment done

    // FIXME this should be from $DAEMON_HOME/config/priv_validator_key.json, not
    // some random thing from the validator set
    let tmp_s = get_separated_val(
        &cosmovisor("query tendermint-validator-set", &[]).await?,
        "\n",
        "value",
        ":",
    )?;
    let mut consensus_pubkey = r#"{"@type":"/cosmos.crypto.ed25519.PubKey","key":""#.to_owned();
    consensus_pubkey.push_str(&tmp_s);
    consensus_pubkey.push_str("\"}}");

    //info!("ccvkey: {consensus_pubkey}");

    // do this before getting the consumer-genesis
    cosmovisor(
        "tx provider assign-consensus-key market",
        &[[consensus_pubkey.as_str()].as_slice(), gas_args].concat(),
    )
    .await?;

    wait_for_height(STD_TRIES, STD_DELAY, 5).await?;

    let ccvconsumer_state =
        cosmovisor("query provider consumer-genesis market -o json", &[]).await?;

    //info!("ccvconsumer_state:\n{ccvconsumer_state}\n\n");

    nm_hermes.send::<String>(&mnemonic).await?;

    // send to `marketd`
    nm_marketd.send::<String>(&ccvconsumer_state).await?;

    let genesis_s =
        FileOptions::read_to_string(&format!("{daemon_home}/config/genesis.json")).await?;
    //info!("genesis: {genesis_s}");
    let genesis: Value = serde_json::from_str(&genesis_s)?;
    nm_marketd
        .send::<String>(&genesis["app_state"]["auth"]["accounts"].to_string())
        .await?;
    nm_marketd
        .send::<String>(&genesis["app_state"]["bank"].to_string())
        .await?;
    nm_marketd
        .send::<String>(
            &FileOptions::read_to_string(&format!("{daemon_home}/config/node_key.json")).await?,
        )
        .await?;
    nm_marketd
        .send::<String>(
            &FileOptions::read_to_string(&format!("{daemon_home}/config/priv_validator_key.json"))
                .await?,
        )
        .await?;

    // wait for marketd to be online
    nm_marketd.recv::<()>().await?;
    nm_hermes.send::<()>(&()).await?;
    nm_hermes.recv::<()>().await?;

    //cosmovisor("tx ibc-transfer transfer", &[port, channel, receiver,
    // amount]).await?;

    sleep(TIMEOUT).await;
    cosmovisor_runner.terminate().await?;
    Ok(())
}

async fn marketd_runner() -> Result<()> {
    let mut nm_onomyd = NetMessenger::listen_single_connect("0.0.0.0:26001", TIMEOUT).await?;

    let daemon_home = DAEMON_HOME.as_str();
    let chain_id = "market";
    cosmovisor("config chain-id", &[chain_id]).await?;
    cosmovisor("config keyring-backend test", &[]).await?;
    cosmovisor("init --overwrite", &[chain_id]).await?;
    let genesis_file_path = format!("{daemon_home}/config/genesis.json");

    // we need both the initial consumer state and the accounts, plus we just copy
    // over the bank (or else we need some kind of funding) for the test to work
    let ccvconsumer_state_s: String = nm_onomyd.recv().await?;
    let ccvconsumer_state: Value = serde_json::from_str(&ccvconsumer_state_s)?;

    let accounts_s: String = nm_onomyd.recv().await?;
    let accounts: Value = serde_json::from_str(&accounts_s)?;

    let bank_s: String = nm_onomyd.recv().await?;
    let bank: Value = serde_json::from_str(&bank_s)?;

    // add `ccvconsumer_state` to genesis

    let genesis_s = FileOptions::read_to_string(&genesis_file_path).await?;

    let mut genesis: Value = serde_json::from_str(&genesis_s)?;
    genesis["app_state"]["ccvconsumer"] = ccvconsumer_state;
    genesis["app_state"]["auth"]["accounts"] = accounts;
    genesis["app_state"]["bank"] = bank;
    let genesis_s = genesis.to_string();
    let genesis_s = genesis_s.replace("\"stake\"", "\"anom\"");

    //info!("genesis: {genesis_s}");

    FileOptions::write_str(&genesis_file_path, &genesis_s).await?;
    FileOptions::write_str(&"/logs/market_genesis.json", &genesis_s).await?;

    // we used same keys for consumer as producer, need to copy them over or else
    // the node will not be a working validator for itself
    FileOptions::write_str(
        &format!("{daemon_home}/config/node_key.json"),
        &nm_onomyd.recv::<String>().await?,
    )
    .await?;
    FileOptions::write_str(
        &format!("{daemon_home}/config/priv_validator_key.json"),
        &nm_onomyd.recv::<String>().await?,
    )
    .await?;

    let mut cosmovisor_runner = cosmovisor_start("marketd_runner.log", true, None).await?;

    // signal that we have started
    nm_onomyd.send::<()>(&()).await?;

    sleep(TIMEOUT).await;
    cosmovisor_runner.terminate().await?;
    Ok(())
}
