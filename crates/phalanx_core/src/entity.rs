//! Stable logical entity identifiers (ADR-0001, `RtsEngineDoc.md` §6.2).
//!
//! An [`EntityId`] is a logical `(index, generation)` pair, never an ECS
//! archetype location or a raw memory index, so it stays stable across normal
//! storage changes. [`EntityAllocator`] hands out IDs, recycles slots with a
//! bumped generation, and rejects stale IDs with structured errors instead of
//! letting them resolve to a newly created entity. The allocator supports
//! validated, atomic snapshot/restore (issue #45): a snapshot captures
//! everything that affects future allocation (slot generations, alive flags,
//! free-list stack order, live count), and restore rejects inconsistent input
//! without touching the allocator. When a slot's generation reaches `u64::MAX`
//! it is permanently retired instead of wrapping around, so a stale ID can
//! never become valid again through generation overflow (issue #44).

use crate::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink,
    canonical_decode, canonical_encode,
};

/// A stable logical entity identifier.
///
/// The `index` selects a slot in the allocator; the `generation` distinguishes
/// the current occupant of that slot from all previous occupants. Two IDs are
/// equal only if both parts match.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct EntityId {
    index: u32,
    generation: u64,
}

impl EntityId {
    /// Reconstructs an ID from its raw parts, for save/load restoration.
    /// Callers must validate the result against the allocator before use.
    #[must_use]
    pub const fn from_raw_parts(index: u32, generation: u64) -> Self {
        Self { index, generation }
    }

    /// Returns the slot index. This is not a memory address and must not be
    /// exposed as a persistent identifier outside this type.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.index
    }

    /// Returns the slot generation.
    #[must_use]
    pub const fn generation(self) -> u64 {
        self.generation
    }
}

/// A stable squad identifier (issue #110): assigned monotonically by the
/// battle world and never reused, so a stale [`SquadId`] can never resolve to
/// a different squad (unlike entity slots, squads are not recycled).
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SquadId(pub u32);

impl SquadId {
    /// Returns the raw squad index.
    #[must_use]
    pub const fn index(self) -> u32 {
        self.0
    }
}

impl CanonicalSerialize for SquadId {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.0);
    }
}

impl CanonicalDecode for SquadId {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self(reader.read_u32()?))
    }
}

/// Errors produced when validating or releasing an [`EntityId`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntityError {
    /// The index does not refer to any slot.
    UnknownSlot { index: u32 },
    /// The slot exists but the ID's generation does not match the current
    /// occupant, so the ID is stale (already released or superseded).
    StaleId {
        index: u32,
        expected: u64,
        found: u64,
    },
    /// The allocator cannot grow further because the slot index space is
    /// exhausted.
    SpaceExhausted,
    /// The slot's generation is at u64::MAX, so releasing cannot bump it
    /// further. The slot is retired permanently: it is marked dead, the live
    /// count is decremented, but it is NOT returned to the free list, so no
    /// wrapped generation can ever make a stale ID valid again (issue #44).
    GenerationExhausted { index: u32 },
}

impl std::fmt::Display for EntityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownSlot { index } => write!(formatter, "unknown entity slot {index}"),
            Self::StaleId {
                index,
                expected,
                found,
            } => write!(
                formatter,
                "stale entity id for slot {index}: generation {found} does not match current generation {expected}"
            ),
            Self::SpaceExhausted => formatter.write_str("entity slot index space exhausted"),
            Self::GenerationExhausted { index } => write!(
                formatter,
                "entity slot {index} generation exhausted; the slot is permanently retired"
            ),
        }
    }
}

impl std::error::Error for EntityError {}

#[derive(Debug)]
struct Slot {
    generation: u64,
    alive: bool,
}

/// Allocates and recycles [`EntityId`]s.
///
/// Releasing an ID bumps the slot's generation immediately, so any stale copy
/// of that ID fails validation; the slot is then reused by the next allocation
/// with the new generation.
#[derive(Debug)]
pub struct EntityAllocator {
    slots: Vec<Slot>,
    free: Vec<u32>,
    alive: u64,
}

impl EntityAllocator {
    /// Creates an empty allocator.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: Vec::new(),
            free: Vec::new(),
            alive: 0,
        }
    }

    /// Allocates a new entity ID, reusing a released slot when available.
    pub fn allocate(&mut self) -> Result<EntityId, EntityError> {
        if let Some(index) = self.free.pop() {
            let slot = &mut self.slots[index as usize];
            slot.alive = true;
            self.alive += 1;
            return Ok(EntityId {
                index,
                generation: slot.generation,
            });
        }

        let index = u32::try_from(self.slots.len()).map_err(|_| EntityError::SpaceExhausted)?;
        self.slots.push(Slot {
            generation: 1,
            alive: true,
        });
        self.alive += 1;
        Ok(EntityId {
            index,
            generation: 1,
        })
    }

    /// Releases an entity ID.
    ///
    /// The slot's generation is bumped immediately, so stale copies of `id`
    /// can never resolve to the entity that later reuses the slot. Releasing
    /// an unknown or stale ID is an explicit error, not a silent no-op.
    ///
    /// If the slot's generation is already `u64::MAX` it cannot be bumped
    /// further: the release still marks the slot dead and decrements the live
    /// count, but returns [`EntityError::GenerationExhausted`] and does NOT
    /// return the slot to the free list, permanently retiring the slot so no
    /// wrapped generation can resurrect a stale ID (issue #44).
    pub fn release(&mut self, id: EntityId) -> Result<(), EntityError> {
        let slot = self
            .slots
            .get_mut(id.index as usize)
            .ok_or(EntityError::UnknownSlot { index: id.index })?;
        if !slot.alive || slot.generation != id.generation {
            return Err(EntityError::StaleId {
                index: id.index,
                expected: slot.generation,
                found: id.generation,
            });
        }
        slot.alive = false;
        self.alive -= 1;
        match slot.generation.checked_add(1) {
            Some(next) => {
                slot.generation = next;
                self.free.push(id.index);
                Ok(())
            }
            None => {
                // Generation exhaustion (issue #44): the slot stays dead with
                // generation u64::MAX and is never recycled, so a stale ID can
                // never become valid again through wrap-around.
                Err(EntityError::GenerationExhausted { index: id.index })
            }
        }
    }

    /// Returns whether `id` currently refers to a live entity.
    #[must_use]
    pub fn contains(&self, id: EntityId) -> bool {
        self.slots
            .get(id.index as usize)
            .is_some_and(|slot| slot.alive && slot.generation == id.generation)
    }

    /// Returns the number of live entity IDs.
    #[must_use]
    pub const fn alive_count(&self) -> u64 {
        self.alive
    }

    /// Returns the total number of slots, live or recycled.
    #[must_use]
    pub fn slot_count(&self) -> usize {
        self.slots.len()
    }

    /// Encodes a full snapshot of the allocator: every slot (alive flag +
    /// generation) in index order, the free list in stack order, and the live
    /// count — everything that determines future allocation behavior
    /// (issue #45).
    #[must_use]
    pub fn snapshot(&self) -> Vec<u8> {
        canonical_encode(self)
    }

    /// Restores from a snapshot produced by [`EntityAllocator::snapshot`].
    ///
    /// The encoding is validated (slot/free-list consistency, alive count,
    /// and generation sanity) before anything is assigned, so restore is
    /// atomic: on any error the allocator is left unchanged.
    pub fn restore(&mut self, bytes: &[u8]) -> Result<(), CanonicalError> {
        let restored = canonical_decode::<Self>(bytes)?;
        *self = restored;
        Ok(())
    }
}

impl Default for EntityAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl CanonicalSerialize for EntityId {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.index);
        sink.write_u64(self.generation);
    }
}

impl CanonicalDecode for EntityId {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let index = reader.read_u32()?;
        let generation = reader.read_u64()?;
        Ok(Self { index, generation })
    }
}

/// Canonical form: slots in index order (alive flag + generation), then the
/// free list in stack order. The hash depends on logical slot state, never on
/// storage layout or allocation history beyond what the slots record.
impl CanonicalSerialize for EntityAllocator {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u64(self.slots.len() as u64);
        sink.write_u64(self.alive);
        for slot in &self.slots {
            sink.write_bool(slot.alive);
            sink.write_u64(slot.generation);
        }
        sink.write_u64(self.free.len() as u64);
        for index in &self.free {
            sink.write_u32(*index);
        }
    }
}

impl CanonicalDecode for EntityAllocator {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        let slot_count = reader.read_u64()?;
        let slot_count =
            usize::try_from(slot_count).map_err(|_| CanonicalError::InvalidLength(slot_count))?;
        let alive = reader.read_u64()?;
        let mut slots = Vec::with_capacity(slot_count.min(reader.remaining() / 9));
        for _ in 0..slot_count {
            let slot_alive = reader.read_bool()?;
            let generation = reader.read_u64()?;
            slots.push(Slot {
                generation,
                alive: slot_alive,
            });
        }
        let free_count = reader.read_u64()?;
        let free_count =
            usize::try_from(free_count).map_err(|_| CanonicalError::InvalidLength(free_count))?;
        let mut free = Vec::with_capacity(free_count.min(reader.remaining() / 4));
        for _ in 0..free_count {
            free.push(reader.read_u32()?);
        }

        // Structural validation at the boundary: the alive count must match
        // the slots, and the free list must be exactly the dead slots.
        let live = slots.iter().filter(|slot| slot.alive).count() as u64;
        if live != alive {
            return Err(CanonicalError::InvalidState(
                "alive count does not match slots",
            ));
        }
        let mut seen = vec![false; slots.len()];
        for index in &free {
            let slot = slots
                .get(*index as usize)
                .ok_or(CanonicalError::InvalidState(
                    "free list references unknown slot",
                ))?;
            if slot.alive || seen[*index as usize] {
                return Err(CanonicalError::InvalidState("invalid free list entry"));
            }
            seen[*index as usize] = true;
        }
        // Dead slots not in the free list are permanently retired
        // (generation-exhausted, issue #44) and must be at generation u64::MAX.
        for (index, slot) in slots.iter().enumerate() {
            if !slot.alive && !seen[index] && slot.generation != u64::MAX {
                return Err(CanonicalError::InvalidState(
                    "dead slot outside the free list is not generation-exhausted",
                ));
            }
        }
        if free.len() as u64 + alive > slots.len() as u64 {
            return Err(CanonicalError::InvalidState(
                "free list size does not match slots",
            ));
        }
        // Generation sanity: live slots start at generation 1, released slots
        // are bumped on release, so a dead slot is always at generation >= 2.
        for slot in &slots {
            let minimum = if slot.alive { 1 } else { 2 };
            if slot.generation < minimum {
                return Err(CanonicalError::InvalidState(
                    "slot generation below its minimum",
                ));
            }
        }
        Ok(Self { slots, free, alive })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::{SnapshotSink, canonical_digest};

    #[test]
    fn ids_are_stable_while_alive_regardless_of_other_allocations() {
        let mut allocator = EntityAllocator::new();
        let first = allocator.allocate().expect("allocate");
        for _ in 0..100 {
            allocator.allocate().expect("allocate");
        }
        assert!(allocator.contains(first));
        assert_eq!(allocator.alive_count(), 101);
        assert_eq!(
            first,
            EntityId::from_raw_parts(first.index(), first.generation())
        );
    }

    #[test]
    fn released_slot_is_reused_with_a_new_generation() {
        let mut allocator = EntityAllocator::new();
        let first = allocator.allocate().expect("allocate");
        allocator.release(first).expect("release");
        assert!(!allocator.contains(first));

        let second = allocator.allocate().expect("reuse slot");
        assert_eq!(second.index(), first.index());
        assert_ne!(second.generation(), first.generation());
        assert!(allocator.contains(second));
        assert!(!allocator.contains(first));
        assert_eq!(allocator.alive_count(), 1);
        assert_eq!(allocator.slot_count(), 1);
    }

    #[test]
    fn stale_ids_are_rejected_not_resolved_to_new_entities() {
        let mut allocator = EntityAllocator::new();
        let first = allocator.allocate().expect("allocate");
        allocator.release(first).expect("release");
        let second = allocator.allocate().expect("reuse slot");

        assert_eq!(
            allocator.release(first),
            Err(EntityError::StaleId {
                index: first.index(),
                expected: second.generation(),
                found: first.generation(),
            })
        );
        assert!(allocator.contains(second));
    }

    #[test]
    fn unknown_slots_are_rejected() {
        let mut allocator = EntityAllocator::new();
        let id = EntityId {
            index: 7,
            generation: 1,
        };
        assert_eq!(
            allocator.release(id),
            Err(EntityError::UnknownSlot { index: 7 })
        );
        assert!(!allocator.contains(id));
    }

    #[test]
    fn double_release_is_an_error() {
        let mut allocator = EntityAllocator::new();
        let id = allocator.allocate().expect("allocate");
        allocator.release(id).expect("first release");
        assert!(matches!(
            allocator.release(id),
            Err(EntityError::StaleId { .. })
        ));
        assert_eq!(allocator.alive_count(), 0);
    }

    #[test]
    fn default_allocator_is_empty() {
        let allocator = EntityAllocator::default();
        assert_eq!(allocator.alive_count(), 0);
        assert_eq!(allocator.slot_count(), 0);
    }

    #[test]
    fn snapshot_restore_round_trip_preserves_future_allocations() {
        let mut allocator = EntityAllocator::new();
        // Build a specific slot/free-list state: allocate 5, release 2.
        let ids: Vec<EntityId> = (0..5)
            .map(|_| allocator.allocate().expect("allocate"))
            .collect();
        allocator.release(ids[1]).expect("release");
        allocator.release(ids[3]).expect("release");
        let snapshot = allocator.snapshot();
        let digest_before = canonical_digest(&allocator);

        // Mutate, then restore: the snapshot is the exact prior state.
        allocator.allocate().expect("allocate");
        allocator.release(ids[0]).expect("release");
        allocator.restore(&snapshot).expect("valid snapshot");
        assert_eq!(canonical_digest(&allocator), digest_before);

        // Identical subsequent allocate/release sequences produce identical
        // IDs on the restored allocator and on a fresh decode of the snapshot.
        let mut reference: EntityAllocator = canonical_decode(&snapshot).expect("decode snapshot");
        let sequence = |alloc: &mut EntityAllocator| {
            let mut out = Vec::new();
            out.push(alloc.allocate().expect("allocate"));
            out.push(alloc.allocate().expect("allocate"));
            alloc.release(ids[2]).expect("release");
            out.push(alloc.allocate().expect("allocate"));
            let first = out[0];
            alloc.release(first).expect("release");
            out.push(alloc.allocate().expect("allocate"));
            out
        };
        assert_eq!(sequence(&mut allocator), sequence(&mut reference));
        assert_eq!(canonical_digest(&allocator), canonical_digest(&reference));
    }

    #[test]
    fn restore_rejects_malformed_snapshots_and_leaves_allocator_unchanged() {
        let mut allocator = EntityAllocator::new();
        let id = allocator.allocate().expect("allocate");
        allocator.release(id).expect("release");
        let digest_before = canonical_digest(&allocator);
        let alive_before = allocator.alive_count();
        let snapshot = allocator.snapshot();

        // Truncated bytes.
        assert!(allocator.restore(&snapshot[..snapshot.len() / 2]).is_err());
        // Corrupted length prefix (huge slot count).
        let mut corrupted = snapshot.clone();
        corrupted[0] ^= 0xFF;
        assert!(allocator.restore(&corrupted).is_err());
        // Structurally valid encoding but inconsistent state: the free list
        // references a live slot (slot_count=1, alive=1, slot alive gen 1,
        // free_count=1, free=[0]).
        let mut sink = SnapshotSink::new();
        sink.write_u64(1);
        sink.write_u64(1);
        sink.write_bool(true);
        sink.write_u64(1);
        sink.write_u64(1);
        sink.write_u32(0);
        assert!(allocator.restore(&sink.bytes).is_err());

        // Failed restores left the allocator unchanged and functional.
        assert_eq!(canonical_digest(&allocator), digest_before);
        assert_eq!(allocator.alive_count(), alive_before);
        assert!(!allocator.contains(id));
        let next = allocator.allocate().expect("allocate");
        assert_eq!(next.index(), id.index());
        assert_ne!(next.generation(), id.generation());
    }

    #[test]
    fn release_at_max_generation_retires_the_slot_permanently() {
        let mut allocator = EntityAllocator::new();
        let id = allocator.allocate().expect("allocate");
        assert_eq!(id, EntityId::from_raw_parts(0, 1));
        // Simulate a slot whose generation has reached u64::MAX (issue #44).
        allocator.slots[0].generation = u64::MAX;
        let exhausted = EntityId::from_raw_parts(0, u64::MAX);

        // Releasing cannot bump the generation further: structured error, and
        // the slot is retired (dead, not returned to the free list).
        assert_eq!(
            allocator.release(exhausted),
            Err(EntityError::GenerationExhausted { index: 0 })
        );
        assert_eq!(allocator.alive_count(), 0);
        assert!(allocator.free.is_empty());
        assert!(!allocator.slots[0].alive);
        assert_eq!(allocator.slots[0].generation, u64::MAX);
        assert!(!allocator.contains(exhausted));

        // The stale ID is still rejected afterwards (no wrap-around
        // resurrection)...
        assert!(matches!(
            allocator.release(exhausted),
            Err(EntityError::StaleId { .. })
        ));
        // ...and the next allocation gets a FRESH slot instead of recycling
        // the retired one.
        let next = allocator.allocate().expect("allocate");
        assert_eq!(next.index(), 1);
        assert_eq!(next.generation(), 1);
        assert_eq!(allocator.slot_count(), 2);
        assert!(!allocator.contains(exhausted));

        // The retired slot round-trips through the canonical snapshot: decode
        // accepts a dead slot outside the free list only at generation
        // u64::MAX, and restore keeps the slot permanently out of the free list.
        let snapshot = allocator.snapshot();
        let digest_before = canonical_digest(&allocator);
        let mut reference: EntityAllocator = canonical_decode(&snapshot).expect("decode snapshot");
        assert_eq!(canonical_digest(&reference), digest_before);
        allocator.restore(&snapshot).expect("restore snapshot");
        assert_eq!(canonical_digest(&allocator), digest_before);
        let after = allocator.allocate().expect("allocate");
        assert_eq!(after.index(), 2);
        assert!(!allocator.contains(exhausted));
        let after_ref = reference.allocate().expect("allocate");
        assert_eq!(after_ref, after);
    }

    #[test]
    fn decode_rejects_dead_slots_outside_the_free_list_unless_retired() {
        // A dead slot that is not in the free list and not at u64::MAX is
        // inconsistent state, not a retired slot.
        let mut sink = SnapshotSink::new();
        sink.write_u64(1); // slot count
        sink.write_u64(0); // alive count
        sink.write_bool(false); // slot 0 dead
        sink.write_u64(5); // generation 5: bumped, but not exhausted
        sink.write_u64(0); // free count
        assert!(canonical_decode::<EntityAllocator>(&sink.bytes).is_err());

        // A dead slot at u64::MAX outside the free list is a valid retired
        // slot, and it is never recycled.
        let mut sink = SnapshotSink::new();
        sink.write_u64(1);
        sink.write_u64(0);
        sink.write_bool(false);
        sink.write_u64(u64::MAX);
        sink.write_u64(0);
        let mut restored: EntityAllocator =
            canonical_decode(&sink.bytes).expect("retired slot decodes");
        assert_eq!(restored.alive_count(), 0);
        assert_eq!(restored.slot_count(), 1);
        let next = restored.allocate().expect("allocate");
        assert_eq!(next.index(), 1);
        assert!(!restored.contains(EntityId::from_raw_parts(0, u64::MAX)));
    }
}
