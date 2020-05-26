//! Downloads a testnet configuration from Github.

use reqwest;
use std::env;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;

const TESTNET_ID: &str = "witti-v0-11-3";

const DOWNLOAD_GENESIS_STATE: bool = false;

fn main() {
    if !base_dir().exists() {
        std::fs::create_dir_all(base_dir()).expect(&format!("Unable to create {:?}", base_dir()));

        match get_all_files() {
            Ok(()) => (),
            Err(e) => {
                std::fs::remove_dir_all(base_dir()).expect(&format!(
                    "{}. Failed to remove {:?}, please remove the directory manually because it may contains incomplete testnet data.",
                    e,
                    base_dir(),
                ));
                panic!(e);
            }
        }
    }
}

pub fn get_all_files() -> Result<(), String> {
    get_file("boot_enr.yaml")?;
    get_file("config.yaml")?;
    get_file("deploy_block.txt")?;
    get_file("deposit_contract.txt")?;
    if DOWNLOAD_GENESIS_STATE {
        get_file("genesis.ssz")?;
    }

    Ok(())
}

pub fn get_file(filename: &str) -> Result<(), String> {
    let url = format!(
        "https://raw.githubusercontent.com/goerli/witti/6aa9043b089939f3833681e4b1bbd61cafd92045/lighthouse/{}",
        filename
    );

    let path = base_dir().join(filename);
    let mut file =
        File::create(path).map_err(|e| format!("Failed to create {}: {:?}", filename, e))?;

    let request = reqwest::blocking::Client::builder()
        .build()
        .map_err(|_| "Could not build request client".to_string())?
        .get(&url)
        .timeout(std::time::Duration::from_secs(120));

    let contents = request
        .send()
        .map_err(|e| format!("Failed to download {}: {}", filename, e))?
        .error_for_status()
        .map_err(|e| format!("Error downloading {}: {}", filename, e))?
        .bytes()
        .map_err(|e| format!("Failed to read {} response bytes: {}", filename, e))?;

    file.write(&contents)
        .map_err(|e| format!("Failed to write to {}: {:?}", filename, e))?;

    Ok(())
}

fn base_dir() -> PathBuf {
    env::var("CARGO_MANIFEST_DIR")
        .expect("should know manifest dir")
        .parse::<PathBuf>()
        .expect("should parse manifest dir as path")
        .join(TESTNET_ID)
}
