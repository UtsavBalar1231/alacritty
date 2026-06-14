//! Kitty graphics transmission layer.
//!
//! Implements the data-loading half of kitty's `handle_add_command`
//! (`kitty/graphics.c`): the four transmission mediums (`t=d/f/t/s`), the
//! single-slot chunking state machine (`m=`), zlib decompression (`o=z`) and
//! PNG decoding (`f=100`), producing a finished RGBA8 image for
//! [`GraphicsManager::add_image`].
//!
//! # Differences from kitty
//!
//! * Kitty accumulates the full compressed payload and inflates it once on the final chunk. Here
//!   `o=z` payloads are decompressed *as each chunk arrives* through a persistent [`ZlibStream`],
//!   so per-chunk work on the PTY thread stays bounded and compressed data is never buffered in
//!   full. Consequence: a compressed non-PNG payload whose *compressed* size exceeds kitty's
//!   accumulation buffer no longer fails with `EFBIG` as long as the decompressed size fits; the
//!   decompressed-size cap (kitty's `Z_BUF_ERROR` → `EINVAL`) is enforced instead.
//! * Files are read instead of mmapped, with the `S=`/`O=` range and the 400 MB cap enforced
//!   *during* the read.
//! * Kitty answers an over-dimension *decoded* PNG with `ENOMEM` (`kitty/png-reader.c:69`); per the
//!   plan's contract this implementation uses `EFBIG` for that case. Declared over-dimensions
//!   (`s=`/`v=`) keep kitty's `EINVAL` (`kitty/graphics.c:720`).
//! * `f=24` (RGB) data is expanded to RGBA8 at completion since the storage layer is RGBA-only;
//!   kitty keeps 3-byte data and uploads it as-is.

use std::mem;
use std::sync::Arc;

use flate2::{Decompress, FlushDecompress, Status};

use crate::graphics::kitty_command::{CommandError, ErrorCode, GraphicsCommand};
use crate::graphics::{GraphicsManager, LoadData};

/// Maximum total transmitted data size (kitty's `MAX_DATA_SZ`, 400 MB).
pub const MAX_DATA_SZ: usize = 4 * 100_000_000;

/// Maximum image dimension in pixels (kitty's `MAX_IMAGE_DIMENSION`).
pub const MAX_IMAGE_DIMENSION: u32 = 10_000;

/// Maximum file path length for `t=f/t/s` payloads (kitty caps at 2048).
pub const MAX_FILE_PATH: usize = 2048;

/// Expected PNG size when `S=` is omitted (kitty defaults to 100 KiB).
const DEFAULT_PNG_DATA_SZ: usize = 100 * 1024;

/// Kitty's slack on the uncompressed direct accumulation buffer.
const DIRECT_SLACK: usize = 10;

const FMT_RGB: u32 = 24;
const FMT_RGBA: u32 = 32;
const FMT_PNG: u32 = 100;

/// A fully received and decoded transmission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompletedLoad {
    /// The saved first-chunk command: identity (`i=`/`I=`), `q=`, action and
    /// placement keys for the response/display handling in the caller.
    pub start: GraphicsCommand,

    /// Decoded image width in pixels.
    pub width: u32,

    /// Decoded image height in pixels.
    pub height: u32,

    /// RGBA8 pixel data, ready for [`GraphicsManager::add_image`].
    pub data: Arc<Vec<u8>>,
}

/// Outcome of processing one transmission command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransmissionResult {
    /// The image is fully transferred and decoded.
    Complete(CompletedLoad),

    /// `m=1`: the chunk was accumulated and more chunks are expected.
    MoreDataNeeded,
}

/// Streaming zlib decompressor for `o=z` payloads.
#[derive(Debug)]
pub struct ZlibStream {
    stream: Decompress,
    ended: bool,
}

impl ZlibStream {
    fn new() -> Self {
        Self { stream: Decompress::new(true), ended: false }
    }

    /// Decompress `input` into `out`, capping output at `data_sz` bytes.
    ///
    /// `out` must have at least `data_sz + 1` bytes of capacity; the spare
    /// byte lets the stream trailer be consumed once the output is full and
    /// makes any overflow observable. Producing more than `data_sz` bytes is
    /// kitty's `Z_BUF_ERROR` path and fails with `EINVAL` *before* any
    /// allocation beyond the cap (zlib bomb defense). Input past the end of
    /// the stream is ignored, like kitty's single `inflate(Z_FINISH)` call.
    fn push(
        &mut self,
        mut input: &[u8],
        out: &mut Vec<u8>,
        data_sz: usize,
    ) -> Result<(), CommandError> {
        while !self.ended && !input.is_empty() {
            let before_in = self.stream.total_in();
            let before_out = out.len();
            let status =
                self.stream.decompress_vec(input, out, FlushDecompress::None).map_err(|e| {
                    err(ErrorCode::EINVAL, format!("Failed to inflate image data with error: {e}"))
                })?;
            let consumed = (self.stream.total_in() - before_in) as usize;
            input = &input[consumed..];
            if out.len() > data_sz {
                return Err(inflated_size_mismatch());
            }
            match status {
                Status::StreamEnd => self.ended = true,
                // No progress is possible: the stream needs more input.
                _ if consumed == 0 && out.len() == before_out => break,
                Status::Ok | Status::BufError => (),
            }
        }
        Ok(())
    }

    /// Finalize the stream; the decompressed size must equal `data_sz`.
    fn finish(&mut self, out: &mut Vec<u8>, data_sz: usize) -> Result<(), CommandError> {
        if !self.ended {
            let status =
                self.stream.decompress_vec(&[], out, FlushDecompress::Finish).map_err(|e| {
                    err(ErrorCode::EINVAL, format!("Failed to inflate image data with error: {e}"))
                })?;
            if out.len() > data_sz {
                return Err(inflated_size_mismatch());
            }
            if status != Status::StreamEnd {
                return Err(err(
                    ErrorCode::EINVAL,
                    "Failed to inflate image data: truncated stream",
                ));
            }
            self.ended = true;
        }
        if out.len() != data_sz {
            return Err(inflated_size_mismatch());
        }
        Ok(())
    }
}

impl GraphicsManager {
    /// Process the transmission part of a graphics command (`a=t/T/q`).
    ///
    /// Mirrors the load half of kitty's `handle_add_command`: a resolved
    /// `t=d` while a chunked load is in flight is a continuation and restores
    /// the saved first-chunk metadata; anything else starts a new
    /// transmission, aborting the in-flight load. File mediums complete in a
    /// single command even with `m=1` (kitty behavior). All errors abort the
    /// in-flight load (kitty's `ABRT` frees `currently_loading`).
    pub fn handle_transmission(
        &mut self,
        mut cmd: GraphicsCommand,
    ) -> Result<TransmissionResult, CommandError> {
        let tt = if cmd.transmission_type == 0 { b'd' } else { cmd.transmission_type };
        let payload = mem::take(&mut cmd.payload);
        let more = cmd.more != 0;

        let mut load = if tt == b'd' && self.loading().is_some() {
            self.take_loading().unwrap()
        } else {
            self.abort_load();
            begin_load(cmd)?
        };

        match tt {
            b'd' => {
                accumulate_direct(&mut load, &payload)?;
                if more {
                    self.start_load(load);
                    return Ok(TransmissionResult::MoreDataNeeded);
                }
            },
            b'f' | b't' | b's' => {
                let bytes = read_path_source(tt, &payload, &load.start)?;
                accumulate_file(&mut load, bytes)?;
            },
            _ => {
                return Err(err(
                    ErrorCode::EINVAL,
                    format!("Unknown transmission type: {}", tt as char),
                ));
            },
        }

        finish_load(load).map(TransmissionResult::Complete)
    }
}

/// `f=` with kitty's default of RGBA when absent.
fn effective_format(cmd: &GraphicsCommand) -> u32 {
    if cmd.format == 0 { FMT_RGBA } else { cmd.format }
}

/// Validate the first chunk and set up the load slot
/// (kitty's `initialize_load_data` plus the dimension check at
/// `handle_add_command`, graphics.c:720).
fn begin_load(cmd: GraphicsCommand) -> Result<LoadData, CommandError> {
    if cmd.data_width > MAX_IMAGE_DIMENSION || cmd.data_height > MAX_IMAGE_DIMENSION {
        return Err(err(
            ErrorCode::EINVAL,
            format!("Image too large, width or height greater than {MAX_IMAGE_DIMENSION}"),
        ));
    }

    let data_sz = match effective_format(&cmd) {
        FMT_PNG => {
            if cmd.data_sz as usize > MAX_DATA_SZ {
                return Err(err(ErrorCode::EINVAL, "PNG data size too large"));
            }
            if cmd.data_sz != 0 { cmd.data_sz as usize } else { DEFAULT_PNG_DATA_SZ }
        },
        fmt @ (FMT_RGB | FMT_RGBA) => {
            let sz = cmd.data_width as usize * cmd.data_height as usize * (fmt as usize / 8);
            if sz == 0 {
                return Err(err(ErrorCode::EINVAL, "Zero width/height not allowed"));
            }
            sz
        },
        fmt => return Err(err(ErrorCode::EINVAL, format!("Unknown image format: {fmt}"))),
    };

    let compressed = cmd.compressed != 0;
    Ok(LoadData {
        start: cmd,
        data_sz,
        // The decompressed-size cap is the allocation: `data_sz + 1` is the
        // most the buffer will ever hold (see `ZlibStream::push`).
        buf: if compressed { Vec::with_capacity(data_sz + 1) } else { Vec::new() },
        inflate: compressed.then(ZlibStream::new),
    })
}

/// Accumulate one `t=d` chunk (kitty `load_image_data`, direct case).
fn accumulate_direct(load: &mut LoadData, chunk: &[u8]) -> Result<(), CommandError> {
    if let Some(inflate) = load.inflate.as_mut() {
        return inflate.push(chunk, &mut load.buf, load.data_sz);
    }
    let cap = if effective_format(&load.start) == FMT_PNG {
        MAX_DATA_SZ
    } else {
        load.data_sz + DIRECT_SLACK
    };
    if load.buf.len() + chunk.len() > cap {
        return Err(err(ErrorCode::EFBIG, "Too much data"));
    }
    load.buf.extend_from_slice(chunk);
    Ok(())
}

/// Feed bytes read from a file/shm source into the load slot.
fn accumulate_file(load: &mut LoadData, bytes: Vec<u8>) -> Result<(), CommandError> {
    match load.inflate.as_mut() {
        Some(inflate) => inflate.push(&bytes, &mut load.buf, load.data_sz),
        // Size caps were enforced during the read; unlike `t=d`, extra bytes
        // beyond `data_sz` are silently ignored (kitty mmaps whatever the
        // file contains and uses the first `data_sz` bytes).
        None => {
            load.buf = bytes;
            Ok(())
        },
    }
}

/// Read the data for a `t=f/t/s` transmission, whose payload is a path.
///
/// Kitty reference: `load_image_data` (graphics.c:566-598). The file is
/// opened `O_RDONLY|O_NONBLOCK|O_CLOEXEC` (`O_NONBLOCK` so a FIFO does not
/// block the PTY thread). `t=t` unlinks the file only if the path contains
/// `tty-graphics-protocol` *and* resolves into a temp directory; `t=s`
/// always unlinks the shm object once it was opened, even if reading failed.
fn read_path_source(
    tt: u8,
    payload: &[u8],
    start: &GraphicsCommand,
) -> Result<Vec<u8>, CommandError> {
    if payload.len() > MAX_FILE_PATH {
        return Err(err(ErrorCode::EINVAL, "Filename too long"));
    }
    // Kitty snprintf's the payload into a C string: stop at the first NUL.
    let raw = &payload[..payload.iter().position(|&b| b == 0).unwrap_or(payload.len())];
    let size = start.data_sz as usize;
    let offset = u64::from(start.data_offset);

    imp::read_path_source(tt, raw, size, offset)
}

#[cfg(unix)]
mod imp {
    use std::ffi::OsStr;
    use std::fs::{File, OpenOptions};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    use super::*;

    pub(super) fn read_path_source(
        tt: u8,
        raw: &[u8],
        size: usize,
        offset: u64,
    ) -> Result<Vec<u8>, CommandError> {
        let name = OsStr::from_bytes(raw);
        if tt == b's' {
            let fd =
                rustix::shm::open(name, rustix::shm::OFlags::RDONLY, rustix::fs::Mode::empty())
                    .map_err(|e| open_failed(e.to_string()))?;
            // Always unlink once opened, even if the read below fails.
            let result = read_range(File::from(fd), size, offset);
            let _ = rustix::shm::unlink(name);
            return result;
        }

        let path = Path::new(name);
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| open_failed(e.to_string()))?;
        let result = read_range(file, size, offset);
        if tt == b't' && should_unlink_temp(path) {
            let _ = std::fs::remove_file(path);
        }
        result
    }

    /// `t=t` deletion safety rule: the path must contain
    /// `tty-graphics-protocol` (kitty graphics.c:590) and resolve into
    /// `/tmp`, `/dev/shm` or `$TMPDIR` (kitty's `safe_delete_temp_file`).
    fn should_unlink_temp(path: &Path) -> bool {
        const MARKER: &[u8] = b"tty-graphics-protocol";
        let bytes = path.as_os_str().as_bytes();
        if !bytes.windows(MARKER.len()).any(|w| w == MARKER) {
            return false;
        }
        let Ok(resolved) = path.canonicalize() else { return false };
        is_safe_temp_dir(
            &resolved,
            std::env::var_os("TMPDIR").map(std::path::PathBuf::from).as_deref(),
        )
    }

    pub(super) fn is_safe_temp_dir(resolved: &Path, tmpdir: Option<&Path>) -> bool {
        let under = |dir: &Path| dir.canonicalize().is_ok_and(|dir| resolved.starts_with(dir));
        under(Path::new("/tmp")) || under(Path::new("/dev/shm")) || tmpdir.is_some_and(under)
    }
}

#[cfg(not(unix))]
mod imp {
    use std::fs::File;
    use std::path::Path;

    use super::*;

    pub(super) fn read_path_source(
        tt: u8,
        raw: &[u8],
        size: usize,
        offset: u64,
    ) -> Result<Vec<u8>, CommandError> {
        if tt == b's' {
            return Err(open_failed("POSIX shared memory is not supported on this platform"));
        }
        let path = Path::new(std::str::from_utf8(raw).map_err(|e| open_failed(e.to_string()))?);
        let file = File::open(path).map_err(|e| open_failed(e.to_string()))?;
        // `t=t` never unlinks here: the safety rule requires a POSIX temp dir.
        read_range(file, size, offset)
    }
}

/// Read `size` bytes (or everything, if `S=` was omitted) starting at
/// `offset`, enforcing the 400 MB cap during the read.
fn read_range(mut file: std::fs::File, size: usize, offset: u64) -> Result<Vec<u8>, CommandError> {
    use std::io::{Read, Seek, SeekFrom};

    if size > MAX_DATA_SZ {
        return Err(err(ErrorCode::EFBIG, "Too much data"));
    }
    if offset != 0 {
        file.seek(SeekFrom::Start(offset)).map_err(|e| open_failed(e.to_string()))?;
    }
    let limit = if size != 0 { size as u64 } else { MAX_DATA_SZ as u64 + 1 };
    let mut out = Vec::new();
    file.take(limit).read_to_end(&mut out).map_err(|e| open_failed(e.to_string()))?;
    if out.len() > MAX_DATA_SZ {
        return Err(err(ErrorCode::EFBIG, "Too much data"));
    }
    Ok(out)
}

/// Finalize a completed transmission (kitty `process_image_data` plus the
/// final size check in `handle_add_command`).
fn finish_load(mut load: LoadData) -> Result<CompletedLoad, CommandError> {
    if let Some(mut inflate) = load.inflate.take() {
        inflate.finish(&mut load.buf, load.data_sz)?;
    }

    let fmt = effective_format(&load.start);
    let (width, height, rgba) = if fmt == FMT_PNG {
        decode_png(&load.buf)?
    } else {
        if load.buf.len() < load.data_sz {
            return Err(err(
                ErrorCode::ENODATA,
                format!("Insufficient image data: {} < {}", load.buf.len(), load.data_sz),
            ));
        }
        load.buf.truncate(load.data_sz);
        let rgba = if fmt == FMT_RGB { rgb_to_rgba(&load.buf) } else { mem::take(&mut load.buf) };
        (load.start.data_width, load.start.data_height, rgba)
    };

    Ok(CompletedLoad { start: load.start, width, height, data: Arc::new(rgba) })
}

/// Decode a PNG payload to RGBA8 (kitty `inflate_png` via `inflate_png_inner`, png-reader.c).
///
/// Shared by the kitty and iTerm2 paths. Applies `normalize_to_color8()` so
/// 16-bit PNGs are normalised before pixel data is read, enforces
/// `MAX_IMAGE_DIMENSION`, and caps memory via `MAX_DATA_SZ`.
pub fn decode_png(data: &[u8]) -> Result<(u32, u32, Vec<u8>), CommandError> {
    let mut decoder = png::Decoder::new_with_limits(std::io::Cursor::new(data), png::Limits {
        bytes: MAX_DATA_SZ,
    });
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(png_failed)?;

    let info = reader.info();
    let (width, height) = (info.width, info.height);
    // Dimension check before the output buffer is allocated. Kitty responds
    // ENOMEM here (png-reader.c:69); EFBIG per the plan's error contract.
    if width > MAX_IMAGE_DIMENSION || height > MAX_IMAGE_DIMENSION {
        return Err(err(ErrorCode::EFBIG, "PNG image is too large"));
    }

    let size = reader
        .output_buffer_size()
        .ok_or_else(|| err(ErrorCode::EBADPNG, "PNG output size overflows"))?;
    let mut out = vec![0; size];
    let frame = reader.next_frame(&mut out).map_err(png_failed)?;
    out.truncate(frame.buffer_size());

    // `normalize_to_color8` leaves one of these four 8-bit layouts.
    let rgba = match frame.color_type {
        png::ColorType::Rgba => out,
        png::ColorType::Rgb => rgb_to_rgba(&out),
        png::ColorType::Grayscale => out.iter().flat_map(|&l| [l, l, l, 0xff]).collect(),
        png::ColorType::GrayscaleAlpha => {
            out.chunks_exact(2).flat_map(|la| [la[0], la[0], la[0], la[1]]).collect()
        },
        png::ColorType::Indexed => {
            return Err(err(ErrorCode::EBADPNG, "Unexpected indexed PNG output"));
        },
    };
    Ok((width, height, rgba))
}

fn rgb_to_rgba(rgb: &[u8]) -> Vec<u8> {
    rgb.chunks_exact(3).flat_map(|px| [px[0], px[1], px[2], 0xff]).collect()
}

fn err(code: ErrorCode, message: impl Into<String>) -> CommandError {
    CommandError { code, message: message.into(), sends_response: true }
}

fn open_failed(detail: impl std::fmt::Display) -> CommandError {
    err(
        ErrorCode::EBADF,
        format!("Failed to open file for graphics transmission with error: {detail}"),
    )
}

fn inflated_size_mismatch() -> CommandError {
    err(ErrorCode::EINVAL, "Image data size post inflation does not match expected size")
}

fn png_failed(e: png::DecodingError) -> CommandError {
    match e {
        png::DecodingError::LimitsExceeded => {
            err(ErrorCode::ENOMEM, "Out of memory allocating decompression buffer for PNG")
        },
        e => err(ErrorCode::EBADPNG, e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn direct(width: u32, height: u32, format: u32, payload: &[u8], more: u32) -> GraphicsCommand {
        GraphicsCommand {
            data_width: width,
            data_height: height,
            format,
            more,
            payload: payload.to_vec(),
            ..Default::default()
        }
    }

    fn complete(mgr: &mut GraphicsManager, cmd: GraphicsCommand) -> CompletedLoad {
        match mgr.handle_transmission(cmd).unwrap() {
            TransmissionResult::Complete(load) => load,
            TransmissionResult::MoreDataNeeded => panic!("expected a completed load"),
        }
    }

    fn error_code(mgr: &mut GraphicsManager, cmd: GraphicsCommand) -> ErrorCode {
        let err = mgr.handle_transmission(cmd).unwrap_err();
        assert!(err.sends_response, "transmission errors must send a response: {err}");
        err.code
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn encode_png(width: u32, height: u32, color: png::ColorType, data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut enc = png::Encoder::new(&mut out, width, height);
        enc.set_color(color);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc.write_header().unwrap();
        writer.write_image_data(data).unwrap();
        writer.finish().unwrap();
        out
    }

    /// 2x2 RGBA test pixels.
    const RGBA_2X2: [u8; 16] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];

    #[test]
    fn direct_rgba_single_chunk() {
        let mut mgr = GraphicsManager::new();
        let load = complete(&mut mgr, direct(2, 2, 32, &RGBA_2X2, 0));
        assert_eq!((load.width, load.height), (2, 2));
        assert_eq!(*load.data, RGBA_2X2);
        assert!(mgr.loading().is_none());
    }

    #[test]
    fn format_defaults_to_rgba() {
        let mut mgr = GraphicsManager::new();
        let load = complete(&mut mgr, direct(2, 2, 0, &RGBA_2X2, 0));
        assert_eq!(*load.data, RGBA_2X2);
    }

    #[test]
    fn direct_rgb_expands_to_rgba() {
        let mut mgr = GraphicsManager::new();
        let rgb = [10, 20, 30, 40, 50, 60];
        let load = complete(&mut mgr, direct(2, 1, 24, &rgb, 0));
        assert_eq!(*load.data, [10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn chunked_reassembly_and_metadata_restore() {
        let mut mgr = GraphicsManager::new();
        let mut first = direct(2, 2, 32, &RGBA_2X2[..6], 1);
        first.id = 5;
        first.quiet = 1;
        assert_eq!(mgr.handle_transmission(first).unwrap(), TransmissionResult::MoreDataNeeded);
        assert!(mgr.loading().is_some());

        // Continuation chunks carry no metadata; everything is restored from
        // the saved first chunk.
        let cont =
            GraphicsCommand { more: 1, payload: RGBA_2X2[6..12].to_vec(), ..Default::default() };
        assert_eq!(mgr.handle_transmission(cont).unwrap(), TransmissionResult::MoreDataNeeded);

        let last = GraphicsCommand { payload: RGBA_2X2[12..].to_vec(), ..Default::default() };
        let load = complete(&mut mgr, last);
        assert_eq!(*load.data, RGBA_2X2);
        assert_eq!((load.start.id, load.start.quiet), (5, 1));
        assert!(mgr.loading().is_none());
    }

    #[test]
    fn chunked_compressed_reassembly() {
        let mut mgr = GraphicsManager::new();
        let compressed = zlib(&RGBA_2X2);
        let (a, rest) = compressed.split_at(3);
        let (b, c) = rest.split_at(3);

        let mut first = direct(2, 2, 32, a, 1);
        first.compressed = b'z';
        mgr.handle_transmission(first).unwrap();
        mgr.handle_transmission(GraphicsCommand {
            more: 1,
            payload: b.to_vec(),
            ..Default::default()
        })
        .unwrap();
        let load =
            complete(&mut mgr, GraphicsCommand { payload: c.to_vec(), ..Default::default() });
        assert_eq!(*load.data, RGBA_2X2);
    }

    #[test]
    fn zlib_bomb_rejected_before_allocation() {
        let mut mgr = GraphicsManager::new();
        // 4 MiB of zeros declared as a 2x2 image: the decompressed-size cap
        // (16 bytes) must reject it without buffering the decompressed data.
        let bomb = zlib(&vec![0; 4 * 1024 * 1024]);
        let mut cmd = direct(2, 2, 32, &bomb, 0);
        cmd.compressed = b'z';
        assert_eq!(error_code(&mut mgr, cmd), ErrorCode::EINVAL);
        assert!(mgr.loading().is_none(), "failed loads must be aborted");
    }

    #[test]
    fn zlib_short_output_is_einval() {
        let mut mgr = GraphicsManager::new();
        let mut cmd = direct(2, 2, 32, &zlib(&RGBA_2X2[..8]), 0);
        cmd.compressed = b'z';
        assert_eq!(error_code(&mut mgr, cmd), ErrorCode::EINVAL);
    }

    #[test]
    fn zlib_garbage_is_einval() {
        let mut mgr = GraphicsManager::new();
        let mut cmd = direct(2, 2, 32, b"definitely not zlib data", 0);
        cmd.compressed = b'z';
        assert_eq!(error_code(&mut mgr, cmd), ErrorCode::EINVAL);
    }

    #[test]
    fn truncated_raw_data_is_enodata() {
        let mut mgr = GraphicsManager::new();
        assert_eq!(error_code(&mut mgr, direct(2, 2, 32, &RGBA_2X2[..8], 0)), ErrorCode::ENODATA);
    }

    #[test]
    fn oversized_raw_data_is_efbig() {
        let mut mgr = GraphicsManager::new();
        // Kitty allows 10 bytes of slack over w*h*bpp before EFBIG.
        let mut data = RGBA_2X2.to_vec();
        data.extend_from_slice(&[0; 10]);
        let load = complete(&mut mgr, direct(2, 2, 32, &data, 0));
        assert_eq!(*load.data, RGBA_2X2, "excess within slack is truncated");

        data.push(0);
        assert_eq!(error_code(&mut mgr, direct(2, 2, 32, &data, 0)), ErrorCode::EFBIG);
    }

    #[test]
    fn declared_dimension_caps_are_einval() {
        let mut mgr = GraphicsManager::new();
        assert_eq!(error_code(&mut mgr, direct(10_001, 1, 32, &[], 0)), ErrorCode::EINVAL);
        assert_eq!(error_code(&mut mgr, direct(1, 10_001, 32, &[], 0)), ErrorCode::EINVAL);
        assert_eq!(error_code(&mut mgr, direct(0, 2, 32, &[], 0)), ErrorCode::EINVAL);
        assert_eq!(error_code(&mut mgr, direct(2, 2, 7, &[], 0)), ErrorCode::EINVAL);
    }

    #[test]
    fn png_declared_size_over_cap_is_einval() {
        let mut mgr = GraphicsManager::new();
        let cmd =
            GraphicsCommand { format: 100, data_sz: MAX_DATA_SZ as u32 + 1, ..Default::default() };
        assert_eq!(error_code(&mut mgr, cmd), ErrorCode::EINVAL);
    }

    #[test]
    fn png_round_trip_rgba() {
        let mut mgr = GraphicsManager::new();
        let png = encode_png(2, 2, png::ColorType::Rgba, &RGBA_2X2);
        let load = complete(&mut mgr, direct(0, 0, 100, &png, 0));
        assert_eq!((load.width, load.height), (2, 2));
        assert_eq!(*load.data, RGBA_2X2);
    }

    #[test]
    fn png_rgb_and_grayscale_expand_to_rgba() {
        let mut mgr = GraphicsManager::new();
        let png = encode_png(2, 1, png::ColorType::Rgb, &[10, 20, 30, 40, 50, 60]);
        let load = complete(&mut mgr, direct(0, 0, 100, &png, 0));
        assert_eq!(*load.data, [10, 20, 30, 255, 40, 50, 60, 255]);

        let png = encode_png(2, 1, png::ColorType::Grayscale, &[7, 9]);
        let load = complete(&mut mgr, direct(0, 0, 100, &png, 0));
        assert_eq!(*load.data, [7, 7, 7, 255, 9, 9, 9, 255]);
    }

    #[test]
    fn png_compressed_with_exact_size_round_trips() {
        let mut mgr = GraphicsManager::new();
        let png = encode_png(2, 2, png::ColorType::Rgba, &RGBA_2X2);
        let cmd = GraphicsCommand {
            format: 100,
            compressed: b'z',
            // Kitty inflates compressed PNGs into exactly `S=` bytes.
            data_sz: png.len() as u32,
            payload: zlib(&png),
            ..Default::default()
        };
        let load = complete(&mut mgr, cmd);
        assert_eq!(*load.data, RGBA_2X2);
    }

    #[test]
    fn oversized_png_is_efbig() {
        let mut mgr = GraphicsManager::new();
        let png = encode_png(10_001, 1, png::ColorType::Grayscale, &[0; 10_001]);
        assert_eq!(error_code(&mut mgr, direct(0, 0, 100, &png, 0)), ErrorCode::EFBIG);
    }

    #[test]
    fn corrupt_png_is_ebadpng() {
        let mut mgr = GraphicsManager::new();
        let mut png = encode_png(2, 2, png::ColorType::Rgba, &RGBA_2X2);
        png.truncate(20);
        assert_eq!(error_code(&mut mgr, direct(0, 0, 100, &png, 0)), ErrorCode::EBADPNG);

        let not_png = b"GIF89a definitely not a png".to_vec();
        assert_eq!(error_code(&mut mgr, direct(0, 0, 100, &not_png, 0)), ErrorCode::EBADPNG);
    }

    #[test]
    fn abort_on_delete_discards_inflight_load() {
        let mut mgr = GraphicsManager::new();
        mgr.handle_transmission(direct(2, 2, 32, &RGBA_2X2[..8], 1)).unwrap();
        // Task 9's delete handling calls abort_load.
        mgr.abort_load();
        assert!(mgr.loading().is_none());

        // The continuation chunk is now treated as a fresh transmission with
        // no metadata and fails kitty's zero width/height check.
        let cont = GraphicsCommand { payload: RGBA_2X2[8..].to_vec(), ..Default::default() };
        assert_eq!(error_code(&mut mgr, cont), ErrorCode::EINVAL);
    }

    #[test]
    fn parse_to_transmission_round_trip() {
        use base64::Engine;
        let mut mgr = GraphicsManager::new();
        let b64 = base64::engine::general_purpose::STANDARD.encode(zlib(&RGBA_2X2));
        let body = format!("a=t,i=3,s=2,v=2,f=32,o=z;{b64}");
        let cmd = GraphicsCommand::parse(body.as_bytes()).unwrap();
        let load = complete(&mut mgr, cmd);
        assert_eq!(*load.data, RGBA_2X2);
        assert_eq!(load.start.id, 3);
    }

    #[test]
    fn bad_base64_payload_is_einval() {
        let err = GraphicsCommand::parse(b"a=t,s=2,v=2;!!!not base64!!!").unwrap_err();
        assert_eq!(err.code, ErrorCode::EINVAL);
    }

    #[cfg(unix)]
    mod unix {
        use std::path::{Path, PathBuf};

        use super::*;

        fn unique(name: &str) -> String {
            format!("alacritty-graphics-test-{}-{name}", std::process::id())
        }

        /// Create a file with `contents`, run `f`, then clean up.
        fn with_file<T>(path: &Path, contents: &[u8], f: impl FnOnce() -> T) -> T {
            std::fs::write(path, contents).unwrap();
            let result = f();
            let _ = std::fs::remove_file(path);
            result
        }

        fn file_cmd(tt: u8, path: &Path, width: u32, height: u32) -> GraphicsCommand {
            GraphicsCommand {
                transmission_type: tt,
                data_width: width,
                data_height: height,
                format: 32,
                payload: path.as_os_str().as_encoded_bytes().to_vec(),
                ..Default::default()
            }
        }

        #[test]
        fn file_medium_reads_and_never_unlinks() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("tty-graphics-protocol-f.rgba"));
            with_file(&path, &RGBA_2X2, || {
                let load = complete(&mut mgr, file_cmd(b'f', &path, 2, 2));
                assert_eq!(*load.data, RGBA_2X2);
                assert!(path.exists(), "t=f must never unlink, even with the marker");
            });
        }

        #[test]
        fn file_medium_honors_size_and_offset() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("range.rgba"));
            let mut contents = vec![0xee; 10];
            contents.extend_from_slice(&RGBA_2X2);
            contents.extend_from_slice(&[0xee; 7]);
            with_file(&path, &contents, || {
                let mut cmd = file_cmd(b'f', &path, 2, 2);
                cmd.data_offset = 10;
                cmd.data_sz = 16;
                let load = complete(&mut mgr, cmd);
                assert_eq!(*load.data, RGBA_2X2);
            });
        }

        #[test]
        fn file_medium_compressed() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("z.rgba"));
            with_file(&path, &zlib(&RGBA_2X2), || {
                let mut cmd = file_cmd(b'f', &path, 2, 2);
                cmd.compressed = b'z';
                let load = complete(&mut mgr, cmd);
                assert_eq!(*load.data, RGBA_2X2);
            });
        }

        #[test]
        fn file_medium_png() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("img.png"));
            with_file(&path, &encode_png(2, 2, png::ColorType::Rgba, &RGBA_2X2), || {
                let mut cmd = file_cmd(b'f', &path, 0, 0);
                cmd.format = 100;
                let load = complete(&mut mgr, cmd);
                assert_eq!((load.width, load.height), (2, 2));
                assert_eq!(*load.data, RGBA_2X2);
            });
        }

        #[test]
        fn file_truncated_is_enodata() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("short.rgba"));
            with_file(&path, &RGBA_2X2[..8], || {
                assert_eq!(error_code(&mut mgr, file_cmd(b'f', &path, 2, 2)), ErrorCode::ENODATA);
            });
        }

        #[test]
        fn missing_file_is_ebadf() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("does-not-exist"));
            assert_eq!(error_code(&mut mgr, file_cmd(b'f', &path, 2, 2)), ErrorCode::EBADF);
        }

        #[test]
        fn overlong_path_is_einval() {
            let mut mgr = GraphicsManager::new();
            let mut cmd = file_cmd(b'f', Path::new("/tmp/x"), 2, 2);
            cmd.payload = vec![b'a'; MAX_FILE_PATH + 1];
            assert_eq!(error_code(&mut mgr, cmd), ErrorCode::EINVAL);
        }

        #[test]
        fn temp_file_with_safe_path_is_unlinked() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("tty-graphics-protocol-t.rgba"));
            std::fs::write(&path, RGBA_2X2).unwrap();
            let load = complete(&mut mgr, file_cmd(b't', &path, 2, 2));
            assert_eq!(*load.data, RGBA_2X2);
            assert!(!path.exists(), "t=t with marker under /tmp must unlink");
        }

        #[test]
        fn temp_file_without_marker_is_not_unlinked() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/tmp").join(unique("no-marker.rgba"));
            with_file(&path, &RGBA_2X2, || {
                complete(&mut mgr, file_cmd(b't', &path, 2, 2));
                assert!(path.exists());
            });
        }

        #[test]
        fn temp_file_outside_temp_dirs_is_not_unlinked() {
            let mut mgr = GraphicsManager::new();
            let path = PathBuf::from("/var/tmp").join(unique("tty-graphics-protocol.rgba"));
            with_file(&path, &RGBA_2X2, || {
                complete(&mut mgr, file_cmd(b't', &path, 2, 2));
                assert!(path.exists(), "marker outside /tmp, /dev/shm, $TMPDIR must survive");
            });
        }

        #[test]
        fn safe_temp_dir_honors_tmpdir() {
            let resolved = Path::new("/var/tmp/tty-graphics-protocol-x");
            assert!(!imp::is_safe_temp_dir(resolved, None));
            assert!(imp::is_safe_temp_dir(resolved, Some(Path::new("/var/tmp"))));
            assert!(imp::is_safe_temp_dir(Path::new("/tmp/x"), None));
            assert!(imp::is_safe_temp_dir(Path::new("/dev/shm/x"), None));
        }

        fn create_shm(name: &str, contents: &[u8]) {
            use std::io::Write;
            use std::os::fd::OwnedFd;
            let flags =
                rustix::shm::OFlags::CREATE | rustix::shm::OFlags::EXCL | rustix::shm::OFlags::RDWR;
            let mode = rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR;
            let fd: OwnedFd = rustix::shm::open(name, flags, mode).unwrap();
            std::fs::File::from(fd).write_all(contents).unwrap();
        }

        fn shm_exists(name: &str) -> bool {
            rustix::shm::open(name, rustix::shm::OFlags::RDONLY, rustix::fs::Mode::empty())
                .map(|fd| {
                    drop(fd);
                    true
                })
                .unwrap_or(false)
        }

        #[test]
        fn shm_round_trip_unlinks() {
            let mut mgr = GraphicsManager::new();
            let name = format!("/{}", unique("shm-ok"));
            create_shm(&name, &RGBA_2X2);

            let mut cmd = file_cmd(b's', Path::new(&name), 2, 2);
            cmd.data_sz = RGBA_2X2.len() as u32;
            let load = complete(&mut mgr, cmd);
            assert_eq!(*load.data, RGBA_2X2);
            assert!(!shm_exists(&name), "shm object must be unlinked after the read");
        }

        #[test]
        fn shm_unlinked_even_when_read_fails() {
            let mut mgr = GraphicsManager::new();
            let name = format!("/{}", unique("shm-short"));
            create_shm(&name, &RGBA_2X2[..4]);

            assert_eq!(
                error_code(&mut mgr, file_cmd(b's', Path::new(&name), 2, 2)),
                ErrorCode::ENODATA
            );
            assert!(!shm_exists(&name), "shm object must be unlinked even on error");
        }

        #[test]
        fn missing_shm_is_ebadf() {
            let mut mgr = GraphicsManager::new();
            let name = format!("/{}", unique("shm-missing"));
            assert_eq!(
                error_code(&mut mgr, file_cmd(b's', Path::new(&name), 2, 2)),
                ErrorCode::EBADF
            );
        }

        #[test]
        fn new_file_transmission_aborts_inflight_chunked_load() {
            let mut mgr = GraphicsManager::new();
            let mut first = direct(2, 2, 32, &RGBA_2X2[..8], 1);
            first.id = 9;
            mgr.handle_transmission(first).unwrap();
            assert!(mgr.loading().is_some());

            let path = PathBuf::from("/tmp").join(unique("abort.rgba"));
            with_file(&path, &RGBA_2X2, || {
                let load = complete(&mut mgr, file_cmd(b'f', &path, 2, 2));
                assert_eq!(*load.data, RGBA_2X2);
            });
            assert!(mgr.loading().is_none(), "file transmission must abort the chunked load");
        }
    }
}
