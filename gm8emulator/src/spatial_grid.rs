//! A transient, ad-hoc uniform spatial grid used to prune collision-candidate pairs whose
//! bounding boxes can't possibly overlap, before falling through to the existing precise
//! (AABB then per-pixel mask) test in `Game::check_collision`. This is purely a broad-phase
//! performance optimization -- it never changes which pairs end up colliding, only how many
//! pairs need to be considered in the first place. See `game/events.rs`'s `run_collisions` for
//! the primary caller.
//!
//! The grid is built fresh per call site (never stored on `Instance`/`InstanceList`), so it has
//! no savestate/serialization surface at all.

use std::collections::HashMap;

type Cell = (i32, i32);
type Bbox = (i32, i32, i32, i32); // (left, top, right, bottom), inclusive

/// Cells are sized to at least this many pixels, so a room full of tiny particles doesn't
/// produce a huge number of near-empty buckets.
const MIN_CELL_SIZE: i32 = 64;

pub struct SpatialGrid {
    cell_size: i32,
    // Locked in by the first `insert` and never changed afterward -- cell coordinates are
    // `position.div_euclid(cell_size)`, so mixing cell sizes across inserts/queries would put
    // members bucketed under one size into keys a query computed at a different size would never
    // look up, silently dropping them. A later, bigger bbox than the size was picked for just
    // spans more cells at the fixed size -- less efficient, still correct.
    cell_size_locked: bool,
    buckets: HashMap<Cell, Vec<usize>>,
    // handle -> (cells it's currently bucketed in, its stable original-order key)
    membership: HashMap<usize, (Vec<Cell>, usize)>,
}

fn cells_for(bbox: Bbox, cell_size: i32) -> impl Iterator<Item = Cell> {
    let (left, top, right, bottom) = bbox;
    let (cx0, cy0) = (left.div_euclid(cell_size), top.div_euclid(cell_size));
    let (cx1, cy1) = (right.div_euclid(cell_size), bottom.div_euclid(cell_size));
    (cy0..=cy1).flat_map(move |cy| (cx0..=cx1).map(move |cx| (cx, cy)))
}

impl SpatialGrid {
    pub fn new() -> Self {
        Self { cell_size: MIN_CELL_SIZE, cell_size_locked: false, buckets: HashMap::new(), membership: HashMap::new() }
    }

    /// Picks the cell size from the first bbox ever inserted, then locks it. No-op after that.
    fn ensure_cell_size(&mut self, bbox: Bbox) {
        if self.cell_size_locked {
            return
        }
        let (left, top, right, bottom) = bbox;
        let dim = (right - left + 1).max(bottom - top + 1);
        self.cell_size = dim.max(MIN_CELL_SIZE);
        self.cell_size_locked = true;
    }

    pub fn insert(&mut self, handle: usize, bbox: Bbox, orig_index: usize) {
        self.ensure_cell_size(bbox);
        let cells: Vec<Cell> = cells_for(bbox, self.cell_size).collect();
        for &cell in &cells {
            self.buckets.entry(cell).or_default().push(handle);
        }
        self.membership.insert(handle, (cells, orig_index));
    }

    pub fn remove(&mut self, handle: usize) {
        if let Some((cells, _)) = self.membership.remove(&handle) {
            for cell in cells {
                if let Some(bucket) = self.buckets.get_mut(&cell) {
                    if let Some(pos) = bucket.iter().position(|&h| h == handle) {
                        bucket.swap_remove(pos);
                    }
                }
            }
        }
    }

    /// Re-buckets a handle that moved, keeping its original-order key stable.
    pub fn reinsert(&mut self, handle: usize, new_bbox: Bbox) {
        let orig_index = self.membership.get(&handle).map(|(_, i)| *i).unwrap_or(0);
        self.remove(handle);
        self.insert(handle, new_bbox, orig_index);
    }

    pub fn contains(&self, handle: usize) -> bool {
        self.membership.contains_key(&handle)
    }

    /// Returns every distinct handle whose bbox was inserted overlapping any cell `bbox` also
    /// overlaps, in ascending `orig_index` order (matching the creation-order walk a full linear
    /// scan over the same members would produce).
    pub fn query(&self, bbox: Bbox) -> Vec<usize> {
        let mut found: Vec<usize> = cells_for(bbox, self.cell_size)
            .filter_map(|cell| self.buckets.get(&cell))
            .flat_map(|bucket| bucket.iter().copied())
            .collect();
        found.sort_by_key(|h| self.membership.get(h).map(|(_, i)| *i).unwrap_or(usize::MAX));
        found.dedup();
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn insert_and_query_finds_overlapping() {
        let mut grid = SpatialGrid::new();
        grid.insert(1, (0, 0, 10, 10), 0);
        grid.insert(2, (1000, 1000, 1010, 1010), 1);
        assert_eq!(grid.query((0, 0, 10, 10)), vec![1]);
        assert_eq!(grid.query((1000, 1000, 1010, 1010)), vec![2]);
        assert_eq!(grid.query((500, 500, 510, 510)), Vec::<usize>::new());
    }

    #[test]
    fn query_dedups_multi_cell_spanning_bbox() {
        let mut grid = SpatialGrid::new();
        // First insert locks the cell size small (MIN_CELL_SIZE); a later bbox several times
        // wider than that spans multiple cells and must still come back as one result.
        grid.insert(1, (0, 0, 4, 4), 0);
        grid.insert(2, (0, 0, 500, 500), 1);
        let results = grid.query((0, 0, 500, 500));
        assert_eq!(results, vec![1, 2]);
    }

    #[test]
    fn query_preserves_orig_index_order() {
        let mut grid = SpatialGrid::new();
        // Insert out of order; query must come back sorted by orig_index (creation order).
        grid.insert(5, (0, 0, 10, 10), 2);
        grid.insert(6, (0, 0, 10, 10), 0);
        grid.insert(7, (0, 0, 10, 10), 1);
        assert_eq!(grid.query((0, 0, 10, 10)), vec![6, 7, 5]);
    }

    #[test]
    fn reinsert_moves_and_keeps_orig_index() {
        let mut grid = SpatialGrid::new();
        grid.insert(1, (0, 0, 10, 10), 3);
        grid.reinsert(1, (1000, 1000, 1010, 1010));
        assert_eq!(grid.query((0, 0, 10, 10)), Vec::<usize>::new());
        assert_eq!(grid.query((1000, 1000, 1010, 1010)), vec![1]);
        // orig_index survived the move.
        assert_eq!(grid.membership.get(&1).map(|(_, i)| *i), Some(3));
    }

    #[test]
    fn remove_clears_all_spanned_cells() {
        let mut grid = SpatialGrid::new();
        grid.insert(1, (0, 0, 500, 500), 0);
        grid.remove(1);
        assert!(!grid.contains(1));
        assert_eq!(grid.query((0, 0, 500, 500)), Vec::<usize>::new());
    }

    #[test]
    fn empty_grid_query_returns_nothing() {
        let grid = SpatialGrid::new();
        assert_eq!(grid.query((0, 0, 100, 100)), Vec::<usize>::new());
    }

    #[test]
    fn large_bbox_still_findable() {
        let mut grid = SpatialGrid::new();
        // One instance covering a huge area (bigger than MIN_CELL_SIZE).
        grid.insert(1, (0, 0, 2000, 2000), 0);
        assert_eq!(grid.query((1900, 1900, 1950, 1950)), vec![1]);
        assert_eq!(grid.query((0, 0, 10, 10)), vec![1]);
    }
}
