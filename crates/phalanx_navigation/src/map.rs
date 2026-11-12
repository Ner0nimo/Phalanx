//! Static map data (scenario data, not runtime state) and the "Crossing"
//! prototype map from `docs/vertical-slice-scenario.md` §2.

use phalanx_core::hash::{CanonicalSerialize, CanonicalSink};

/// An axis-aligned rectangular static obstacle in meters on the `(x, z)`
/// ground plane.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Obstacle {
    /// Minimum corner (x, z), meters.
    pub min: (f32, f32),
    /// Maximum corner (x, z), meters.
    pub max: (f32, f32),
}

impl Obstacle {
    /// Creates an obstacle from its center `(cx, cz)` and size `(w, d)`.
    pub fn new(cx: f32, cz: f32, w: f32, d: f32) -> Self {
        Self {
            min: (cx - w / 2.0, cz - d / 2.0),
            max: (cx + w / 2.0, cz + d / 2.0),
        }
    }
}

impl CanonicalSerialize for Obstacle {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        self.min.write_canonical(sink);
        self.max.write_canonical(sink);
    }
}

/// A static heightfield/obstacle map. The prototype treats the terrain as
/// flat (height 0) with rectangular blocking obstacles.
#[derive(Clone, Debug, PartialEq)]
pub struct Map {
    /// Map extent in meters along x.
    pub width_m: f32,
    /// Map extent in meters along z.
    pub depth_m: f32,
    /// Static blocking obstacles.
    pub obstacles: Vec<Obstacle>,
}

impl CanonicalSerialize for Map {
    fn write_canonical(&self, sink: &mut dyn CanonicalSink) {
        sink.write_u32(self.width_m.to_bits());
        sink.write_u32(self.depth_m.to_bits());
        self.obstacles.write_canonical(sink);
    }
}

/// The "Crossing" prototype map: 200 m × 200 m, flat, with two walls leaving a
/// single chokepoint lane and two rocks narrowing its entrances
/// (`docs/vertical-slice-scenario.md` §2).
#[must_use]
pub fn crossing_map() -> Map {
    Map {
        width_m: 200.0,
        depth_m: 200.0,
        obstacles: vec![
            Obstacle::new(100.0, 60.0, 90.0, 4.0),  // Wall N
            Obstacle::new(100.0, 140.0, 90.0, 4.0), // Wall S
            Obstacle::new(60.0, 100.0, 8.0, 8.0),   // Rock W
            Obstacle::new(140.0, 100.0, 8.0, 8.0),  // Rock E
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crossing_map_matches_the_spec() {
        let map = crossing_map();
        assert_eq!(map.width_m, 200.0);
        assert_eq!(map.depth_m, 200.0);
        assert_eq!(map.obstacles.len(), 4);
        // Wall N occupies x in [55, 145], z in [58, 62].
        let wall_n = map.obstacles[0];
        assert_eq!(wall_n.min, (55.0, 58.0));
        assert_eq!(wall_n.max, (145.0, 62.0));
    }
}
