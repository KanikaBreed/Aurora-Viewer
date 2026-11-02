//! Where a global position falls on a grid with regions larger than 256 m
//! (OpenSim): its region's corner and the position in that region. As
//! LLWorldMap::simInfoFromPosGlobal, a region is found from the sizes the
//! world map and the connected regions give; a position nowhere known is
//! taken in its 256 m slot, as before.
//!
//! The interface turns global positions into "Region (x, y, z)" in many
//! places that only hold the position, so the large regions seen are kept
//! here rather than handed down to each. They live on the thread that runs
//! the world and the interface; each test thread starts with none.

use std::cell::RefCell;

const SLOT: f64 = 256.0;

thread_local! {
    /// Corner and size of each region larger than a slot: [x, y, w, h].
    static LARGE: RefCell<Vec<[f64; 4]>> = const { RefCell::new(Vec::new()) };
}

/// A region's corner (global metres) and size, as learned.
pub fn note(corner_x: f64, corner_y: f64, size_x: u32, size_y: u32) {
    if size_x <= 256 && size_y <= 256 {
        return;
    }
    // what a map reply announces is held to the grid's largest region too
    let (size_x, size_y) = (size_x.min(super::MAX_REGION_SIZE), size_y.min(super::MAX_REGION_SIZE));
    let rect = [corner_x, corner_y, f64::from(size_x), f64::from(size_y)];
    LARGE.with_borrow_mut(|large| match large.iter_mut().find(|r| r[0] == corner_x && r[1] == corner_y) {
        Some(known) => *known = rect,
        None => large.push(rect),
    });
}

/// Logout: the next grid has its own regions.
pub fn clear() {
    LARGE.with_borrow_mut(Vec::clear);
}

/// The size of the large region whose corner is (`x`, `y`); None for a
/// region of one slot, or one not seen.
pub fn size(x: f64, y: f64) -> Option<(u32, u32)> {
    LARGE.with_borrow(|large| large.iter().find(|r| r[0] == x && r[1] == y).map(|r| (r[2] as u32, r[3] as u32)))
}

/// The corner of the region holding (`x`, `y`).
pub fn corner(x: f64, y: f64) -> (f64, f64) {
    LARGE
        .with_borrow(|large| {
            large
                .iter()
                .find(|r| x >= r[0] && y >= r[1] && x < r[0] + r[2] && y < r[1] + r[3])
                .map(|r| (r[0], r[1]))
        })
        .unwrap_or(((x / SLOT).floor() * SLOT, (y / SLOT).floor() * SLOT))
}

/// The position of (`x`, `y`) in its region.
pub fn local(x: f64, y: f64) -> (f64, f64) {
    let (cx, cy) = corner(x, y);
    (x - cx, y - cy)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_position_in_a_large_region_is_measured_from_its_corner() {
        assert_eq!(local(256_700.0, 256_300.0), (188.0, 44.0), "unknown: its 256 m slot");
        note(256_000.0, 256_000.0, 1024, 1024);
        assert_eq!(local(256_700.0, 256_300.0), (700.0, 300.0));
        assert_eq!(corner(256_700.0, 256_300.0), (256_000.0, 256_000.0));
        assert_eq!(local(257_100.0, 256_000.0), (76.0, 0.0), "past its east edge: the next slot");
        note(256_000.0, 256_000.0, 512, 256);
        assert_eq!(
            local(256_700.0, 256_100.0),
            (188.0, 100.0),
            "a size learned again replaces the first"
        );
        assert_eq!(size(256_000.0, 256_000.0), Some((512, 256)));
        note(512_000.0, 512_000.0, u32::MAX, 300);
        assert_eq!(size(512_000.0, 512_000.0), Some((4096, 300)), "held to the largest region");
        note(300_000.0, 300_000.0, 256, 256);
        assert_eq!(size(300_000.0, 300_000.0), None, "one slot: not kept");
        clear();
        assert_eq!(local(256_300.0, 256_100.0), (44.0, 100.0));
    }
}
