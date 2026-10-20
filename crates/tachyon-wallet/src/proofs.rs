//! Rebuild spendability from durable public inputs, including closed/empty epochs.
//! Proofs are deliberately ephemeral; restarting never depends on serializing mock PCDs.

use anyhow::{Context, Result, ensure};
use ragu::{Pcd, Proof};
use zcash_tachyon::{
    EpochIndex, Note, Tachygram,
    keys::private::SpendingKey,
    nullifier::NF_DERIVATION_WIDTH,
    stamp::proof::{PROOF_SYSTEM, delegation, pool, qr, spendable},
    witness,
};

use crate::{
    chain::{Block, anchor},
    progress::{Step, StepId},
};

mod epoch;
use epoch::{Bucket, bucket};

fn trivial() -> Pcd<()> {
    Proof::trivial().carry(())
}

pub fn secret(key: &SpendingKey, note: Note) -> Result<Pcd<delegation::NoteSecret>> {
    Ok(secret_with_id(key, note)?.0)
}

fn secret_with_id(key: &SpendingKey, note: Note) -> Result<(Pcd<delegation::NoteSecret>, StepId)> {
    let step = Step::start("NoteSeed", &[]);
    let id = step.id();
    let proof = step
        .run(|| {
            PROOF_SYSTEM.seed(
                &mut rand::rng(),
                delegation::NoteSeed,
                witness::note_seed(((), ()), note, key.derive_proof_private()),
            )
        })?
        .0;
    Ok((proof, id))
}

fn derivation(
    secret: &Pcd<delegation::NoteSecret>,
    secret_id: StepId,
    epoch: u32,
) -> Result<(Pcd<delegation::NoteNullifiers>, StepId)> {
    let width = u32::try_from(NF_DERIVATION_WIDTH)?;
    let start = EpochIndex::new(epoch - epoch % width);
    let step = Step::start("NullifierDerive", &[secret_id]);
    let id = step.id();
    let proof = step
        .run(|| {
            PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                delegation::NullifierDerive,
                witness::nullifier_derive((*secret.data(), ()), start),
                secret.clone(),
                trivial(),
            )
        })?
        .0;
    Ok((proof, id))
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
    let (secret, secret_id) = secret_with_id(key, note)?;
    if created == current {
        let (derived, derived_id) = derivation(&secret, secret_id, current)?;
        let step = Step::start("SpendableInit", &[derived_id]);
        return Ok(step
            .run(|| {
                PROOF_SYSTEM.fuse(
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
                )
            })?
            .0);
    }
    let master = key.derive_nullifier_private().derive_note_private(note.psi);
    let mut lineage = None;
    for epoch in created..current {
        let nf = master.derive_nullifier(EpochIndex::new(epoch));
        let Bucket {
            proof: absence_bucket,
            members,
            step_id: absence_id,
        } = bucket(blocks, epoch, nf.into())?;
        let step = Step::start("QrUnspentInit", &[absence_id]);
        let arbitrary_id = step.id();
        let (arbitrary, ()) = step.run(|| {
            PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                qr::QrUnspentInit,
                witness::qr_unspent_init((*absence_bucket.data(), ()), nf.into(), &members),
                absence_bucket.clone(),
                trivial(),
            )
        })?;
        let (derived, derived_id) = derivation(&secret, secret_id, epoch)?;
        let (_, first, _, last) = *derived.data();
        let window: Vec<_> = (u32::from(first)..=u32::from(last))
            .map(|e| master.derive_nullifier(EpochIndex::new(e)))
            .collect();
        let step = Step::start("UnspentBind", &[arbitrary_id, derived_id]);
        let unspent_id = step.id();
        let (unspent, ()) = step.run(|| {
            PROOF_SYSTEM.fuse(
                &mut rand::rng(),
                pool::UnspentBind,
                witness::unspent_bind((*arbitrary.data(), *derived.data()), &window, &[nf]),
                arbitrary,
                derived,
            )
        })?;
        lineage = Some(match lineage {
            None => {
                // The note commitment need not route to its nullifier's bucket.
                // Both proofs cover the same complete epoch and anchor bounds.
                let Bucket {
                    proof: creation_bucket,
                    members,
                    step_id: creation_id,
                } = if members.contains(&cm) {
                    Bucket {
                        proof: absence_bucket,
                        members,
                        step_id: absence_id,
                    }
                } else {
                    bucket(blocks, epoch, cm)?
                };
                let step = Step::start("QrSpendableInit", &[unspent_id, creation_id]);
                let step_id = step.id();
                let proof = step
                    .run(|| {
                        PROOF_SYSTEM.fuse(
                            &mut rand::rng(),
                            spendable::QrSpendableInit,
                            witness::qr_spendable_init(
                                (*unspent.data(), *creation_bucket.data()),
                                &members,
                            ),
                            unspent,
                            creation_bucket,
                        )
                    })?
                    .0;
                (proof, step_id)
            }
            Some((left, left_id)) => {
                let step = Step::start("SpendableLift", &[left_id, unspent_id]);
                let step_id = step.id();
                let proof = step
                    .run(|| {
                        PROOF_SYSTEM.fuse(
                            &mut rand::rng(),
                            spendable::SpendableLift,
                            (),
                            left,
                            unspent,
                        )
                    })?
                    .0;
                (proof, step_id)
            }
        });
    }
    lineage
        .map(|(proof, _)| proof)
        .context("closed creation epoch must produce a spendable proof")
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
    let step = Step::start("ProofStamp::lift", &[]);
    Ok(step.run(|| stamp.lift(&mut rand::rng(), descriptors, links))?)
}
