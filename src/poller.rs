use crate::{
    bitcoind::{
        interface::{BitcoinD, ChainTip},
        BitcoindError,
    },
    config::Config,
    database::{
        db_cancel_signatures, db_canceling_vaults, db_del_vault, db_delegated_vaults, db_instance,
        db_revoc_confirmed, db_should_cancel_vault, db_should_not_cancel_vault, db_update_tip,
        db_vault, schema::DbVault, DatabaseError,
    },
    plugins::{NewBlockInfo, VaultInfo},
};
use revault_tx::{
    bitcoin::{consensus::encode, secp256k1},
    scripts::{DerivedCpfpDescriptor, DerivedDepositDescriptor, DerivedUnvaultDescriptor},
    transactions::{CancelTransaction, RevaultTransaction, UnvaultTransaction},
    txins::{DepositTxIn, RevaultTxIn, UnvaultTxIn},
    txouts::DepositTxOut,
};

use std::{convert::TryInto, path, thread};

/// How many blocks are we waiting to consider a consumed vault irreversably spent
const REORG_WATCH_LIMIT: i32 = 288;

/// An error happened in the main loop
#[derive(Debug)]
pub enum PollerError {
    Database(DatabaseError),
    Bitcoind(BitcoindError),
}

impl std::fmt::Display for PollerError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::Database(ref e) => write!(f, "Database error in main loop: '{}'", e),
            Self::Bitcoind(ref e) => write!(f, "Bitcoind error in main loop: '{}'", e),
        }
    }
}

impl std::error::Error for PollerError {}

impl From<BitcoindError> for PollerError {
    fn from(e: BitcoindError) -> Self {
        Self::Bitcoind(e)
    }
}

impl From<DatabaseError> for PollerError {
    fn from(e: DatabaseError) -> Self {
        Self::Database(e)
    }
}

fn descriptors(
    secp: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    config: &Config,
    db_vault: &DbVault,
) -> (
    DerivedDepositDescriptor,
    DerivedUnvaultDescriptor,
    DerivedCpfpDescriptor,
) {
    (
        config
            .scripts_config
            .deposit_descriptor
            .derive(db_vault.derivation_index, secp),
        config
            .scripts_config
            .unvault_descriptor
            .derive(db_vault.derivation_index, secp),
        config
            .scripts_config
            .cpfp_descriptor
            .derive(db_vault.derivation_index, secp),
    )
}

fn unvault_tx(
    db_vault: &DbVault,
    deposit_desc: &DerivedDepositDescriptor,
    unvault_desc: &DerivedUnvaultDescriptor,
    cpfp_desc: &DerivedCpfpDescriptor,
) -> Result<UnvaultTransaction, revault_tx::error::TransactionCreationError> {
    let deposit_txo = DepositTxOut::new(db_vault.amount, deposit_desc);
    let deposit_txin = DepositTxIn::new(db_vault.deposit_outpoint, deposit_txo);
    UnvaultTransaction::new(
        deposit_txin,
        &unvault_desc,
        &cpfp_desc,
        /* FIXME: remove from the API */ 0,
    )
}

fn manage_cancel_attempts(
    db_path: &path::Path,
    secp: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    config: &Config,
    bitcoind: &BitcoinD,
    current_tip: &ChainTip,
) -> Result<(), PollerError> {
    let canceling_vaults = db_canceling_vaults(db_path)?;

    for db_vault in canceling_vaults {
        let (deposit_desc, unvault_desc, cpfp_desc) = descriptors(secp, config, &db_vault);
        let unvault_tx = match unvault_tx(&db_vault, &deposit_desc, &unvault_desc, &cpfp_desc) {
            Ok(tx) => tx,
            Err(e) => {
                // TODO: handle dust better (they should never send us dust vaults though)
                log::error!("Unexpected error deriving Unvault transaction: '{}'", e);
                continue;
            }
        };
        let unvault_txin = unvault_tx.revault_unvault_txin(&unvault_desc);
        let unvault_outpoint = unvault_txin.outpoint();
        let cancel_tx = CancelTransaction::new(
            unvault_txin,
            None,
            &deposit_desc,
            /* FIXME: remove from the API */ 0,
        )
        .expect("Can only fail if we have an insane feebump input");

        // If it was confirmed, check for how long and maybe forget it. Otherwise check if it got
        // confirmed since our last poll.
        if let Some(conf_height) = db_vault.revoc_height {
            let n_confs = current_tip
                .height
                .checked_add(1)
                .expect("A block height>2147483647?")
                .checked_sub(conf_height)
                .expect("Impossible, the confirmation height is always <= tip");
            if n_confs > REORG_WATCH_LIMIT {
                db_del_vault(db_path, db_vault.id)?;
                log::info!(
                    "Forgetting about consumed vault at '{}' after its Cancel transaction had '{}' confirmations.",
                    &db_vault.deposit_outpoint, n_confs
                );
            }
            continue;
        }

        // Check if it just got confirmed.
        let cancel_outpoint = cancel_tx.deposit_txin(&deposit_desc).outpoint();
        if let Some(utxoinfo) = bitcoind.utxoinfo(&cancel_outpoint) {
            if utxoinfo.bestblock != current_tip.hash {
                // TODO
            }

            let confirmation_height = current_tip
                .height
                .checked_add(1)
                .expect("A block height>2147483647?")
                .checked_sub(
                    // Can't be 0, as we don't include the mempool
                    utxoinfo
                        .confirmations
                        .try_into()
                        .expect("A block height>2147483648?"),
                )
                .expect("Impossible, we just checked the tip is in sync");
            db_revoc_confirmed(db_path, db_vault.id, confirmation_height)?;

            log::debug!(
                "Vault at '{}' Cancel transaction '{}' confirmed at height '{}'",
                &db_vault.deposit_outpoint,
                cancel_tx.txid(),
                confirmation_height
            );
            continue;
        }

        // If the chain didn't change, and there is no Cancel UTXO at the best block there
        // are only 2 possibilities before the expiration of the CSV: either the Cancel
        // transaction is still unconfirmed (and therefore the Unvault UTXO is still present)
        // or it was spent.
        if bitcoind.utxoinfo(&unvault_outpoint).is_none() {
            if bitcoind.chain_tip().hash != current_tip.hash {
                // TODO
            }

            let csv: i32 = config
                .scripts_config
                .unvault_descriptor
                .csv_value()
                .try_into()
                .expect("CSV value doesn't fit in i32?");
            let unvault_height = db_vault
                .unvault_height
                .expect("No unvault_height for unvaulted vault?");
            db_revoc_confirmed(db_path, db_vault.id, current_tip.height)?;
            if current_tip.height < unvault_height + csv {
                log::debug!(
                    "Noticed at height '{}' that Cancel transaction '{}' was confirmed for vault at '{}'",
                    current_tip.height,
                    cancel_tx.txid(),
                    &db_vault.deposit_outpoint,
                );
            } else {
                // If the timelock matured and we have no confirmed Cancel UTxO, either:
                //  - a Spend transaction has spent the Unvault txo.
                //  - another Cancel transaction was mined
                //  - our Cancel transaction was mined but spent again
                // In this case we just set the revocation height to the tip as a hack
                // to still have it expire after REORG_WATCH_LIMIT
                log::info!(
                    "Noticed at height '{}' that Unvault UTxO '{}' was spent for vault at '{}', \
                     but our Cancel transaction output is not part of the UTxO set.",
                    current_tip.height,
                    unvault_outpoint,
                    &db_vault.deposit_outpoint,
                );
            }
        }

        // Ok the Cancel is still unconfirmed.
        log::debug!(
            "Cancel transaction '{}' for vault at '{}' is still unconfirmed at height '{}'",
            cancel_tx.txid(),
            &db_vault.deposit_outpoint,
            current_tip.height
        );
    }

    Ok(())
}

// TODO: actual feebump computation, register attempt in db, ..
fn revault(
    db_path: &path::Path,
    secp: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    bitcoind: &BitcoinD,
    db_vault: &DbVault,
    unvault_txin: UnvaultTxIn,
    deposit_desc: &DerivedDepositDescriptor,
) -> Result<(), PollerError> {
    let mut cancel_tx = CancelTransaction::new(
        unvault_txin,
        None,
        &deposit_desc,
        /* FIXME: remove from the API */ 0,
    )
    .expect("Can only fail if we have an insane feebumping input");

    for db_sig in db_cancel_signatures(db_path, db_vault.id)? {
        if let Err(e) = cancel_tx.add_cancel_sig(db_sig.pubkey, db_sig.signature, secp) {
            log::error!(
                "Error adding signature '{:?}' to Cancel transaction '{}': '{:?}'",
                db_sig,
                cancel_tx,
                e
            );
        } else {
            log::trace!(
                "Added signature '{:?}' to Cancel transaction '{}'",
                db_sig,
                cancel_tx
            );
        }
    }

    if let Err(e) = cancel_tx.finalize(secp) {
        log::error!(
            "Error finalizing Cancel transaction '{}': '{:?}'",
            cancel_tx,
            e
        );
        return Ok(()); // Don't crash, though.
    } else {
        log::trace!("Finalized Cancel transaction '{}'", cancel_tx);
    }

    let cancel_tx = cancel_tx.into_tx();
    if let Err(e) = bitcoind.broadcast_tx(&cancel_tx) {
        log::error!(
            "Error broadcasting Cancel transaction '{}': '{:?}'",
            encode::serialize_hex(&cancel_tx),
            e
        );
    } else {
        log::debug!(
            "Broadcasted Cancel transaction '{}'",
            encode::serialize_hex(&cancel_tx)
        );
    }

    Ok(())
}

// Poll bitcoind for new Unvault UTxO of delegated vaults we are watching. Return info about each
// new Unvault attempt.
fn check_for_unvault(
    db_path: &path::Path,
    secp: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    config: &Config,
    bitcoind: &BitcoinD,
    current_tip: &ChainTip,
) -> Result<NewBlockInfo, PollerError> {
    let deleg_vaults = db_delegated_vaults(db_path)?;
    let mut new_attempts = vec![];

    for db_vault in deleg_vaults {
        let (deposit_desc, unvault_desc, cpfp_desc) = descriptors(secp, config, &db_vault);
        let unvault_tx = match unvault_tx(&db_vault, &deposit_desc, &unvault_desc, &cpfp_desc) {
            Ok(tx) => tx,
            Err(e) => {
                // TODO: handle dust better (they should never send us dust vaults though)
                log::error!("Unexpected error deriving Unvault transaction: '{}'", e);
                continue;
            }
        };
        let unvault_txin = unvault_tx.revault_unvault_txin(&unvault_desc);

        if let Some(utxoinfo) = bitcoind.utxoinfo(&unvault_txin.outpoint()) {
            if current_tip.hash != utxoinfo.bestblock {
                // TODO
            }
            let confs: i32 = utxoinfo
                .confirmations
                .try_into()
                .expect("A number of confs that doesn't fit in a i32?");
            let unvault_height = current_tip.height - (confs - 1);
            assert!(confs > 0 && unvault_height > 0);
            log::debug!(
                "Got a confirmed Unvault UTXO at '{}': '{:?}'",
                &unvault_txin.outpoint(),
                utxoinfo
            );

            // If needed to be canceled it will be marked as such when plugins tell us so.
            db_should_not_cancel_vault(db_path, db_vault.id, unvault_height)?;
            let vault_info = VaultInfo {
                value: db_vault.amount,
                deposit_outpoint: db_vault.deposit_outpoint,
                unvault_tx,
            };
            new_attempts.push(vault_info);
        }
    }

    Ok(NewBlockInfo {
        new_attempts,
        // TODO: record and share canceled and spent attempts
        successful_attempts: vec![],
        revaulted_attempts: vec![],
    })
}

// Poll each of our plugins for vaults to be revaulted given the updates to our vaults' state
// (which might be an empty set) in the latest block.
fn maybe_revault(
    db_path: &path::Path,
    secp: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    config: &Config,
    bitcoind: &BitcoinD,
    block_height: i32,
    block_info: &NewBlockInfo,
) -> Result<(), PollerError> {
    let outpoints_to_revault = config
        .plugins
        .iter()
        .fold(vec![], |mut to_revault, plugin| {
            match plugin.poll(block_height, block_info) {
                Ok(mut res) => to_revault.append(&mut res),
                Err(e) => {
                    // FIXME: should we crash instead?
                    log::error!("Error when polling plugin: '{}'", e);
                }
            };
            to_revault
        });

    for deposit_outpoint in outpoints_to_revault {
        if let Some(db_vault) = db_vault(db_path, &deposit_outpoint)? {
            let unvault_height = match db_vault.unvault_height {
                Some(h) => h,
                None => {
                    // FIXME: should we crash? This must never happen.
                    log::error!("One of the plugins told us to revault a non-unvaulted vault");
                    continue;
                }
            };
            let (deposit_desc, unvault_desc, cpfp_desc) = descriptors(secp, config, &db_vault);
            let unvault_tx = match unvault_tx(&db_vault, &deposit_desc, &unvault_desc, &cpfp_desc) {
                Ok(tx) => tx,
                Err(e) => {
                    // TODO: handle dust better (they should never send us dust vaults though)
                    log::error!("Unexpected error deriving Unvault transaction: '{}'", e);
                    continue;
                }
            };
            let unvault_txin = unvault_tx.revault_unvault_txin(&unvault_desc);

            db_should_cancel_vault(db_path, db_vault.id, unvault_height)?;
            revault(
                db_path,
                secp,
                bitcoind,
                &db_vault,
                unvault_txin,
                &deposit_desc,
            )?;
        } else {
            // FIXME: should we crash? This must never happen.
            log::error!("One of the plugins returned an inexistant outpoint.");
        }
    }

    Ok(())
}

// We only do actual processing on new blocks. This puts a natural limit on the amount of work
// we are doing, reduces the number of edge cases we need to handle, and there is no benefit to try
// to cancel Unvaults right after their broadcast.
// NOTE: we don't handle Emergency transactions for now.
fn new_block(
    db_path: &path::Path,
    secp: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    config: &Config,
    bitcoind: &BitcoinD,
    current_tip: &ChainTip,
) -> Result<(), PollerError> {
    // Update the fee-bumping reserves estimates
    // TODO

    // Any vault to forget and feebump coins to unregister?
    // TODO

    // Any Unvault txo confirmed?
    let new_blk_info = check_for_unvault(db_path, secp, config, bitcoind, current_tip)?;

    // Any vault plugins tell us to revault?
    maybe_revault(
        db_path,
        secp,
        config,
        bitcoind,
        current_tip.height,
        &new_blk_info,
    )?;

    // TODO: manage the non to-cancel too!
    // Any Cancel tx still unconfirmed? Any to forget about?
    manage_cancel_attempts(db_path, secp, config, bitcoind, current_tip)?;

    // Any coin received on the FB wallet?
    // TODO

    // Any FB coin to be registered for consolidation?
    // TODO

    // Any consolidation to be processed given the current fee market?
    // TODO

    Ok(())
}

pub fn main_loop(
    db_path: &path::Path,
    secp: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    config: &Config,
    bitcoind: &BitcoinD,
) -> Result<(), PollerError> {
    loop {
        let db_instance = db_instance(db_path)?;
        let bitcoind_tip = bitcoind.chain_tip();

        if bitcoind_tip.height > db_instance.tip_blockheight {
            let curr_tip_hash = bitcoind.block_hash(db_instance.tip_blockheight);
            if db_instance.tip_blockheight != 0 && curr_tip_hash != db_instance.tip_blockhash {
                panic!("No reorg handling yet");
            }

            new_block(db_path, secp, config, bitcoind, &bitcoind_tip)?;
            db_update_tip(db_path, bitcoind_tip.height, bitcoind_tip.hash)?;
        } else if bitcoind_tip.hash != db_instance.tip_blockhash {
            panic!("No reorg handling yet");
        }

        thread::sleep(config.bitcoind_config.poll_interval_secs);
    }
}
