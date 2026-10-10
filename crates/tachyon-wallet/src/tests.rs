use anyhow::Result;
use ff::PrimeField;
use pasta_curves::Fp;
use primitives::transaction::Transaction;
use protocol::{consensus::BranchId, value::Zatoshis};
use sha2::{Digest, Sha256};
use transparent::bundle::{OutPoint, TxOut};
use zcash_tachyon::{
    Anchor, EpochIndex, TachyonBundle, constants::EPOCH_SIZE, keys::private::SpendingKey,
};

use crate::{
    chain::{self, Block, Stamp},
    note::{self, Receipt},
    transaction::{self, FEE, Funding, Payment},
};

pub(crate) const SEED: [u8; 32] = [7; 32];
pub(crate) const NETWORK: &str = "test-network";

pub(crate) fn genesis() -> Block {
    let anchor = chain::anchor_hex(Anchor::default());
    Block {
        hash: hex::encode([1; 32]),
        previous_block_hash: hex::encode([0; 32]),
        height: 8,
        activation_height: 8,
        pool_height: 0,
        epoch: 0,
        epoch_length: EPOCH_SIZE,
        anchor_before: anchor.clone(),
        epoch_start_anchor: Some(anchor.clone()),
        anchor_after: anchor,
        stamps: vec![],
        transactions: vec![],
    }
}

pub(crate) fn mine(blocks: &mut Vec<Block>, tx: Option<&Transaction>) -> Result<()> {
    let parent = blocks.last().unwrap();
    let height = parent.height + 1;
    let pool_height = height - 8;
    let epoch = pool_height / EPOCH_SIZE;
    let before = chain::anchor(&parent.anchor_after)?;
    let entry = if epoch != parent.epoch {
        before.next_epoch(EpochIndex::new(epoch))?
    } else {
        before
    };
    let mut after = entry;
    let mut stamps = Vec::new();
    if let Some(tx) = tx {
        let TachyonBundle::Proven(bundle) = tx.tachyon_bundle() else {
            panic!("expected proven bundle")
        };
        after = entry.next_stamp(EpochIndex::new(epoch), &bundle.stamp.tachygram_set)?;
        let mut commitment = Vec::new();
        bundle.stamp.tachygram_set.write(&mut commitment)?;
        stamps.push(Stamp {
            transaction_index: 0,
            txid: tx.txid().to_string(),
            tachygram_set: hex::encode(commitment),
            tachygrams: bundle
                .stamp
                .tachygrams
                .iter()
                .map(|t| hex::encode(Fp::from(*t).to_repr()))
                .collect(),
        });
    }
    let block = Block {
        hash: hex::encode(Sha256::digest(height.to_le_bytes())),
        previous_block_hash: parent.hash.clone(),
        height,
        activation_height: 8,
        pool_height,
        epoch,
        epoch_length: EPOCH_SIZE,
        anchor_before: parent.anchor_after.clone(),
        epoch_start_anchor: (epoch != parent.epoch).then(|| chain::anchor_hex(entry)),
        anchor_after: chain::anchor_hex(after),
        stamps,
        transactions: tx.map(|t| vec![t.txid().to_string()]).unwrap_or_default(),
    };
    block.validate(Some(parent), 8)?;
    blocks.push(block);
    Ok(())
}

pub(crate) fn own(value: u64) -> Receipt {
    Receipt::new(NETWORK, &note::address(&SpendingKey::from(SEED)), value).unwrap()
}

pub(crate) fn shield(blocks: &[Block], receipt: &Receipt) -> Result<Transaction> {
    transaction::build(Payment {
        seed: SEED,
        blocks,
        spend: None,
        funding: Some(Funding {
            outpoint: OutPoint::new([2; 32], 0),
            output: TxOut::new(
                Zatoshis::from_u64(receipt.value_zat + FEE)?,
                transaction::transparent_script(&transaction::funding_address(SEED)?)?,
            ),
        }),
        outputs: std::slice::from_ref(receipt),
        transparent_output: None,
        expiry: blocks.last().unwrap().height + 20,
    })
}

#[test]
fn receipts_are_canonical_and_bound_to_owner_and_network() -> Result<()> {
    let receipt = own(100_000);
    receipt.check_owner(&SpendingKey::from(SEED), NETWORK)?;
    assert!(
        receipt
            .check_owner(&SpendingKey::from([8; 32]), NETWORK)
            .is_err()
    );
    assert!(
        receipt
            .check_owner(&SpendingKey::from(SEED), "other-chain")
            .is_err()
    );
    let mut broken = receipt.clone();
    broken.rcm = hex::encode([0; 32]);
    assert!(broken.note().is_err());
    broken.rcm = hex::encode([255; 32]);
    assert!(broken.note().is_err());
    assert!(Receipt::new(NETWORK, "tachyon-test:deadbeef", 1).is_err());
    assert!(Receipt::new(NETWORK, &note::address(&SpendingKey::from(SEED)), 0).is_err());
    Ok(())
}

#[test]
fn chain_rejects_corrupt_inputs_and_discontinuities() -> Result<()> {
    let mut blocks = vec![genesis()];
    blocks[0].validate(None, 8)?;
    let tx = shield(&blocks, &own(1_000_000))?;
    mine(&mut blocks, Some(&tx))?;
    let mut bad = blocks[1].clone();
    bad.stamps[0].tachygrams.pop();
    assert!(bad.validate(Some(&blocks[0]), 8).is_err());
    let mut bad = blocks[1].clone();
    bad.previous_block_hash = hex::encode([9; 32]);
    assert!(bad.validate(Some(&blocks[0]), 8).is_err());
    let mut bad = blocks[1].clone();
    bad.epoch_start_anchor = Some(bad.anchor_before.clone());
    assert!(bad.validate(Some(&blocks[0]), 8).is_err());
    let mut bad = blocks[1].clone();
    bad.epoch_length /= 2;
    assert!(bad.validate(Some(&blocks[0]), 8).is_err());
    Ok(())
}

#[test]
fn shield_transfer_change_and_unshield_roundtrip_with_valid_authorization() -> Result<()> {
    let mut blocks = vec![genesis()];
    let funded = own(1_000_000);
    let tx = shield(&blocks, &funded)?;
    transaction::verify_for_test(
        &tx,
        Some(TxOut::new(
            Zatoshis::from_u64(1_000_000 + FEE)?,
            transaction::transparent_script(&transaction::funding_address(SEED)?)?,
        )),
    )?;
    mine(&mut blocks, Some(&tx))?;
    // A later unrelated stamp forces the spending stamp to lift within the active epoch.
    let unrelated = shield(&blocks, &own(200_000))?;
    mine(&mut blocks, Some(&unrelated))?;
    let recipient = Receipt::new(
        NETWORK,
        &note::address(&SpendingKey::from([8; 32])),
        300_000,
    )?;
    let change = own(650_000);
    let sent = transaction::build(Payment {
        seed: SEED,
        blocks: &blocks,
        spend: Some(&funded),
        funding: None,
        outputs: &[recipient, change.clone()],
        transparent_output: None,
        expiry: 30,
    })?;
    transaction::verify_for_test(&sent, None)?;
    let TachyonBundle::Proven(bundle) = sent.tachyon_bundle() else {
        panic!()
    };
    assert_eq!(bundle.descriptors().len(), 3); // Spend + recipient + change, not output-only.
    assert_eq!(
        bundle.stamp.anchor,
        chain::anchor(&blocks.last().unwrap().anchor_after)?
    );
    mine(&mut blocks, Some(&sent))?;
    let unshield = transaction::build(Payment {
        seed: SEED,
        blocks: &blocks,
        spend: Some(&change),
        funding: None,
        outputs: &[],
        transparent_output: Some((transaction::funding_address(SEED)?, 600_000)),
        expiry: 30,
    })?;
    transaction::verify_for_test(&unshield, None)?;
    assert_eq!(
        unshield.transparent_bundle().unwrap().vout[0]
            .value()
            .into_u64(),
        600_000
    );
    let mut bytes = Vec::new();
    unshield.write(&mut bytes)?;
    let decoded = Transaction::read(&bytes[..], BranchId::NuTachyon)?;
    assert_eq!(decoded.txid(), unshield.txid());
    transaction::verify_for_test(&decoded, None)?;
    Ok(())
}

#[test]
fn old_note_spends_after_populated_and_empty_closed_epochs() -> Result<()> {
    let mut blocks = vec![genesis()];
    let receipt = own(1_000_000);
    let tx = shield(&blocks, &receipt)?;
    mine(&mut blocks, Some(&tx))?;
    // Epoch zero contains the minted note; epoch one has no stamps at all.
    while blocks.last().unwrap().epoch < 2 {
        mine(&mut blocks, None)?;
    }
    let sent = transaction::build(Payment {
        seed: SEED,
        blocks: &blocks,
        spend: Some(&receipt),
        funding: None,
        outputs: &[own(950_000)],
        transparent_output: None,
        expiry: blocks.last().unwrap().height + 20,
    })?;
    transaction::verify_for_test(&sent, None)?;
    Ok(())
}

#[test]
fn spent_note_cannot_produce_closed_epoch_absence_evidence() -> Result<()> {
    let mut blocks = vec![genesis()];
    let receipt = own(1_000_000);
    let funded = shield(&blocks, &receipt)?;
    mine(&mut blocks, Some(&funded))?;
    let sent = transaction::build(Payment {
        seed: SEED,
        blocks: &blocks,
        spend: Some(&receipt),
        funding: None,
        outputs: &[own(950_000)],
        transparent_output: None,
        expiry: 30,
    })?;
    mine(&mut blocks, Some(&sent))?;
    while blocks.last().unwrap().epoch < 1 {
        mine(&mut blocks, None)?;
    }
    assert!(crate::proofs::spendable(&blocks, &SpendingKey::from(SEED), receipt.note()?).is_err());
    Ok(())
}
