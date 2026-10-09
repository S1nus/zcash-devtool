//! Rebuild spendability from durable public inputs, including closed/empty epochs.
//! Proofs are deliberately ephemeral; restarting never depends on serializing mock PCDs.

use anyhow::{Context, Result, ensure};
use pasta_curves::Fp;
use ragu::{Pcd, Proof};
use zcash_tachyon::{
    Anchor, EpochIndex, Note, QrDiscriminant, Tachygram,
    keys::private::SpendingKey,
    nullifier::NF_DERIVATION_WIDTH,
    stamp::proof::{PROOF_SYSTEM, delegation, pool, qr, spendable},
    witness,
};

use crate::chain::{Block, MAX_EPOCH_MEMBERS, anchor};

fn trivial() -> Pcd<()> {
    Proof::trivial().carry(())
}

pub fn secret(key: &SpendingKey, note: Note) -> Result<Pcd<delegation::NoteSecret>> {
    Ok(PROOF_SYSTEM
        .seed(
            &mut rand::rng(),
            delegation::NoteSeed,
            witness::note_seed(((), ()), note, key.derive_proof_private()),
        )?
        .0)
}

fn derivation(
    secret: &Pcd<delegation::NoteSecret>,
    epoch: u32,
) -> Result<Pcd<delegation::NoteNullifiers>> {
    let width = u32::try_from(NF_DERIVATION_WIDTH)?;
    let start = EpochIndex::new(epoch - epoch % width);
    Ok(PROOF_SYSTEM
        .fuse(
            &mut rand::rng(),
            delegation::NullifierDerive,
            witness::nullifier_derive((*secret.data(), ()), start),
            secret.clone(),
            trivial(),
        )?
        .0)
}

fn bucket(blocks: &[Block], epoch: u32) -> Result<(Pcd<qr::QrBucket>, Vec<Tachygram>)> {
    let epoch_blocks: Vec<_> = blocks.iter().filter(|b| b.epoch == epoch).collect();
    let first = *epoch_blocks.first().context("missing epoch inputs")?;
    let next = blocks
        .iter()
        .find(|b| b.epoch == epoch + 1)
        .context("epoch has not closed")?;
    let epoch_index = EpochIndex::new(epoch);
    let discriminant = QrDiscriminant(Fp::from(1));
    let mut members = Vec::new();
    let mut intake: Option<Pcd<qr::QrIntake>> = None;
    for block in epoch_blocks {
        let mut cursor = block.entry()?;
        for stamp in &block.stamps {
            let grams = stamp.members()?;
            ensure!(
                members.len() + grams.len() <= MAX_EPOCH_MEMBERS,
                "epoch exceeds experimental wallet capacity; a partitioned proof service is needed"
            );
            let (right, ()) = PROOF_SYSTEM.seed(
                &mut rand::rng(),
                qr::QrStampIntakeSeed,
                witness::qr_stamp_intake_seed(((), ()), cursor, epoch_index, discriminant, &grams),
            )?;
            intake = Some(match intake {
                None => right,
                Some(left) => {
                    PROOF_SYSTEM
                        .fuse(
                            &mut rand::rng(),
                            qr::QrIntakeMerge,
                            witness::qr_intake_merge(
                                (*left.data(), *right.data()),
                                &members,
                                &grams,
                            ),
                            left,
                            right,
                        )?
                        .0
                }
            });
            members.extend(grams);
            cursor = cursor.next_stamp(epoch_index, &stamp.commitment()?)?;
        }
    }
    let intake = match intake {
        Some(intake) => intake,
        None => {
            PROOF_SYSTEM
                .seed(
                    &mut rand::rng(),
                    qr::QrEmptyIntakeSeed,
                    witness::qr_empty_intake_seed(
                        ((), ()),
                        first.entry()?,
                        epoch_index,
                        discriminant,
                    ),
                )?
                .0
        }
    };
    let predecessor = if epoch == 0 {
        Anchor(Fp::from(0))
    } else {
        anchor(&first.anchor_before)?
    };
    let (bucket, ()) = PROOF_SYSTEM.fuse(
        &mut rand::rng(),
        qr::QrBucketSeal,
        witness::qr_bucket_seal((*intake.data(), ()), predecessor),
        intake,
        trivial(),
    )?;
    ensure!(
        bucket.data().2 == next.entry()?,
        "epoch evidence does not meet the next entry anchor"
    );
    Ok((bucket, members))
}

pub fn spendable(
    blocks: &[Block],
    key: &SpendingKey,
    note: Note,
) -> Result<Pcd<spendable::NoteSpendable>> {
    let cm = Tachygram::from(note.commitment());
    let current = blocks.last().context("wallet has not synced")?.epoch;
    let mut creation = None;
    for block in blocks {
        let mut cursor = block.entry()?;
        for stamp in &block.stamps {
            let members = stamp.members()?;
            if members.contains(&cm) {
                creation = Some((block.epoch, cursor, members));
                break;
            }
            cursor = cursor.next_stamp(EpochIndex::new(block.epoch), &stamp.commitment()?)?;
        }
        if creation.is_some() {
            break;
        }
    }
    let (created, before, members) = creation.context("note commitment has not been mined")?;
    let secret = secret(key, note)?;
    if created == current {
        let derived = derivation(&secret, current)?;
        return Ok(PROOF_SYSTEM
            .fuse(
                &mut rand::rng(),
                spendable::SpendableInit,
                witness::spendable_init(
                    (*derived.data(), ()),
                    before,
                    &members,
                    EpochIndex::new(current),
                ),
                derived,
                trivial(),
            )?
            .0);
    }
    let master = key.derive_nullifier_private().derive_note_private(note.psi);
    let mut lineage = None;
    for epoch in created..current {
        let (bucket, members) = bucket(blocks, epoch)?;
        let nf = master.derive_nullifier(EpochIndex::new(epoch));
        let (arbitrary, ()) = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            qr::QrUnspentInit,
            witness::qr_unspent_init((*bucket.data(), ()), nf.into(), &members),
            bucket.clone(),
            trivial(),
        )?;
        let derived = derivation(&secret, epoch)?;
        let (_, first, _, last) = *derived.data();
        let window: Vec<_> = (u32::from(first)..=u32::from(last))
            .map(|e| master.derive_nullifier(EpochIndex::new(e)))
            .collect();
        let (unspent, ()) = PROOF_SYSTEM.fuse(
            &mut rand::rng(),
            pool::UnspentBind,
            witness::unspent_bind((*arbitrary.data(), *derived.data()), &window, &[nf]),
            arbitrary,
            derived,
        )?;
        lineage = Some(match lineage {
            None => {
                PROOF_SYSTEM
                    .fuse(
                        &mut rand::rng(),
                        spendable::QrSpendableInit,
                        witness::qr_spendable_init((*unspent.data(), *bucket.data()), &members),
                        unspent,
                        bucket,
                    )?
                    .0
            }
            Some(left) => {
                PROOF_SYSTEM
                    .fuse(
                        &mut rand::rng(),
                        spendable::SpendableLift,
                        (),
                        left,
                        unspent,
                    )?
                    .0
            }
        });
    }
    lineage.context("closed creation epoch must produce a spendable proof")
}

pub fn lift_stamp(
    blocks: &[Block],
    stamp: zcash_tachyon::ProofStamp,
    descriptors: &std::collections::BTreeSet<zcash_tachyon::action::Descriptor>,
) -> Result<zcash_tachyon::ProofStamp> {
    let tip = blocks.last().context("wallet has not synced")?;
    let target = anchor(&tip.anchor_after)?;
    if stamp.anchor == target {
        return Ok(stamp);
    }
    let mut cursor = stamp.anchor;
    let mut links = Vec::new();
    for block in blocks.iter().filter(|b| b.epoch == tip.epoch) {
        let mut before = block.entry()?;
        for published in &block.stamps {
            let commitment = published.commitment()?;
            let after = before.next_stamp(EpochIndex::new(tip.epoch), &commitment)?;
            if before == cursor {
                links.push((before, EpochIndex::new(tip.epoch), commitment));
                cursor = after;
            }
            before = after;
        }
    }
    ensure!(cursor == target, "cannot lift the stamp to the synced tip");
    Ok(stamp.lift(&mut rand::rng(), descriptors, links)?)
}
