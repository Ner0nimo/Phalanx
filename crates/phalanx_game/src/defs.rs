//! Data-driven unit and weapon definitions (issue #13;
//! `docs/vertical-slice-scenario.md` §3; `RtsEngineDoc.md` §6.3: definitions
//! are data, not code constants).

use std::collections::BTreeMap;

use phalanx_core::hash::{
    CanonicalDecode, CanonicalError, CanonicalReader, CanonicalSerialize, CanonicalSink, Digest,
    canonical_digest,
};
use phalanx_navigation::MovementProfile;

/// A weapon definition. Hitscan in the slice (projectiles are a later
/// contract extension).
#[derive(Clone, Debug, PartialEq)]
pub struct WeaponDef {
    /// Maximum range in meters.
    pub range_m: f32,
    /// Damage per hit before armor.
    pub damage: u32,
    /// Cooldown in ticks.
    pub cooldown_ticks: u32,
}

impl CanonicalSerialize for WeaponDef {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.range_m.to_bits());
        sink.write_u32(self.damage);
        sink.write_u32(self.cooldown_ticks);
    }
}

impl CanonicalDecode for WeaponDef {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            range_m: f32::from_bits(reader.read_u32()?),
            damage: reader.read_u32()?,
            cooldown_ticks: reader.read_u32()?,
        })
    }
}

/// A unit definition (placeholder balance values from the slice spec §3).
#[derive(Clone, Debug, PartialEq)]
pub struct UnitDef {
    /// Definition id, e.g. `unit.striker`.
    pub id: String,
    /// Health points.
    pub health: u32,
    /// Armor subtracted from each hit (minimum damage is 1).
    pub armor: u32,
    /// Top speed in meters per second.
    pub speed_m_per_s: f32,
    /// Sight range in meters (target acquisition).
    pub sight_range_m: f32,
    /// Collision radius in meters.
    pub radius_m: f32,
    /// The weapon.
    pub weapon: WeaponDef,
}

impl UnitDef {
    /// The movement profile derived from this definition.
    #[must_use]
    pub fn movement_profile(&self) -> MovementProfile {
        MovementProfile {
            speed_m_per_s: self.speed_m_per_s,
            radius_m: self.radius_m,
            separation_m: 1.0,
            stop_distance_m: 0.5,
        }
    }
}

impl CanonicalSerialize for UnitDef {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.id.write_canonical(sink);
        sink.write_u32(self.health);
        sink.write_u32(self.armor);
        sink.write_u32(self.speed_m_per_s.to_bits());
        sink.write_u32(self.sight_range_m.to_bits());
        sink.write_u32(self.radius_m.to_bits());
        self.weapon.write_canonical(sink);
    }
}

impl CanonicalDecode for UnitDef {
    fn read_canonical(reader: &mut CanonicalReader<'_>) -> Result<Self, CanonicalError> {
        Ok(Self {
            id: CanonicalDecode::read_canonical(reader)?,
            health: reader.read_u32()?,
            armor: reader.read_u32()?,
            speed_m_per_s: f32::from_bits(reader.read_u32()?),
            sight_range_m: f32::from_bits(reader.read_u32()?),
            radius_m: f32::from_bits(reader.read_u32()?),
            weapon: CanonicalDecode::read_canonical(reader)?,
        })
    }
}

/// The unit roster: definition id → definition. Content identity for replays
/// and saves is the canonical digest of the roster.
#[derive(Clone, Debug, PartialEq)]
pub struct Roster {
    /// Units by definition id.
    pub units: BTreeMap<String, UnitDef>,
}

impl Roster {
    /// Looks up a definition by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&UnitDef> {
        self.units.get(id)
    }

    /// Canonical digest of the roster (content identity).
    #[must_use]
    pub fn digest(&self) -> Digest {
        canonical_digest(self)
    }
}

impl CanonicalSerialize for Roster {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.units.write_canonical(sink);
    }
}

/// The initial test roster from the slice spec §3 (striker + skirmisher).
#[must_use]
pub fn slice_roster() -> Roster {
    let mut units = BTreeMap::new();
    units.insert(
        "unit.striker".to_owned(),
        UnitDef {
            id: "unit.striker".to_owned(),
            health: 100,
            armor: 0,
            speed_m_per_s: 4.0,
            sight_range_m: 40.0,
            radius_m: 0.5,
            weapon: WeaponDef {
                range_m: 10.0,
                damage: 10,
                cooldown_ticks: 20,
            },
        },
    );
    units.insert(
        "unit.skirmisher".to_owned(),
        UnitDef {
            id: "unit.skirmisher".to_owned(),
            health: 60,
            armor: 0,
            speed_m_per_s: 3.0,
            sight_range_m: 40.0,
            radius_m: 0.5,
            weapon: WeaponDef {
                range_m: 30.0,
                damage: 6,
                cooldown_ticks: 20,
            },
        },
    );
    Roster { units }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_roster_matches_the_spec() {
        let roster = slice_roster();
        let striker = roster.get("unit.striker").expect("striker");
        assert_eq!(striker.health, 100);
        assert_eq!(striker.weapon.range_m, 10.0);
        assert_eq!(striker.weapon.damage, 10);
        assert_eq!(striker.weapon.cooldown_ticks, 20);
        assert_eq!(striker.speed_m_per_s, 4.0);
        let skirmisher = roster.get("unit.skirmisher").expect("skirmisher");
        assert_eq!(skirmisher.health, 60);
        assert_eq!(skirmisher.weapon.range_m, 30.0);
        assert_eq!(skirmisher.weapon.damage, 6);
        // Content identity is stable.
        assert_eq!(roster.digest(), slice_roster().digest());
    }
}
