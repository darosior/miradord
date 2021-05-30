mod bitcoind;
mod config;
mod keys;

use bitcoind::{load_watchonly_wallet, start_bitcoind, wait_bitcoind_synced};
use config::{config_folder_path, Config};
use keys::read_or_create_noise_key;
use revault_net::{
    bitcoin::hashes::hex::ToHex,
    noise::PublicKey as NoisePubKey,
    sodiumoxide::{self, crypto::scalarmult::curve25519},
};

use std::{env, fs, os::unix::fs::DirBuilderExt, path, process, time};

const VAULT_WATCHONLY_FILENAME: &str = "vault_watchonly";
const NOISE_KEY_FILENAME: &str = "noise_secret";

fn parse_args(args: Vec<String>) -> Option<path::PathBuf> {
    if args.len() == 1 {
        return None;
    }

    if args.len() != 3 {
        eprintln!("Unknown arguments '{:?}'.", args);
        eprintln!("Only '--conf <configuration file path>' is supported.");
        process::exit(1);
    }

    Some(path::PathBuf::from(args[2].to_owned()))
}

// We always log on stdout, it'll be piped if we are daemonized.
fn setup_logger(log_level: log::LevelFilter) -> Result<(), fern::InitError> {
    let dispatcher = fern::Dispatch::new()
        .format(|out, message, record| {
            out.finish(format_args!(
                "[{}][{}][{}] {}",
                time::SystemTime::now()
                    .duration_since(time::UNIX_EPOCH)
                    .unwrap_or_else(|e| {
                        println!("Can't get time since epoch: '{}'. Using a dummy value.", e);
                        time::Duration::from_secs(0)
                    })
                    .as_secs(),
                record.target(),
                record.level(),
                message
            ))
        })
        .level(log_level);

    dispatcher.chain(std::io::stdout()).apply()?;

    Ok(())
}

fn create_datadir(datadir_path: &path::Path) -> Result<(), std::io::Error> {
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700).recursive(true).create(datadir_path)
}

fn main() {
    #[cfg(not(unix))]
    {
        eprintln!("Only Linux is supported.");
        process::exit(1);
    }

    let args = env::args().collect();
    let conf_file = parse_args(args);

    // We use libsodium for Noise keys and Noise channels (through revault_net)
    sodiumoxide::init().unwrap_or_else(|_| {
        eprintln!("Error init'ing libsodium");
        process::exit(1);
    });

    let config = Config::from_file(conf_file).unwrap_or_else(|e| {
        eprintln!("Error parsing config: {}", e);
        process::exit(1);
    });
    setup_logger(config.log_level).unwrap_or_else(|e| {
        eprintln!("Error setting up logger: {}", e);
        process::exit(1);
    });

    let mut data_dir = config.data_dir.unwrap_or_else(|| {
        config_folder_path().unwrap_or_else(|e| {
            eprintln!("Error getting default data directory: '{}'.", e);
            process::exit(1);
        })
    });
    data_dir.push(config.bitcoind_config.network.to_string());
    log::info!("Using data directory at '{}'.", data_dir.to_string_lossy());
    if !data_dir.as_path().exists() {
        log::info!("Data directory doesn't exist, creating it.");
        create_datadir(&data_dir).unwrap_or_else(|e| {
            eprintln!("Error creating data directory: '{}'.", e);
            process::exit(1);
        });
    }
    data_dir = fs::canonicalize(data_dir).unwrap_or_else(|e| {
        eprintln!("Error canonicalizing data directory: '{}'.", e);
        process::exit(1);
    });

    log::info!("Setting up bitcoind connection");
    let mut vault_watchonly_path = data_dir
        .to_str()
        .expect("Data dir must be valid unicode")
        .to_string();
    vault_watchonly_path.push_str(VAULT_WATCHONLY_FILENAME);
    let bitcoind = start_bitcoind(&config.bitcoind_config, vault_watchonly_path.clone())
        .unwrap_or_else(|e| {
            log::error!("Error setting up bitcoind RPC connection: '{}'", e);
            process::exit(1);
        });

    log::info!("Checking if bitcoind is synced");
    wait_bitcoind_synced(&bitcoind);

    load_watchonly_wallet(&bitcoind, vault_watchonly_path).unwrap_or_else(|e| {
        log::error!("Error loading vault watchonly wallet: '{}'", e);
        process::exit(1);
    });
    // TODO: load feebumping wallet too.

    let mut noise_secret_path = data_dir.clone();
    noise_secret_path.push(path::Path::new(NOISE_KEY_FILENAME));
    log::info!(
        "Reading or generating Noise key at '{:?}'",
        noise_secret_path
    );
    let noise_secret = read_or_create_noise_key(&noise_secret_path).unwrap_or_else(|e| {
        log::error!("Error reading or generating Noise key: '{}'", e);
        process::exit(1);
    });
    log::info!(
        "Using Noise key '{}'.",
        NoisePubKey(curve25519::scalarmult_base(&curve25519::Scalar(noise_secret.0)).0)
            .0
            .to_hex()
    );
}
