//! OSC 1337 `File=` inline-image parser — iTerm2 image protocol, v1.
//!
//! # V1 divergences (documented)
//!
//! * **PNG only**: only PNG images are decoded; all other formats (JPEG, GIF, BMP, WebP, …) are
//!   silently skipped with a `debug!` log.  Non-PNG input does **not** produce an error response or
//!   a panic.
//! * **Non-PNG clean skip**: if the `png` crate returns a decode error the image is skipped; no
//!   partial data is stored.
//! * **Animated GIF**: GIF animation is deferred to Phase 8.  For v1, a GIF payload is skipped
//!   entirely (falls under the non-PNG skip rule above).
//! * **`inline=0` is a no-op**: iTerm2 "download" semantics (saving to disk) are not implemented.
//!   When `inline` is absent or 0 the sequence is silently ignored.
//! * **WezTerm #3266 fix**: `doNotMoveCursor=1` on the last row does NOT scroll the terminal; the
//!   cursor stays at its current position.
//! * **MultipartFile / FilePart / FileEnd**: the three-message multipart reassembly protocol is
//!   supported; parts are accumulated in [`MultipartBuffer`] and decoded on `FileEnd`.

use std::sync::Arc;

use crate::graphics::transmission;

/// Maximum OSC 1337 payload size (base64 of the image).
///
/// 32 MiB decoded ≈ 43 MiB base64; cap the raw OSC bytes here.
pub const MAX_OSC1337_LEN: usize = 48 * 1024 * 1024;

// ── Dimension unit ──────────────────────────────────────────────────────────

/// One resolved iTerm2 dimension (width **or** height), before aspect-ratio
/// adjustment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ITermDimension {
    /// Not specified — use the image's native pixel size.
    Auto,
    /// N terminal cells.
    Cells(u32),
    /// N pixels.
    Pixels(u32),
    /// N percent of the viewport dimension (columns×cell_w or rows×cell_h).
    Percent(u32),
}

impl ITermDimension {
    /// Parse a raw value string from the `File=` argument list.
    ///
    /// Forms accepted:
    /// * `""` / absent → [`ITermDimension::Auto`]
    /// * `"0"` → [`ITermDimension::Auto`]
    /// * `"Npx"` → [`ITermDimension::Pixels`]
    /// * `"N%"` → [`ITermDimension::Percent`]
    /// * `"N"` → [`ITermDimension::Cells`]
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.is_empty() || s == "0" {
            return ITermDimension::Auto;
        }
        if let Some(Ok(n)) = s.strip_suffix("px").map(|r| r.trim().parse::<u32>()) {
            return ITermDimension::Pixels(n);
        }
        if let Some(Ok(n)) = s.strip_suffix('%').map(|r| r.trim().parse::<u32>()) {
            return ITermDimension::Percent(n);
        }
        if let Ok(n) = s.parse::<u32>() {
            return ITermDimension::Cells(n);
        }
        ITermDimension::Auto
    }

    /// Convert this dimension to a pixel count.
    ///
    /// * `img_px`     — image native pixel size in this axis.
    /// * `cell_px`    — cell pixel size in this axis (width or height).
    /// * `viewport_px`— total viewport pixels in this axis (cols×cell_w or rows×cell_h).
    pub fn to_pixels(self, img_px: u32, cell_px: u32, viewport_px: u32) -> u32 {
        match self {
            ITermDimension::Auto => img_px,
            ITermDimension::Cells(n) => n.saturating_mul(cell_px),
            ITermDimension::Pixels(n) => n,
            ITermDimension::Percent(pct) => {
                ((viewport_px as u64 * pct as u64) / 100).min(u32::MAX as u64) as u32
            },
        }
    }
}

// ── Parsed argument block ────────────────────────────────────────────────────

/// Parsed arguments from an OSC 1337 `File=` header.
///
/// Only the keys relevant to rendering are extracted; unknown keys are
/// silently ignored (forward-compatibility).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileArgs {
    /// Base file name (informational only in v1).
    pub name: Option<String>,
    /// Declared byte count of the decoded payload (informational; we do not
    /// validate it against the actual decode size).
    pub size: Option<u64>,
    /// Requested display width.
    pub width: ITermDimension,
    /// Requested display height.
    pub height: ITermDimension,
    /// Whether to preserve the image's natural aspect ratio.
    pub preserve_aspect_ratio: bool,
    /// Whether to display inline (true) or download (false).
    ///
    /// When false the whole sequence is a no-op (v1 does not implement
    /// download semantics).
    pub inline: bool,
    /// When true the cursor is NOT moved after the image is placed.
    pub do_not_move_cursor: bool,
}

impl Default for FileArgs {
    fn default() -> Self {
        Self {
            name: None,
            size: None,
            width: ITermDimension::Auto,
            height: ITermDimension::Auto,
            preserve_aspect_ratio: true,
            inline: false,
            do_not_move_cursor: false,
        }
    }
}

impl FileArgs {
    /// Parse the `key=value;key=value` header that follows `File=` (or
    /// `MultipartFile=`).
    ///
    /// The header terminates at the first `:` which introduces the base64
    /// payload.  This function receives only the header slice (no colon,
    /// no payload).
    pub fn parse(header: &str) -> Self {
        let mut args = FileArgs::default();
        for pair in header.split(';') {
            let mut it = pair.splitn(2, '=');
            let key = match it.next() {
                Some(k) => k.trim().to_ascii_lowercase(),
                None => continue,
            };
            let val = it.next().unwrap_or("").trim();
            match key.as_str() {
                "name" => args.name = Some(val.to_owned()),
                "size" => args.size = val.parse().ok(),
                "width" => args.width = ITermDimension::parse(val),
                "height" => args.height = ITermDimension::parse(val),
                "preserveaspectratio" => args.preserve_aspect_ratio = val != "0",
                "inline" => args.inline = val == "1",
                "donotmovecursor" => args.do_not_move_cursor = val == "1",
                _ => {}, // forward-compatible: ignore unknown keys
            }
        }
        args
    }
}

// ── Dimension resolution ─────────────────────────────────────────────────────

/// Cell and viewport metrics needed for dimension resolution.
#[derive(Debug, Clone, Copy)]
pub struct CellMetrics {
    /// Cell width in pixels.
    pub cell_w: u32,
    /// Cell height in pixels.
    pub cell_h: u32,
    /// Terminal width in cells.
    pub cols: u32,
    /// Terminal height in cells.
    pub rows: u32,
}

impl CellMetrics {
    /// Total viewport width in pixels.
    pub fn viewport_w(&self) -> u32 {
        self.cols.saturating_mul(self.cell_w)
    }

    /// Total viewport height in pixels.
    pub fn viewport_h(&self) -> u32 {
        self.rows.saturating_mul(self.cell_h)
    }
}

/// Resolve `(width_dim, height_dim)` → `(display_w_px, display_h_px)`.
///
/// Implements the WezTerm `ITermDimension` resolution rules:
///
/// 1. Convert each dimension to pixels using [`ITermDimension::to_pixels`].
/// 2. If `preserve_aspect_ratio` is true AND both were specified, fit the image within the
///    requested box without stretching (letterbox).
/// 3. If only ONE dimension was specified and `preserve_aspect_ratio` is true, derive the other
///    from the native aspect ratio.
/// 4. `preserve_aspect_ratio=0` → stretch to exactly the requested box.
pub fn resolve_dimensions(
    width_dim: ITermDimension,
    height_dim: ITermDimension,
    img_w: u32,
    img_h: u32,
    preserve_ar: bool,
    metrics: CellMetrics,
) -> (u32, u32) {
    // Avoid divide-by-zero for degenerate images.
    if img_w == 0 || img_h == 0 {
        return (0, 0);
    }

    let w_auto = width_dim == ITermDimension::Auto;
    let h_auto = height_dim == ITermDimension::Auto;

    let req_w = width_dim.to_pixels(img_w, metrics.cell_w, metrics.viewport_w());
    let req_h = height_dim.to_pixels(img_h, metrics.cell_h, metrics.viewport_h());

    match (w_auto, h_auto, preserve_ar) {
        // Both auto → native size.
        (true, true, _) => (img_w, img_h),

        // Both specified, preserve AR → letterbox.
        (false, false, true) => {
            let scale_w = req_w as f64 / img_w as f64;
            let scale_h = req_h as f64 / img_h as f64;
            let scale = scale_w.min(scale_h);
            (
                ((img_w as f64 * scale).ceil() as u32).max(1),
                ((img_h as f64 * scale).ceil() as u32).max(1),
            )
        },

        // Both specified, no preserve AR → stretch.
        (false, false, false) => (req_w.max(1), req_h.max(1)),

        // Only width specified, preserve AR → derive height.
        (false, true, true) => {
            let h = (req_w as f64 * img_h as f64 / img_w as f64).ceil() as u32;
            (req_w.max(1), h.max(1))
        },

        // Only width specified, no preserve AR → auto height.
        (false, true, false) => (req_w.max(1), img_h),

        // Only height specified, preserve AR → derive width.
        (true, false, true) => {
            let w = (req_h as f64 * img_w as f64 / img_h as f64).ceil() as u32;
            (w.max(1), req_h.max(1))
        },

        // Only height specified, no preserve AR → auto width.
        (true, false, false) => (img_w, req_h.max(1)),
    }
}

// ── PNG decode ───────────────────────────────────────────────────────────────

/// Decode a PNG byte slice into `(width, height, rgba_bytes)`, or `None` on
/// any error. Delegates to [`transmission::decode_png`] for correct 16-bit
/// normalisation and dimension capping.
pub fn decode_png(data: &[u8]) -> Option<(u32, u32, Arc<Vec<u8>>)> {
    let (w, h, rgba) = transmission::decode_png(data).ok()?;
    Some((w, h, Arc::new(rgba)))
}

// ── Multipart reassembly ─────────────────────────────────────────────────────

/// Accumulator for iTerm2 multipart image transfers.
///
/// iTerm2 multipart:
/// * `OSC 1337 ; MultipartFile = <args> : <base64-chunk> ST`  — starts a transfer, `args` carry the
///   same `File=` key-val set.
/// * `OSC 1337 ; FilePart = <base64-chunk> ST`  — continues it.
/// * `OSC 1337 ; FileEnd = <base64-chunk> ST`  — final chunk; triggers decode.
#[derive(Debug, Default)]
pub struct MultipartBuffer {
    /// Saved args from `MultipartFile=`.
    pub args: Option<FileArgs>,
    /// Accumulated raw (not-yet-decoded) base64 bytes.
    pub base64: Vec<u8>,
    /// Whether the buffer is currently active (between MultipartFile and
    /// FileEnd).
    pub active: bool,
}

impl MultipartBuffer {
    /// Start a new multipart transfer, discarding any prior incomplete one.
    pub fn start(&mut self, args: FileArgs, initial_b64: &[u8]) {
        self.args = Some(args);
        self.base64.clear();
        self.base64.extend_from_slice(initial_b64);
        self.active = true;
    }

    /// Append a `FilePart=` chunk.
    pub fn append(&mut self, chunk: &[u8]) {
        if self.active {
            // Cap growth to avoid unbounded allocation.
            let remaining = MAX_OSC1337_LEN.saturating_sub(self.base64.len());
            let take = chunk.len().min(remaining);
            self.base64.extend_from_slice(&chunk[..take]);
        }
    }

    /// Finish the transfer, returning `(args, full_base64)` if active.
    pub fn finish(&mut self, final_chunk: &[u8]) -> Option<(FileArgs, Vec<u8>)> {
        if !self.active {
            return None;
        }
        self.append(final_chunk);
        self.active = false;
        let args = self.args.take()?;
        let b64 = std::mem::take(&mut self.base64);
        Some((args, b64))
    }

    /// Discard any in-progress transfer.
    pub fn reset(&mut self) {
        self.args = None;
        self.base64.clear();
        self.active = false;
    }
}

// ── OSC 1337 payload splitter ────────────────────────────────────────────────

/// Parse an OSC 1337 parameter block (the bytes after `1337;`).
///
/// Returns `(kind, args_header, base64_payload)` where `kind` is one of
/// `"File"`, `"MultipartFile"`, `"FilePart"`, `"FileEnd"`, or an unrecognised
/// string.
///
/// The colon `:` separates the `<keyword>=<header>` part from the base64
/// payload.
pub fn split_osc1337(payload: &[u8]) -> Option<(&[u8], &str, &[u8])> {
    // Find the '=' that terminates the keyword.
    let eq_pos = payload.iter().position(|&b| b == b'=')?;
    let keyword = &payload[..eq_pos];
    let rest = &payload[eq_pos + 1..];

    // `File=` and `MultipartFile=` carry a `key=val;…:base64` body where ':'
    // separates the argument header from the base64 payload.
    // `FilePart=` and `FileEnd=` carry only raw base64 with no header or ':'.
    let (header_bytes, b64) = match keyword {
        b"File" | b"MultipartFile" => {
            if let Some(colon) = rest.iter().position(|&b| b == b':') {
                (&rest[..colon], &rest[colon + 1..])
            } else {
                // Malformed: no ':' separator; treat all as b64 with empty header.
                (&b""[..], rest)
            }
        },
        _ => (&b""[..], rest),
    };

    let header = std::str::from_utf8(header_bytes).ok()?;
    Some((keyword, header, b64))
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── ITermDimension parsing ────────────────────────────────────────────

    #[test]
    fn dimension_auto_on_empty() {
        assert_eq!(ITermDimension::parse(""), ITermDimension::Auto);
    }

    #[test]
    fn dimension_auto_on_zero() {
        assert_eq!(ITermDimension::parse("0"), ITermDimension::Auto);
    }

    #[test]
    fn dimension_cells() {
        assert_eq!(ITermDimension::parse("10"), ITermDimension::Cells(10));
    }

    #[test]
    fn dimension_pixels() {
        assert_eq!(ITermDimension::parse("200px"), ITermDimension::Pixels(200));
    }

    #[test]
    fn dimension_percent() {
        assert_eq!(ITermDimension::parse("50%"), ITermDimension::Percent(50));
    }

    // ── to_pixels ─────────────────────────────────────────────────────────

    #[test]
    fn to_pixels_auto_returns_img_native() {
        assert_eq!(ITermDimension::Auto.to_pixels(100, 8, 800), 100);
    }

    #[test]
    fn to_pixels_cells() {
        // 4 cells × 8 px/cell = 32 px.
        assert_eq!(ITermDimension::Cells(4).to_pixels(100, 8, 800), 32);
    }

    #[test]
    fn to_pixels_pixels() {
        assert_eq!(ITermDimension::Pixels(123).to_pixels(100, 8, 800), 123);
    }

    #[test]
    fn to_pixels_percent_50() {
        // 50% of 800 = 400.
        assert_eq!(ITermDimension::Percent(50).to_pixels(100, 8, 800), 400);
    }

    // ── resolve_dimensions ────────────────────────────────────────────────

    fn metrics() -> CellMetrics {
        CellMetrics { cell_w: 8, cell_h: 16, cols: 80, rows: 24 }
    }

    #[test]
    fn resolve_both_auto_returns_native() {
        let (w, h) = resolve_dimensions(
            ITermDimension::Auto,
            ITermDimension::Auto,
            100,
            50,
            true,
            metrics(),
        );
        assert_eq!((w, h), (100, 50));
    }

    #[test]
    fn resolve_width_cells_height_auto_with_ar() {
        // 10 cells × 8 px/cell = 80 px wide; height derived from AR 100:50 → 40 px.
        let (w, h) = resolve_dimensions(
            ITermDimension::Cells(10),
            ITermDimension::Auto,
            100,
            50,
            true,
            metrics(),
        );
        assert_eq!(w, 80);
        assert_eq!(h, 40);
    }

    #[test]
    fn resolve_height_cells_width_auto_with_ar() {
        // 5 cells × 16 px/cell = 80 px tall; width derived from AR 100:50 → 160 px.
        let (w, h) = resolve_dimensions(
            ITermDimension::Auto,
            ITermDimension::Cells(5),
            100,
            50,
            true,
            metrics(),
        );
        assert_eq!(h, 80);
        assert_eq!(w, 160);
    }

    #[test]
    fn resolve_both_specified_no_ar_stretches() {
        // 10 cells × 8 px = 80 wide, 5 cells × 16 px = 80 tall.
        // No AR → stretch to exactly 80×80.
        let (w, h) = resolve_dimensions(
            ITermDimension::Cells(10),
            ITermDimension::Cells(5),
            100,
            50,
            false,
            metrics(),
        );
        assert_eq!((w, h), (80, 80));
    }

    #[test]
    fn resolve_both_specified_with_ar_letterboxes() {
        // Requested box: 80×80.  Image: 100×50 (AR=2:1).
        // scale_w = 80/100 = 0.8, scale_h = 80/50 = 1.6 → min = 0.8.
        // Result: 80 × 40.
        let (w, h) = resolve_dimensions(
            ITermDimension::Cells(10),
            ITermDimension::Cells(5),
            100,
            50,
            true,
            metrics(),
        );
        assert_eq!(w, 80);
        assert_eq!(h, 40);
    }

    #[test]
    fn resolve_pixels_form() {
        let (w, h) = resolve_dimensions(
            ITermDimension::Pixels(200),
            ITermDimension::Auto,
            100,
            50,
            false,
            metrics(),
        );
        assert_eq!(w, 200);
        assert_eq!(h, 50); // auto → native
    }

    #[test]
    fn resolve_percent_form() {
        // 50% of 80 cols × 8 px = 50% of 640 = 320 px wide; height auto.
        let (w, h) = resolve_dimensions(
            ITermDimension::Percent(50),
            ITermDimension::Auto,
            100,
            50,
            false,
            metrics(),
        );
        assert_eq!(w, 320);
        assert_eq!(h, 50);
    }

    // ── FileArgs parsing ──────────────────────────────────────────────────

    #[test]
    fn parse_args_basic() {
        let args = FileArgs::parse("inline=1;width=10;height=5px;preserveAspectRatio=0");
        assert!(args.inline);
        assert_eq!(args.width, ITermDimension::Cells(10));
        assert_eq!(args.height, ITermDimension::Pixels(5));
        assert!(!args.preserve_aspect_ratio);
    }

    #[test]
    fn parse_args_defaults() {
        let args = FileArgs::default();
        assert!(!args.inline);
        assert!(args.preserve_aspect_ratio);
        assert_eq!(args.width, ITermDimension::Auto);
        assert_eq!(args.height, ITermDimension::Auto);
        assert!(!args.do_not_move_cursor);
    }

    #[test]
    fn parse_args_do_not_move_cursor() {
        let args = FileArgs::parse("inline=1;doNotMoveCursor=1");
        assert!(args.do_not_move_cursor);
    }

    #[test]
    fn parse_args_inline_zero_not_inline() {
        let args = FileArgs::parse("inline=0");
        assert!(!args.inline);
    }

    // ── split_osc1337 ─────────────────────────────────────────────────────

    #[test]
    fn split_file_with_payload() {
        let input = b"File=inline=1;width=10:AAAA";
        let (kw, header, b64) = split_osc1337(input).unwrap();
        assert_eq!(kw, b"File");
        assert_eq!(header, "inline=1;width=10");
        assert_eq!(b64, b"AAAA");
    }

    #[test]
    fn split_multipart_file() {
        let input = b"MultipartFile=inline=1:BBBB";
        let (kw, header, b64) = split_osc1337(input).unwrap();
        assert_eq!(kw, b"MultipartFile");
        assert_eq!(header, "inline=1");
        assert_eq!(b64, b"BBBB");
    }

    #[test]
    fn split_file_part() {
        let input = b"FilePart=CCCC";
        let (kw, header, b64) = split_osc1337(input).unwrap();
        assert_eq!(kw, b"FilePart");
        assert_eq!(header, "");
        assert_eq!(b64, b"CCCC");
    }

    #[test]
    fn split_file_end() {
        let input = b"FileEnd=DDDD";
        let (kw, header, b64) = split_osc1337(input).unwrap();
        assert_eq!(kw, b"FileEnd");
        assert_eq!(header, "");
        assert_eq!(b64, b"DDDD");
    }

    // ── MultipartBuffer ───────────────────────────────────────────────────

    #[test]
    fn multipart_reassembly() {
        let mut buf = MultipartBuffer::default();
        let args = FileArgs { inline: true, ..Default::default() };
        buf.start(args.clone(), b"AAAA");
        buf.append(b"BBBB");
        let (out_args, b64) = buf.finish(b"CCCC").unwrap();
        assert_eq!(out_args, args);
        assert_eq!(b64, b"AAAABBBBCCCC");
        assert!(!buf.active);
    }

    #[test]
    fn multipart_finish_without_start_returns_none() {
        let mut buf = MultipartBuffer::default();
        assert!(buf.finish(b"AAAA").is_none());
    }

    #[test]
    fn multipart_reset_clears_state() {
        let mut buf = MultipartBuffer::default();
        let args = FileArgs::default();
        buf.start(args, b"AAAA");
        buf.reset();
        assert!(!buf.active);
        assert!(buf.args.is_none());
        assert!(buf.base64.is_empty());
    }

    // ── decode_png ────────────────────────────────────────────────────────

    #[test]
    fn non_png_bytes_return_none() {
        // A valid JPEG magic number — should return None cleanly.
        let jpeg_magic = &[0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        assert!(decode_png(jpeg_magic).is_none());
    }

    #[test]
    fn empty_bytes_return_none() {
        assert!(decode_png(&[]).is_none());
    }
}
