//! Stable logical entity identifiers (ADR-0001, `RtsEngineDoc.md` §6.2).
//!
//! An [`EntityId`] is a logical `(index, generation)` pair, never an ECS
//! archetype location or a raw memory index, so it stays stable across normal
//! storage changes. [`EntityAllocator`] hands out IDs, recycles slots with a
//! bumped generation, and rejects stale IDs with structured errors instead of
//! letting them resolve to a newly created entity.

use crate::hash::{CanonicalHasher, CanonicalSerialize};

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
        slot.generation += 1;
        self.free.push(id.index);
        self.alive -= 1;
        Ok(())
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
}

impl Default for EntityAllocator {
    fn default() -> Self {
        Self::new()
    }
}

/// Canonical form: slots in index order (alive flag + generation), then the
/// free list in stack order. The hash depends on logical slot state, never on
/// storage layout or allocation history beyond what the slots record.
impl CanonicalSerialize for EntityAllocator {
    fn write_canonical(&self, hasher: &mut CanonicalHasher) {
        hasher.write_u64(self.slots.len() as u64);
        hasher.write_u64(self.alive);
        for slot in &self.slots {
            hasher.write_bool(slot.alive);
            hasher.write_u64(slot.generation);
        }
        hasher.write_u64(self.free.len() as u64);
        for index in &self.free {
            hasher.write_u32(*index);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
