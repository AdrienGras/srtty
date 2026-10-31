//! Dashboard geometry: how tall the inline viewport is, which chunks are listed
//! and how the progress band is laid out. Drawing lives further down.

use std::ops::Range;

use ratatui::layout::Size;

/// Most chunk rows the list shows; a longer list scrolls.
pub const MAX_CHUNK_ROWS: u16 = 8;
/// Most rows the progress band may span.
pub const MAX_CELL_ROWS: u16 = 3;
/// Narrowest terminal that gets the full frame.
pub const MIN_WIDTH: u16 = 40;

/// Cells of the progress band: size of one cell, gap between cells, how many
/// chunks a cell stands for, and how many rows the band spans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellGeometry {
    pub width: u16,
    pub gap: u16,
    pub per_cell: usize,
    pub rows: u16,
}

/// What the viewport shows: the full frame, or a single compact line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    Full {
        chunk_rows: u16,
        cells: CellGeometry,
    },
    Compact,
}

impl Layout {
    /// Height of the inline viewport for this layout, in rows.
    pub fn height(&self) -> u16 {
        match self {
            Layout::Compact => 1,
            Layout::Full { chunk_rows, cells } => {
                let list = if *chunk_rows > 0 { 2 + chunk_rows } else { 0 };
                3 + list + 2 + cells.rows + 1
            }
        }
    }
}

/// Geometry of the progress band for `chunks` chunks.
/// `inner_width`: columns inside the progress box (terminal width - 4).
pub fn cell_geometry(chunks: usize, inner_width: u16) -> CellGeometry {
    if chunks == 0 || inner_width == 0 {
        // Nothing to draw (a zero width only happens in the compact layout).
        return CellGeometry {
            width: 1,
            gap: 0,
            per_cell: 1,
            rows: 0,
        };
    }
    let width = usize::from(inner_width);
    if chunks * 5 - 1 <= width {
        return CellGeometry {
            width: 4,
            gap: 1,
            per_cell: 1,
            rows: 1,
        };
    }
    let rows = chunks.div_ceil(width);
    if rows <= usize::from(MAX_CELL_ROWS) {
        // `rows` is at most MAX_CELL_ROWS here.
        return CellGeometry {
            width: 1,
            gap: 0,
            per_cell: 1,
            rows: rows as u16,
        };
    }
    let per_cell = chunks.div_ceil(width * usize::from(MAX_CELL_ROWS));
    let rows = chunks.div_ceil(per_cell).div_ceil(width);
    // By construction `rows` is at most MAX_CELL_ROWS.
    CellGeometry {
        width: 1,
        gap: 0,
        per_cell,
        rows: rows as u16,
    }
}

/// Picks the layout for `chunks` chunks in a terminal of the given size,
/// keeping one free line under the viewport.
pub fn plan_layout(chunks: usize, size: Size) -> Layout {
    if size.width < MIN_WIDTH {
        return Layout::Compact;
    }
    let available = size.height.saturating_sub(1);
    let cells = cell_geometry(chunks, size.width.saturating_sub(4));
    let list_frame = if chunks > 0 { 2 } else { 0 };
    let fixed = 3 + 2 + cells.rows + 1 + list_frame;
    let wanted = u16::try_from(chunks).unwrap_or(u16::MAX);
    let chunk_rows = wanted
        .min(MAX_CHUNK_ROWS)
        .min(available.saturating_sub(fixed));
    if fixed <= available && chunk_rows >= wanted.min(2) {
        Layout::Full { chunk_rows, cells }
    } else {
        Layout::Compact
    }
}

/// Chunks to list and whether the last row summarizes the hidden ones.
pub fn visible_window(
    chunks: usize,
    first_unfinished: Option<usize>,
    rows: u16,
) -> (Range<usize>, bool) {
    let rows = usize::from(rows);
    if chunks <= rows {
        return (0..chunks, false);
    }
    let shown = rows.saturating_sub(1);
    let start = first_unfinished
        .unwrap_or(chunks)
        .saturating_sub(1)
        .min(chunks - shown);
    (start..start + shown, true)
}

#[cfg(test)]
mod tests {
    use ratatui::layout::Size;

    use super::*;

    fn size(w: u16, h: u16) -> Size {
        Size::new(w, h)
    }

    #[test]
    fn cell_geometry_cases() {
        assert_eq!(cell_geometry(0, 76).rows, 0);
        assert_eq!(
            cell_geometry(8, 66),
            CellGeometry {
                width: 4,
                gap: 1,
                per_cell: 1,
                rows: 1
            }
        );
        assert_eq!(
            cell_geometry(20, 76),
            CellGeometry {
                width: 1,
                gap: 0,
                per_cell: 1,
                rows: 1
            }
        );
        assert_eq!(
            cell_geometry(200, 76),
            CellGeometry {
                width: 1,
                gap: 0,
                per_cell: 1,
                rows: 3
            }
        );
        assert_eq!(
            cell_geometry(500, 76),
            CellGeometry {
                width: 1,
                gap: 0,
                per_cell: 3,
                rows: 3
            }
        );
    }

    #[test]
    fn full_layout_heights() {
        assert_eq!(plan_layout(0, size(80, 40)).height(), 6); // before the chunk plan
        assert_eq!(plan_layout(1, size(80, 40)).height(), 10); // --speakers: one chunk row
        assert_eq!(plan_layout(8, size(80, 40)).height(), 17);
        assert!(matches!(
            plan_layout(20, size(80, 40)),
            Layout::Full { chunk_rows: 8, .. }
        ));
        assert!(matches!(
            plan_layout(20, size(80, 14)),
            Layout::Full { chunk_rows: 4, .. }
        ));
    }

    #[test]
    fn layout_degrades_to_compact() {
        assert_eq!(plan_layout(20, size(80, 10)), Layout::Compact);
        assert_eq!(plan_layout(8, size(39, 40)), Layout::Compact);
        assert_eq!(plan_layout(0, size(80, 5)), Layout::Compact);
        assert_eq!(Layout::Compact.height(), 1);
    }

    #[test]
    fn visible_window_follows_first_unfinished() {
        assert_eq!(visible_window(8, Some(3), 8), (0..8, false));
        assert_eq!(visible_window(20, Some(10), 8), (9..16, true));
        assert_eq!(visible_window(20, Some(0), 8), (0..7, true));
        assert_eq!(visible_window(20, Some(19), 8), (13..20, true));
        assert_eq!(visible_window(20, None, 8), (13..20, true));
    }
}
