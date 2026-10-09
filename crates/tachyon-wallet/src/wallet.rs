//! Scriptable, single-note development wallet commands. No background scheduler.

use crate::{
    chain::{Block, bytes32},
    note::{self, Receipt},
    rpc::Rpc,
    store::{Config, Pending, Store, write_private},
    transaction::{self, FEE, Funding, Payment},
};
use anyhow::{Context, Result, ensure};
use clap::{Args, Subcommand};
use ff::PrimeField;
use pasta_curves::Fp;
use primitives::transaction::Transaction;
use protocol::consensus::BranchId;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::PathBuf,
};
use zcash_tachyon::{EpochIndex, constants::EPOCH_SIZE, keys::private::SpendingKey};

#[derive(Debug, Args)]
pub struct Command {
    /// Separate, private directory for this experimental wallet (not the ordinary wallet DB).
    #[arg(long)]
    wallet_dir: PathBuf,
    #[command(subcommand)]
    command: Operation,
}

#[derive(Debug, Subcommand)]
enum Operation {
    /// Create a mock-proof wallet for a private development network. Keys are NOT encrypted.
    Init {
        #[arg(long)]
        rpc_url: String,
        #[arg(long)]
        activation_height: u32,
        #[arg(long)]
        cookie_file: Option<PathBuf>,
        /// Required acknowledgement: test funds only; neither proofs nor storage are production-safe.
        #[arg(long)]
        accept_mock_proofs: bool,
    },
    /// Print Tachyon payment and transparent funding addresses.
    Address,
    /// Show balances and note states from the last successful sync.
    Balance,
    /// Validate and persist chain inputs; repeat until caught_up is true.
    Sync {
        #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=10000))]
        max_blocks: u32,
    },
    /// Import a confidential payment receipt received out of band.
    Receive {
        #[arg(long)]
        receipt: PathBuf,
    },
    /// Shield an entire owned, confirmed P2PKH output, less the fee.
    Shield {
        #[arg(long)]
        txid: String,
        #[arg(long)]
        index: u32,
    },
    /// Spend one sufficiently large Tachyon note, returning change to this wallet.
    Send {
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount_zat: u64,
    },
    /// Spend one Tachyon note to a transparent testnet address, with Tachyon change.
    Unshield {
        #[arg(long)]
        to: String,
        #[arg(long)]
        amount_zat: u64,
    },
    /// Export the recipient's confidential note receipt, including after a failed broadcast.
    ExportReceipt {
        #[arg(long)]
        txid: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Retry broadcasting the exact stored transaction, without creating a replacement.
    Submit {
        #[arg(long)]
        txid: String,
    },
    /// List mined, pending, or expired transactions (receipts and keys are not printed).
    Transactions,
}

impl Command {
    pub fn run(self) -> Result<()> {
        let result = self.execute()?;
        println!("{}", serde_json::to_string(&result)?);
        Ok(())
    }

    fn execute(self) -> Result<Value> {
        if let Operation::Init {
            rpc_url,
            activation_height,
            cookie_file,
            accept_mock_proofs,
        } = self.command
        {
            ensure!(
                accept_mock_proofs,
                "init requires --accept-mock-proofs; NEVER use real funds"
            );
            ensure!(
                activation_height > 0,
                "NuTachyon must activate after genesis"
            );
            let rpc = Rpc::new(&rpc_url, cookie_file.clone())?;
            let info: Value = rpc.call("getblockchaininfo", json!([]))?;
            ensure!(
                matches!(info["chain"].as_str(), Some("test" | "regtest")),
                "only test/regtest networks are supported"
            );
            let genesis = rpc.hash(0)?;
            let block = rpc
                .block(activation_height)
                .context("node must have activated NuTachyon and expose gettachyonblock")?;
            block.validate(None, activation_height)?;
            let identity = Sha256::new()
                .chain_update(bytes32(&genesis)?)
                .chain_update(activation_height.to_le_bytes())
                .chain_update(bytes32(&block.hash)?)
                .finalize();
            let config = Config {
                version: 1,
                rpc_url,
                cookie_file,
                activation: activation_height,
                activation_hash: block.hash.clone(),
                genesis,
                network_id: hex::encode(identity),
                seed: note::random_seed(),
            };
            let store = Store::open(&self.wallet_dir, true)?;
            store.set_config(&config)?;
            store.put_block(&block)?;
            store.commit()?;
            return addresses(&config);
        }
        let store = Store::open(&self.wallet_dir, false)?;
        let config = store.config()?;
        let key = SpendingKey::from(config.seed);
        let result = match self.command {
            Operation::Init { .. } => unreachable!("init returned above"),
            Operation::Address => addresses(&config)?,
            Operation::Balance => {
                let blocks = store.blocks()?;
                let states = note_states(&store, &config, &blocks)?;
                let available = states
                    .iter()
                    .filter(|(_, s)| *s == "available")
                    .try_fold(0u64, |total, (r, _)| {
                        total.checked_add(r.value_zat).context("balance overflow")
                    })?;
                json!({"synced_height":blocks.last().map(|b| b.height), "available_zat":available,
                    "notes":states.into_iter().map(|(r,s)| Ok(json!({"id":r.id()?,"value_zat":r.value_zat,"status":s}))).collect::<Result<Vec<_>>>()?})
            }
            Operation::Sync { max_blocks } => sync(&store, &config, max_blocks)?,
            Operation::Receive { receipt } => {
                let mut data = Vec::new();
                std::fs::File::open(receipt)?
                    .take(16_385)
                    .read_to_end(&mut data)?;
                ensure!(data.len() <= 16_384, "receipt exceeds 16 KiB");
                let receipt: Receipt = serde_json::from_slice(&data)?;
                receipt.check_owner(&key, &config.network_id)?;
                store.put_note(&receipt)?;
                json!({"note_id":receipt.id()?, "status":"imported; sync to confirm"})
            }
            Operation::ExportReceipt { txid, out } => {
                let pending = find_pending(&store, &txid)?;
                let receipt = pending
                    .receipt
                    .context("this transaction has no recipient receipt")?;
                write_private(&out, &serde_json::to_vec_pretty(&receipt)?)?;
                json!({"txid":txid,"receipt":out})
            }
            Operation::Transactions => {
                let blocks = store.blocks()?;
                json!({"transactions":store.pending()?.into_iter().map(|p| {
                    json!({"txid":p.txid,"expiry_height":p.expiry,"status":pending_status(&p,&blocks)})
                }).collect::<Vec<_>>()})
            }
            Operation::Submit { txid } => {
                ensure!(
                    sync(&store, &config, 1000)?["caught_up"] == true,
                    "run sync until caught up before submitting"
                );
                let pending = find_pending(&store, &txid)?;
                let blocks = store.blocks()?;
                let status = pending_status(&pending, &blocks);
                ensure!(
                    status != "expired",
                    "transaction expired; create a new payment"
                );
                store.commit()?;
                if status != "mined" {
                    rpc(&config)?.submit(&txid, &pending.raw)?;
                }
                return Ok(
                    json!({"txid":txid,"status":if status == "mined" {"mined"} else {"submitted"}}),
                );
            }
            operation @ (Operation::Shield { .. }
            | Operation::Send { .. }
            | Operation::Unshield { .. }) => {
                return pay(&store, &config, operation);
            }
        };
        store.commit()?;
        Ok(result)
    }
}

fn rpc(config: &Config) -> Result<Rpc> {
    Rpc::new(&config.rpc_url, config.cookie_file.clone())
}
fn addresses(config: &Config) -> Result<Value> {
    Ok(
        json!({"network_id":config.network_id,"tachyon_address":note::address(&SpendingKey::from(config.seed)),
        "funding_address":transaction::funding_address(config.seed)?,"warning":"mock proofs; unencrypted keys; test funds only"}),
    )
}

fn sync(store: &Store, config: &Config, max_blocks: u32) -> Result<Value> {
    let rpc = rpc(config)?;
    ensure!(
        rpc.hash(0)? == config.genesis && rpc.hash(config.activation)? == config.activation_hash,
        "network identity changed (including possible regenesis); use a separate wallet directory"
    );
    let tip = rpc.height()?;
    let mut blocks = store.blocks()?;
    while let Some(last) = blocks.last() {
        if last.height <= tip && rpc.hash(last.height)? == last.hash {
            break;
        }
        blocks.pop();
    }
    let last = blocks
        .last()
        .context("activation block changed; reinitialize on the intended network")?;
    store.rewind(last.height)?;
    let stop = last.height.saturating_add(max_blocks).min(tip);
    for height in last.height.saturating_add(1)..=stop {
        let block = rpc.block(height)?;
        block.validate(blocks.last(), config.activation)?;
        store.put_block(&block)?;
        blocks.push(block);
    }
    let last = blocks.last().context("activation block exists")?;
    ensure!(
        rpc.hash(last.height)? == last.hash,
        "chain reorganized during sync; retry"
    );
    Ok(
        json!({"height":last.height,"hash":last.hash,"epoch":last.epoch,"caught_up":last.height == tip}),
    )
}

fn pending_status(pending: &Pending, blocks: &[Block]) -> &'static str {
    if blocks
        .iter()
        .any(|b| b.transactions.contains(&pending.txid))
    {
        "mined"
    } else if blocks.last().is_some_and(|b| b.height > pending.expiry) {
        "expired"
    } else {
        "pending"
    }
}

fn find_pending(store: &Store, txid: &str) -> Result<Pending> {
    store
        .pending()?
        .into_iter()
        .find(|p| p.txid == txid)
        .context("unknown wallet transaction")
}

fn note_states(
    store: &Store,
    config: &Config,
    blocks: &[Block],
) -> Result<Vec<(Receipt, &'static str)>> {
    let key = SpendingKey::from(config.seed);
    let reserved: BTreeSet<_> = store
        .pending()?
        .into_iter()
        .filter(|p| pending_status(p, blocks) != "expired")
        .map(|p| p.source)
        .collect();
    let mut epoch_members: BTreeMap<u32, BTreeSet<&str>> = BTreeMap::new();
    for block in blocks {
        let members = epoch_members.entry(block.epoch).or_default();
        members.extend(
            block
                .stamps
                .iter()
                .flat_map(|s| s.tachygrams.iter().map(String::as_str)),
        );
    }
    store
        .notes()?
        .into_iter()
        .map(|receipt| {
            receipt.check_owner(&key, &config.network_id)?;
            let id = receipt.id()?;
            let note = receipt.note()?;
            let master = key.derive_nullifier_private().derive_note_private(note.psi);
            let mined = epoch_members
                .values()
                .any(|members| members.contains(id.as_str()));
            let spent = epoch_members.iter().any(|(epoch, members)| {
                let nf = hex::encode(
                    Fp::from(master.derive_nullifier(EpochIndex::new(*epoch))).to_repr(),
                );
                members.contains(nf.as_str())
            });
            let status = if spent {
                "spent"
            } else if !mined {
                "unconfirmed"
            } else if reserved.contains(&format!("note:{id}")) {
                "reserved"
            } else {
                "available"
            };
            Ok((receipt, status))
        })
        .collect()
}

fn pay(store: &Store, config: &Config, operation: Operation) -> Result<Value> {
    match &operation {
        Operation::Send { to, .. } => {
            note::decode_address(to)?;
        }
        Operation::Unshield { to, .. } => {
            transaction::transparent_script(to)?;
        }
        _ => {}
    }
    ensure!(
        sync(store, config, 1000)?["caught_up"] == true,
        "run sync until caught up before creating transactions"
    );
    let blocks = store.blocks()?;
    let tip = blocks.last().context("wallet has not synced")?;
    let epoch_end = config
        .activation
        .checked_add(
            tip.epoch
                .checked_add(1)
                .and_then(|e| e.checked_mul(EPOCH_SIZE))
                .context("epoch overflow")?,
        )
        .and_then(|h| h.checked_sub(1))
        .context("height overflow")?;
    ensure!(
        tip.height < epoch_end,
        "wait for the next epoch's first block before sending"
    );
    let expiry = tip.height.saturating_add(20).min(epoch_end);
    let own_address = note::address(&SpendingKey::from(config.seed));
    let mut outputs = Vec::new();
    let mut spend = None;
    let mut funding = None;
    let mut unshield = None;
    let mut recipient = None;
    let source;
    let rpc = rpc(config)?;
    match operation {
        Operation::Shield { txid, index } => {
            bytes32(&txid)?;
            source = format!("outpoint:{txid}:{index}");
            ensure!(
                !store
                    .pending()?
                    .iter()
                    .any(|p| p.source == source && pending_status(p, &blocks) != "expired"),
                "funding output is already reserved"
            );
            let available: Value = rpc.call("gettxout", json!([txid, index, true]))?;
            ensure!(
                !available.is_null(),
                "funding output is unavailable or already spent"
            );
            let confirmations = available["confirmations"]
                .as_u64()
                .context("missing confirmations")?;
            let coinbase = available["coinbase"]
                .as_bool()
                .context("missing coinbase flag")?;
            ensure!(
                confirmations >= if coinbase { 100 } else { 1 },
                "funding output is not mature/confirmed"
            );
            let raw: String = rpc.call("getrawtransaction", json!([txid, 0]))?;
            let bytes = hex::decode(raw)?;
            let previous = Transaction::read(&bytes[..], BranchId::NuTachyon)?;
            ensure!(
                previous.txid().to_string() == txid,
                "funding transaction ID mismatch"
            );
            let output = previous
                .transparent_bundle()
                .and_then(|b| b.vout.get(usize::try_from(index).ok()?))
                .context("funding output missing")?
                .clone();
            let amount = output
                .value()
                .into_u64()
                .checked_sub(FEE)
                .context("funding output cannot pay fee")?;
            outputs.push(Receipt::new(&config.network_id, &own_address, amount)?);
            funding = Some(Funding {
                outpoint: transparent::bundle::OutPoint::new(*previous.txid().as_ref(), index),
                output,
            });
        }
        Operation::Send { to, amount_zat } | Operation::Unshield { to, amount_zat } => {
            ensure!(amount_zat > 0, "amount must be positive");
            let needed = amount_zat.checked_add(FEE).context("amount overflow")?;
            let selected = note_states(store, config, &blocks)?.into_iter().filter(|(r,s)| *s == "available" && r.value_zat >= needed)
                .min_by_key(|(r,_)| r.value_zat).map(|(r,_)| r).context("no single available note covers amount plus fee; this wallet does not consolidate notes")?;
            source = format!("note:{}", selected.id()?);
            // The command-specific address format was checked before accessing the node.
            if to.starts_with("tachyon-test:") {
                let receipt = Receipt::new(&config.network_id, &to, amount_zat)?;
                outputs.push(receipt.clone());
                recipient = Some(receipt);
            } else {
                transaction::transparent_script(&to)?;
                unshield = Some((to, amount_zat));
            }
            let change = selected.value_zat - needed;
            if change > 0 {
                outputs.push(Receipt::new(&config.network_id, &own_address, change)?);
            }
            spend = Some(selected);
        }
        _ => anyhow::bail!("not a payment operation"),
    }
    let tx = transaction::build(Payment {
        seed: config.seed,
        blocks: &blocks,
        spend: spend.as_ref(),
        funding,
        outputs: &outputs,
        unshield,
        expiry,
    })?;
    ensure!(
        rpc.hash(tip.height)? == tip.hash && rpc.height()? <= expiry,
        "chain changed or transaction expired during proving; sync and retry"
    );
    let txid = tx.txid().to_string();
    let mut raw = Vec::new();
    tx.write(&mut raw)?;
    for output in &outputs {
        if output.payment_key == note::payment_key(&SpendingKey::from(config.seed)) {
            store.put_note(output)?;
        }
    }
    let pending = Pending {
        txid: txid.clone(),
        raw: hex::encode(raw),
        expiry,
        source,
        receipt: recipient,
    };
    store.reserve(&pending)?;
    // A timeout is not a rejection: commit both change receipts and the reservation BEFORE broadcast.
    store.commit()?;
    rpc.submit(&txid, &pending.raw).with_context(|| {
        format!("transaction {txid} saved; use transactions/submit/export-receipt to recover")
    })?;
    Ok(
        json!({"txid":txid,"status":"submitted","fee_zat":FEE,"expiry_height":expiry,"has_receipt":pending.receipt.is_some()}),
    )
}

#[cfg(test)]
mod tests;
