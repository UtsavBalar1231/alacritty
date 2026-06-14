//! Unicode-placeholder cell scanner for the kitty graphics protocol.
//!
//! Implements the U+10EEEE placeholder decode path: diacritic table, row
//! scanner with L-to-R inheritance, RLE grouping, and fit-to-box math.
//! All algorithms are ported from kitty's `rowcolumn-diacritics.c`,
//! `screen.c:screen_render_line_graphics`, and `graphics.c:grman_put_cell_image`.

use crate::vte::ansi::{Color, Rgb};

/// U+10EEEE — the private-use image-placeholder character.
pub const IMAGE_PLACEHOLDER_CHAR: char = '\u{10EEEE}';

// ── Diacritic table ──────────────────────────────────────────────────────────

/// Map a combining diacritic code-point to its 1-based encoded value.
///
/// Returns `0` if the code-point is not in kitty's table.
/// Verbatim port of `diacritic_to_num` from `kitty/kitty/rowcolumn-diacritics.c`
/// (Unicode Standard 17.0.0). The order of entries defines the value mapping.
pub fn diacritic_to_num(cp: u32) -> u32 {
    match cp {
        0x305..=0x306 => cp - 0x305 + 1,
        0x30d..=0x30f => cp - 0x30d + 2,
        0x310..=0x311 => cp - 0x310 + 4,
        0x312..=0x313 => cp - 0x312 + 5,
        0x33d..=0x340 => cp - 0x33d + 6,
        0x346..=0x347 => cp - 0x346 + 9,
        0x34a..=0x34d => cp - 0x34a + 10,
        0x350..=0x353 => cp - 0x350 + 13,
        0x357..=0x358 => cp - 0x357 + 16,
        0x35b..=0x35c => cp - 0x35b + 17,
        0x363..=0x370 => cp - 0x363 + 18,
        0x483..=0x488 => cp - 0x483 + 31,
        0x592..=0x596 => cp - 0x592 + 36,
        0x597..=0x59a => cp - 0x597 + 40,
        0x59c..=0x5a2 => cp - 0x59c + 43,
        0x5a8..=0x5aa => cp - 0x5a8 + 49,
        0x5ab..=0x5ad => cp - 0x5ab + 51,
        0x5af..=0x5b0 => cp - 0x5af + 53,
        0x5c4..=0x5c5 => cp - 0x5c4 + 54,
        0x610..=0x618 => cp - 0x610 + 55,
        0x657..=0x65c => cp - 0x657 + 63,
        0x65d..=0x65f => cp - 0x65d + 68,
        0x6d6..=0x6dd => cp - 0x6d6 + 70,
        0x6df..=0x6e3 => cp - 0x6df + 77,
        0x6e4..=0x6e5 => cp - 0x6e4 + 81,
        0x6e7..=0x6e9 => cp - 0x6e7 + 82,
        0x6eb..=0x6ed => cp - 0x6eb + 84,
        0x730..=0x731 => cp - 0x730 + 86,
        0x732..=0x734 => cp - 0x732 + 87,
        0x735..=0x737 => cp - 0x735 + 89,
        0x73a..=0x73b => cp - 0x73a + 91,
        0x73d..=0x73e => cp - 0x73d + 92,
        0x73f..=0x742 => cp - 0x73f + 93,
        0x743..=0x744 => cp - 0x743 + 96,
        0x745..=0x746 => cp - 0x745 + 97,
        0x747..=0x748 => cp - 0x747 + 98,
        0x749..=0x74b => cp - 0x749 + 99,
        0x7eb..=0x7f2 => cp - 0x7eb + 101,
        0x7f3..=0x7f4 => cp - 0x7f3 + 108,
        0x816..=0x81a => cp - 0x816 + 109,
        0x81b..=0x824 => cp - 0x81b + 113,
        0x825..=0x828 => cp - 0x825 + 122,
        0x829..=0x82e => cp - 0x829 + 125,
        0x951..=0x952 => cp - 0x951 + 130,
        0x953..=0x955 => cp - 0x953 + 131,
        0xf82..=0xf84 => cp - 0xf82 + 133,
        0xf86..=0xf88 => cp - 0xf86 + 135,
        0x135d..=0x1360 => cp - 0x135d + 137,
        0x17dd..=0x17de => cp - 0x17dd + 140,
        0x193a..=0x193b => cp - 0x193a + 141,
        0x1a17..=0x1a18 => cp - 0x1a17 + 142,
        0x1a75..=0x1a7d => cp - 0x1a75 + 143,
        0x1b6b..=0x1b6c => cp - 0x1b6b + 151,
        0x1b6d..=0x1b74 => cp - 0x1b6d + 152,
        0x1cd0..=0x1cd3 => cp - 0x1cd0 + 159,
        0x1cda..=0x1cdc => cp - 0x1cda + 162,
        0x1ce0..=0x1ce1 => cp - 0x1ce0 + 164,
        0x1dc0..=0x1dc2 => cp - 0x1dc0 + 165,
        0x1dc3..=0x1dca => cp - 0x1dc3 + 167,
        0x1dcb..=0x1dcd => cp - 0x1dcb + 174,
        0x1dd1..=0x1de7 => cp - 0x1dd1 + 176,
        0x1dfe..=0x1dff => cp - 0x1dfe + 198,
        0x20d0..=0x20d2 => cp - 0x20d0 + 199,
        0x20d4..=0x20d8 => cp - 0x20d4 + 201,
        0x20db..=0x20dd => cp - 0x20db + 205,
        0x20e1..=0x20e2 => cp - 0x20e1 + 207,
        0x20e7..=0x20e8 => cp - 0x20e7 + 208,
        0x20e9..=0x20ea => cp - 0x20e9 + 209,
        0x20f0..=0x20f1 => cp - 0x20f0 + 210,
        0x2cef..=0x2cf2 => cp - 0x2cef + 211,
        0x2de0..=0x2e00 => cp - 0x2de0 + 214,
        0xa66f..=0xa670 => cp - 0xa66f + 246,
        0xa67c..=0xa67e => cp - 0xa67c + 247,
        0xa6f0..=0xa6f2 => cp - 0xa6f0 + 249,
        0xa8e0..=0xa8f2 => cp - 0xa8e0 + 251,
        0xaab0..=0xaab1 => cp - 0xaab0 + 269,
        0xaab2..=0xaab4 => cp - 0xaab2 + 270,
        0xaab7..=0xaab9 => cp - 0xaab7 + 272,
        0xaabe..=0xaac0 => cp - 0xaabe + 274,
        0xaac1..=0xaac2 => cp - 0xaac1 + 276,
        0xfe20..=0xfe27 => cp - 0xfe20 + 277,
        0x10a0f..=0x10a10 => cp - 0x10a0f + 284,
        0x10a38..=0x10a39 => cp - 0x10a38 + 285,
        0x1d185..=0x1d18a => cp - 0x1d185 + 286,
        0x1d1aa..=0x1d1ae => cp - 0x1d1aa + 291,
        0x1d242..=0x1d245 => cp - 0x1d242 + 295,
        _ => 0,
    }
}

// ── Color → id ───────────────────────────────────────────────────────────────

/// Convert a cell color to a 24-bit image-id fragment.
///
/// `Spec(Rgb)` packs the three bytes directly. `Indexed(u8)` uses the index
/// value as the id (kitty's 8-bit path). `Named` colors carry no id.
#[inline]
pub fn color_to_id(color: Color) -> u32 {
    match color {
        Color::Spec(Rgb { r, g, b }) => (r as u32) << 16 | (g as u32) << 8 | (b as u32),
        Color::Indexed(idx) => idx as u32,
        Color::Named(_) => 0,
    }
}

// ── Placeholder run ──────────────────────────────────────────────────────────

/// A contiguous horizontal run of placeholder cells sharing the same image,
/// placement, and image row. Ephemeral — recreated each snapshot, never stored.
#[derive(Debug, Clone, PartialEq)]
pub struct PlaceholderRun {
    /// Full 32-bit image id (lower 24 from fg | msb from 3rd diacritic).
    pub image_id: u32,
    /// Placement id from underline color; `0` = any virtual placement.
    pub placement_id: u32,
    /// 0-based image-box row.
    pub img_row: u32,
    /// 0-based image-box column of the first cell of this run.
    pub img_col_start: u32,
    /// Number of cells in this run.
    pub run_length: u32,
    /// 0-based screen column of the first cell of this run.
    pub screen_col: u32,
}

// ── Pre-decoded cell data ─────────────────────────────────────────────────────

/// Pre-decoded data for one placeholder cell, extracted from the grid by the
/// render-snapshot caller before passing a row slice to [`scan_placeholder_cells`].
#[derive(Debug, Clone, Default)]
pub struct PlaceholderCellData {
    pub screen_col: u32,
    /// Lower 24 bits of image id from fg color.
    pub id_lo: u32,
    /// Placement id from underline color (0 = none).
    pub placement_id: u32,
    /// 1-based image row from 1st diacritic (0 = not present).
    pub img_row: u32,
    /// 1-based image column from 2nd diacritic (0 = not present).
    pub img_col: u32,
    /// 1-based msb byte from 3rd diacritic (0 = not present).
    pub id_hi: u32,
}

// ── Row scanner ──────────────────────────────────────────────────────────────

/// Scan a slice of pre-decoded placeholder cells and produce [`PlaceholderRun`]s.
///
/// Port of `screen_render_line_graphics` (kitty/screen.c:3519). Diacritic
/// values are 1-based internally; 0 means "not present / inherit." A run is
/// continued when the lower-24-bit id and placement id are unchanged, and the
/// row and column are compatible with the inheritance rules. The MSB byte
/// inherits identically to the row.
pub fn scan_placeholder_cells(cells: &[PlaceholderCellData]) -> Vec<PlaceholderRun> {
    let mut runs: Vec<PlaceholderRun> = Vec::new();

    let mut run_length: u32 = 0;
    let mut prev_id_lo: u32 = 0;
    let mut prev_placement_id: u32 = 0;
    let mut prev_id_hi: u32 = 0; // 1-based; actual msb = prev_id_hi - 1
    let mut prev_img_row: u32 = 0; // 1-based
    let mut prev_img_col: u32 = 0; // 1-based
    let mut run_start_screen_col: u32 = 0;

    for cell in cells {
        let cur_id_lo = cell.id_lo;
        let cur_placement_id = cell.placement_id;
        let mut cur_img_row = cell.img_row;
        let mut cur_img_col = cell.img_col;
        let mut cur_id_hi = cell.id_hi;

        // Continuation check: same id/placement, compatible row/col/msb.
        // Mirrors kitty screen.c:3560-3579.
        let continues = run_length > 0
            && cur_id_lo == prev_id_lo
            && cur_placement_id == prev_placement_id
            && (cur_img_row == 0 || cur_img_row == prev_img_row)
            && (cur_img_col == 0 || cur_img_col == prev_img_col + 1)
            && (cur_id_hi == 0 || cur_id_hi == prev_id_hi);

        if continues {
            run_length += 1;
            cur_img_row = prev_img_row.max(1);
            cur_img_col = prev_img_col + 1;
            cur_id_hi = prev_id_hi.max(1);
        } else {
            if run_length > 0 {
                flush_run(&mut runs, &RunState {
                    id_lo: prev_id_lo,
                    placement_id: prev_placement_id,
                    id_hi: prev_id_hi,
                    img_row: prev_img_row,
                    img_col: prev_img_col,
                    run_length,
                    screen_col: run_start_screen_col,
                });
            }

            // Start new run if the cell carries any id.
            if cur_id_lo != 0 || cur_placement_id != 0 {
                run_length = 1;
                if cur_img_col == 0 {
                    cur_img_col = 1;
                }
                if cur_img_row == 0 {
                    cur_img_row = 1;
                }
                if cur_id_hi == 0 {
                    cur_id_hi = 1;
                }
                run_start_screen_col = cell.screen_col;
            } else {
                run_length = 0;
            }
        }

        prev_id_lo = cur_id_lo;
        prev_placement_id = cur_placement_id;
        prev_img_row = cur_img_row;
        prev_img_col = cur_img_col;
        prev_id_hi = cur_id_hi;
    }

    if run_length > 0 {
        flush_run(&mut runs, &RunState {
            id_lo: prev_id_lo,
            placement_id: prev_placement_id,
            id_hi: prev_id_hi,
            img_row: prev_img_row,
            img_col: prev_img_col,
            run_length,
            screen_col: run_start_screen_col,
        });
    }

    runs
}

struct RunState {
    id_lo: u32,
    placement_id: u32,
    id_hi: u32,
    img_row: u32,
    img_col: u32,
    run_length: u32,
    screen_col: u32,
}

#[inline]
fn flush_run(runs: &mut Vec<PlaceholderRun>, s: &RunState) {
    let actual_msb = if s.id_hi > 0 { s.id_hi - 1 } else { 0 };
    runs.push(PlaceholderRun {
        image_id: s.id_lo | (actual_msb << 24),
        placement_id: s.placement_id,
        img_row: if s.img_row > 0 { s.img_row - 1 } else { 0 },
        img_col_start: s.img_col.saturating_sub(s.run_length),
        run_length: s.run_length,
        screen_col: s.screen_col,
    });
}

// ── Fit-to-box ────────────────────────────────────────────────────────────────

/// Source pixel rectangle from fitting the image into a cell box.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FitRect {
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub cell_x_offset: u32,
    pub cell_y_offset: u32,
}

/// Compute the source sub-rect for one placeholder run.
///
/// Port of kitty `grman_put_cell_image` (graphics.c:940-980): fit the image
/// into a `box_cols × box_rows` pixel box preserving aspect ratio with
/// letterbox/pillarbox centering, then sample the destination sub-rectangle
/// corresponding to `(img_col_start, img_row, run_length, 1)` back to source.
///
/// Returns `None` when the run falls entirely within a bar (no image content).
#[allow(clippy::too_many_arguments)]
pub fn fit_to_box(
    img_w: u32,
    img_h: u32,
    box_cols: u32,
    box_rows: u32,
    cell_w: u32,
    cell_h: u32,
    img_col_start: u32,
    img_row: u32,
    run_length: u32,
) -> Option<FitRect> {
    if img_w == 0
        || img_h == 0
        || box_cols == 0
        || box_rows == 0
        || cell_w == 0
        || cell_h == 0
        || run_length == 0
    {
        return None;
    }

    let iw = img_w as f32;
    let ih = img_h as f32;
    let bw = (box_cols * cell_w) as f32;
    let bh = (box_rows * cell_h) as f32;

    let (x_scale, y_scale, x_offset, y_offset) = if iw * bh > ih * bw {
        // Fit to width, letterbox vertically.
        let xs = bw / iw;
        (xs, xs, 0.0_f32, (bh - ih * xs) / 2.0)
    } else {
        // Fit to height, pillarbox horizontally.
        let ys = bh / ih;
        (ys, ys, (bw - iw * ys) / 2.0, 0.0_f32)
    };

    let x_dst = (img_col_start * cell_w) as f32;
    let y_dst = (img_row * cell_h) as f32;
    let w_dst = (run_length * cell_w) as f32;
    let h_dst = cell_h as f32;

    let src_x = ((x_dst - x_offset) / x_scale).max(0.0);
    let src_y = ((y_dst - y_offset) / y_scale).max(0.0);
    let src_w = w_dst / x_scale;
    let src_h = h_dst / y_scale;

    if src_x >= iw || src_y >= ih {
        return None;
    }
    let src_w = src_w.min(iw - src_x);
    let src_h = src_h.min(ih - src_y);
    if src_w <= 0.0 || src_h <= 0.0 {
        return None;
    }

    let cell_x_offset =
        ((x_dst - (src_x * x_scale + x_offset)).max(0.0) as u32).min(cell_w.saturating_sub(1));
    let cell_y_offset =
        ((y_dst - (src_y * y_scale + y_offset)).max(0.0) as u32).min(cell_h.saturating_sub(1));

    let src_xi = src_x as u32;
    let src_yi = src_y as u32;
    // Clamp after ceil so src_xi + src_wi never exceeds image dimensions
    let src_wi = (src_w.ceil() as u32).min(img_w - src_xi);
    let src_hi = (src_h.ceil() as u32).min(img_h - src_yi);

    Some(FitRect {
        src_x: src_xi,
        src_y: src_yi,
        src_w: src_wi,
        src_h: src_hi,
        cell_x_offset,
        cell_y_offset,
    })
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diacritic_u0305_is_one() {
        assert_eq!(diacritic_to_num(0x0305), 1);
    }

    #[test]
    fn diacritic_u030d_is_two() {
        assert_eq!(diacritic_to_num(0x030d), 2);
    }

    #[test]
    fn diacritic_u030e_is_three() {
        assert_eq!(diacritic_to_num(0x030e), 3);
    }

    #[test]
    fn diacritic_u0306_follows_u0305() {
        assert_eq!(diacritic_to_num(0x0306), 2);
    }

    #[test]
    fn diacritic_unknown_is_zero() {
        assert_eq!(diacritic_to_num(0x0041), 0);
        assert_eq!(diacritic_to_num(0x0000), 0);
    }

    #[test]
    fn diacritic_last_range() {
        assert_eq!(diacritic_to_num(0x1d242), 295);
        assert_eq!(diacritic_to_num(0x1d245), 298);
    }

    #[test]
    fn color_rgb_packs_correctly() {
        assert_eq!(color_to_id(Color::Spec(Rgb { r: 0, g: 0, b: 42 })), 42);
        assert_eq!(color_to_id(Color::Spec(Rgb { r: 1, g: 2, b: 3 })), (1 << 16) | (2 << 8) | 3);
    }

    #[test]
    fn color_indexed_is_identity() {
        assert_eq!(color_to_id(Color::Indexed(42)), 42);
    }

    #[test]
    fn color_named_is_zero() {
        use crate::vte::ansi::NamedColor;
        assert_eq!(color_to_id(Color::Named(NamedColor::Foreground)), 0);
    }

    fn cell(
        col: u32,
        id_lo: u32,
        pid: u32,
        row: u32,
        img_col: u32,
        hi: u32,
    ) -> PlaceholderCellData {
        PlaceholderCellData {
            screen_col: col,
            id_lo,
            placement_id: pid,
            img_row: row,
            img_col,
            id_hi: hi,
        }
    }

    #[test]
    fn single_cell_explicit_diacritics() {
        let runs = scan_placeholder_cells(&[cell(0, 42, 0, 1, 1, 1)]);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].image_id, 42);
        assert_eq!(runs[0].img_row, 0);
        assert_eq!(runs[0].img_col_start, 0);
        assert_eq!(runs[0].run_length, 1);
        assert_eq!(runs[0].screen_col, 0);
    }

    #[test]
    fn two_cells_with_inheritance() {
        let cells = [cell(0, 42, 0, 1, 1, 1), cell(1, 42, 0, 0, 0, 0)];
        let runs = scan_placeholder_cells(&cells);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_length, 2);
        assert_eq!(runs[0].img_col_start, 0);
        assert_eq!(runs[0].img_row, 0);
    }

    #[test]
    fn rle_breaks_on_row_change() {
        let cells = [cell(0, 42, 0, 1, 1, 1), cell(1, 42, 0, 2, 1, 1)];
        let runs = scan_placeholder_cells(&cells);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].img_row, 0);
        assert_eq!(runs[1].img_row, 1);
    }

    #[test]
    fn rle_breaks_on_non_contiguous_col() {
        let cells = [cell(0, 42, 0, 1, 1, 1), cell(1, 42, 0, 1, 3, 1)];
        let runs = scan_placeholder_cells(&cells);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn rle_breaks_on_id_change() {
        let cells = [cell(0, 42, 0, 1, 1, 1), cell(1, 99, 0, 1, 2, 1)];
        let runs = scan_placeholder_cells(&cells);
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn three_cell_run_col_start_correct() {
        let cells = [cell(0, 42, 0, 1, 1, 1), cell(1, 42, 0, 1, 2, 1), cell(2, 42, 0, 1, 3, 1)];
        let runs = scan_placeholder_cells(&cells);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].run_length, 3);
        assert_eq!(runs[0].img_col_start, 0);
    }

    #[test]
    fn msb_diacritic_sets_id_high_byte() {
        // id_hi diacritic = 3 (1-based) → actual msb = 2, full id = 42 | (2<<24).
        let runs = scan_placeholder_cells(&[cell(0, 42, 0, 1, 1, 3)]);
        assert_eq!(runs[0].image_id, 42 | (2 << 24));
    }

    #[test]
    fn fit_to_box_square_image_full_cell() {
        let r = fit_to_box(10, 10, 1, 1, 10, 10, 0, 0, 1).unwrap();
        assert_eq!((r.src_x, r.src_y, r.src_w, r.src_h), (0, 0, 10, 10));
        assert_eq!((r.cell_x_offset, r.cell_y_offset), (0, 0));
    }

    #[test]
    fn fit_to_box_second_column() {
        // 20×10 image in 2×1 box of 10×10 cells. Run at col 1 → src_x=10.
        let r = fit_to_box(20, 10, 2, 1, 10, 10, 1, 0, 1).unwrap();
        assert_eq!(r.src_x, 10);
        assert_eq!(r.src_w, 10);
    }

    #[test]
    fn fit_to_box_returns_none_for_zero_dims() {
        assert!(fit_to_box(0, 10, 1, 1, 10, 10, 0, 0, 1).is_none());
        assert!(fit_to_box(10, 0, 1, 1, 10, 10, 0, 0, 1).is_none());
    }
}
