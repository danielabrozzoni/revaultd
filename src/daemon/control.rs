//! By itself, the daemon is not doing much: it basically just keeps its database updated with the
//! chain events in the bitcoind thread.
//! Any process is at first initiated by a manual interaction. This interaction is possible using the
//! JSONRPC api, which events are handled in the RPC thread.
//!
//! The main thread handles and coordinates all processes, which (for now) all originates from a
//! command sent to the RPC server. This control handling is what happens here.

use crate::{
    bitcoind::BitcoindError,
    database::{
        actions::db_update_presigned_tx,
        interface::{
            db_cancel_transaction, db_emer_transaction, db_tip, db_unvault_emer_transaction,
            db_unvault_transaction, db_vault_by_deposit, db_vaults,
        },
        DatabaseError,
    },
    revaultd::{BlockchainTip, RevaultD, VaultStatus},
    sigfetcher::presigned_tx_sighash,
    threadmessages::*,
};
use common::{assume_ok, assume_some};

use revault_net::{message::server::Sig, transport::KKTransport};
use revault_tx::{
    bitcoin::{
        secp256k1::{self, Signature},
        Network, OutPoint, PublicKey as BitcoinPubKey, SigHashType, Txid,
    },
    transactions::{
        transaction_chain, CancelTransaction, EmergencyTransaction, RevaultTransaction,
        UnvaultEmergencyTransaction, UnvaultTransaction,
    },
    txins::DepositTxIn,
    txouts::DepositTxOut,
};

use std::{
    collections::BTreeMap,
    fmt,
    path::PathBuf,
    process,
    sync::{
        mpsc::{self, Receiver, RecvError, SendError, Sender},
        Arc, RwLock,
    },
    thread::JoinHandle,
};

/// Any error that could arise during the process of executing the user's will.
/// Usually fatal.
#[derive(Debug)]
pub enum ControlError {
    ChannelCommunication(String),
    Database(String),
    Bitcoind(String),
    TransactionManagement(String),
}

impl fmt::Display for ControlError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::ChannelCommunication(s) => write!(f, "Channel communication error: '{}'", s),
            Self::Database(s) => write!(f, "Database error: '{}'", s),
            Self::Bitcoind(s) => write!(f, "Bitcoind error: '{}'", s),
            Self::TransactionManagement(s) => write!(f, "Transaction management error: '{}'", s),
        }
    }
}

impl std::error::Error for ControlError {}

impl<T> From<SendError<T>> for ControlError {
    fn from(e: SendError<T>) -> Self {
        Self::ChannelCommunication(format!("Sending to channel: '{}'", e))
    }
}

impl From<RecvError> for ControlError {
    fn from(e: RecvError) -> Self {
        Self::ChannelCommunication(format!("Receiving from channel: '{}'", e))
    }
}

impl From<DatabaseError> for ControlError {
    fn from(e: DatabaseError) -> Self {
        Self::Database(format!("Database error: {}", e))
    }
}

impl From<BitcoindError> for ControlError {
    fn from(e: BitcoindError) -> Self {
        Self::Bitcoind(format!("Bitcoind error: {}", e))
    }
}

impl From<revault_tx::Error> for ControlError {
    fn from(e: revault_tx::Error) -> Self {
        Self::TransactionManagement(format!("Revault transaction error: {}", e))
    }
}

impl From<revault_tx::error::TransactionCreationError> for ControlError {
    fn from(e: revault_tx::error::TransactionCreationError) -> Self {
        Self::TransactionManagement(format!("Revault transaction creation error: {}", e))
    }
}

// Ask bitcoind for a wallet transaction
fn bitcoind_wallet_tx(
    bitcoind_tx: &Sender<BitcoindMessageOut>,
    txid: Txid,
) -> Result<Option<WalletTransaction>, ControlError> {
    log::trace!("Sending WalletTx to bitcoind thread for {}", txid);

    let (bitrep_tx, bitrep_rx) = mpsc::sync_channel(0);
    bitcoind_tx.send(BitcoindMessageOut::WalletTransaction(txid, bitrep_tx))?;
    bitrep_rx.recv().map_err(|e| e.into())
}

// List the vaults from DB, and filter out the info the RPC wants
// FIXME: we could make this more efficient with smarter SQL queries
fn listvaults_from_db(
    revaultd: &RevaultD,
    statuses: Option<Vec<VaultStatus>>,
    outpoints: Option<Vec<OutPoint>>,
) -> Result<Vec<ListVaultsEntry>, DatabaseError> {
    db_vaults(&revaultd.db_file()).map(|db_vaults| {
        db_vaults
            .into_iter()
            .filter_map(|db_vault| {
                if let Some(ref statuses) = statuses {
                    if !statuses.contains(&db_vault.status) {
                        return None;
                    }
                }

                if let Some(ref outpoints) = &outpoints {
                    if !outpoints.contains(&db_vault.deposit_outpoint) {
                        return None;
                    }
                }

                let address = revaultd.vault_address(db_vault.derivation_index);
                Some(ListVaultsEntry {
                    amount: db_vault.amount,
                    status: db_vault.status,
                    deposit_outpoint: db_vault.deposit_outpoint,
                    derivation_index: db_vault.derivation_index,
                    updated_at: db_vault.updated_at,
                    address,
                })
            })
            .collect()
    })
}

// List all the presigned transactions from these confirmed vaults.
fn presigned_txs_list_from_outpoints(
    revaultd: &RevaultD,
    outpoints: Option<Vec<OutPoint>>,
) -> Result<Result<Vec<VaultPresignedTransactions>, RpcControlError>, ControlError> {
    let db_path = &revaultd.db_file();

    // If they didn't provide us with a list of outpoints, catch'em all!
    let db_vaults = if let Some(outpoints) = outpoints {
        // FIXME: we can probably make this more efficient with some SQL magic
        let mut vaults = Vec::with_capacity(outpoints.len());
        for outpoint in outpoints.iter() {
            if let Some(vault) = db_vault_by_deposit(db_path, &outpoint)? {
                // If it's unconfirmed, the presigned transactions are not in db!
                match vault.status {
                    VaultStatus::Unconfirmed => {
                        return Ok(Err(RpcControlError::InvalidStatus((
                            vault.status,
                            VaultStatus::Funded,
                        ))))
                    }
                    _ => vaults.push(vault),
                }
            } else {
                return Ok(Err(RpcControlError::UnknownOutpoint(*outpoint)));
            }
        }
        vaults
    } else {
        db_vaults(db_path)?
    };

    let mut tx_list = Vec::with_capacity(db_vaults.len());
    for db_vault in db_vaults {
        let outpoint = db_vault.deposit_outpoint;

        let (_, unvault) = db_unvault_transaction(db_path, db_vault.id)?;
        let (_, cancel) = db_cancel_transaction(db_path, db_vault.id)?;
        let mut emergency = None;
        let mut unvault_emergency = None;
        if revaultd.is_stakeholder() {
            emergency = Some(db_emer_transaction(db_path, db_vault.id)?.1);
            unvault_emergency = Some(db_unvault_emer_transaction(db_path, db_vault.id)?.1);
        }

        tx_list.push(VaultPresignedTransactions {
            outpoint,
            unvault,
            cancel,
            emergency,
            unvault_emergency,
        });
    }

    Ok(Ok(tx_list))
}

// List all the onchain transactions from these vaults.
fn onchain_txs_list_from_outpoints(
    revaultd: &RevaultD,
    bitcoind_tx: &Sender<BitcoindMessageOut>,
    outpoints: Option<Vec<OutPoint>>,
) -> Result<Result<Vec<VaultOnchainTransactions>, RpcControlError>, ControlError> {
    let db_path = &revaultd.db_file();

    // If they didn't provide us with a list of outpoints, catch'em all!
    let db_vaults = if let Some(outpoints) = outpoints {
        // FIXME: we can probably make this more efficient with some SQL magic
        let mut vaults = Vec::with_capacity(outpoints.len());
        for outpoint in outpoints.iter() {
            if let Some(vault) = db_vault_by_deposit(db_path, &outpoint)? {
                // Note that we accept any status
                vaults.push(vault);
            } else {
                return Ok(Err(RpcControlError::UnknownOutpoint(*outpoint)));
            }
        }
        vaults
    } else {
        db_vaults(db_path)?
    };

    let mut tx_list = Vec::with_capacity(db_vaults.len());
    for db_vault in db_vaults {
        let outpoint = db_vault.deposit_outpoint;

        // If the vault exist, there must always be a deposit transaction available.
        let deposit = assume_some!(
            bitcoind_wallet_tx(bitcoind_tx, db_vault.deposit_outpoint.txid)?,
            "Vault exists but not deposit tx?"
        );

        // For the other transactions, it depends on the status of the vault. For the sake of
        // simplicity bitcoind will tell us (but we could have some optimisation eventually here,
        // eg returning None early on Funded vaults).
        let (unvault, cancel, emergency, unvault_emergency, spend) = match db_vault.status {
            // We allow the unconfirmed status, for which we don't have any presigned tx in db!
            VaultStatus::Unconfirmed => (None, None, None, None, None),
            _ => {
                let (_, unvault) = db_unvault_transaction(db_path, db_vault.id)?;
                let unvault =
                    bitcoind_wallet_tx(bitcoind_tx, unvault.into_psbt().extract_tx().txid())?;
                let (_, cancel) = db_cancel_transaction(db_path, db_vault.id)?;
                let cancel =
                    bitcoind_wallet_tx(bitcoind_tx, cancel.into_psbt().extract_tx().txid())?;

                // Emergencies are only for stakeholders!
                let mut emergency = None;
                let mut unvault_emergency = None;
                if revaultd.is_stakeholder() {
                    let emer = db_emer_transaction(db_path, db_vault.id)?.1;
                    emergency =
                        bitcoind_wallet_tx(bitcoind_tx, emer.into_psbt().extract_tx().txid())?;

                    let unemer = db_unvault_emer_transaction(db_path, db_vault.id)?.1;
                    unvault_emergency =
                        bitcoind_wallet_tx(bitcoind_tx, unemer.into_psbt().extract_tx().txid())?;
                }
                let spend = None; // TODO!

                (unvault, cancel, emergency, unvault_emergency, spend)
            }
        };

        tx_list.push(VaultOnchainTransactions {
            outpoint,
            deposit,
            unvault,
            cancel,
            emergency,
            unvault_emergency,
            spend,
        });
    }

    Ok(Ok(tx_list))
}

/// An error thrown when the verification of a signature fails
#[derive(Debug)]
enum SigError {
    InvalidLength,
    InvalidSighash,
    VerifError(secp256k1::Error),
}

impl std::fmt::Display for SigError {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            Self::InvalidLength => write!(f, "Invalid length of signature"),
            Self::InvalidSighash => write!(f, "Invalid SIGHASH type"),
            Self::VerifError(e) => write!(f, "Signature verification error: '{}'", e),
        }
    }
}

impl std::error::Error for SigError {}

impl From<secp256k1::Error> for SigError {
    fn from(e: secp256k1::Error) -> Self {
        Self::VerifError(e)
    }
}

// Check all complete signatures for revocation transactions (ie Cancel, Emergency,
// or UnvaultEmergency)
fn check_revocation_signatures(
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
    tx: &impl RevaultTransaction,
    sigs: &BTreeMap<BitcoinPubKey, Vec<u8>>,
) -> Result<(), SigError> {
    let sighash_type = SigHashType::AllPlusAnyoneCanPay;
    let sighash = presigned_tx_sighash(tx, sighash_type);

    for (pubkey, sig) in sigs {
        let (sighash_type, sig) = sig.split_last().unwrap();
        if *sighash_type != SigHashType::AllPlusAnyoneCanPay as u8 {
            return Err(SigError::InvalidSighash);
        }
        secp.verify(&sighash, &Signature::from_der(&sig)?, &pubkey.key)?;
    }

    Ok(())
}

fn check_unvault_signatures(
    secp: &secp256k1::Secp256k1<secp256k1::VerifyOnly>,
    tx: &UnvaultTransaction,
) -> Result<(), SigError> {
    let sighash_type = SigHashType::All;
    let sighash = presigned_tx_sighash(tx, sighash_type);
    let sigs = &tx
        .inner_tx()
        .inputs
        .get(0)
        .expect("Unvault always has 1 input")
        .partial_sigs;

    for (pubkey, sig) in sigs.iter() {
        let (sighash_type, sig) = sig.split_last().unwrap();
        if *sighash_type != SigHashType::All as u8 {
            return Err(SigError::InvalidSighash);
        }
        secp.verify(&sighash, &Signature::from_der(&sig)?, &pubkey.key)?;
    }

    Ok(())
}

// Send a `sig` (https://github.com/re-vault/practical-revault/blob/master/messages.md#sig-1)
// message to the server for all the sigs of this mapping.
// Note that we are looping, but most (if not all) will only have a single signature
// attached. We are called by the `revocationtxs` RPC, sent after a `getrevocationtxs`
// which generates fresh unsigned transactions.
//
// `sigs` MUST contain valid signatures (including the attached sighash type)
fn send_sig_msg(
    transport: &mut KKTransport,
    id: Txid,
    sigs: BTreeMap<BitcoinPubKey, Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error>> {
    for (pubkey, sig) in sigs {
        let pubkey = pubkey.key;
        let (sigtype, sig) = sig
            .split_last()
            .expect("They must provide valid signatures");
        assert!(
            *sigtype == SigHashType::AllPlusAnyoneCanPay as u8
                || *sigtype == SigHashType::All as u8
        );

        let signature = Signature::from_der(&sig).expect("They must provide valid signatures");
        let sig_msg = Sig {
            pubkey,
            signature,
            id,
        };
        log::debug!(
            "Sending sig '{:?}' to sync server: '{}'",
            sig_msg,
            serde_json::to_string(&sig_msg)?,
        );
        // This will retry 5 times
        transport.write(&serde_json::to_vec(&sig_msg)?)?;
    }

    Ok(())
}

// Send the signatures for the 3 revocation txs to the Coordinator
fn share_rev_signatures(
    revaultd: &RevaultD,
    cancel: (&CancelTransaction, BTreeMap<BitcoinPubKey, Vec<u8>>),
    emer: (&EmergencyTransaction, BTreeMap<BitcoinPubKey, Vec<u8>>),
    unvault_emer: (
        &UnvaultEmergencyTransaction,
        BTreeMap<BitcoinPubKey, Vec<u8>>,
    ),
) -> Result<(), Box<dyn std::error::Error>> {
    // We would not spam the coordinator, would we?
    assert!(cancel.1.len() > 0 && emer.1.len() > 0 && unvault_emer.1.len() > 0);
    let mut transport = KKTransport::connect(
        revaultd.coordinator_host,
        &revaultd.noise_secret,
        &revaultd.coordinator_noisekey,
    )?;

    let cancel_txid = cancel.0.inner_tx().global.unsigned_tx.txid();
    send_sig_msg(&mut transport, cancel_txid, cancel.1)?;
    let emer_txid = emer.0.inner_tx().global.unsigned_tx.txid();
    send_sig_msg(&mut transport, emer_txid, emer.1)?;
    let unvault_emer_txid = unvault_emer.0.inner_tx().global.unsigned_tx.txid();
    send_sig_msg(&mut transport, unvault_emer_txid, unvault_emer.1)?;

    Ok(())
}

fn share_unvault_signatures(
    revaultd: &RevaultD,
    unvault_tx: &UnvaultTransaction,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut transport = KKTransport::connect(
        revaultd.coordinator_host,
        &revaultd.noise_secret,
        &revaultd.coordinator_noisekey,
    )?;

    let sigs = &unvault_tx
        .inner_tx()
        .inputs
        .get(0)
        .expect("Unvault has a single input")
        .partial_sigs;
    log::trace!("Sharing unvault sigs {:?}", sigs);
    let txid = unvault_tx.inner_tx().global.unsigned_tx.txid();
    send_sig_msg(&mut transport, txid, sigs.clone())
}

/// Handle events incoming from the JSONRPC interface.
pub fn handle_rpc_messages(
    revaultd: Arc<RwLock<RevaultD>>,
    db_path: PathBuf,
    network: Network,
    rpc_rx: Receiver<RpcMessageIn>,
    jsonrpc_thread: JoinHandle<()>,
    bitcoind_tx: Sender<BitcoindMessageOut>,
    bitcoind_thread: JoinHandle<()>,
    sigfetcher_tx: Sender<SigFetcherMessageOut>,
    sigfetcher_thread: JoinHandle<()>,
) -> Result<(), ControlError> {
    for msg in rpc_rx {
        match msg {
            RpcMessageIn::Shutdown => {
                log::info!("Stopping revaultd.");
                bitcoind_tx.send(BitcoindMessageOut::Shutdown)?;
                sigfetcher_tx.send(SigFetcherMessageOut::Shutdown)?;

                assume_ok!(jsonrpc_thread.join(), "Joining RPC server thread");
                assume_ok!(bitcoind_thread.join(), "Joining bitcoind thread");
                assume_ok!(sigfetcher_thread.join(), "Joining bitcoind thread");

                process::exit(0);
            }
            RpcMessageIn::GetInfo(response_tx) => {
                log::trace!("Got getinfo from RPC thread");

                let (bitrep_tx, bitrep_rx) = mpsc::sync_channel(0);
                bitcoind_tx.send(BitcoindMessageOut::SyncProgress(bitrep_tx))?;
                let progress = bitrep_rx.recv()?;

                // This means blockheight == 0 for IBD.
                let BlockchainTip {
                    height: blockheight,
                    ..
                } = db_tip(&db_path)?;

                response_tx.send((network.to_string(), blockheight, progress))?;
            }
            RpcMessageIn::ListVaults((statuses, outpoints), response_tx) => {
                log::trace!("Got listvaults from RPC thread");
                response_tx.send(listvaults_from_db(
                    &revaultd.read().unwrap(),
                    statuses,
                    outpoints,
                )?)?;
            }
            RpcMessageIn::DepositAddr(response_tx) => {
                log::trace!("Got 'depositaddr' request from RPC thread");
                response_tx.send(revaultd.read().unwrap().deposit_address())?;
            }
            RpcMessageIn::GetRevocationTxs(outpoint, response_tx) => {
                log::trace!("Got 'getrevocationtxs' request from RPC thread");
                let revaultd = revaultd.read().unwrap();
                let xpub_ctx = revaultd.xpub_ctx();
                let db_file = &revaultd.db_file();

                // First, make sure the vault exists and is confirmed.
                let vault = match db_vault_by_deposit(db_file, &outpoint)? {
                    None => None,
                    Some(vault) => match vault.status {
                        VaultStatus::Unconfirmed => None,
                        _ => Some(vault),
                    },
                };
                if let Some(vault) = vault {
                    // Second, derive the fully-specified deposit txout.
                    let deposit_descriptor =
                        revaultd.deposit_descriptor.derive(vault.derivation_index);
                    let deposit_txin = DepositTxIn::new(
                        outpoint,
                        DepositTxOut::new(vault.amount.as_sat(), &deposit_descriptor, xpub_ctx),
                    );

                    // Third, re-derive all the transactions out of it.
                    let unvault_descriptor =
                        revaultd.unvault_descriptor.derive(vault.derivation_index);
                    let cpfp_descriptor = revaultd.cpfp_descriptor.derive(vault.derivation_index);
                    let emer_address = assume_some!(
                        revaultd.emergency_address.clone(),
                        "The JSONRPC API checked we were a stakeholder"
                    );

                    let (_, cancel, emergency, unvault_emer) = transaction_chain(
                        deposit_txin,
                        &deposit_descriptor,
                        &unvault_descriptor,
                        &cpfp_descriptor,
                        emer_address,
                        xpub_ctx,
                        revaultd.lock_time,
                        revaultd.unvault_csv,
                    )?;

                    response_tx.send(Some((cancel, emergency, unvault_emer)))?;
                } else {
                    response_tx.send(None)?;
                }
            }
            RpcMessageIn::RevocationTxs(
                (outpoint, cancel_tx, emer_tx, unvault_emer_tx),
                response_tx,
            ) => {
                log::trace!("Got 'revocationtxs' from RPC thread");
                let revaultd = revaultd.read().unwrap();
                let secp_ctx = &revaultd.secp_ctx;

                // Checked by the RPC server
                assert!(revaultd.is_stakeholder());

                // They may only send revocation transactions for confirmed and not-yet-presigned
                // vaults.
                let db_vault = match db_vault_by_deposit(&revaultd.db_file(), &outpoint)? {
                    Some(v) => match v.status {
                        VaultStatus::Funded => v,
                        status => {
                            response_tx.send(Some(format!(
                                "Invalid vault status: expected {} but got {}",
                                VaultStatus::Funded,
                                status
                            )))?;
                            continue;
                        }
                    },
                    None => {
                        response_tx.send(Some(
                            "Outpoint does not correspond to an existing vault".to_string(),
                        ))?;
                        continue;
                    }
                };

                // Sanity check they didn't send us garbaged PSBTs
                let (cancel_db_id, db_cancel_tx) =
                    db_cancel_transaction(&revaultd.db_file(), db_vault.id)?;
                let rpc_txid = cancel_tx.inner_tx().global.unsigned_tx.wtxid();
                let db_txid = db_cancel_tx.inner_tx().global.unsigned_tx.wtxid();
                if rpc_txid != db_txid {
                    response_tx.send(Some(format!(
                        "Invalid Cancel tx: db wtxid is '{}' but this PSBT's is '{}' ",
                        db_txid, rpc_txid
                    )))?;
                    continue;
                }
                let (emer_db_id, db_emer_tx) =
                    db_emer_transaction(&revaultd.db_file(), db_vault.id)?;
                let rpc_txid = emer_tx.inner_tx().global.unsigned_tx.wtxid();
                let db_txid = db_emer_tx.inner_tx().global.unsigned_tx.wtxid();
                if rpc_txid != db_txid {
                    response_tx.send(Some(format!(
                        "Invalid Emergency tx: db wtxid is '{}' but this PSBT's is '{}' ",
                        db_txid, rpc_txid
                    )))?;
                    continue;
                }
                let (unvault_emer_db_id, db_unemer_tx) =
                    db_unvault_emer_transaction(&revaultd.db_file(), db_vault.id)?;
                let rpc_txid = unvault_emer_tx.inner_tx().global.unsigned_tx.wtxid();
                let db_txid = db_unemer_tx.inner_tx().global.unsigned_tx.wtxid();
                if rpc_txid != db_txid {
                    response_tx.send(Some(format!(
                        "Invalid Unvault Emergency tx: db wtxid is '{}' but this PSBT's is '{}' ",
                        db_txid, rpc_txid
                    )))?;
                    continue;
                }

                let deriv_index = db_vault.derivation_index;
                let cancel_sigs = cancel_tx
                    .inner_tx()
                    .inputs
                    .get(0)
                    .expect("Cancel tx has a single input, inbefore fee bumping.")
                    .partial_sigs
                    .clone();
                let emer_sigs = emer_tx
                    .inner_tx()
                    .inputs
                    .get(0)
                    .expect("Emergency tx has a single input, inbefore fee bumping.")
                    .partial_sigs
                    .clone();
                let unvault_emer_sigs = unvault_emer_tx
                    .inner_tx()
                    .inputs
                    .get(0)
                    .expect("UnvaultEmergency tx has a single input, inbefore fee bumping.")
                    .partial_sigs
                    .clone();

                // They must have included *at least* a signature for our pubkey
                let our_pubkey = revaultd
                    .our_stk_xpub
                    .expect("We are a stakeholder")
                    .derive_pub(secp_ctx, &[deriv_index])
                    .expect("The derivation index stored in the database is sane (unhardened)")
                    .public_key;
                if !cancel_sigs.contains_key(&our_pubkey) {
                    response_tx.send(Some(format!(
                        "No signature for ourselves ({}) in Cancel transaction",
                        our_pubkey
                    )))?;
                    continue;
                }
                // We use the same public key across the transaction chain, that's pretty
                // neat from an usability perspective.
                if !emer_sigs.contains_key(&our_pubkey) {
                    response_tx.send(Some(
                        "No signature for ourselves in Emergency transaction".to_string(),
                    ))?;
                    continue;
                }
                if !unvault_emer_sigs.contains_key(&our_pubkey) {
                    response_tx.send(Some(
                        "No signature for ourselves in UnvaultEmergency transaction".to_string(),
                    ))?;
                    continue;
                }

                // Don't share anything if we were given invalid signatures. This
                // checks for the presence (and the validity!) of a SIGHASH type flag.
                if let Err(e) = check_revocation_signatures(secp_ctx, &cancel_tx, &cancel_sigs) {
                    response_tx.send(Some(format!(
                        "Invalid signature in Cancel transaction: {}",
                        e
                    )))?;
                    continue;
                }
                if let Err(e) = check_revocation_signatures(secp_ctx, &emer_tx, &emer_sigs) {
                    response_tx.send(Some(format!(
                        "Invalid signature in Emergency transaction: {}",
                        e
                    )))?;
                    continue;
                }
                if let Err(e) =
                    check_revocation_signatures(secp_ctx, &unvault_emer_tx, &unvault_emer_sigs)
                {
                    response_tx.send(Some(format!(
                        "Invalid signature in Unvault Emergency transaction: {}",
                        e
                    )))?;
                    continue;
                }

                // Ok, signatures look legit. Add them to the PSBTs in database.
                // FIXME: edgy edge case: don't crash here, rather return an error if
                // deposit tx was reorged out in between now and the above status check.
                db_update_presigned_tx(
                    &revaultd.db_file(),
                    db_vault.id,
                    cancel_db_id,
                    cancel_sigs.clone(),
                    secp_ctx,
                )?;
                db_update_presigned_tx(
                    &revaultd.db_file(),
                    db_vault.id,
                    emer_db_id,
                    emer_sigs.clone(),
                    secp_ctx,
                )?;
                db_update_presigned_tx(
                    &revaultd.db_file(),
                    db_vault.id,
                    unvault_emer_db_id,
                    unvault_emer_sigs.clone(),
                    secp_ctx,
                )?;

                // Share them with our felow stakeholders.
                if let Err(e) = share_rev_signatures(
                    &revaultd,
                    (&cancel_tx, cancel_sigs),
                    (&emer_tx, emer_sigs),
                    (&unvault_emer_tx, unvault_emer_sigs),
                ) {
                    response_tx.send(Some(format!("Error while sharing signatures: {}", e)))?;
                    continue;
                }

                // Ok, RPC server, tell them that everything is fine.
                response_tx.send(None)?;
            }
            RpcMessageIn::GetUnvaultTx(outpoint, response_tx) => {
                log::trace!("Got 'getunvaulttx' request from RPC thread");
                let revaultd = revaultd.read().unwrap();
                let xpub_ctx = revaultd.xpub_ctx();
                let db_file = &revaultd.db_file();

                // We allow the call for Funded 'only' as unvaulttx would later fail if it's
                // not 'secured'.
                let vault = match db_vault_by_deposit(db_file, &outpoint)? {
                    None => {
                        response_tx.send(Err(RpcControlError::UnknownOutpoint(outpoint)))?;
                        continue;
                    }
                    Some(vault) => match vault.status {
                        VaultStatus::Unconfirmed => {
                            response_tx.send(Err(RpcControlError::InvalidStatus((
                                vault.status,
                                VaultStatus::Funded,
                            ))))?;
                            continue;
                        }
                        _ => vault,
                    },
                };

                // Derive the descriptors needed to create the UnvaultTransaction
                let deposit_descriptor = revaultd.deposit_descriptor.derive(vault.derivation_index);
                let deposit_txin = DepositTxIn::new(
                    outpoint,
                    DepositTxOut::new(vault.amount.as_sat(), &deposit_descriptor, xpub_ctx),
                );
                let unvault_descriptor = revaultd.unvault_descriptor.derive(vault.derivation_index);
                let cpfp_descriptor = revaultd.cpfp_descriptor.derive(vault.derivation_index);

                let unvault_tx = UnvaultTransaction::new(
                    deposit_txin,
                    &unvault_descriptor,
                    &cpfp_descriptor,
                    xpub_ctx,
                    0,
                )?;
                response_tx.send(Ok(unvault_tx))?;
            }
            RpcMessageIn::UnvaultTx((outpoint, unvault_tx), response_tx) => {
                log::trace!("Got 'unvaulttx' from RPC thread");
                let revaultd = revaultd.read().unwrap();
                let secp_ctx = &revaultd.secp_ctx;

                // If they haven't got all the signatures for the revocation transactions, we'd
                // better not send our unvault sig!
                // If the vault is already active (or more) there is no point in spamming the
                // coordinator.
                let db_vault = match db_vault_by_deposit(&revaultd.db_file(), &outpoint)? {
                    None => {
                        response_tx.send(Err(RpcControlError::UnknownOutpoint(outpoint)))?;
                        continue;
                    }
                    Some(vault) => match vault.status {
                        VaultStatus::Secured => vault,
                        s => {
                            response_tx.send(Err(RpcControlError::InvalidStatus((
                                s,
                                VaultStatus::Funded,
                            ))))?;
                            continue;
                        }
                    },
                };

                // Sanity check they didn't send us a garbaged PSBT
                let (unvault_db_id, db_unvault_tx) =
                    db_unvault_transaction(&revaultd.db_file(), db_vault.id)?;
                let rpc_txid = unvault_tx.inner_tx().global.unsigned_tx.wtxid();
                let db_txid = db_unvault_tx.inner_tx().global.unsigned_tx.wtxid();
                if rpc_txid != db_txid {
                    response_tx.send(Err(RpcControlError::InvalidPsbt(format!(
                        "Invalid Unvault tx: db wtxid is '{}' but this PSBT's is '{}' ",
                        db_txid, rpc_txid
                    ))))?;
                    continue;
                }

                let sigs = &unvault_tx
                    .inner_tx()
                    .inputs
                    .get(0)
                    .expect("UnvaultTransaction always has 1 input")
                    .partial_sigs;
                // They must have included *at least* a signature for our pubkey
                let our_pubkey = revaultd
                    .our_stk_xpub
                    .expect("We are a stakeholder")
                    .derive_pub(secp_ctx, &[db_vault.derivation_index])
                    .expect("The derivation index stored in the database is sane (unhardened)")
                    .public_key;
                if !sigs.contains_key(&our_pubkey) {
                    response_tx.send(Err(RpcControlError::InvalidPsbt(format!(
                        "No signature for ourselves ({}) in Unvault transaction",
                        our_pubkey
                    ))))?;
                    continue;
                }

                // Of course, don't send a PSBT with an invalid signature
                if let Err(e) = check_unvault_signatures(secp_ctx, &unvault_tx) {
                    response_tx.send(Err(RpcControlError::InvalidPsbt(format!(
                        "Invalid signature in Unvault transaction: '{}'",
                        e
                    ))))?;
                    continue;
                }

                // Sanity checks passed. Store it then share it.
                // FIXME: edgy edge case: don't crash here, rather return an error if
                // deposit tx was reorged out in between now and the above status check.
                db_update_presigned_tx(
                    &revaultd.db_file(),
                    db_vault.id,
                    unvault_db_id,
                    sigs.clone(),
                    secp_ctx,
                )?;
                if let Err(e) = share_unvault_signatures(&revaultd, &unvault_tx) {
                    response_tx.send(Err(RpcControlError::Communication(format!(
                        "Sharing Unvault signatures with coordinator: '{}'",
                        e
                    ))))?;
                    continue;
                }

                response_tx.send(Ok(()))?;
            }
            RpcMessageIn::ListPresignedTransactions(outpoints, response_tx) => {
                log::trace!("Got 'listpresignedtransactions' request from RPC thread");
                response_tx.send(presigned_txs_list_from_outpoints(
                    &revaultd.read().unwrap(),
                    outpoints,
                )?)?;
            }
            RpcMessageIn::ListOnchainTransactions(outpoints, response_tx) => {
                log::trace!("Got 'listonchaintransactions' request from RPC thread");
                response_tx.send(onchain_txs_list_from_outpoints(
                    &revaultd.read().unwrap(),
                    &bitcoind_tx,
                    outpoints,
                )?)?;
            }
        }
    }

    Ok(())
}
