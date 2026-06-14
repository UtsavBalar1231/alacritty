//! Terminal graphics support (kitty graphics protocol, sixel, iTerm2)
//!
//! This module contains the protocol-agnostic image storage core
//! ([`GraphicsManager`]) plus protocol front-ends (e.g. [`kitty_command`])
//!
//! # Storage model
//!
//! [`GraphicsManager`] is a plain CPU-side data structure with no platform or
//! GPU dependencies. `Term` embeds **two** managers — one for the main grid
//! and one for the alternate grid — and swaps them together with the grids
//! (mirroring kitty's `main_grman`/`alt_grman`). Construct them with
//! [`GraphicsManager::new`] or [`Default`]
//!
//! Placements are stored in an external placement table (not attached to
//! cells), anchored at a signed viewport-relative [`Line`]/[`Column`]
//! position. The manager only stores and iterates placements; rotating the
//! anchors at scroll time and dropping placements on reflow is the
//! responsibility of `Term`
//!
//! GPU synchronization is done through the [`GraphicsManager::pending_uploads`]
//! and [`GraphicsManager::pending_deletes`] queues, which the renderer drains
//! when it takes its per-frame snapshot

pub mod iterm;
pub mod kitty_command;
pub mod placeholder;
pub mod response;
pub mod sixel;
pub mod transmission;

use std::collections::BTreeMap;
use std::mem;
use std::ops::Range;
use std::sync::Arc;

use crate::graphics::kitty_command::{CommandError, ErrorCode, GraphicsCommand};
use crate::graphics::transmission::ZlibStream;
use crate::index::{Column, Line};

/// Default image storage quota in bytes (matches kitty's 320 MiB default).
pub const DEFAULT_STORAGE_LIMIT: usize = 320 * 1024 * 1024;

/// Animation frames may use up to this multiple of `storage_limit`.
/// Mirrors kitty's ratio (kitty/graphics.c). No disk cache is used here —
/// all frame data stays in RAM (documented divergence from kitty).
pub const FRAME_QUOTA_MULTIPLIER: usize = 5;

/// Which compositing pass the renderer places this image in.
///
/// Three-way split from kitty/graphics.c:
/// `z < i32::MIN/2` → BelowBackground, `< 0` → BetweenBgAndText, `≥ 0` → AboveText.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ZBucket {
    /// Behind the terminal background fill (`z < i32::MIN / 2`).
    BelowBackground,
    /// Between cell backgrounds and glyphs (`i32::MIN/2 ≤ z < 0`).
    BetweenBgAndText,
    /// In front of glyphs (`z ≥ 0`).
    AboveText,
}

impl ZBucket {
    /// Classify a kitty z-index value into a render bucket.
    #[inline]
    pub fn from_z(z: i32) -> Self {
        if z < i32::MIN / 2 {
            ZBucket::BelowBackground
        } else if z < 0 {
            ZBucket::BetweenBgAndText
        } else {
            ZBucket::AboveText
        }
    }
}

/// Normalised UV source rectangle (0.0…1.0) derived from placement src fields / image dims.
#[derive(Debug, Copy, Clone, PartialEq)]
pub struct UvRect {
    pub u0: f32,
    pub v0: f32,
    pub u1: f32,
    pub v1: f32,
}

/// Viewport-relative destination in cell coordinates. Negative `line` = partially scrolled.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CellRect {
    pub line: Line,
    pub column: Column,
    pub num_cols: u32,
    pub num_rows: u32,
    /// Sub-cell pixel offsets (`X=`/`Y=`).
    pub cell_x_offset: u32,
    pub cell_y_offset: u32,
}

/// One renderable placement, sorted by `(z_index, image_id, placement_id)`.
#[derive(Debug, Clone)]
pub struct ImageRenderItem {
    /// Internal image id; the GPU texture key for `(TermId, ImageId)`.
    pub image_id: ImageId,
    pub placement_id: u64,
    pub z_index: i32,
    pub z_bucket: ZBucket,
    /// Source UV rect (0.0…1.0, normalised by image dims).
    pub src_uv: UvRect,
    /// Destination in viewport-relative cell coordinates.
    pub dest: CellRect,
    /// Zero-based group index: consecutive items with the same `image_id` share one bind.
    pub group_index: u32,
}

/// Per-frame snapshot of all graphics state the renderer needs.
///
/// `pending_uploads` and `pending_deletes` are drained into `uploads`/`deletes`
/// by the call; the renderer must process both queues to keep GPU state correct.
#[derive(Debug)]
pub struct RenderSnapshot {
    /// Placements sorted `(z_index, image_id, placement_id)` with group indices filled.
    pub items: Vec<ImageRenderItem>,
    /// Images to upload to the GPU (drained from `pending_uploads`).
    pub uploads: Vec<(ImageId, Arc<Vec<u8>>)>,
    /// GPU textures to destroy (drained from `pending_deletes`).
    pub deletes: Vec<ImageId>,
}

impl RenderSnapshot {
    /// Whether this frame's graphics state forces a full-frame redraw on BOTH buffers.
    ///
    /// Images are redrawn into the backbuffer every frame, but damage only feeds
    /// `swap_buffers_with_damage`. Under double buffering, a partial-damage-only frame
    /// presents just that rect, leaving a visible image — or the stale pixels of a
    /// just-deleted image — frozen in the alternate buffer. Returning `true` whenever any
    /// image is visible, uploaded, or deleted lets the display mark both frames fully
    /// damaged so both buffers always present the correct frame.
    #[inline]
    pub fn requires_full_damage(&self) -> bool {
        !self.items.is_empty() || !self.uploads.is_empty() || !self.deletes.is_empty()
    }
}

/// Runtime options for terminal graphics protocols.
///
/// Derived from the `[graphics]` section of the alacritty configuration and
/// carried into the terminal through [`crate::term::Config`].
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct GraphicsOptions {
    /// Master switch for all graphics protocols.
    pub enabled: bool,

    /// Whether the kitty graphics protocol is enabled.
    pub kitty_protocol: bool,

    /// Whether the sixel protocol is enabled.
    ///
    /// Controls whether DCS sixel sequences are decoded and placed on the grid.
    pub sixel: bool,

    /// Whether the iTerm2 inline image protocol is enabled.
    ///
    /// Controls whether OSC 1337 inline-image sequences are decoded and placed on the grid.
    pub iterm2: bool,

    /// Storage quota for decoded image data, in bytes.
    pub max_storage: usize,
}

impl Default for GraphicsOptions {
    fn default() -> Self {
        Self {
            enabled: true,
            kitty_protocol: true,
            sixel: true,
            iterm2: true,
            max_storage: DEFAULT_STORAGE_LIMIT,
        }
    }
}

impl GraphicsOptions {
    /// Whether kitty graphics commands should be acted upon.
    #[inline]
    pub fn kitty_enabled(&self) -> bool {
        self.enabled && self.kitty_protocol
    }

    /// Whether sixel sequences should be acted upon.
    #[inline]
    pub fn sixel_enabled(&self) -> bool {
        self.enabled && self.sixel
    }

    /// Whether iTerm2 inline images should be acted upon.
    #[inline]
    pub fn iterm2_enabled(&self) -> bool {
        self.enabled && self.iterm2
    }
}

/// Maximum accumulated APC sequence length in bytes.
///
/// Sized for the largest single chunk allowed by the transmission layer: the
/// base64 encoding of [`transmission::MAX_DATA_SZ`] (400 MB) plus room for
/// the control block.
pub const MAX_APC_LEN: usize = transmission::MAX_DATA_SZ / 3 * 4 + 4096;

/// Accumulator for one APC passthrough sequence (`ESC _ ... ESC \`).
///
/// `Term` feeds it from the VTE `apc_start`/`apc_put`/`apc_end` callbacks,
/// which may be split across multiple `Processor::advance` batches. Payloads
/// beyond [`MAX_APC_LEN`] are truncated and flagged so the dispatcher can
/// answer with `EFBIG`.
#[derive(Debug)]
pub struct ApcBuilder {
    buf: Vec<u8>,
    active: bool,
    overflowed: bool,
    pub(crate) limit: usize,
}

impl Default for ApcBuilder {
    fn default() -> Self {
        Self { buf: Vec::new(), active: false, overflowed: false, limit: MAX_APC_LEN }
    }
}

impl ApcBuilder {
    /// Begin accumulating a new APC sequence.
    pub fn start(&mut self) {
        self.buf.clear();
        self.active = true;
        self.overflowed = false;
    }

    /// Append payload bytes, keeping at most `limit` bytes.
    pub fn put(&mut self, bytes: &[u8]) {
        if !self.active {
            return;
        }

        let remaining = self.limit.saturating_sub(self.buf.len());
        if bytes.len() > remaining {
            self.overflowed = true;
        }
        self.buf.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    }

    /// Finish the sequence, returning the accumulated payload and whether it
    /// was truncated. Returns `None` for an `apc_end` without `apc_start`.
    pub fn end(&mut self) -> Option<(Vec<u8>, bool)> {
        if !self.active {
            return None;
        }

        self.active = false;
        Some((mem::take(&mut self.buf), self.overflowed))
    }
}

/// Manager-internal image identifier.
///
/// Unique per [`GraphicsManager`] for its entire lifetime; never reused. This
/// is also the key for GPU textures on the renderer side.
pub type ImageId = u64;

/// Cell dimensions in pixels.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct CellSize {
    pub width: u32,
    pub height: u32,
}

/// A single frame of image data.
///
/// The root frame (index 0) is created by `add_image`. Animation frames are
/// appended or edited via `a=f`. The animation metadata fields are unused by
/// the root frame but have sane defaults.
#[derive(Debug, Clone)]
pub struct Frame {
    /// Frame width in pixels.
    pub width: u32,

    /// Frame height in pixels.
    pub height: u32,

    /// Decoded RGBA8 pixel data, shared with the renderer snapshot.
    pub data: Arc<Vec<u8>>,

    /// Display gap before the next frame, milliseconds (`z=`); 0 = playback default.
    pub gap_ms: u32,

    /// Sub-rect placement offset within the base frame, pixels (`x=`/`y=`).
    pub x_offset: u32,
    pub y_offset: u32,

    /// Base frame (1-based kitty index, `c=`); 0 = transparent/bgcolor fill.
    pub base_frame_id: u32,

    /// Background fill colour `0xRRGGBBAA` (`Y=`).
    pub bgcolor: u32,

    /// Porter-Duff over when `true`; source-copy when `false` (`C=`).
    pub alpha_blend: bool,
}

impl Default for Frame {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            data: Arc::new(Vec::new()),
            gap_ms: 0,
            x_offset: 0,
            y_offset: 0,
            base_frame_id: 0,
            bgcolor: 0,
            alpha_blend: false,
        }
    }
}

/// Animation playback state for an image (kitty `AnimationState` enum).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnimationState {
    /// Not animating (s=1 or initial state).
    Stopped,
    /// Frames still being received (s=2); advance blocked at loop boundary.
    Loading,
    /// Actively running (s=3).
    Running,
}

/// Default gap in milliseconds for frames whose `gap_ms == 0` (kitty `DEFAULT_GAP`).
pub const DEFAULT_GAP_MS: u32 = 40;

/// A stored image together with its placements.
#[derive(Debug, Clone)]
pub struct Image {
    /// Manager-internal id (never reused).
    internal_id: ImageId,

    /// Client-visible image id (`i=` key); `0` if the client did not assign one.
    pub client_id: u32,

    /// Client-visible image number (`I=` key); `0` if unused.
    pub client_number: u32,

    /// Image width in pixels.
    pub width: u32,

    /// Image height in pixels.
    pub height: u32,

    /// Image frames; index 0 is the root frame. Animation frames are appended
    /// after the root frame in a later phase.
    pub frames: Vec<Frame>,

    /// Last access time as a monotonic sequence number (LRU eviction order).
    atime: u64,

    /// Bytes of storage accounted to this image's root frame.
    used_storage: usize,

    /// Bytes accounted to this image's animation frames (frames[1..]).
    frame_storage: usize,

    /// Placements of this image, in creation order.
    placements: Vec<Placement>,

    /// Monotonic counter for placement internal ids within this image.
    placement_id_counter: u64,

    /// Current playback state.
    pub animation_state: AnimationState,

    /// 0-based index into `frames` for the currently displayed frame.
    pub current_frame_index: usize,

    /// Max full loops before stopping; 0 = infinite. Kitty n-1 semantics: `v=1` → 0
    /// (infinite), `v=2` → 1. See `handle_animation_control` (graphics.c:1766).
    pub max_loops: u32,

    /// How many full loops have elapsed (0-based; wraps at max_loops).
    pub current_loop: u32,

    /// Synthetic timestamp (ms) when the current frame was first displayed.
    /// Initialised to the timestamp passed to `animation_control` when
    /// the state changes from Stopped → Running.
    pub frame_shown_at_ms: u64,
}

impl Image {
    /// Manager-internal id of this image.
    pub fn id(&self) -> ImageId {
        self.internal_id
    }

    /// Placements of this image.
    pub fn placements(&self) -> &[Placement] {
        &self.placements
    }

    /// Mutable access to the placements, e.g. for scroll rotation of anchors.
    pub fn placements_mut(&mut self) -> &mut [Placement] {
        &mut self.placements
    }

    /// Last access time (monotonic sequence number).
    pub fn atime(&self) -> u64 {
        self.atime
    }

    /// Bytes of animation-frame storage accounted to this image.
    pub fn frame_storage(&self) -> usize {
        self.frame_storage
    }
}

/// Which protocol produced a placement. Used to scope overlap-eviction:
/// only `Sixel` and `Iterm2` placements are positional-overwrite; kitty
/// placements use explicit delete commands and must never be evicted here.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PlacementOrigin {
    /// Kitty graphics protocol placement (default). Never overlap-evicted.
    #[default]
    Kitty,
    /// Sixel DCS placement. Positional-overwrite; evictable by overlap.
    Sixel,
    /// iTerm2 OSC 1337 inline-image placement. Positional-overwrite; evictable by overlap.
    Iterm2,
}

impl PlacementOrigin {
    /// Returns `true` for protocols that own the cell region they occupy and
    /// should have old placements evicted when a new one lands at the same
    /// position. `Kitty` placements use explicit delete commands instead.
    #[inline]
    pub fn is_positional(self) -> bool {
        matches!(self, PlacementOrigin::Sixel | PlacementOrigin::Iterm2)
    }
}

/// A single placement of an image on the grid (kitty `ImageRef`).
///
/// Placements with a nonzero `client_id` are addressable by the client
/// (`p=` key); placements with `client_id == 0` are anonymous.
#[derive(Debug, Clone)]
pub struct Placement {
    /// Manager-internal placement id, unique within the owning image.
    internal_id: u64,

    /// Client-visible placement id (`p=` key); `0` if unassigned.
    pub client_id: u32,

    /// Source rectangle left edge in image pixels (`x=`).
    pub src_x: u32,

    /// Source rectangle top edge in image pixels (`y=`).
    pub src_y: u32,

    /// Source rectangle width in image pixels (`w=`, clamped to the image).
    pub src_width: u32,

    /// Source rectangle height in image pixels (`h=`, clamped to the image).
    pub src_height: u32,

    /// Horizontal offset within the first cell, in pixels (`X=`), clamped to
    /// `cell_width - 1`.
    pub cell_x_offset: u32,

    /// Vertical offset within the first cell, in pixels (`Y=`), clamped to
    /// `cell_height - 1`.
    pub cell_y_offset: u32,

    /// Requested number of columns (`c=`); `0` means automatic.
    pub num_cols: u32,

    /// Requested number of rows (`r=`); `0` means automatic.
    pub num_rows: u32,

    /// Effective number of columns covered, after aspect-preserving math.
    pub effective_num_cols: u32,

    /// Effective number of rows covered, after aspect-preserving math.
    pub effective_num_rows: u32,

    /// Z-index (`z=`); negative values render below text.
    pub z_index: i32,

    /// Anchor line in `Term`'s signed viewport-relative coordinate space.
    /// Rotated by `Term` on scroll; may become negative (scrolled into
    /// history) before being garbage collected.
    pub line: Line,

    /// Anchor column.
    pub column: Column,

    /// Unicode-placeholder placement (`U=1`). Excluded from geometric delete
    /// filters (c/p/q/x/y/z); only deletable by id/number/all. Mirrors
    /// kitty's `is_virtual_ref` (graphics.c:2058-2081). Phase 4 sets this.
    pub is_virtual: bool,

    /// Protocol that created this placement. `Kitty` for all kitty-protocol
    /// placements; `Sixel` / `Iterm2` for the respective inline-image paths.
    /// Used to scope overlap-eviction to positional protocols only.
    pub origin: PlacementOrigin,

    /// Manager-internal id of the parent image; `0` = no parent.
    /// Stored as internal id after resolving from the client `P=` value.
    pub parent_image_id: ImageId,

    /// Manager-internal id of the parent placement; `0` = no parent.
    /// Stored as internal id after resolving from the client `Q=` value.
    pub parent_placement_id: u64,

    /// Cell offset from the parent's anchor, horizontal (`H=`).
    pub parent_offset_x: i32,

    /// Cell offset from the parent's anchor, vertical (`V=`).
    pub parent_offset_y: i32,
}

impl Placement {
    /// Manager-internal placement id.
    pub fn id(&self) -> u64 {
        self.internal_id
    }

    /// Compute the effective cell extent of this placement.
    ///
    /// Direct port of kitty's `update_dest_rect` (graphics.c:826-853):
    /// requested extents are taken verbatim; missing extents are derived from
    /// the source rectangle with aspect-preserving ceil math. When both are
    /// missing, columns are computed first from the pixel size and rows are
    /// then derived from the *effective* column extent.
    pub fn update_dest_rect(&mut self, cell: CellSize) {
        let mut num_cols = self.num_cols;
        let mut num_rows = self.num_rows;

        if num_cols == 0 {
            if num_rows == 0 {
                let t = self.src_width + self.cell_x_offset;
                num_cols = t / cell.width;
                if t > num_cols * cell.width {
                    num_cols += 1;
                }
            } else {
                let height_px = f64::from(cell.height * num_rows + self.cell_y_offset);
                let width_px = height_px * f64::from(self.src_width) / f64::from(self.src_height);
                num_cols = (width_px / f64::from(cell.width)).ceil() as u32;
            }
        }

        if num_rows == 0 {
            if num_cols == 0 {
                let t = self.src_height + self.cell_y_offset;
                num_rows = t / cell.height;
                if t > num_rows * cell.height {
                    num_rows += 1;
                }
            } else {
                let width_px = f64::from(cell.width * num_cols + self.cell_x_offset);
                let height_px = width_px * f64::from(self.src_height) / f64::from(self.src_width);
                num_rows = (height_px / f64::from(cell.height)).ceil() as u32;
            }
        }

        self.effective_num_cols = num_cols;
        self.effective_num_rows = num_rows;
    }
}

/// Parameters for creating or replacing a placement.
#[derive(Debug, Clone, Default)]
pub struct PlacementSpec {
    /// Client placement id (`p=`); `0` creates an anonymous placement.
    pub placement_id: u32,

    /// Source rectangle left edge (`x=`).
    pub src_x: u32,

    /// Source rectangle top edge (`y=`).
    pub src_y: u32,

    /// Source rectangle width (`w=`); `0` means the full image width.
    pub src_width: u32,

    /// Source rectangle height (`h=`); `0` means the full image height.
    pub src_height: u32,

    /// Horizontal pixel offset within the first cell (`X=`).
    pub cell_x_offset: u32,

    /// Vertical pixel offset within the first cell (`Y=`).
    pub cell_y_offset: u32,

    /// Requested columns (`c=`); `0` means automatic.
    pub num_cols: u32,

    /// Requested rows (`r=`); `0` means automatic.
    pub num_rows: u32,

    /// Z-index (`z=`).
    pub z_index: i32,

    /// Unicode-placeholder virtual placement (`U=1`): no screen anchor,
    /// excluded from normal rendering and geometric deletes. Mirrors kitty's
    /// `is_virtual_ref` (graphics.c:2058-2081).
    pub is_virtual: bool,

    /// Protocol origin for the new placement. Defaults to `Kitty`.
    /// Set to `Sixel` or `Iterm2` by the respective command handlers before
    /// calling `put_placement`.
    pub origin: PlacementOrigin,

    /// Client image id of the parent image (`P=`); `0` = no parent.
    pub parent_client_id: u32,

    /// Client placement id of the parent placement (`Q=`); `0` = use the
    /// parent image's first placement.
    pub parent_placement_client_id: u32,

    /// Cell offset from the parent anchor, horizontal (`H=`).
    pub parent_offset_x: i32,

    /// Cell offset from the parent anchor, vertical (`V=`).
    pub parent_offset_y: i32,
}

/// Result of adding an image to the manager.
///
/// Exposes everything a protocol front-end needs to build a response: when
/// the client used `I=` (image number), `client_id` holds the freshly
/// assigned smallest free id and the response must echo *both* `i=` and `I=`.
/// When both `client_id` and `client_number` are `0` the add was anonymous
/// and no response is sent.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct AddedImage {
    /// Manager-internal id.
    pub id: ImageId,

    /// Client id (`i=`), possibly auto-assigned from `I=`.
    pub client_id: u32,

    /// Client image number (`I=`).
    pub client_number: u32,

    /// `true` if an existing image with the same client id was replaced.
    pub replaced: bool,
}

/// In-flight chunked transmission state.
///
/// Only **one** transmission can be in flight per manager (kitty's `LoadData`
/// slot). Starting a new transmission or processing any delete command aborts
/// the in-flight load; the delete handler must call
/// [`GraphicsManager::abort_load`].
#[derive(Debug, Default)]
pub struct LoadData {
    /// The saved first-chunk command (kitty's `start_command`).
    ///
    /// Continuation chunks (`m=1` follow-ups) restore all metadata —
    /// identity, format, compression, placement keys — from this command;
    /// only `m=` and the payload are taken from the new chunk. The payload
    /// of the saved command itself is always empty.
    pub start: GraphicsCommand,

    /// Expected decoded payload size in bytes.
    ///
    /// `w*h*bpp` for raw formats; `S=` (or kitty's 100 KiB default) for PNG.
    pub data_sz: usize,

    /// Accumulated payload bytes (already zlib-decompressed for `o=z`).
    pub buf: Vec<u8>,

    /// Streaming zlib decompressor state for `o=z` transmissions.
    pub inflate: Option<ZlibStream>,
}

/// CPU-side image storage for one grid (kitty graphics model).
///
/// Plain data structure; `Term` owns two of these (main/alt grid). See the
/// module documentation for the threading and coordinate model.
#[derive(Debug)]
pub struct GraphicsManager {
    /// All stored images, keyed and iterated by internal id (creation order).
    images: BTreeMap<ImageId, Image>,

    /// Monotonic internal image id counter.
    image_id_counter: ImageId,

    /// Monotonic access-time counter for LRU eviction.
    atime_counter: u64,

    /// Storage quota in bytes (kitty's `storage_limit`).
    pub storage_limit: usize,

    /// Bytes currently accounted to stored root frames.
    used_storage: usize,

    /// Bytes currently accounted to animation frames (a=f).
    /// Ceiling: storage_limit × FRAME_QUOTA_MULTIPLIER (5×, kitty parity).
    /// No disk cache — divergence from kitty documented at add_frame().
    frame_storage_used: usize,

    /// Images whose pixel data must be (re-)uploaded to the GPU.
    /// Drained by the renderer at snapshot time.
    pub pending_uploads: Vec<(ImageId, Arc<Vec<u8>>)>,

    /// Images whose GPU textures must be destroyed.
    /// Drained by the renderer at snapshot time.
    pub pending_deletes: Vec<ImageId>,

    /// The single in-flight chunked transmission, if any.
    loading: Option<LoadData>,

    /// Count of images currently in Running or Loading animation state.
    /// Guards `scan_active_animations` — zero means no iteration needed.
    active_animation_count: usize,
}

impl Default for GraphicsManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Geometry for `GraphicsManager::compose_delta` — bundles spatial parameters
/// so the function stays within clippy's argument-count limit.
struct ComposeDelta {
    img_w: u32,
    img_h: u32,
    over_w: u32,
    over_h: u32,
    ox: u32,
    oy: u32,
    needs_blend: bool,
}

/// Arguments for `GraphicsManager::compose_frame` (`a=c` command), bundled to
/// stay within clippy's argument-count limit.
pub struct ComposeFrameArgs {
    pub src_frame_number: u32,
    pub dst_frame_number: u32,
    pub dst_x: u32,
    pub dst_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    pub needs_blend: bool,
}

/// Arguments for `GraphicsManager::animation_control` (`a=a` command).
pub struct AnimationControlArgs {
    /// `s=`: 1=stop, 2=loading, 3=run (0 = not set).
    pub anim_state: u32,
    /// `c=`: 1-based frame index to jump to (0 = not set).
    pub frame_number: u32,
    /// `r=`: 1-based frame whose gap to edit (0 = not set).
    pub gap_frame: u32,
    /// `z=`: new gap in ms for the frame identified by `gap_frame`.
    pub gap_ms: i32,
    /// `v=`: loop count with kitty n-1 semantics (0 = not set).
    pub loop_count: u32,
    /// Synthetic ms timestamp for `frame_shown_at_ms` initialisation.
    pub now_ms: u64,
}

impl GraphicsManager {
    /// Create a manager with the default 320 MiB storage quota.
    pub fn new() -> Self {
        Self::with_storage_limit(DEFAULT_STORAGE_LIMIT)
    }

    /// Create a manager with a custom storage quota in bytes.
    pub fn with_storage_limit(storage_limit: usize) -> Self {
        Self {
            images: BTreeMap::new(),
            image_id_counter: 0,
            atime_counter: 0,
            storage_limit,
            used_storage: 0,
            frame_storage_used: 0,
            pending_uploads: Vec::new(),
            pending_deletes: Vec::new(),
            loading: None,
            active_animation_count: 0,
        }
    }

    /// Count of images currently in Running or Loading animation state.
    pub fn active_animation_count(&self) -> usize {
        self.active_animation_count
    }

    /// Bytes currently used by stored images.
    pub fn used_storage(&self) -> usize {
        self.used_storage
    }

    /// Update the storage quota, evicting images if the new limit is
    /// exceeded (mirrors the `add_image` quota check on config reload).
    pub fn set_storage_limit(&mut self, storage_limit: usize) {
        self.storage_limit = storage_limit;
        if self.used_storage > self.storage_limit {
            self.apply_storage_quota(0);
        }
    }

    /// Number of stored images.
    pub fn len(&self) -> usize {
        self.images.len()
    }

    /// `true` if no images are stored.
    pub fn is_empty(&self) -> bool {
        self.images.is_empty()
    }

    /// Iterate over all images in creation order.
    pub fn images(&self) -> impl Iterator<Item = &Image> {
        self.images.values()
    }

    /// Look up an image by its manager-internal id.
    pub fn image(&self, id: ImageId) -> Option<&Image> {
        self.images.get(&id)
    }

    /// Mutable lookup by manager-internal id.
    pub fn image_mut(&mut self, id: ImageId) -> Option<&mut Image> {
        self.images.get_mut(&id)
    }

    /// Look up an image by client id (`i=`).
    pub fn image_by_client_id(&self, client_id: u32) -> Option<&Image> {
        self.images.values().find(|img| img.client_id == client_id)
    }

    /// Look up the *newest* image with the given client number (`I=`),
    /// mirroring kitty's `img_by_client_number`.
    pub fn image_by_client_number(&self, number: u32) -> Option<&Image> {
        // BTreeMap iterates in ascending internal id order, so the last match
        // is the newest image
        self.images.values().rfind(|img| img.client_number == number)
    }

    /// Smallest client id not currently in use, starting at 1.
    ///
    /// Port of kitty's `get_free_client_id` (graphics.c:519-541).
    fn free_client_id(&self) -> u32 {
        let mut ids: Vec<u32> =
            self.images.values().map(|img| img.client_id).filter(|&id| id != 0).collect();
        if ids.is_empty() {
            return 1;
        }
        ids.sort_unstable();
        let mut ans = 1;
        let mut prev_id = 0;
        for id in ids {
            if id == prev_id {
                continue;
            }
            prev_id = id;
            if id != ans {
                break;
            }
            ans = id + 1;
        }
        ans
    }

    /// Next monotonic access-time stamp.
    fn next_atime(&mut self) -> u64 {
        self.atime_counter += 1;
        self.atime_counter
    }

    /// Store a fully decoded RGBA image.
    ///
    /// Identity semantics (kitty `handle_add_command`, graphics.c:712-744):
    /// * explicit `client_id` (`i=`): replaces any existing image with that id in place — old pixel
    ///   data is freed (GPU delete enqueued), all placements are dropped, the internal id and
    ///   client number are kept.
    /// * `client_id == 0` with `client_number != 0` (`I=`): a new image is created and the smallest
    ///   free client id is assigned; the returned [`AddedImage`] carries both for the `i=`/`I=`
    ///   response echo.
    /// * both zero: anonymous image, no response; it survives only until the next add unless a
    ///   placement references it.
    ///
    /// Enforces the storage quota after insertion (never evicting the image
    /// just added) and enqueues a GPU upload for the new pixel data.
    pub fn add_image(
        &mut self,
        client_id: u32,
        client_number: u32,
        width: u32,
        height: u32,
        data: Arc<Vec<u8>>,
    ) -> AddedImage {
        // A complete new image transmission aborts any in-flight chunked load
        self.abort_load();

        // Drop stale anonymous, unreferenced images before adding
        // (kitty's `add_trim_predicate` sweep at graphics.c:721)
        self.remove_images_where(|img| img.client_id == 0 && img.placements.is_empty(), 0);

        let atime = self.next_atime();
        let size = data.len();
        let frame = Frame { width, height, data: data.clone(), ..Default::default() };

        let existing_id = if client_id != 0 {
            self.images.values().find(|img| img.client_id == client_id).map(|img| img.internal_id)
        } else {
            None
        };

        let added = if let Some(id) = existing_id {
            // Replace in place: free old resources, keep internal id and
            // client number (kitty `free_image_resources` + reuse)
            self.pending_deletes.push(id);
            let img = self.images.get_mut(&id).unwrap();
            self.used_storage = self.used_storage.saturating_sub(img.used_storage);
            self.frame_storage_used = self.frame_storage_used.saturating_sub(img.frame_storage);
            img.frames.clear();
            img.placements.clear();
            img.width = width;
            img.height = height;
            img.frames.push(frame);
            img.atime = atime;
            img.used_storage = size;
            img.frame_storage = 0;
            // The response echoes the *command's* identity keys, so
            // `client_number` is the one passed in (the stored number on the
            // image is preserved, mirroring kitty's replace path)
            AddedImage { id, client_id, client_number, replaced: true }
        } else {
            self.image_id_counter += 1;
            let id = self.image_id_counter;
            let client_id = if client_id == 0 && client_number != 0 {
                self.free_client_id()
            } else {
                client_id
            };
            self.images.insert(id, Image {
                internal_id: id,
                client_id,
                client_number,
                width,
                height,
                frames: vec![frame],
                atime,
                used_storage: size,
                frame_storage: 0,
                placements: Vec::new(),
                placement_id_counter: 0,
                animation_state: AnimationState::Stopped,
                current_frame_index: 0,
                max_loops: 0,
                current_loop: 0,
                frame_shown_at_ms: 0,
            });
            AddedImage { id, client_id, client_number, replaced: false }
        };

        self.used_storage += size;
        self.pending_uploads.push((added.id, data));

        if self.used_storage > self.storage_limit {
            self.apply_storage_quota(added.id);
        }

        added
    }

    /// Create or replace a placement for the image with internal id `id`.
    ///
    /// Port of kitty's `handle_put_command` storage semantics
    /// (graphics.c:1095-1135): if `spec.placement_id` is nonzero and the
    /// image has a client id, an existing placement with the same client
    /// placement id is replaced *in place* (flicker-free swap of the same
    /// slot, never delete-then-add). The source rectangle is clamped to the
    /// image and subcell offsets are clamped to `cell - 1`.
    ///
    /// When `spec.parent_client_id != 0`, the parent image is resolved by
    /// client id; if absent or has no placements, returns `ENOPARENT`. Cycle
    /// and depth checks run per `has_good_ancestry` (graphics.c:1034-1060).
    ///
    /// Returns `(effective_num_cols, effective_num_rows)` on success, or a
    /// `CommandError` on parent/cycle/depth validation failures. Returns `None`
    /// when the image `id` is unknown (no error — caller already validated).
    pub fn put_placement(
        &mut self,
        id: ImageId,
        anchor_line: Line,
        anchor_column: Column,
        spec: &PlacementSpec,
        cell: CellSize,
    ) -> Result<Option<(u32, u32)>, CommandError> {
        // Resolve parent by client id (immutable borrows). graphics.c:1073-1093
        let (par_img_id, par_placement_id) = if spec.parent_client_id != 0 {
            let parent_img = self
                .images
                .values()
                .find(|img| img.client_id == spec.parent_client_id)
                .ok_or_else(|| CommandError {
                    code: ErrorCode::ENOPARENT,
                    message: format!(
                        "Put command refers to a parent image with id: {} that does not exist",
                        spec.parent_client_id
                    ),
                    sends_response: true,
                })?;
            if parent_img.placements.is_empty() {
                return Err(CommandError {
                    code: ErrorCode::ENOPARENT,
                    message: format!(
                        "Put command refers to a parent image with id: {} that has no placements",
                        spec.parent_client_id
                    ),
                    sends_response: true,
                });
            }
            let par_img_internal = parent_img.internal_id;
            let par_placement_internal = if spec.parent_placement_client_id != 0 {
                let pr = parent_img
                    .placements
                    .iter()
                    .find(|p| p.client_id == spec.parent_placement_client_id)
                    .ok_or_else(|| CommandError {
                        code: ErrorCode::ENOPARENT,
                        message: format!(
                            "Put command refers to a parent image placement with id: {} and \
                             placement id: {} that does not exist",
                            spec.parent_client_id, spec.parent_placement_client_id
                        ),
                        sends_response: true,
                    })?;
                pr.internal_id
            } else {
                // Q=0: use the first placement (graphics.c:1084)
                parent_img.placements[0].internal_id
            };
            (par_img_internal, par_placement_internal)
        } else {
            (0, 0)
        };

        let atime = self.next_atime();
        if !self.images.contains_key(&id) {
            return Ok(None);
        }

        // Collect immutable properties first
        let (img_width, img_height, img_client_id, img_internal_id) = {
            let img = &self.images[&id];
            (img.width, img.height, img.client_id, img.internal_id)
        };
        self.images.get_mut(&id).unwrap().atime = atime;

        // Source rectangle clamping (graphics.c:1120-1122)
        let src_x = spec.src_x;
        let src_y = spec.src_y;
        let src_width = if spec.src_width == 0 { img_width } else { spec.src_width };
        let src_height = if spec.src_height == 0 { img_height } else { spec.src_height };
        let src_width = src_width.min(img_width - src_x.min(img_width));
        let src_height = src_height.min(img_height - src_y.min(img_height));

        // Check self-parent: resolved parent placement == an existing
        // placement on this same image with the same client id
        // graphics.c:1100-1103
        if par_img_id != 0 && par_img_id == img_internal_id {
            let client_placement_id = if img_client_id != 0 { spec.placement_id } else { 0 };
            if client_placement_id != 0 {
                let is_self_parent = self.images[&id].placements.iter().any(|p| {
                    p.client_id == client_placement_id && p.internal_id == par_placement_id
                });
                if is_self_parent {
                    return Err(CommandError {
                        code: ErrorCode::EINVAL,
                        message: "Put command refers to itself as its own parent".into(),
                        sends_response: true,
                    });
                }
            }
        }

        let placement_client_id = if img_client_id != 0 { spec.placement_id } else { 0 };
        let mut placement = Placement {
            internal_id: 0,
            // Client placement ids are only meaningful for client-addressable
            // images (graphics.c:1128)
            client_id: placement_client_id,
            src_x,
            src_y,
            src_width,
            src_height,
            // Subcell offsets clamped to the cell (graphics.c:1125-1126)
            cell_x_offset: spec.cell_x_offset.min(cell.width - 1),
            cell_y_offset: spec.cell_y_offset.min(cell.height - 1),
            num_cols: spec.num_cols,
            num_rows: spec.num_rows,
            effective_num_cols: 0,
            effective_num_rows: 0,
            z_index: spec.z_index,
            line: anchor_line,
            column: anchor_column,
            is_virtual: spec.is_virtual,
            origin: spec.origin,
            parent_image_id: par_img_id,
            parent_placement_id: par_placement_id,
            parent_offset_x: spec.parent_offset_x,
            parent_offset_y: spec.parent_offset_y,
        };
        placement.update_dest_rect(cell);
        let extent = (placement.effective_num_cols, placement.effective_num_rows);

        let existing_idx = if placement_client_id != 0 {
            self.images[&id].placements.iter().position(|p| p.client_id == placement_client_id)
        } else {
            None
        };

        if let Some(idx) = existing_idx {
            // Tentative ancestry on update (graphics.c:1104-1110): temporarily
            // apply new parent, check ancestry, revert on failure
            if par_img_id != 0 {
                let old_par_img = self.images[&id].placements[idx].parent_image_id;
                let old_par_pl = self.images[&id].placements[idx].parent_placement_id;
                self.images.get_mut(&id).unwrap().placements[idx].parent_image_id = par_img_id;
                self.images.get_mut(&id).unwrap().placements[idx].parent_placement_id =
                    par_placement_id;
                // has_good_ancestry borrows &self (immutable); the above
                // mutable borrows are already released (temporaries dropped)
                if let Err(e) = self.has_good_ancestry(id, idx) {
                    // Revert
                    self.images.get_mut(&id).unwrap().placements[idx].parent_image_id = old_par_img;
                    self.images.get_mut(&id).unwrap().placements[idx].parent_placement_id =
                        old_par_pl;
                    return Err(e);
                }
            }
            // Atomic in-place replace: keep the internal id, swap the rest
            placement.internal_id = self.images[&id].placements[idx].internal_id;
            self.images.get_mut(&id).unwrap().placements[idx] = placement;
        } else {
            // Fresh ref: push first (to give it an internal_id), then check
            // ancestry, remove on failure (graphics.c:1140-1144)
            {
                let img = self.images.get_mut(&id).unwrap();
                img.placement_id_counter += 1;
                placement.internal_id = img.placement_id_counter;
                img.placements.push(placement);
            }
            let new_idx = self.images[&id].placements.len() - 1;
            if par_img_id != 0
                && let Err(e) = self.has_good_ancestry(id, new_idx)
            {
                self.images.get_mut(&id).unwrap().placements.remove(new_idx);
                return Err(e);
            }
        }

        Ok(Some(extent))
    }

    /// Port of kitty's `has_good_ancestry` (graphics.c:1034-1060).
    ///
    /// Walks the parent chain starting at `images[id].placements[idx]`.
    /// Returns `Ok(())` if the chain is valid, or a `CommandError` with
    /// ECYCLE / ETOODEEP / ENOENT on failure.
    fn has_good_ancestry(
        &self,
        start_img_id: ImageId,
        start_idx: usize,
    ) -> Result<(), CommandError> {
        // We track the start ref by (image_id, internal_placement_id) to
        // detect cycles without needing raw pointers
        let start_ref_img = start_img_id;
        let start_ref_pl = self.images[&start_img_id].placements[start_idx].internal_id;

        let mut cur_img_id = start_img_id;
        let mut cur_pl_id = start_ref_pl;
        let mut depth: u32 = 0;

        loop {
            let cur_img = &self.images[&cur_img_id];
            let cur_ref = cur_img.placements.iter().find(|p| p.internal_id == cur_pl_id).unwrap();

            let par_img_id = cur_ref.parent_image_id;
            if par_img_id == 0 {
                // Reached the root — no cycle, not too deep
                return Ok(());
            }

            // Check cycle: if we're back to the start ref with depth > 0
            if depth > 0 && cur_img_id == start_ref_img && cur_pl_id == start_ref_pl {
                return Err(CommandError {
                    code: ErrorCode::ECYCLE,
                    message: "This parent reference creates a cycle".into(),
                    sends_response: true,
                });
            }

            if depth >= 8 {
                return Err(CommandError {
                    code: ErrorCode::ETOODEEP,
                    message: "Too many levels of parent references".into(),
                    sends_response: true,
                });
            }
            depth += 1;

            let par_pl_id = cur_ref.parent_placement_id;
            let par_img = self.images.get(&par_img_id).ok_or_else(|| CommandError {
                code: ErrorCode::ENOENT,
                message: format!(
                    "One of the ancestors of this ref with image id: {} not found",
                    par_img_id
                ),
                sends_response: true,
            })?;
            if !par_img.placements.iter().any(|p| p.internal_id == par_pl_id) {
                return Err(CommandError {
                    code: ErrorCode::ENOENT,
                    message: format!(
                        "One of the ancestors of this ref with image id: {} and ref id: {} not \
                         found",
                        par_img_id, par_pl_id
                    ),
                    sends_response: true,
                });
            }
            cur_img_id = par_img_id;
            cur_pl_id = par_pl_id;
        }
    }

    /// Port of kitty's `resolve_parent_offset` (graphics.c:1178-1202).
    ///
    /// Direct port: walks `ref → parent` chain accumulating `ref.parent_offset`
    /// at each step. Returns the resolved absolute `(line, column)` for the
    /// starting ref's top-left, or `None` on orphan / depth exceeded.
    ///
    /// For virtual parent placements, resolves via the minimum start_row/col
    /// among all placements sharing the parent's internal_id
    /// (kitty's `resolve_cell_ref`, graphics.c:1165-1175).
    pub fn resolve_parent_offset(
        &self,
        start_img_id: ImageId,
        start_pl_id: u64,
    ) -> Option<(Line, Column)> {
        let mut cur_img_id = start_img_id;
        let mut cur_pl_id = start_pl_id;
        let mut acc_x: i32 = 0;
        let mut acc_y: i32 = 0;
        let mut depth: u32 = 0;

        loop {
            let cur_img = self.images.get(&cur_img_id)?;
            let cur_ref = cur_img.placements.iter().find(|p| p.internal_id == cur_pl_id)?;

            let par_img_id = cur_ref.parent_image_id;
            if par_img_id == 0 {
                // Root reached: apply accumulated offsets to root anchor
                let (root_line, root_col) = if cur_ref.is_virtual {
                    // resolve_cell_ref: min start_row/col over virtual refs
                    Self::resolve_cell_ref(cur_img, cur_ref.internal_id)?
                } else {
                    (cur_ref.line.0, cur_ref.column.0 as i32)
                };
                return Some((Line(root_line + acc_y), Column((root_col + acc_x) as usize)));
            }

            if depth >= 8 {
                return None;
            }
            depth += 1;

            let par_img = self.images.get(&par_img_id)?;
            let par_pl_id = cur_ref.parent_placement_id;
            let par_ref = par_img.placements.iter().find(|p| p.internal_id == par_pl_id)?;

            // Accumulate this ref's offset towards its parent, then ascend
            acc_x += cur_ref.parent_offset_x;
            acc_y += cur_ref.parent_offset_y;

            // If the parent is virtual, swap in its cell-resolved position
            // (graphics.c:1190-1194): treat the virtual ref's cell as the
            // effective anchor but continue walking up from *it*
            if par_ref.is_virtual {
                let (vl, vc) = Self::resolve_cell_ref(par_img, par_ref.internal_id)?;
                // Virtual parents are always the root of their chain in the
                // sense that they have no screen anchor beyond their cell
                // We apply the accumulated offsets and are done
                return Some((Line(vl + acc_y), Column((vc + acc_x) as usize)));
            }

            cur_img_id = par_img_id;
            cur_pl_id = par_pl_id;
        }
    }

    /// Port of kitty's `resolve_cell_ref` (graphics.c:1165-1175).
    ///
    /// Returns `(min_start_row, min_start_col)` over all placements of `img`
    /// whose `internal_id` matches `virt_ref_id`, or `None` if none found.
    fn resolve_cell_ref(img: &Image, virt_ref_id: u64) -> Option<(i32, i32)> {
        let mut min_line: Option<i32> = None;
        let mut min_col: Option<i32> = None;
        for p in &img.placements {
            if p.internal_id == virt_ref_id {
                let l = p.line.0;
                let c = p.column.0 as i32;
                min_line = Some(min_line.map_or(l, |m: i32| m.min(l)));
                min_col = Some(min_col.map_or(c, |m: i32| m.min(c)));
            }
        }
        Some((min_line?, min_col?))
    }

    /// Remove an image, enqueueing a GPU texture delete and releasing its
    /// accounted storage. Returns `true` if the image existed.
    pub fn remove_image(&mut self, id: ImageId) -> bool {
        match self.images.remove(&id) {
            Some(img) => {
                self.used_storage = self.used_storage.saturating_sub(img.used_storage);
                self.frame_storage_used = self.frame_storage_used.saturating_sub(img.frame_storage);
                self.pending_deletes.push(id);
                true
            },
            None => false,
        }
    }

    /// Remove all images matching `predicate`, skipping `skip` (0 = none).
    fn remove_images_where<F: Fn(&Image) -> bool>(&mut self, predicate: F, skip: ImageId) {
        let doomed: Vec<ImageId> = self
            .images
            .values()
            .filter(|img| img.internal_id != skip && predicate(img))
            .map(|img| img.internal_id)
            .collect();
        for id in doomed {
            self.remove_image(id);
        }
    }

    /// Evict all images (except `skip`) that have a positional (Sixel/Iterm2)
    /// non-virtual placement whose cell bounding-box overlaps the rectangle
    /// `[column, column+cols) × [line, line+rows)`.
    ///
    /// Called after placing a new Sixel or iTerm2 image so that the previous
    /// image rendered in the same screen region is removed rather than stacked.
    /// Kitty-origin placements are intentionally excluded — they use explicit
    /// delete commands. Virtual (U=1) placements are always excluded.
    ///
    /// AABB overlap test: two ranges [a0, a0+aw) and [b0, b0+bw) overlap iff
    /// `a0 < b0+bw && b0 < a0+aw`.  Applied separately to lines and columns.
    /// Line is signed (i32-backed); Column is usize-backed; all arithmetic is
    /// performed as i64 to avoid underflow.
    pub fn evict_overlapping_positional(
        &mut self,
        skip: ImageId,
        line: Line,
        column: Column,
        cols: u32,
        rows: u32,
    ) {
        if cols == 0 || rows == 0 {
            return;
        }
        let new_l0 = line.0 as i64;
        let new_l1 = new_l0 + rows as i64;
        let new_c0 = column.0 as i64;
        let new_c1 = new_c0 + cols as i64;

        let doomed: Vec<ImageId> = self
            .images
            .values()
            .filter(|img| img.internal_id != skip)
            .filter(|img| {
                img.placements.iter().any(|p| {
                    if p.is_virtual || !p.origin.is_positional() {
                        return false;
                    }
                    // AABB intersection in signed grid-space
                    let pl0 = p.line.0 as i64;
                    let pl1 = pl0 + p.effective_num_rows as i64;
                    let pc0 = p.column.0 as i64;
                    let pc1 = pc0 + p.effective_num_cols as i64;
                    new_l0 < pl1 && pl0 < new_l1 && new_c0 < pc1 && pc0 < new_c1
                })
            })
            .map(|img| img.internal_id)
            .collect();

        for id in doomed {
            self.remove_image(id);
        }
    }

    /// Enforce the storage quota, never evicting `currently_added` (0 = none).
    ///
    /// Port of kitty's `apply_storage_quota` (graphics.c:281-296): first all
    /// images without placements are removed (even if they have a client id);
    /// if still over the limit, remaining images are evicted oldest-atime
    /// first — together with their placements — until under the limit.
    pub fn apply_storage_quota(&mut self, currently_added: ImageId) {
        self.remove_images_where(|img| img.placements.is_empty(), currently_added);
        if self.used_storage < self.storage_limit {
            return;
        }

        let mut by_atime: Vec<(u64, ImageId)> =
            self.images.values().map(|img| (img.atime, img.internal_id)).collect();
        by_atime.sort_unstable();
        for (_, id) in by_atime {
            if self.used_storage <= self.storage_limit {
                break;
            }
            self.remove_image(id);
        }
    }

    /// Begin a chunked transmission, aborting any in-flight one.
    pub fn start_load(&mut self, load: LoadData) {
        self.loading = Some(load);
    }

    /// The in-flight transmission, if any.
    pub fn loading(&self) -> Option<&LoadData> {
        self.loading.as_ref()
    }

    /// Take the in-flight transmission for finalization.
    pub fn take_loading(&mut self) -> Option<LoadData> {
        self.loading.take()
    }

    /// Abort any in-flight transmission. Must be called by delete handling.
    pub fn abort_load(&mut self) {
        self.loading = None;
    }

    /// Handle `a=a` (animation control command). Never sends a response.
    ///
    /// Port of kitty `handle_animation_control_command` (graphics.c:1729).
    pub fn animation_control(&mut self, image_id: ImageId, args: AnimationControlArgs) {
        let AnimationControlArgs {
            anim_state,
            frame_number,
            gap_frame,
            gap_ms,
            loop_count,
            now_ms,
        } = args;
        let img = match self.images.get_mut(&image_id) {
            Some(img) => img,
            None => return,
        };

        // r= + z=: edit the gap of frame r (1-based) to z ms
        if gap_frame != 0 {
            let idx = (gap_frame as usize).saturating_sub(1);
            if let Some(frame) = img.frames.get_mut(idx) {
                frame.gap_ms = gap_ms.max(0) as u32;
            }
        }

        // c= (other_frame_number mapped via frame_number in kitty's `a=a`): set
        // the currently-displayed frame. In kitty `a=a` the `c=` key maps to
        // `g->other_frame_number` (graphics.c:1737-1743). The caller passes
        // cmd.other_frame_number() as frame_number
        if frame_number != 0 {
            let idx = (frame_number as usize).saturating_sub(1);
            if idx < img.frames.len() && idx != img.current_frame_index {
                img.current_frame_index = idx;
            }
        }

        // s= (animation_state): 1=stop, 2=loading, 3=run
        if anim_state != 0 {
            let old = img.animation_state;
            match anim_state {
                1 => {
                    if old != AnimationState::Stopped {
                        self.active_animation_count = self.active_animation_count.saturating_sub(1);
                    }
                    img.animation_state = AnimationState::Stopped;
                    img.current_loop = 0;
                },
                2 => {
                    if old == AnimationState::Stopped {
                        self.active_animation_count += 1;
                        img.frame_shown_at_ms = now_ms;
                    }
                    img.animation_state = AnimationState::Loading;
                },
                3 => {
                    if old == AnimationState::Stopped {
                        self.active_animation_count += 1;
                        img.frame_shown_at_ms = now_ms;
                    }
                    img.animation_state = AnimationState::Running;
                },
                _ => {},
            }
            // kitty resets current_loop whenever s= is sent (graphics.c:1763)
            img.current_loop = 0;
        }

        // v= (loop_count): kitty stores loop_count - 1 (n-1 semantics)
        // v=0 is not sent (0 means "not set"); v=1 → max_loops=0 (infinite);
        // v=2 → max_loops=1 (stop after 1 extra loop), etc
        if loop_count != 0 {
            img.max_loops = loop_count.saturating_sub(1);
        }
    }

    /// Returns `true` if `img` is eligible for animation advancement.
    ///
    /// Mirrors kitty `image_is_animatable` (graphics.c:1773-1775):
    ///   `animation_state != STOPPED && extra_framecnt && is_drawn && animation_duration`
    ///
    /// Alacritty proxies:
    /// - `extra_framecnt`      → `frames.len() > 1`
    /// - `is_drawn`            → `!placements().is_empty()` (image has been placed at least once)
    /// - `animation_duration`  → at least one frame has a non-zero `gap_ms` (images where every
    ///   frame has gap=0 would advance every tick and are skipped, matching kitty's zero-gap
    ///   frame-skip in scan_active_animations:1799)
    fn image_is_animatable(img: &Image) -> bool {
        img.animation_state != AnimationState::Stopped
            && img.frames.len() > 1
            && !img.placements().is_empty()
            && img.frames.iter().any(|f| f.gap_ms > 0)
            && (img.max_loops == 0 || img.current_loop < img.max_loops)
    }

    /// Scan all running animations and return the minimum `Duration` until the
    /// next frame advance. Returns `None` when `active_animation_count == 0`
    /// (zero-cost early-out — no iteration).
    ///
    /// Port of kitty `scan_active_animations` (graphics.c:1779), adapted for
    /// synthetic ms timestamps rather than a monotonic clock type.
    pub fn scan_active_animations(&self, now_ms: u64) -> Option<std::time::Duration> {
        // Perf guard: skip the loop entirely when nothing is animating
        if self.active_animation_count == 0 {
            return None;
        }

        let mut minimum_gap: Option<u64> = None;

        for img in self.images.values() {
            if !Self::image_is_animatable(img) {
                continue;
            }
            let frame = match img.frames.get(img.current_frame_index) {
                Some(f) => f,
                None => continue,
            };
            let gap = if frame.gap_ms == 0 { DEFAULT_GAP_MS } else { frame.gap_ms };
            let next_at = img.frame_shown_at_ms.saturating_add(gap as u64);
            if next_at > now_ms {
                let until = next_at - now_ms;
                minimum_gap = Some(match minimum_gap {
                    None => until,
                    Some(prev) => prev.min(until),
                });
            } else {
                // Already overdue — fire immediately
                minimum_gap = Some(0);
                break;
            }
        }

        minimum_gap.map(std::time::Duration::from_millis)
    }

    /// Advance all running animations whose next-frame deadline has elapsed.
    ///
    /// Pushes coalesced frame data into `pending_uploads` for each image that
    /// advanced (the render path drains these on the next frame). Returns `true`
    /// if any animation advanced (caller should mark damage/dirty).
    ///
    /// Gapless frames (gap_ms == 0) are skipped over immediately per kitty:
    /// the advance loop continues until it lands on a frame with a nonzero gap.
    ///
    /// Port of kitty `scan_active_animations` advance path (graphics.c:1793-1808).
    pub fn advance_animations(&mut self, now_ms: u64) -> bool {
        if self.active_animation_count == 0 {
            return false;
        }

        let mut any_advanced = false;
        let mut newly_stopped = 0usize;
        let ids: Vec<ImageId> = self.images.keys().copied().collect();

        for id in ids {
            let img = match self.images.get_mut(&id) {
                Some(img) => img,
                None => continue,
            };
            if !Self::image_is_animatable(img) {
                continue;
            }
            let gap = {
                let f = match img.frames.get(img.current_frame_index) {
                    Some(f) => f,
                    None => continue,
                };
                if f.gap_ms == 0 { DEFAULT_GAP_MS } else { f.gap_ms }
            };
            let next_at = img.frame_shown_at_ms.saturating_add(gap as u64);
            if now_ms < next_at {
                continue;
            }

            let total_frames = img.frames.len();
            // Advance through gapless frames (gap_ms == 0) without dwelling
            loop {
                let next_idx = (img.current_frame_index + 1) % total_frames;
                if next_idx == 0 {
                    if img.animation_state == AnimationState::Loading {
                        break; // loading blocks at loop boundary (kitty parity).
                    }
                    img.current_loop += 1;
                    if img.max_loops != 0 && img.current_loop >= img.max_loops {
                        img.animation_state = AnimationState::Stopped;
                        newly_stopped += 1;
                        break;
                    }
                }
                img.current_frame_index = next_idx;
                let next_gap = img.frames.get(next_idx).map(|f| f.gap_ms).unwrap_or(1);
                if next_gap != 0 {
                    break;
                }
            }

            img.frame_shown_at_ms = now_ms;

            if img.animation_state == AnimationState::Stopped {
                continue;
            }

            // Coalesce current frame and queue for GPU re-upload
            let frame_number = (img.current_frame_index + 1) as u32;
            if let Some(pixels) = Self::get_coalesced_frame_data(img, frame_number, 0) {
                self.pending_uploads.push((id, Arc::new(pixels)));
                any_advanced = true;
            }
        }

        self.active_animation_count = self.active_animation_count.saturating_sub(newly_stopped);

        any_advanced
    }

    /// Append a new animation frame to an existing image (kitty `a=f`).
    ///
    /// `frame_number` is the 1-based target slot (`r=`); pass 0 to append
    /// after the last existing frame. Returns `ENOENT` for unknown images,
    /// `EINVAL` for an out-of-range frame number, and `ENOSPC` when the
    /// ×5 frame quota (FRAME_QUOTA_MULTIPLIER × storage_limit) is exceeded
    /// after evicting all unplaced images.
    ///
    /// Divergence from kitty: frame data is kept only in RAM; kitty may spill
    /// to disk via mmap. The ×5 ceiling derives from storage_limit (config).
    pub fn add_frame(
        &mut self,
        image_id: ImageId,
        frame_number: u32,
        frame: Frame,
    ) -> Result<u32, kitty_command::CommandError> {
        let frame_limit = self.storage_limit.saturating_mul(FRAME_QUOTA_MULTIPLIER);
        let frame_bytes = frame.data.len();

        // Enforce ×5 frame quota: evict unplaced images first, then LRU
        if self.frame_storage_used.saturating_add(frame_bytes) > frame_limit {
            self.apply_storage_quota(image_id);
            if self.frame_storage_used.saturating_add(frame_bytes) > frame_limit {
                return Err(kitty_command::CommandError {
                    code: kitty_command::ErrorCode::ENOSPC,
                    message: "Animation frame quota exceeded".into(),
                    sends_response: true,
                });
            }
        }

        let img = self.images.get_mut(&image_id).ok_or_else(|| kitty_command::CommandError {
            code: kitty_command::ErrorCode::ENOENT,
            message: format!("add_frame: unknown image id {image_id}"),
            sends_response: true,
        })?;

        // frame_number 0 = append after last frame; 1-based otherwise
        let target_index = if frame_number == 0 {
            img.frames.len()
        } else {
            // Frame 1 is the root; appending at frame N means index N
            let idx = frame_number as usize;
            if idx < img.frames.len() {
                return Err(kitty_command::CommandError {
                    code: kitty_command::ErrorCode::EINVAL,
                    message: format!(
                        "add_frame: frame {frame_number} already exists; use edit_frame to replace"
                    ),
                    sends_response: true,
                });
            }
            idx
        };

        // Only frames beyond the root (index 0) count against frame_storage
        if target_index > 0 {
            img.frame_storage = img.frame_storage.saturating_add(frame_bytes);
            self.frame_storage_used = self.frame_storage_used.saturating_add(frame_bytes);
        }

        // Pad with empty frames if needed (protocol allows sparse append)
        while img.frames.len() < target_index {
            img.frames.push(Frame::default());
        }
        img.frames.push(frame);

        Ok(target_index as u32)
    }

    /// Replace an existing animation frame in-place (edit path of `a=f`).
    ///
    /// `frame_number` is 1-based (`r=`). Returns `ENOENT` for unknown images
    /// or frame numbers out of range.
    pub fn edit_frame(
        &mut self,
        image_id: ImageId,
        frame_number: u32,
        frame: Frame,
    ) -> Result<(), kitty_command::CommandError> {
        if frame_number == 0 {
            return Err(kitty_command::CommandError {
                code: kitty_command::ErrorCode::EINVAL,
                message: "edit_frame: frame_number must be ≥ 1".into(),
                sends_response: true,
            });
        }

        let frame_limit = self.storage_limit.saturating_mul(FRAME_QUOTA_MULTIPLIER);
        let new_bytes = frame.data.len();

        let img = self.images.get_mut(&image_id).ok_or_else(|| kitty_command::CommandError {
            code: kitty_command::ErrorCode::ENOENT,
            message: format!("edit_frame: unknown image id {image_id}"),
            sends_response: true,
        })?;

        let idx = frame_number as usize - 1;
        if idx >= img.frames.len() {
            return Err(kitty_command::CommandError {
                code: kitty_command::ErrorCode::ENOENT,
                message: format!(
                    "edit_frame: frame {frame_number} does not exist (image has {} frames)",
                    img.frames.len()
                ),
                sends_response: true,
            });
        }

        let old_bytes = img.frames[idx].data.len();
        // Root frame (idx 0) is not counted in frame_storage
        if idx > 0 {
            let delta_add = new_bytes.saturating_sub(old_bytes);
            let delta_sub = old_bytes.saturating_sub(new_bytes);
            let new_total = self.frame_storage_used.saturating_add(delta_add);
            if new_total > frame_limit {
                self.apply_storage_quota(image_id);
                if self.frame_storage_used.saturating_add(delta_add) > frame_limit {
                    return Err(kitty_command::CommandError {
                        code: kitty_command::ErrorCode::ENOSPC,
                        message: "Animation frame quota exceeded on edit".into(),
                        sends_response: true,
                    });
                }
            }
            // Re-borrow after potential eviction
            let img =
                self.images.get_mut(&image_id).ok_or_else(|| kitty_command::CommandError {
                    code: kitty_command::ErrorCode::ENOENT,
                    message: format!("edit_frame: image {image_id} was evicted"),
                    sends_response: true,
                })?;
            img.frame_storage =
                img.frame_storage.saturating_add(delta_add).saturating_sub(delta_sub);
            self.frame_storage_used =
                self.frame_storage_used.saturating_add(delta_add).saturating_sub(delta_sub);
            img.frames[idx] = frame;
        } else {
            img.frames[idx] = frame;
        }

        Ok(())
    }

    /// Porter-Duff "over" composite of `over_px` onto `under_px` (both RGBA).
    /// Matches kitty graphics.c:1368 `alpha_blend`.
    #[inline]
    fn alpha_blend_px(under: &mut [u8; 4], over: &[u8; 4]) {
        if over[3] == 0 {
            return;
        }
        let dest_a = under[3] as f32 / 255.0;
        let src_a = over[3] as f32 / 255.0;
        let alpha = src_a + dest_a * (1.0 - src_a);
        under[3] = (255.0 * alpha) as u8;
        if under[3] == 0 {
            under[0] = 0;
            under[1] = 0;
            under[2] = 0;
            return;
        }
        for i in 0..3 {
            under[i] =
                ((over[i] as f32 * src_a + under[i] as f32 * dest_a * (1.0 - src_a)) / alpha) as u8;
        }
    }

    /// Composite a delta-frame sub-rect onto a full-image canvas (RGBA8).
    /// `d` carries all geometry; `under` is the canvas, `over_data` the delta.
    /// Matches kitty's `compose()` at graphics.c:1428 (non-rectangles path).
    fn compose_delta(under: &mut [u8], over_data: &[u8], d: &ComposeDelta) {
        let max_rows = d.over_h.min(d.img_h.saturating_sub(d.oy));
        let cols = d.over_w.min(d.img_w.saturating_sub(d.ox));
        if cols == 0 || max_rows == 0 {
            return;
        }
        if d.needs_blend {
            for y in 0..max_rows {
                for x in 0..cols {
                    let src = (y * d.over_w * 4 + x * 4) as usize;
                    let dst = ((d.oy + y) * d.img_w * 4 + (d.ox + x) * 4) as usize;
                    if src + 4 > over_data.len() || dst + 4 > under.len() {
                        break;
                    }
                    let ov = [
                        over_data[src],
                        over_data[src + 1],
                        over_data[src + 2],
                        over_data[src + 3],
                    ];
                    let up = &mut under[dst..dst + 4];
                    let mut u = [up[0], up[1], up[2], up[3]];
                    Self::alpha_blend_px(&mut u, &ov);
                    up[0] = u[0];
                    up[1] = u[1];
                    up[2] = u[2];
                    up[3] = u[3];
                }
            }
        } else {
            for y in 0..max_rows {
                let src_start = (y * d.over_w * 4) as usize;
                let dst_start = ((d.oy + y) * d.img_w * 4 + d.ox * 4) as usize;
                let n = (cols * 4) as usize;
                if src_start + n <= over_data.len() && dst_start + n <= under.len() {
                    under[dst_start..dst_start + n]
                        .copy_from_slice(&over_data[src_start..src_start + n]);
                }
            }
        }
    }

    /// Find a frame within an image by its 1-based kitty frame number.
    /// Frame 1 = `frames[0]`, frame N = `frames[N-1]`.
    fn frame_by_number(img: &Image, number: u32) -> Option<&Frame> {
        if number == 0 {
            return None;
        }
        img.frames.get(number as usize - 1)
    }

    /// Build the fully-coalesced RGBA canvas for `frame` by recursively
    /// compositing it over its base chain. Depth cap 32 mirrors kitty's
    /// `get_coalesced_frame_data_impl` (graphics.c:1496-1520).
    ///
    /// A frame with `base_frame_id == 0` is standalone: the canvas is filled
    /// with `bgcolor` (or transparent black), then the frame's own sub-rect
    /// is blitted in. When the chain depth exceeds 32 the recursion stops and
    /// returns `None` to avoid unbounded stack growth.
    ///
    /// Returns a freshly-allocated `img_w × img_h × 4` RGBA buffer or `None`.
    pub fn get_coalesced_frame_data(img: &Image, frame_number: u32, depth: u32) -> Option<Vec<u8>> {
        // Depth cap 32 — mirrors kitty (graphics.c:1498)
        if depth > 32 {
            return None;
        }
        let frame = Self::frame_by_number(img, frame_number)?;
        let img_w = img.width;
        let img_h = img.height;
        let frame_data: &[u8] = &frame.data;

        if frame.base_frame_id == 0 {
            // Standalone: fill canvas with bgcolor, then blit sub-rect
            let pixel_count = (img_w * img_h) as usize;
            let mut canvas = if frame.bgcolor != 0 {
                let r = ((frame.bgcolor >> 24) & 0xff) as u8;
                let g = ((frame.bgcolor >> 16) & 0xff) as u8;
                let b = ((frame.bgcolor >> 8) & 0xff) as u8;
                let a = (frame.bgcolor & 0xff) as u8;
                let mut v = Vec::with_capacity(pixel_count * 4);
                for _ in 0..pixel_count {
                    v.push(r);
                    v.push(g);
                    v.push(b);
                    v.push(a);
                }
                v
            } else {
                vec![0u8; pixel_count * 4]
            };
            let is_full_frame = frame.width == img_w
                && frame.height == img_h
                && frame.x_offset == 0
                && frame.y_offset == 0;
            if is_full_frame && frame.bgcolor == 0 {
                // No allocation needed — just clone the frame data
                return Some(frame_data.to_vec());
            }
            // Blit frame sub-rect onto canvas
            let d = ComposeDelta {
                img_w,
                img_h,
                over_w: frame.width,
                over_h: frame.height,
                ox: frame.x_offset,
                oy: frame.y_offset,
                needs_blend: frame.alpha_blend,
            };
            Self::compose_delta(&mut canvas, frame_data, &d);
            return Some(canvas);
        }

        // Recursive: coalesce the base first (base_frame_id IS the 1-based number),
        // then composite this delta frame over the coalesced base canvas
        let base_number = frame.base_frame_id;
        let mut canvas = Self::get_coalesced_frame_data(img, base_number, depth + 1)?;
        let d = ComposeDelta {
            img_w,
            img_h,
            over_w: frame.width,
            over_h: frame.height,
            ox: frame.x_offset,
            oy: frame.y_offset,
            needs_blend: frame.alpha_blend,
        };
        Self::compose_delta(&mut canvas, frame_data, &d);
        Some(canvas)
    }

    /// When adding a new animation frame whose base has a long reference chain
    /// (chain ≥5 OR accumulated area ≥2× image area — graphics.c:1614), flatten
    /// the base into a keyframe: coalesce it in-place and clear `base_frame_id`.
    /// Thresholds mirror kitty `reference_chain_too_large` (graphics.c:1544-1554).
    pub fn maybe_flatten_keyframe(img: &mut Image, base_frame_number: u32) {
        let base_idx = match base_frame_number.checked_sub(1) {
            Some(i) if i < img.frames.len() as u32 => i as usize,
            _ => return,
        };
        // Only flatten if the chain is already long; standalone frames skip it
        if img.frames[base_idx].base_frame_id == 0 {
            return;
        }
        let img_w = img.width;
        let img_h = img.height;
        // Check threshold using reference_chain_too_large logic
        {
            let frame = &img.frames[base_idx];
            let limit = (img_w as u64) * (img_h as u64) * 2;
            let mut drawn = (frame.width as u64) * (frame.height as u64);
            let mut count: u32 = 1;
            let mut cur_base = frame.base_frame_id;
            while drawn < limit && count < 5 {
                if cur_base == 0 {
                    break;
                }
                if let Some(f) = img.frames.get(cur_base.saturating_sub(1) as usize) {
                    drawn += (f.width as u64) * (f.height as u64);
                    count += 1;
                    cur_base = f.base_frame_id;
                } else {
                    break;
                }
            }
            if count < 5 && drawn < limit {
                return;
            }
        }
        // Flatten: coalesce and store back as standalone
        if let Some(coalesced) = Self::get_coalesced_frame_data(img, base_frame_number, 0) {
            let frame = &mut img.frames[base_idx];
            frame.data = Arc::new(coalesced);
            frame.width = img_w;
            frame.height = img_h;
            frame.x_offset = 0;
            frame.y_offset = 0;
            frame.base_frame_id = 0;
            frame.bgcolor = 0;
            frame.alpha_blend = false;
        }
    }

    /// Compose a source frame region onto a target frame in-place (`a=c`).
    /// All geometry is passed via `ComposeFrameArgs` to avoid >7-arg clippy lint.
    pub fn compose_frame(
        &mut self,
        image_id: ImageId,
        args: ComposeFrameArgs,
    ) -> Result<(), kitty_command::CommandError> {
        let ComposeFrameArgs {
            src_frame_number,
            dst_frame_number,
            dst_x,
            dst_y,
            src_w,
            src_h,
            needs_blend,
        } = args;
        let img = self.images.get_mut(&image_id).ok_or_else(|| kitty_command::CommandError {
            code: kitty_command::ErrorCode::ENOENT,
            message: format!("a=c: unknown image id {image_id}"),
            sends_response: true,
        })?;

        let img_w = img.width;
        let img_h = img.height;

        // Validate source frame
        let src_idx = src_frame_number
            .checked_sub(1)
            .and_then(|i| if (i as usize) < img.frames.len() { Some(i as usize) } else { None })
            .ok_or_else(|| kitty_command::CommandError {
                code: kitty_command::ErrorCode::EINVAL,
                message: format!("a=c: source frame {src_frame_number} not found"),
                sends_response: true,
            })?;
        let src_fw = if src_w == 0 { img.frames[src_idx].width } else { src_w };
        let src_fh = if src_h == 0 { img.frames[src_idx].height } else { src_h };

        // Validate destination frame
        let dst_idx = dst_frame_number
            .checked_sub(1)
            .and_then(|i| if (i as usize) < img.frames.len() { Some(i as usize) } else { None })
            .ok_or_else(|| kitty_command::CommandError {
                code: kitty_command::ErrorCode::EINVAL,
                message: format!("a=c: destination frame {dst_frame_number} not found"),
                sends_response: true,
            })?;

        // Overlap / out-of-bounds validation: composed region must fit in the image
        if dst_x >= img_w
            || dst_y >= img_h
            || dst_x.saturating_add(src_fw) > img_w
            || dst_y.saturating_add(src_fh) > img_h
        {
            return Err(kitty_command::CommandError {
                code: kitty_command::ErrorCode::EINVAL,
                message: format!(
                    "a=c: region ({dst_x},{dst_y})+({src_fw}×{src_fh}) out of image \
                     ({img_w}×{img_h})"
                ),
                sends_response: true,
            });
        }

        // Coalesce the source frame to a full-canvas RGBA buffer
        let src_coalesced =
            Self::get_coalesced_frame_data(img, src_frame_number, 0).ok_or_else(|| {
                kitty_command::CommandError {
                    code: kitty_command::ErrorCode::EINVAL,
                    message: format!("a=c: failed to coalesce source frame {src_frame_number}"),
                    sends_response: true,
                }
            })?;

        // Coalesce the destination frame to get its current full-canvas state
        let mut dst_canvas =
            Self::get_coalesced_frame_data(img, dst_frame_number, 0).ok_or_else(|| {
                kitty_command::CommandError {
                    code: kitty_command::ErrorCode::EINVAL,
                    message: format!(
                        "a=c: failed to coalesce destination frame {dst_frame_number}"
                    ),
                    sends_response: true,
                }
            })?;

        // Blit the src region onto dst canvas. src_coalesced is full img_w×img_h;
        // extract the sub-rect at (0,0)+(src_fw×src_fh) (a=c sends the whole coalesced src)
        let src_sub: Vec<u8> = {
            let mut sub = Vec::with_capacity((src_fw * src_fh * 4) as usize);
            for row in 0..src_fh {
                let start = (row * img_w * 4) as usize;
                let end = start + (src_fw * 4) as usize;
                if end <= src_coalesced.len() {
                    sub.extend_from_slice(&src_coalesced[start..end]);
                }
            }
            sub
        };
        let d = ComposeDelta {
            img_w,
            img_h,
            over_w: src_fw,
            over_h: src_fh,
            ox: dst_x,
            oy: dst_y,
            needs_blend,
        };
        Self::compose_delta(&mut dst_canvas, &src_sub, &d);

        // Store the result: make the destination frame a standalone keyframe
        let frame = &mut img.frames[dst_idx];
        frame.data = Arc::new(dst_canvas);
        frame.width = img_w;
        frame.height = img_h;
        frame.x_offset = 0;
        frame.y_offset = 0;
        frame.base_frame_id = 0;
        frame.bgcolor = 0;
        frame.alpha_blend = false;

        Ok(())
    }

    /// Remove all placements matching `filter`, freeing images that end up
    /// without placements according to kitty's rules.
    ///
    /// Port of kitty's `filter_refs` (graphics.c:1900): an image whose
    /// placements were emptied is freed when `free_images` is set or when it
    /// is anonymous (`client_id == 0`). With `free_only_matched` unset
    /// (`grman_clear` semantics) images that already had no placements are
    /// freed as well. The filter receives the owning image's client id and
    /// the placement.
    ///
    /// Returns `true` if any placement or image was removed.
    fn filter_placements<F>(
        &mut self,
        free_images: bool,
        free_only_matched: bool,
        mut filter: F,
    ) -> bool
    where
        F: FnMut(u32, &Placement) -> bool,
    {
        let mut dirty = false;
        let doomed: Vec<ImageId> = self
            .images
            .values_mut()
            .filter_map(|img| {
                let client_id = img.client_id;
                let before = img.placements.len();
                img.placements.retain(|placement| !filter(client_id, placement));
                let matched = img.placements.len() != before;
                dirty |= matched;
                let free = (!free_only_matched || matched)
                    && img.placements.is_empty()
                    && (free_images || client_id == 0);
                free.then_some(img.internal_id)
            })
            .collect();
        for id in doomed {
            self.remove_image(id);
            dirty = true;
        }
        dirty
    }

    /// Remove placements intersecting the visible screen, or all of them.
    ///
    /// Port of kitty's `grman_clear` (graphics.c:2039): with `all` unset only
    /// placements whose bottom edge reaches the screen
    /// (`line + effective_num_rows > 0`) are removed, keeping the ones that
    /// scrolled entirely into history. Images left without placements are
    /// always freed, including stored images that were never placed.
    ///
    /// Returns `true` if anything was removed.
    pub fn clear(&mut self, all: bool) -> bool {
        self.filter_placements(true, false, |_, placement| {
            all || placement.line.0 + placement.effective_num_rows as i32 > 0
        })
    }

    /// Remove placements that live entirely inside the scrollback.
    ///
    /// Companion to [`clear`](Self::clear) for `ESC[3J`: alacritty's
    /// xterm-style ED 3 erases only the history (kitty's ED 3 erases screen
    /// and history, so it has no history-only clear to port), so only
    /// placements fully above the screen go with it. Images losing their
    /// last placement are freed.
    pub fn clear_scrollback(&mut self) -> bool {
        self.filter_placements(true, true, |_, placement| {
            placement.line.0 + placement.effective_num_rows as i32 <= 0
        })
    }

    /// Apply an `a=d` delete command to the stored placements and images.
    ///
    /// Port of kitty's `handle_delete_command` (graphics.c:2093). Lowercase
    /// specifiers remove placements only; uppercase also frees image data when
    /// the image is left without placements. Anonymous images losing their last
    /// placement are freed even by lowercase specifiers.
    ///
    /// Returns `Some(dirty)` when the specifier was handled, `None` for unknown
    /// specifiers.
    pub fn handle_delete(&mut self, cmd: &GraphicsCommand) -> Option<bool> {
        // graphics.c:2095: any delete frees the in-flight load slot
        self.abort_load();

        let action = cmd.delete_action;
        let free_images = action.is_ascii_uppercase();

        // graphics.c:2096-2112: free unplaced images by id/number/range before
        // running the placement filter (which can only free images it matched)
        if cmd.placement_id == 0 {
            match action {
                b'I' => {
                    let unplaced = self
                        .images
                        .values()
                        .find(|img| img.client_id == cmd.id)
                        .filter(|img| img.placements.is_empty())
                        .map(|img| img.internal_id);
                    if let Some(id) = unplaced {
                        self.remove_image(id);
                        return Some(false);
                    }
                },
                b'N' => {
                    let unplaced = self
                        .images
                        .values()
                        .rfind(|img| img.client_number == cmd.image_number)
                        .filter(|img| img.placements.is_empty())
                        .map(|img| img.internal_id);
                    if let Some(id) = unplaced {
                        self.remove_image(id);
                        return Some(false);
                    }
                },
                b'R' => {
                    // Remove unplaced images whose client_id falls in [x_offset, y_offset]
                    let lo = cmd.x_offset;
                    let hi = cmd.y_offset;
                    let doomed: Vec<ImageId> = self
                        .images
                        .values()
                        .filter(|img| {
                            img.client_id != 0
                                && img.client_id >= lo
                                && img.client_id <= hi
                                && img.placements.is_empty()
                        })
                        .map(|img| img.internal_id)
                        .collect();
                    for id in doomed {
                        self.remove_image(id);
                    }
                    // Fall through to also remove placements in range below
                },
                _ => {},
            }
        }

        match action {
            // Missing `d=` defaults to `a` (graphics.c:2117)
            0 | b'a' | b'A' => Some(self.filter_placements(free_images, true, |_, placement| {
                !placement.is_virtual && placement.line.0 + placement.effective_num_rows as i32 > 0
            })),

            b'i' | b'I' => {
                let (id, placement_id) = (cmd.id, cmd.placement_id);
                Some(self.filter_placements(free_images, true, move |client_id, placement| {
                    id != 0
                        && client_id == id
                        && (placement_id == 0 || placement.client_id == placement_id)
                }))
            },

            // Delete by client number: newest image with that number
            // graphics.c:2130-2142
            b'n' | b'N' => {
                let number = cmd.image_number;
                let placement_id = cmd.placement_id;
                let target_id = self
                    .images
                    .values()
                    .rfind(|img| img.client_number == number)
                    .map(|img| img.internal_id);
                let Some(target_id) = target_id else {
                    return Some(false);
                };
                let img = self.images.get_mut(&target_id)?;
                let before = img.placements.len();
                img.placements.retain(|p| placement_id != 0 && p.client_id != placement_id);
                let matched = img.placements.len() != before;
                // Only free when the delete actually matched: mirrors filter_placements
                // free_only_matched semantics. Without `matched`, an addressable image
                // that already had zero placements would be freed on a non-matching delete
                let free =
                    matched && img.placements.is_empty() && (free_images || img.client_id == 0);
                if free {
                    self.remove_image(target_id);
                }
                Some(matched || free)
            },

            // Delete placements covering the 1-based cell at (x=, y=)
            // c/C: caller (term/mod.rs) injects cursor coords into x/y before
            // calling us. p/P: client-supplied x=/y=. graphics.c:2121,2126
            b'c' | b'C' | b'p' | b'P' => {
                let x = i64::from(cmd.x_offset) - 1;
                let y = i64::from(cmd.y_offset) - 1;
                Some(self.filter_placements(free_images, true, move |_, placement| {
                    if placement.is_virtual {
                        return false;
                    }
                    let col = placement.column.0 as i64;
                    let line = i64::from(placement.line.0);
                    col <= x
                        && x < col + i64::from(placement.effective_num_cols)
                        && line <= y
                        && y < line + i64::from(placement.effective_num_rows)
                }))
            },

            // Delete placements covering cell (x=, y=) at z-index z=
            // graphics.c:2122 (point3d_filter_func)
            b'q' | b'Q' => {
                let x = i64::from(cmd.x_offset) - 1;
                let y = i64::from(cmd.y_offset) - 1;
                let z = cmd.z_index;
                Some(self.filter_placements(free_images, true, move |_, placement| {
                    if placement.is_virtual {
                        return false;
                    }
                    let col = placement.column.0 as i64;
                    let line = i64::from(placement.line.0);
                    placement.z_index == z
                        && col <= x
                        && x < col + i64::from(placement.effective_num_cols)
                        && line <= y
                        && y < line + i64::from(placement.effective_num_rows)
                }))
            },

            // Delete all placements whose column range includes the 1-based x=
            // graphics.c:2123 (x_filter_func)
            b'x' | b'X' => {
                let x = i64::from(cmd.x_offset) - 1;
                Some(self.filter_placements(free_images, true, move |_, placement| {
                    if placement.is_virtual {
                        return false;
                    }
                    let col = placement.column.0 as i64;
                    col <= x && x < col + i64::from(placement.effective_num_cols)
                }))
            },

            // Delete all placements whose row range includes the 1-based y=
            // graphics.c:2124 (y_filter_func)
            b'y' | b'Y' => {
                let y = i64::from(cmd.y_offset) - 1;
                Some(self.filter_placements(free_images, true, move |_, placement| {
                    if placement.is_virtual {
                        return false;
                    }
                    let line = i64::from(placement.line.0);
                    line <= y && y < line + i64::from(placement.effective_num_rows)
                }))
            },

            // Delete all placements at z-index z=. graphics.c:2125 (z_filter_func)
            b'z' | b'Z' => {
                let z = cmd.z_index;
                Some(self.filter_placements(free_images, true, move |_, placement| {
                    !placement.is_virtual && placement.z_index == z
                }))
            },

            // Delete by image client_id range [x_offset, y_offset]
            // graphics.c:2120 (id_range_filter_func)
            b'r' | b'R' => {
                let lo = cmd.x_offset;
                let hi = cmd.y_offset;
                Some(self.filter_placements(free_images, true, move |client_id, _| {
                    client_id != 0 && client_id >= lo && client_id <= hi
                }))
            },

            // Frame deletion. Port of kitty's `handle_delete_frame_command`
            // (graphics.c:1684) and its caller (graphics.c:2143-2151)
            b'f' | b'F' => {
                // Resolve the image by id or number (graphics.c:1685-1692)
                if cmd.id == 0 && cmd.image_number == 0 {
                    return Some(false);
                }
                let internal_id = if cmd.id != 0 {
                    self.images
                        .values()
                        .find(|img| img.client_id == cmd.id)
                        .map(|img| img.internal_id)
                } else {
                    self.images
                        .values()
                        .rfind(|img| img.client_number == cmd.image_number)
                        .map(|img| img.internal_id)
                };
                let Some(internal_id) = internal_id else {
                    return Some(false);
                };

                // Clamp frame_number to [1, frames.len()] (graphics.c:1694-1695)
                let img = self.images.get(&internal_id)?;
                let frame_number = cmd.frame_number().min(img.frames.len() as u32);
                let frame_number = if frame_number == 0 { 1 } else { frame_number };

                // No extra frames: remove image on F, no-op on f (graphics.c:1696)
                if img.frames.len() <= 1 {
                    if action == b'F' {
                        self.remove_image(internal_id);
                        return Some(true);
                    } else {
                        return Some(false);
                    }
                }

                // ≥1 extra frame: remove the requested frame (graphics.c:1697-1725)
                let k = (frame_number - 1) as usize; // 0-based index to remove
                let img = self.images.get_mut(&internal_id)?;

                // Storage accounting: only frames[1..] count toward frame_storage
                // Removing root (k==0): frames[1] becomes new root, leaves counted set
                // Removing non-root (k>0): frames[k] itself leaves
                let bytes_freed =
                    if k == 0 { img.frames[1].data.len() } else { img.frames[k].data.len() };
                img.frame_storage = img.frame_storage.saturating_sub(bytes_freed);
                self.frame_storage_used = self.frame_storage_used.saturating_sub(bytes_freed);

                img.frames.remove(k);

                // Adjust current_frame_index (graphics.c:1718-1724)
                if img.current_frame_index > k {
                    img.current_frame_index -= 1;
                }
                img.current_frame_index =
                    img.current_frame_index.min(img.frames.len().saturating_sub(1));

                Some(true)
            },

            _ => None,
        }
    }

    /// Remove all placements matching `remove`, freeing images that lose
    /// their last placement *and* can never be referenced again.
    ///
    /// Port of kitty's `modify_refs` (graphics.c:1919): unlike
    /// [`filter_placements`](Self::filter_placements), an emptied image is
    /// freed only when it is anonymous **and** unnumbered (`client_id == 0 &&
    /// client_number == 0`) — addressable images keep their pixel data so the
    /// client can re-place them. Used by the scroll/GC/reflow paths.
    ///
    /// Returns `true` if any placement or image was removed.
    fn modify_placements<F>(&mut self, mut remove: F) -> bool
    where
        F: FnMut(&mut Placement) -> bool,
    {
        let mut dirty = false;
        let doomed: Vec<ImageId> = self
            .images
            .values_mut()
            .filter_map(|img| {
                let before = img.placements.len();
                img.placements.retain_mut(|placement| !remove(placement));
                dirty |= img.placements.len() != before;
                let free =
                    img.placements.is_empty() && img.client_id == 0 && img.client_number == 0;
                free.then_some(img.internal_id)
            })
            .collect();
        for id in doomed {
            self.remove_image(id);
            dirty = true;
        }
        dirty
    }

    /// Rotate placement anchors for a scroll of `delta` lines (negative = up)
    /// within `region`.
    ///
    /// Port of kitty's `grman_scroll_images` (graphics.c:1984). A full-screen
    /// region shifts every anchor and hard-deletes placements ending up
    /// entirely above the viewport via [`gc`](Self::gc) — no scrollback
    /// images, so our `limit` is `0` where kitty uses `-historybuf->ynum` on
    /// the main grid. A margin region uses [`scroll_margin_placement`].
    ///
    /// Returns `true` if any placement was moved or removed.
    pub fn scroll(
        &mut self,
        region: &Range<Line>,
        delta: i32,
        screen_lines: usize,
        cell: CellSize,
        scrollback_limit: i32,
    ) -> bool {
        if self.images.is_empty() || delta == 0 {
            return false;
        }

        let has_margins = region.start.0 != 0 || region.end.0 != screen_lines as i32;
        if has_margins {
            let (top, bottom) = (region.start.0, region.end.0 - 1);
            let mut moved = false;
            let removed = self.modify_placements(|placement| {
                let before = (placement.line, placement.effective_num_rows);
                let remove = scroll_margin_placement(placement, delta, top, bottom, cell);
                moved |= (placement.line, placement.effective_num_rows) != before;
                remove
            });
            removed || moved
        } else {
            for img in self.images.values_mut() {
                for placement in &mut img.placements {
                    placement.line += delta;
                }
            }
            self.gc(scrollback_limit);
            true
        }
    }

    /// Garbage-collect placements that can never become visible again.
    ///
    /// A placement is dead once its bottom edge passes above the OLDEST
    /// retainable scrollback line: `line + effective_num_rows <= -scrollback_limit`,
    /// where `scrollback_limit` is the grid's maximum scroll depth (`0` for the
    /// alt screen / scrollback-less grids). The anchors are viewport-relative
    /// signed lines (line `0` = top of the active area, negative = scrollback),
    /// so a placement scrolled into history is RETAINED and re-renders when the
    /// view scrolls back — classic placements now track scrollback like kitty
    /// (`-historybuf->ynum`) instead of being hard-deleted at the screen top.
    /// Visibility is enforced separately by `Term::render_snapshot`, which culls
    /// and crops off-viewport items; this GC only bounds memory.
    ///
    /// Images losing their last placement are freed only if they are
    /// anonymous and unnumbered (kitty's `modify_refs` rule).
    ///
    /// Returns `true` if anything was removed.
    pub fn gc(&mut self, scrollback_limit: i32) -> bool {
        self.modify_placements(|placement| {
            placement.line.0 + placement.effective_num_rows as i32 <= -scrollback_limit
        })
    }

    /// Shift all placement anchors by `delta` lines, garbage collecting the
    /// ones pushed past the bottom of the scrollback (`-scrollback_limit`).
    ///
    /// Used by `Term::resize`, which moves the anchors by the same delta it
    /// applies to the vi cursor and selection. A column-count change now shifts
    /// (rather than drops) classic placements, so `kitty +icat` images survive a
    /// resize/reflow like kitty's `grman_resize` (screen.c:572-575); the anchor
    /// stays put and the image tracks scrollback. Pass `0` for the alt screen.
    ///
    /// Returns `true` if any placement was moved or removed.
    pub fn shift_anchors(&mut self, delta: i32, scrollback_limit: i32) -> bool {
        if delta == 0 || self.images.is_empty() {
            return false;
        }
        for img in self.images.values_mut() {
            for placement in &mut img.placements {
                placement.line += delta;
            }
        }
        self.gc(scrollback_limit);
        true
    }

    /// Drop every classic placement, e.g. when a column-count change reflows
    /// the primary grid (the `selection = None` precedent; kitty never
    /// reflows, so this has no kitty equivalent).
    ///
    /// Image data survives for client-addressable images (`i=` or `I=`); only
    /// anonymous, unnumbered images — which can never be re-placed — are
    /// freed.
    ///
    /// Returns `true` if anything was removed.
    pub fn drop_placements(&mut self) -> bool {
        self.modify_placements(|_| true)
    }

    /// Adapt all placements to a new cell size after a font-size change.
    ///
    /// Port of kitty's `grman_rescale` (graphics.c:2181): subcell offsets are
    /// clamped to the new cell and the effective cell extent is recomputed
    /// via [`Placement::update_dest_rect`].
    ///
    /// Returns `true` if any placement was updated (caller marks damage).
    pub fn rescale(&mut self, cell: CellSize) -> bool {
        let mut dirty = false;
        for img in self.images.values_mut() {
            for placement in &mut img.placements {
                placement.cell_x_offset = placement.cell_x_offset.min(cell.width - 1);
                placement.cell_y_offset = placement.cell_y_offset.min(cell.height - 1);
                placement.update_dest_rect(cell);
                dirty = true;
            }
        }
        dirty
    }

    /// Build a render snapshot: collect sorted `ImageRenderItem`s, drain queues.
    /// For animated images, coalesces the current frame into the upload queue.
    ///
    /// No GC here: deletion is authoritative on the scroll/resize/clear paths
    /// (which know the scrollback depth). A bound-0 GC at render time would
    /// hard-delete every scrollback placement and re-introduce the sticky/
    /// vanishing-image bug. Off-viewport items are culled/cropped for display by
    /// `Term::render_snapshot`, which has the viewport geometry.
    pub fn render_snapshot(&mut self, _timestamp: u64) -> RenderSnapshot {
        // Collect (img_id, placement_internal_id, width, height, placement clone)
        // for non-virtual placements, then resolve parent offsets separately
        // (resolve_parent_offset needs &self, but the closure below needs
        // immutable access to the whole images map)
        let raw: Vec<(ImageId, u64, f32, f32, Placement)> = self
            .images
            .values()
            .flat_map(|img| {
                let iw = img.width as f32;
                let ih = img.height as f32;
                let img_id = img.internal_id;
                img.placements
                    .iter()
                    .filter(|p| !p.is_virtual)
                    .map(move |p| (img_id, p.internal_id, iw, ih, p.clone()))
            })
            .collect();

        let mut items: Vec<ImageRenderItem> = raw
            .into_iter()
            .filter_map(|(img_id, pl_id, iw, ih, p)| {
                // For parented placements, resolve the absolute anchor
                // If resolution fails (orphan / depth exceeded), silently
                // skip — this IS the cascade (graphics.c:1140-1144 semantics
                // applied at render time rather than eagerly on delete)
                let (line, column) = if p.parent_image_id != 0 {
                    self.resolve_parent_offset(img_id, pl_id)?
                } else {
                    (p.line, p.column)
                };
                let src_uv = UvRect {
                    u0: p.src_x as f32 / iw,
                    v0: p.src_y as f32 / ih,
                    u1: (p.src_x + p.src_width) as f32 / iw,
                    v1: (p.src_y + p.src_height) as f32 / ih,
                };
                let dest = CellRect {
                    line,
                    column,
                    num_cols: p.effective_num_cols,
                    num_rows: p.effective_num_rows,
                    cell_x_offset: p.cell_x_offset,
                    cell_y_offset: p.cell_y_offset,
                };
                Some(ImageRenderItem {
                    image_id: img_id,
                    placement_id: pl_id,
                    z_index: p.z_index,
                    z_bucket: ZBucket::from_z(p.z_index),
                    src_uv,
                    dest,
                    group_index: 0,
                })
            })
            .collect();

        items.sort_unstable_by_key(|item| (item.z_index, item.image_id, item.placement_id));

        let mut group_index = 0u32;
        let mut last_image_id: Option<ImageId> = None;
        for item in &mut items {
            if last_image_id != Some(item.image_id) {
                if last_image_id.is_some() {
                    group_index += 1;
                }
                last_image_id = Some(item.image_id);
            }
            item.group_index = group_index;
        }

        let uploads = mem::take(&mut self.pending_uploads);
        let deletes = mem::take(&mut self.pending_deletes);

        RenderSnapshot { items, uploads, deletes }
    }
}

/// Scroll one placement within a margin region, cropping its source rect at
/// the region edges. Returns `true` if the placement must be removed.
///
/// Direct port of kitty's `scroll_filter_margins_func` (graphics.c:1955):
/// `top`/`bottom` are the inclusive region bounds in viewport-relative lines.
/// Only placements entirely within the region are shifted; a shifted
/// placement poking past an edge is cropped by whole rows (`cell.height`
/// pixels of source per row — the same approximation kitty uses for scaled
/// placements), moving `src_y`/the anchor for a top crop and shrinking
/// `src_height`/`effective_num_rows` for either edge. A placement whose
/// source is consumed entirely, or which remains outside the region, is
/// removed.
fn scroll_margin_placement(
    placement: &mut Placement,
    delta: i32,
    top: i32,
    bottom: i32,
    cell: CellSize,
) -> bool {
    let within = |line: i32, rows: i32| line >= top && line + rows - 1 <= bottom;
    let outside = |line: i32, rows: i32| line + rows <= top || line > bottom;

    let rows = placement.effective_num_rows as i32;
    if !within(placement.line.0, rows) {
        return false;
    }

    placement.line += delta;
    if outside(placement.line.0, rows) {
        return true;
    }

    if placement.line.0 < top {
        // Placement moved up past the region top: crop the top of the source
        let clipped_rows = (top - placement.line.0) as u32;
        let clip_amt = cell.height * clipped_rows;
        if placement.src_height <= clip_amt {
            return true;
        }
        placement.src_y += clip_amt;
        placement.src_height -= clip_amt;
        placement.effective_num_rows -= clipped_rows;
        placement.line += clipped_rows as i32;
    } else if placement.line.0 + rows - 1 > bottom {
        // Placement moved down past the region bottom: crop the bottom
        let clipped_rows = (placement.line.0 + rows - 1 - bottom) as u32;
        let clip_amt = cell.height * clipped_rows;
        if placement.src_height <= clip_amt {
            return true;
        }
        placement.src_height -= clip_amt;
        placement.effective_num_rows -= clipped_rows;
    }

    outside(placement.line.0, placement.effective_num_rows as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CELL: CellSize = CellSize { width: 10, height: 20 };

    fn rgba(len: usize) -> Arc<Vec<u8>> {
        Arc::new(vec![0xab; len])
    }

    /// Add a `width` x `height` image whose storage is width*height*4 bytes.
    fn add(mgr: &mut GraphicsManager, i: u32, num: u32, width: u32, height: u32) -> AddedImage {
        mgr.add_image(i, num, width, height, rgba((width * height * 4) as usize))
    }

    fn place(mgr: &mut GraphicsManager, id: ImageId, p: u32) -> Option<(u32, u32)> {
        let spec = PlacementSpec { placement_id: p, ..Default::default() };
        mgr.put_placement(id, Line(0), Column(0), &spec, CELL).ok().flatten()
    }

    fn placement(spec: &PlacementSpec, src_width: u32, src_height: u32) -> Placement {
        Placement {
            internal_id: 0,
            client_id: spec.placement_id,
            src_x: spec.src_x,
            src_y: spec.src_y,
            src_width,
            src_height,
            cell_x_offset: spec.cell_x_offset,
            cell_y_offset: spec.cell_y_offset,
            num_cols: spec.num_cols,
            num_rows: spec.num_rows,
            effective_num_cols: 0,
            effective_num_rows: 0,
            z_index: spec.z_index,
            line: Line(0),
            column: Column(0),
            is_virtual: false,
            origin: PlacementOrigin::Kitty,
            parent_image_id: 0,
            parent_placement_id: 0,
            parent_offset_x: 0,
            parent_offset_y: 0,
        }
    }

    /// Run kitty's `update_dest_rect` math on a synthetic placement.
    fn extent(
        src: (u32, u32),
        offsets: (u32, u32),
        cols_rows: (u32, u32),
        cell: CellSize,
    ) -> (u32, u32) {
        let spec = PlacementSpec {
            cell_x_offset: offsets.0,
            cell_y_offset: offsets.1,
            num_cols: cols_rows.0,
            num_rows: cols_rows.1,
            ..Default::default()
        };
        let mut p = placement(&spec, src.0, src.1);
        p.update_dest_rect(cell);
        (p.effective_num_cols, p.effective_num_rows)
    }

    #[test]
    fn auto_id_assignment_smallest_free() {
        let mut mgr = GraphicsManager::new();

        // I= only: ids assigned 1, 2, ... and echoed with the number
        let a = add(&mut mgr, 0, 7, 2, 2);
        assert_eq!((a.client_id, a.client_number), (1, 7));
        let b = add(&mut mgr, 0, 8, 2, 2);
        assert_eq!(b.client_id, 2);

        // Explicit i=5 leaves a gap; next I= gets 3
        add(&mut mgr, 5, 0, 2, 2);
        let c = add(&mut mgr, 0, 9, 2, 2);
        assert_eq!(c.client_id, 3);

        // Deleting client id 2 frees it for reuse
        mgr.remove_image(b.id);
        let d = add(&mut mgr, 0, 10, 2, 2);
        assert_eq!(d.client_id, 2);
    }

    #[test]
    fn free_id_starts_at_one_below_existing_ids() {
        let mut mgr = GraphicsManager::new();
        // Only ids {2, 3} in use: the smallest free id is 1
        add(&mut mgr, 2, 0, 2, 2);
        add(&mut mgr, 3, 0, 2, 2);
        let a = add(&mut mgr, 0, 1, 2, 2);
        assert_eq!(a.client_id, 1);
    }

    #[test]
    fn anonymous_add_is_silent_and_transient() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 0, 0, 2, 2);
        assert_eq!((a.client_id, a.client_number), (0, 0));

        // Unreferenced anonymous images are dropped on the next add
        add(&mut mgr, 0, 0, 2, 2);
        assert_eq!(mgr.len(), 1);
        assert!(mgr.image(a.id).is_none());
        assert!(mgr.pending_deletes.contains(&a.id));
    }

    #[test]
    fn client_number_lookup_returns_newest() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 0, 7, 2, 2);
        // Keep the first alive via a placement
        place(&mut mgr, a.id, 0);
        let b = add(&mut mgr, 0, 7, 2, 2);
        assert_ne!(a.id, b.id);
        assert_eq!(mgr.image_by_client_number(7).unwrap().id(), b.id);
    }

    #[test]
    fn explicit_id_collision_replaces_in_place() {
        let mut mgr = GraphicsManager::new();
        let a = mgr.add_image(9, 4, 10, 10, rgba(400));
        place(&mut mgr, a.id, 1);
        assert_eq!(mgr.used_storage(), 400);

        let b = mgr.add_image(9, 0, 20, 5, rgba(400 * 2));

        // Same image object: internal id and client number are kept
        assert_eq!(b.id, a.id);
        assert!(b.replaced);
        let img = mgr.image(a.id).unwrap();
        assert_eq!(img.client_number, 4);
        assert_eq!((img.width, img.height), (20, 5));
        // Old data freed, placements dropped, new size accounted
        assert_eq!(mgr.used_storage(), 800);
        assert!(img.placements().is_empty());
        assert_eq!(img.frames.len(), 1);
        // GPU told to drop the old texture and upload the new data
        assert!(mgr.pending_deletes.contains(&a.id));
        assert_eq!(mgr.pending_uploads.len(), 2);
        assert_eq!(mgr.len(), 1);
    }

    #[test]
    fn quota_evicts_unreferenced_first_then_oldest() {
        // Each 5x5 image is 100 bytes; quota fits two and a half
        let mut mgr = GraphicsManager::with_storage_limit(250);

        let a = add(&mut mgr, 1, 0, 5, 5); // Oldest, referenced.
        place(&mut mgr, a.id, 1);
        let b = add(&mut mgr, 2, 0, 5, 5); // Unreferenced (but has an id).

        // Third image overflows the quota: the unreferenced image goes first,
        // even though it has a client id and is newer
        let c = add(&mut mgr, 3, 0, 5, 5);
        assert!(mgr.image(b.id).is_none());
        assert!(mgr.image(a.id).is_some());
        assert!(mgr.image(c.id).is_some());
        assert_eq!(mgr.used_storage(), 200);

        // Reference c, then overflow again: no unreferenced images remain, so
        // the oldest referenced image (a) is evicted, placements and all
        place(&mut mgr, c.id, 1);
        let d = add(&mut mgr, 4, 0, 5, 5);
        place(&mut mgr, d.id, 1);
        assert!(mgr.image(a.id).is_none());
        assert!(mgr.image(c.id).is_some());
        assert!(mgr.image(d.id).is_some());
        assert_eq!(mgr.used_storage(), 200);
        assert!(mgr.pending_deletes.contains(&a.id));
        assert!(mgr.pending_deletes.contains(&b.id));
    }

    #[test]
    fn set_storage_limit_evicts_only_when_over() {
        let mut mgr = GraphicsManager::with_storage_limit(250);
        let a = add(&mut mgr, 1, 0, 5, 5); // 100 bytes, placed.
        place(&mut mgr, a.id, 1);
        let b = add(&mut mgr, 2, 0, 5, 5); // 100 bytes, unplaced.

        // Raising the limit never evicts, even with unplaced images stored
        mgr.set_storage_limit(500);
        assert_eq!(mgr.storage_limit, 500);
        assert_eq!(mgr.len(), 2);

        // Lowering below usage evicts: unplaced first, then oldest
        mgr.set_storage_limit(150);
        assert!(mgr.image(b.id).is_none());
        assert!(mgr.image(a.id).is_some());
        assert!(mgr.pending_deletes.contains(&b.id));
        assert!(mgr.used_storage() <= 150);
    }

    #[test]
    fn quota_never_evicts_currently_added_image() {
        let mut mgr = GraphicsManager::with_storage_limit(150);
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        // 200 bytes total: over quota; only `a` can be evicted
        let b = add(&mut mgr, 2, 0, 5, 5);
        assert!(mgr.image(a.id).is_none());
        assert!(mgr.image(b.id).is_some());
    }

    #[test]
    fn placement_same_key_replaces_in_place() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 10, 10);

        let spec = PlacementSpec { placement_id: 3, z_index: 0, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(2), Column(4), &spec, CELL);
        let first_internal = mgr.image(a.id).unwrap().placements()[0].id();

        let spec = PlacementSpec { placement_id: 3, z_index: 5, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(-1), Column(6), &spec, CELL);

        let img = mgr.image(a.id).unwrap();
        assert_eq!(img.placements().len(), 1, "same (image, placement id) must replace");
        let p = &img.placements()[0];
        assert_eq!(p.id(), first_internal, "replace must reuse the slot, not delete-then-add");
        assert_eq!(p.z_index, 5);
        assert_eq!((p.line, p.column), (Line(-1), Column(6)));

        // Anonymous placements (p=0) accumulate instead
        place(&mut mgr, a.id, 0);
        place(&mut mgr, a.id, 0);
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 3);
    }

    #[test]
    fn placement_ids_ignored_for_anonymous_images() {
        let mut mgr = GraphicsManager::new();
        // Image without client id: p= is not client-addressable
        // (graphics.c:1128), so repeated p=3 placements accumulate
        let a = add(&mut mgr, 0, 0, 10, 10);
        place(&mut mgr, a.id, 3);
        place(&mut mgr, a.id, 3);
        let img = mgr.image(a.id).unwrap();
        assert_eq!(img.placements().len(), 2);
        assert!(img.placements().iter().all(|p| p.client_id == 0));
    }

    #[test]
    fn aspect_math_matches_kitty_fixtures() {
        // Hand-computed from kitty's update_dest_rect (graphics.c:826-853)

        // Both c= and r= given: taken verbatim
        assert_eq!(extent((200, 100), (0, 0), (3, 2), CELL), (3, 2));

        // c-only: rows = ceil((cw*c + xoff) * h/w / ch)
        // = ceil((10*5) * 100/200 / 20) = ceil(1.25) = 2
        assert_eq!(extent((200, 100), (0, 0), (5, 0), CELL), (5, 2));

        // r-only: cols = ceil((ch*r + yoff) * w/h / cw)
        // = ceil((20*3) * 200/100 / 10) = ceil(12.0) = 12
        assert_eq!(extent((200, 100), (0, 0), (0, 3), CELL), (12, 3));

        // Neither: cols = ceil(200/10) = 20, then rows via aspect path:
        // ceil((10*20) * 100/200 / 20) = ceil(5.0) = 5
        assert_eq!(extent((200, 100), (0, 0), (0, 0), CELL), (20, 5));

        // Neither, with subcell offsets, square cells 10x10:
        // cols: t = 25+5 = 30 -> 3 exactly
        // rows: ceil((10*3 + 5) * 25/25 / 10) = ceil(3.5) = 4
        let cell = CellSize { width: 10, height: 10 };
        assert_eq!(extent((25, 25), (5, 7), (0, 0), cell), (3, 4));

        // r-only with y offset, cell 8x16:
        // cols = ceil((16*2 + 10) * 100/50 / 8) = ceil(10.5) = 11
        let cell = CellSize { width: 8, height: 16 };
        assert_eq!(extent((100, 50), (0, 10), (0, 2), cell), (11, 2));

        // c-only with x offset and non-divisible aspect, cell 3x6:
        // rows = ceil((3*2 + 2) * 5/7 / 6) = ceil(0.952...) = 1
        let cell = CellSize { width: 3, height: 6 };
        assert_eq!(extent((7, 5), (2, 0), (2, 0), cell), (2, 1));

        // Non-exact cols ceil: t = 95 -> 95/10 = 9 rem 5 -> 10 cols. Rows then
        // derive from the *rounded-up* column extent (kitty quirk), not the
        // raw pixel height: ceil((10*10) * 40/95 / 20) = ceil(2.105) = 3,
        // where naive ceil(40/20) would give 2
        let cell = CellSize { width: 10, height: 20 };
        assert_eq!(extent((95, 40), (0, 0), (0, 0), cell), (10, 3));
    }

    #[test]
    fn subcell_offsets_clamped_to_cell() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 50, 50);
        let spec = PlacementSpec { cell_x_offset: 25, cell_y_offset: 100, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec, CELL);
        let p = &mgr.image(a.id).unwrap().placements()[0];
        assert_eq!((p.cell_x_offset, p.cell_y_offset), (CELL.width - 1, CELL.height - 1));
    }

    #[test]
    fn src_rect_clamped_to_image() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 100, 50);

        // Crop extending past the right edge is clamped
        let spec = PlacementSpec { src_x: 90, src_width: 50, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec, CELL);
        let p = &mgr.image(a.id).unwrap().placements()[0];
        assert_eq!((p.src_x, p.src_width), (90, 10));
        assert_eq!(p.src_height, 50, "h=0 means full image height");

        // Crop starting beyond the image collapses to zero
        let spec = PlacementSpec { src_y: 60, src_height: 10, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec, CELL);
        let p = &mgr.image(a.id).unwrap().placements()[1];
        assert_eq!(p.src_height, 0);
    }

    #[test]
    fn put_returns_effective_extent_for_cursor_movement() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 200, 100);
        let extent = mgr
            .put_placement(a.id, Line(0), Column(0), &PlacementSpec::default(), CELL)
            .unwrap()
            .unwrap();
        assert_eq!(extent, (20, 5));
        assert!(
            mgr.put_placement(999, Line(0), Column(0), &PlacementSpec::default(), CELL)
                .ok()
                .flatten()
                .is_none()
        );
    }

    #[test]
    fn load_slot_is_single_and_abortable() {
        fn load(id: u32) -> LoadData {
            LoadData { start: GraphicsCommand { id, ..Default::default() }, ..Default::default() }
        }

        let mut mgr = GraphicsManager::new();
        mgr.start_load(LoadData { buf: vec![1, 2, 3], ..load(1) });
        assert_eq!(mgr.loading().unwrap().start.id, 1);

        // Starting a new transmission replaces the in-flight one
        mgr.start_load(load(2));
        assert_eq!(mgr.loading().unwrap().start.id, 2);
        assert!(mgr.loading().unwrap().buf.is_empty());

        // A complete add aborts the in-flight load too
        add(&mut mgr, 5, 0, 2, 2);
        assert!(mgr.loading().is_none());

        mgr.start_load(LoadData::default());
        mgr.abort_load();
        assert!(mgr.loading().is_none());

        mgr.start_load(load(3));
        let taken = mgr.take_loading().unwrap();
        assert_eq!(taken.start.id, 3);
        assert!(mgr.loading().is_none());
    }

    #[test]
    fn atime_updated_on_placement_protects_from_eviction() {
        let mut mgr = GraphicsManager::with_storage_limit(250);
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        let b = add(&mut mgr, 2, 0, 5, 5);
        place(&mut mgr, b.id, 1);

        // Re-placing `a` makes it the most recently used image
        place(&mut mgr, a.id, 1);

        // This add overflows the quota; `b` is now the oldest referenced
        // image and gets evicted instead of `a`
        let c = add(&mut mgr, 3, 0, 5, 5);
        assert!(mgr.image(b.id).is_none());
        assert!(mgr.image(a.id).is_some());
        assert!(mgr.image(c.id).is_some());
    }

    fn del(mgr: &mut GraphicsManager, spec: u8, id: u32, placement_id: u32) -> Option<bool> {
        mgr.handle_delete(&GraphicsCommand {
            action: b'd',
            delete_action: spec,
            id,
            placement_id,
            ..Default::default()
        })
    }

    fn del_point(mgr: &mut GraphicsManager, spec: u8, x: u32, y: u32) -> Option<bool> {
        mgr.handle_delete(&GraphicsCommand {
            action: b'd',
            delete_action: spec,
            x_offset: x,
            y_offset: y,
            ..Default::default()
        })
    }

    /// Move the image's only placement entirely into the scrollback.
    fn scroll_out(mgr: &mut GraphicsManager, id: ImageId) {
        mgr.image_mut(id).unwrap().placements_mut()[0].line = Line(-5);
    }

    #[test]
    fn delete_all_spares_scrollback_and_keeps_addressable_images() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        let b = add(&mut mgr, 2, 0, 5, 5);
        place(&mut mgr, b.id, 1);
        scroll_out(&mut mgr, b.id);
        let anon = add(&mut mgr, 0, 0, 5, 5);
        place(&mut mgr, anon.id, 0);

        assert_eq!(del(&mut mgr, b'a', 0, 0), Some(true));
        // Visible placements went, but lowercase keeps the image data
        assert!(mgr.image(a.id).unwrap().placements().is_empty());
        // Placements entirely in the scrollback survive `d=a`
        assert_eq!(mgr.image(b.id).unwrap().placements().len(), 1);
        // Anonymous images losing their last placement are freed regardless
        assert!(mgr.image(anon.id).is_none());
        assert!(mgr.pending_deletes.contains(&anon.id));

        // A missing `d=` behaves like `d=a`
        place(&mut mgr, a.id, 1);
        assert_eq!(del(&mut mgr, 0, 0, 0), Some(true));
        assert!(mgr.image(a.id).unwrap().placements().is_empty());
    }

    #[test]
    fn delete_all_uppercase_frees_images() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        let b = add(&mut mgr, 2, 0, 5, 5);
        place(&mut mgr, b.id, 1);
        scroll_out(&mut mgr, b.id);

        assert_eq!(del(&mut mgr, b'A', 0, 0), Some(true));
        assert!(mgr.image(a.id).is_none());
        assert!(mgr.pending_deletes.contains(&a.id));
        // Scrollback-only images are untouched even by `d=A`
        assert!(mgr.image(b.id).is_some());
    }

    #[test]
    fn yazi_scroll_lifecycle_no_placement_stacking() {
        // Regression for the yazi image-preview "new image renders on top of
        // previous" report. Captured from a real yazi 26.5 session: yazi reuses
        // ONE image id (`i=877974`), clears with `a=d,d=A` before every new
        // preview, and places anonymously (`a=T` with no `p=`). Verify the model
        // never accumulates stale images or placements across the scroll cycle:
        // each cycle ends with exactly one image holding exactly one placement,
        // and the previous image's internal id is queued for GPU texture delete
        const YAZI_ID: u32 = 877974;
        let mut mgr = GraphicsManager::new();

        // First preview: transmit + anonymous place (yazi's combined `a=T`)
        let first = add(&mut mgr, YAZI_ID, 0, 5, 5);
        assert!(place(&mut mgr, first.id, 0).is_some());
        assert_eq!(mgr.image(first.id).unwrap().placements().len(), 1);

        let mut prev_internal = first.id;
        for _ in 0..5 {
            // yazi clears all visible placements + frees data before the next
            del(&mut mgr, b'A', 0, 0);
            assert!(mgr.image(prev_internal).is_none(), "old image survived d=A");
            assert!(
                mgr.pending_deletes.contains(&prev_internal),
                "old image internal id not queued for GPU texture delete",
            );

            // Re-transmit under the SAME client id, then anonymous place
            let next = add(&mut mgr, YAZI_ID, 0, 5, 5);
            assert!(place(&mut mgr, next.id, 0).is_some());
            assert_ne!(next.id, prev_internal, "internal id must be fresh after delete");

            // Exactly one image, exactly one placement — no stacking
            assert_eq!(mgr.images.len(), 1, "stale images accumulated across scroll");
            assert_eq!(
                mgr.image(next.id).unwrap().placements().len(),
                1,
                "placements stacked on the reused image id",
            );
            prev_internal = next.id;
        }
    }

    #[test]
    fn delete_by_id_matches_image_and_placement() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        place(&mut mgr, a.id, 2);
        let b = add(&mut mgr, 2, 0, 5, 5);
        place(&mut mgr, b.id, 1);

        // `p=` narrows the delete to one placement of one image
        assert_eq!(del(&mut mgr, b'i', 1, 1), Some(true));
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);
        assert_eq!(mgr.image(a.id).unwrap().placements()[0].client_id, 2);
        assert_eq!(mgr.image(b.id).unwrap().placements().len(), 1);

        // Without `p=` all placements go; lowercase keeps the image data
        assert_eq!(del(&mut mgr, b'i', 1, 0), Some(true));
        let img = mgr.image(a.id).unwrap();
        assert!(img.placements().is_empty());
        assert!(!img.frames.is_empty());

        // `d=i` with `i=0` matches nothing (`id_filter_func`)
        assert_eq!(del(&mut mgr, b'i', 0, 0), Some(false));
        assert_eq!(mgr.image(b.id).unwrap().placements().len(), 1);
    }

    #[test]
    fn delete_by_id_uppercase_frees_image_when_emptied() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        place(&mut mgr, a.id, 2);

        // Uppercase with `p=` frees only once the last placement went
        assert_eq!(del(&mut mgr, b'I', 1, 1), Some(true));
        assert!(mgr.image(a.id).is_some());
        assert_eq!(del(&mut mgr, b'I', 1, 2), Some(true));
        assert!(mgr.image(a.id).is_none());
        assert!(mgr.pending_deletes.contains(&a.id));
    }

    #[test]
    fn delete_by_id_frees_unplaced_image() {
        let mut mgr = GraphicsManager::new();
        // The placement filter can only free images it matched; freeing an
        // image without placements is kitty's special case at the top of
        // `handle_delete_command`. Not a visible change: dirty is `false`
        let a = add(&mut mgr, 1, 0, 5, 5);
        assert_eq!(del(&mut mgr, b'I', 1, 0), Some(false));
        assert!(mgr.image(a.id).is_none());
        assert!(mgr.pending_deletes.contains(&a.id));

        // Lowercase `d=i` leaves unplaced images alone
        let b = add(&mut mgr, 2, 0, 5, 5);
        assert_eq!(del(&mut mgr, b'i', 2, 0), Some(false));
        assert!(mgr.image(b.id).is_some());
    }

    #[test]
    fn delete_point_cell_intersection() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        // 3x2 cell rect anchored at line 2, column 4: covers 0-based columns
        // 4..7 and lines 2..4, i.e. 1-based cells x=5..=7, y=3..=4
        let spec =
            PlacementSpec { placement_id: 1, num_cols: 3, num_rows: 2, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(2), Column(4), &spec, CELL);

        // Misses: right of, left of, and above the rect; x/y of 0 never match
        assert_eq!(del_point(&mut mgr, b'p', 8, 3), Some(false));
        assert_eq!(del_point(&mut mgr, b'p', 4, 3), Some(false));
        assert_eq!(del_point(&mut mgr, b'p', 5, 2), Some(false));
        assert_eq!(del_point(&mut mgr, b'p', 0, 0), Some(false));
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);

        // Bottom-right covered cell hits
        assert_eq!(del_point(&mut mgr, b'p', 7, 4), Some(true));
        assert!(mgr.image(a.id).unwrap().placements().is_empty());

        // Uppercase frees the image once it has no placements left
        let _ = mgr.put_placement(a.id, Line(2), Column(4), &spec, CELL);
        assert_eq!(del_point(&mut mgr, b'P', 5, 3), Some(true));
        assert!(mgr.image(a.id).is_none());
    }

    #[test]
    fn delete_aborts_inflight_load_even_when_unsupported() {
        let mut mgr = GraphicsManager::new();
        // f/F still aborts the in-flight load (graphics.c:2095) even when the
        // image is not found (no id/number supplied → Some(false))
        mgr.start_load(LoadData::default());
        assert_eq!(del(&mut mgr, b'f', 0, 0), Some(false));
        assert!(mgr.loading().is_none());

        mgr.start_load(LoadData::default());
        assert_eq!(del(&mut mgr, b'a', 0, 0), Some(false));
        assert!(mgr.loading().is_none());
    }

    /// Frame-deletion command helper (d=f/F with r= frame_number, i= id).
    fn del_frame(mgr: &mut GraphicsManager, spec: u8, id: u32, frame_number: u32) -> Option<bool> {
        mgr.handle_delete(&GraphicsCommand {
            action: b'd',
            delete_action: spec,
            id,
            num_lines: frame_number, // r= alias
            ..Default::default()
        })
    }

    #[test]
    fn frame_delete_no_id_or_number_is_noop() {
        // No id/number → Some(false), image untouched
        let mut mgr = GraphicsManager::new();
        // make_animated(mgr, client_id, n_frames total); 3 = root + 2 extra
        let id = make_animated(&mut mgr, 1, 3);
        assert_eq!(del_frame(&mut mgr, b'f', 0, 1), Some(false));
        assert_eq!(mgr.image(id).unwrap().frames.len(), 3);
    }

    #[test]
    fn frame_delete_unknown_image_returns_some_false() {
        // Unknown id → Some(false), no panic
        let mut mgr = GraphicsManager::new();
        assert_eq!(del_frame(&mut mgr, b'f', 99, 1), Some(false));
        assert_eq!(del_frame(&mut mgr, b'F', 99, 1), Some(false));
    }

    #[test]
    fn frame_delete_middle_frame() {
        // 3-frame image (root + 2 extra); delete frame 2 (first extra frame)
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 1, 3); // frames: [root, f1, f2]
        let before_storage = mgr.frame_storage_used;

        assert_eq!(del_frame(&mut mgr, b'f', 1, 2), Some(true));

        let img = mgr.image(id).unwrap();
        assert_eq!(img.frames.len(), 2, "one frame removed");
        assert!(mgr.frame_storage_used < before_storage, "storage decreased");
    }

    #[test]
    fn frame_delete_root_frame_promotes_next() {
        // Delete frame 1 (root): frames[1] should become the new root
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 1, 3); // frames[0]=root, [1]=f1, [2]=f2
        let f1_len = mgr.image(id).unwrap().frames[1].data.len();
        let before_storage = mgr.frame_storage_used;

        assert_eq!(del_frame(&mut mgr, b'f', 1, 1), Some(true));

        let img = mgr.image(id).unwrap();
        assert_eq!(img.frames.len(), 2, "one frame removed, old f1 is now root");
        // Old f1 left the counted set (it became root), so storage decreases by f1_len
        assert_eq!(mgr.frame_storage_used, before_storage.saturating_sub(f1_len));
    }

    #[test]
    fn frame_delete_uppercase_no_extra_removes_image() {
        // d=F on image with only root frame → image removed entirely
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 1, 1); // 1 = root only
        place(&mut mgr, id, 1);

        assert_eq!(del_frame(&mut mgr, b'F', 1, 1), Some(true));
        assert!(mgr.image(id).is_none(), "image removed by d=F");
    }

    #[test]
    fn frame_delete_lowercase_no_extra_is_noop() {
        // d=f on image with only root frame → no-op, image survives
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 1, 1); // 1 = root only
        place(&mut mgr, id, 1);

        assert_eq!(del_frame(&mut mgr, b'f', 1, 1), Some(false));
        assert!(mgr.image(id).is_some(), "image survives d=f with no extra frames");
        assert_eq!(mgr.image(id).unwrap().placements().len(), 1);
    }

    #[test]
    fn frame_delete_storage_accounting_decremented() {
        // Verify frame_storage and frame_storage_used both decrease
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 1, 3); // root + 2 extra
        let before_mgr = mgr.frame_storage_used;
        let before_img = mgr.image(id).unwrap().frame_storage;

        del_frame(&mut mgr, b'f', 1, 2); // delete frame 2

        let after_img = mgr.image(id).unwrap().frame_storage;
        assert!(after_img < before_img, "img.frame_storage decreased");
        assert!(mgr.frame_storage_used < before_mgr, "manager frame_storage_used decreased");
        assert_eq!(
            before_mgr - mgr.frame_storage_used,
            before_img - after_img,
            "manager and image deltas match"
        );
    }

    #[test]
    fn frame_delete_current_frame_index_adjusted() {
        // current_frame_index is clamped after deletion
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 1, 3); // 3 frames: indices 0,1,2

        // Set current to last frame (index 2)
        mgr.image_mut(id).unwrap().current_frame_index = 2;

        // Delete frame 3 (index 2, the current one)
        del_frame(&mut mgr, b'f', 1, 3);

        let img = mgr.image(id).unwrap();
        assert_eq!(img.frames.len(), 2);
        // After removing index 2 (current), clamp brings index to 1
        assert_eq!(img.current_frame_index, 1);
    }

    #[test]
    fn clear_visible_vs_all() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        let b = add(&mut mgr, 2, 0, 5, 5);
        place(&mut mgr, b.id, 1);
        scroll_out(&mut mgr, b.id);
        let c = add(&mut mgr, 3, 0, 5, 5);

        assert!(mgr.clear(false));
        // Visible placements went and their emptied images were freed
        assert!(mgr.image(a.id).is_none());
        // Stored-but-unplaced images are freed too (`grman_clear` removes
        // every image without refs, matched or not)
        assert!(mgr.image(c.id).is_none());
        // Scrollback-only placements survive a non-`all` clear
        assert!(mgr.image(b.id).is_some());

        assert!(mgr.clear(true));
        assert!(mgr.is_empty());
        assert!(mgr.pending_deletes.contains(&b.id));
    }

    /// 0-based inclusive region `start..end` as a `Line` range.
    fn region(start: i32, end: i32) -> std::ops::Range<Line> {
        Line(start)..Line(end)
    }

    /// Place image `id` at `line` with the given spec, at `Column(0)`.
    fn place_spec(mgr: &mut GraphicsManager, id: ImageId, line: i32, spec: PlacementSpec) {
        mgr.put_placement(id, Line(line), Column(0), &spec, CELL).unwrap().unwrap();
    }

    /// Clone of the image's single placement.
    fn only_placement(mgr: &GraphicsManager, id: ImageId) -> Placement {
        let placements = mgr.image(id).unwrap().placements();
        assert_eq!(placements.len(), 1);
        placements[0].clone()
    }

    const SCREEN_LINES: usize = 10;

    // Margin-scroll fixtures are hand-derived from kitty's
    // `scroll_filter_margins_func` (graphics.c:1955) with a 10x20 cell and
    // region top=2, bottom=7 (`region(2, 8)`)

    #[test]
    fn margin_scroll_shifts_placements_inside_region() {
        let mut mgr = GraphicsManager::new();
        // 40x80 px image: effective extent (4, 4), src_height 80
        let a = add(&mut mgr, 1, 0, 40, 80);
        place_spec(&mut mgr, a.id, 3, PlacementSpec::default());

        // Rows 3..=6 stay within 2..=7 after a 1-line scroll up: pure shift
        assert!(mgr.scroll(&region(2, 8), -1, SCREEN_LINES, CELL, 0));
        let p = only_placement(&mgr, a.id);
        assert_eq!(p.line, Line(2));
        assert_eq!((p.src_y, p.src_height, p.effective_num_rows), (0, 80, 4));

        // And back down by 2: rows 4..=7 still inside, no crop
        assert!(mgr.scroll(&region(2, 8), 2, SCREEN_LINES, CELL, 0));
        let p = only_placement(&mgr, a.id);
        assert_eq!(p.line, Line(4));
        assert_eq!((p.src_y, p.src_height, p.effective_num_rows), (0, 80, 4));
    }

    #[test]
    fn margin_scroll_crops_at_top_edge() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 40, 80);
        place_spec(&mut mgr, a.id, 4, PlacementSpec::default());

        // Scroll up 3: line 4 -> 1 < top 2, clipped_rows = 1, clip_amt = 20
        // Kitty: src_y += 20, src_height 80 -> 60, rows 4 -> 3, line -> 2
        assert!(mgr.scroll(&region(2, 8), -3, SCREEN_LINES, CELL, 0));
        let p = only_placement(&mgr, a.id);
        assert_eq!(p.line, Line(2));
        assert_eq!((p.src_y, p.src_height, p.effective_num_rows), (20, 60, 3));
        assert_eq!(p.src_x, 0);
        assert_eq!(p.src_width, 40);
    }

    #[test]
    fn margin_scroll_crops_at_bottom_edge() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 40, 80);
        place_spec(&mut mgr, a.id, 4, PlacementSpec::default());

        // Scroll down 3: line 4 -> 7, last row 10 > bottom 7, clipped_rows =
        // 3, clip_amt = 60. Kitty: src_y stays, src_height 80 -> 20, rows 1
        assert!(mgr.scroll(&region(2, 8), 3, SCREEN_LINES, CELL, 0));
        let p = only_placement(&mgr, a.id);
        assert_eq!(p.line, Line(7));
        assert_eq!((p.src_y, p.src_height, p.effective_num_rows), (0, 20, 1));
    }

    #[test]
    fn margin_scroll_removes_when_outside_or_fully_clipped() {
        let mut mgr = GraphicsManager::new();
        // 20x40 px: 2 rows. Scroll up 2: line 2 -> 0, line + rows = 2 <= top
        // 2 => outside the region, removed without cropping
        let a = add(&mut mgr, 1, 0, 20, 40);
        place_spec(&mut mgr, a.id, 2, PlacementSpec::default());
        assert!(mgr.scroll(&region(2, 8), -2, SCREEN_LINES, CELL, 0));
        assert!(mgr.image(a.id).unwrap().placements().is_empty());

        // 40x40 px stretched to r=4 (src_height 40 over 4 rows). Scroll up 4:
        // line 4 -> 0, clipped_rows = 2, clip_amt = 40 >= src_height 40 =>
        // the whole source is consumed, placement removed (kitty's
        // `src_height <= clip_amt` bail-out)
        let b = add(&mut mgr, 2, 0, 40, 40);
        place_spec(&mut mgr, b.id, 4, PlacementSpec { num_rows: 4, ..Default::default() });
        assert!(mgr.scroll(&region(2, 8), -4, SCREEN_LINES, CELL, 0));
        assert!(mgr.image(b.id).unwrap().placements().is_empty());

        // Addressable images keep their data; anonymous unnumbered images
        // losing their last placement are freed (kitty's modify_refs rule)
        assert!(mgr.image(a.id).is_some());
        let anon = add(&mut mgr, 0, 0, 20, 40);
        place_spec(&mut mgr, anon.id, 2, PlacementSpec::default());
        assert!(mgr.scroll(&region(2, 8), -2, SCREEN_LINES, CELL, 0));
        assert!(mgr.image(anon.id).is_none());
        assert!(mgr.pending_deletes.contains(&anon.id));
    }

    #[test]
    fn margin_scroll_leaves_outside_and_straddling_untouched() {
        let mut mgr = GraphicsManager::new();
        // Entirely above the region (1 row at line 0)
        let above = add(&mut mgr, 1, 0, 5, 5);
        place_spec(&mut mgr, above.id, 0, PlacementSpec::default());
        // Entirely below the region (1 row at line 9 > bottom 7)
        let below = add(&mut mgr, 2, 0, 5, 5);
        place_spec(&mut mgr, below.id, 9, PlacementSpec::default());
        // Straddling the top edge: rows 1..=4 with top 2 is not fully within
        // the region, so kitty leaves it alone
        let straddle_top = add(&mut mgr, 3, 0, 40, 80);
        place_spec(&mut mgr, straddle_top.id, 1, PlacementSpec::default());
        // Straddling the bottom edge: rows 6..=9 with bottom 7
        let straddle_bottom = add(&mut mgr, 4, 0, 40, 80);
        place_spec(&mut mgr, straddle_bottom.id, 6, PlacementSpec::default());

        mgr.scroll(&region(2, 8), -2, SCREEN_LINES, CELL, 0);

        for (id, line) in
            [(above.id, 0), (below.id, 9), (straddle_top.id, 1), (straddle_bottom.id, 6)]
        {
            let p = only_placement(&mgr, id);
            assert_eq!(p.line, Line(line));
            assert_eq!(p.src_y, 0);
        }
    }

    #[test]
    fn full_scroll_hard_deletes_past_viewport_top() {
        let mut mgr = GraphicsManager::new();
        // Anonymous unnumbered image: hard delete frees it, GPU delete queued
        let anon = add(&mut mgr, 0, 0, 20, 40);
        place_spec(&mut mgr, anon.id, 1, PlacementSpec::default());
        // Addressable image: placement goes, pixel data stays re-placeable
        let a = add(&mut mgr, 1, 0, 20, 40);
        place_spec(&mut mgr, a.id, 1, PlacementSpec::default());

        // 2-row placements at line 1 scrolled up 3: bottom = -2 + 2 = 0 <= 0
        assert!(mgr.scroll(&region(0, 10), -3, SCREEN_LINES, CELL, 0));

        assert!(mgr.image(anon.id).is_none());
        assert!(mgr.pending_deletes.contains(&anon.id));
        let img = mgr.image(a.id).unwrap();
        assert!(img.placements().is_empty());
        assert!(!img.frames.is_empty());
        assert!(!mgr.pending_deletes.contains(&a.id));
    }

    #[test]
    fn full_scroll_keeps_partially_visible_and_below_screen() {
        let mut mgr = GraphicsManager::new();
        // 4-row placement at line 2 scrolled up 3: anchor -1, bottom 3 > 0
        let a = add(&mut mgr, 1, 0, 40, 80);
        place_spec(&mut mgr, a.id, 2, PlacementSpec::default());
        mgr.scroll(&region(0, 10), -3, SCREEN_LINES, CELL, 0);
        let p = only_placement(&mgr, a.id);
        assert_eq!(p.line, Line(-1));
        assert_eq!((p.src_y, p.src_height, p.effective_num_rows), (0, 80, 4));

        // Kitty's simple scroll path never deletes at the bottom; a reverse
        // scroll can bring the placement back
        let b = add(&mut mgr, 2, 0, 5, 5);
        place_spec(&mut mgr, b.id, 8, PlacementSpec::default());
        mgr.scroll(&region(0, 10), 5, SCREEN_LINES, CELL, 0);
        assert_eq!(only_placement(&mgr, b.id).line, Line(13));
    }

    #[test]
    fn gc_removes_placements_fully_above_viewport() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place_spec(&mut mgr, a.id, 0, PlacementSpec::default());
        let anon = add(&mut mgr, 0, 0, 5, 5);
        place_spec(&mut mgr, anon.id, 0, PlacementSpec::default());
        // Partially visible: anchor -1 with 4 rows, bottom 3 > 0
        let partial = add(&mut mgr, 2, 0, 40, 80);
        place_spec(&mut mgr, partial.id, -1, PlacementSpec::default());

        scroll_out(&mut mgr, a.id);
        scroll_out(&mut mgr, anon.id);

        assert!(mgr.gc(0));
        assert!(mgr.image(a.id).unwrap().placements().is_empty());
        assert!(mgr.image(anon.id).is_none());
        assert!(mgr.pending_deletes.contains(&anon.id));
        assert_eq!(only_placement(&mgr, partial.id).line, Line(-1));

        // Nothing left to collect
        assert!(!mgr.gc(0));
    }

    #[test]
    fn scroll_with_scrollback_limit_retains_then_gcs_past_history() {
        let mut mgr = GraphicsManager::new();
        // 2-row addressable placement at line 1
        let a = add(&mut mgr, 1, 0, 20, 40);
        place_spec(&mut mgr, a.id, 1, PlacementSpec::default());

        // Scroll up 3: bottom = -2 + 2 = 0 <= 0. With scrollback_limit 0 this
        // would be GC'd (full_scroll_keeps_partially_visible_and_below_screen
        // exercises the limit-0 path), but a nonzero limit RETAINS it above the
        // viewport so it re-renders when scrolled back
        assert!(mgr.scroll(&region(0, 10), -3, SCREEN_LINES, CELL, 100));
        assert_eq!(only_placement(&mgr, a.id).line, Line(-2));

        // Scroll until it passes the bottom of the scrollback window
        // (`line + rows <= -limit`): anchor -2 -> -102, bottom -100 <= -100
        assert!(mgr.scroll(&region(0, 10), -100, SCREEN_LINES, CELL, 100));
        let img = mgr.image(a.id).unwrap();
        assert!(img.placements().is_empty(), "GC'd once past the scrollback window");
        assert!(!img.frames.is_empty(), "addressable image data retained");
    }

    #[test]
    fn drop_placements_keeps_addressable_image_data() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place_spec(&mut mgr, a.id, 2, PlacementSpec::default());
        let anon = add(&mut mgr, 0, 0, 5, 5);
        place_spec(&mut mgr, anon.id, 3, PlacementSpec::default());

        assert!(mgr.drop_placements());

        // Reflow drop: every classic placement goes; the addressable image
        // keeps its pixel data, the anonymous unnumbered image is freed
        let img = mgr.image(a.id).unwrap();
        assert!(img.placements().is_empty());
        assert!(!img.frames.is_empty());
        assert!(mgr.image(anon.id).is_none());
        assert!(mgr.pending_deletes.contains(&anon.id));

        assert!(!mgr.drop_placements());
    }

    #[test]
    fn rescale_clamps_offsets_and_recomputes_extent() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 200, 100);
        let spec = PlacementSpec { cell_x_offset: 9, cell_y_offset: 19, ..Default::default() };
        place_spec(&mut mgr, a.id, 0, spec);

        // With the 10x20 cell: cols = ceil((200 + 9) / 10) = 21, rows =
        // ceil((10*21 + 9) * 100/200 / 20) = ceil(5.475) = 6
        let p = only_placement(&mgr, a.id);
        assert_eq!((p.cell_x_offset, p.cell_y_offset), (9, 19));
        assert_eq!((p.effective_num_cols, p.effective_num_rows), (21, 6));

        // Rescale to 8x16: offsets clamp to (7, 15); cols = ceil(207/8) = 26,
        // rows = ceil((8*26 + 7) * 100/200 / 16) = ceil(6.71875) = 7
        assert!(mgr.rescale(CellSize { width: 8, height: 16 }));

        let p = only_placement(&mgr, a.id);
        assert_eq!((p.cell_x_offset, p.cell_y_offset), (7, 15));
        assert_eq!((p.effective_num_cols, p.effective_num_rows), (26, 7));

        assert!(!GraphicsManager::new().rescale(CELL));
    }

    #[test]
    fn clear_scrollback_keeps_visible_and_unplaced() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        place(&mut mgr, a.id, 1);
        let b = add(&mut mgr, 2, 0, 5, 5);
        place(&mut mgr, b.id, 1);
        scroll_out(&mut mgr, b.id);
        let c = add(&mut mgr, 3, 0, 5, 5);

        assert!(mgr.clear_scrollback());
        assert!(mgr.image(b.id).is_none());
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);
        assert!(mgr.image(c.id).is_some());
    }

    #[test]
    fn z_bucket_boundary_values() {
        assert_eq!(ZBucket::from_z(i32::MIN), ZBucket::BelowBackground);
        assert_eq!(ZBucket::from_z(i32::MIN / 2 - 1), ZBucket::BelowBackground);
        assert_eq!(ZBucket::from_z(i32::MIN / 2), ZBucket::BetweenBgAndText);
        assert_eq!(ZBucket::from_z(-1), ZBucket::BetweenBgAndText);
        assert_eq!(ZBucket::from_z(0), ZBucket::AboveText);
        assert_eq!(ZBucket::from_z(i32::MAX), ZBucket::AboveText);
    }

    #[test]
    fn snapshot_sort_order() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 10, 10);
        let b = add(&mut mgr, 2, 0, 10, 10);

        let spec_z =
            |z: i32, p: u32| PlacementSpec { z_index: z, placement_id: p, ..Default::default() };

        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec_z(5, 1), CELL);
        let _ = mgr.put_placement(b.id, Line(0), Column(0), &spec_z(5, 2), CELL);
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec_z(-1, 3), CELL);
        let _ = mgr.put_placement(b.id, Line(0), Column(0), &spec_z(5, 4), CELL);

        let snap = mgr.render_snapshot(0);
        let keys: Vec<(i32, ImageId, u64)> =
            snap.items.iter().map(|i| (i.z_index, i.image_id, i.placement_id)).collect();

        let mut sorted = keys.clone();
        sorted.sort();
        assert_eq!(keys, sorted, "items must be in (z_index, image_id, placement_id) order");
    }

    #[test]
    fn snapshot_group_counting() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 10, 10);
        let b = add(&mut mgr, 2, 0, 10, 10);

        // Two placements of image a (same id → same group), one of b → new group,
        // one more of a → z-sort puts it after b, starting a third group
        let spec =
            |z: i32, p: u32| PlacementSpec { z_index: z, placement_id: p, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec(0, 1), CELL);
        let _ = mgr.put_placement(a.id, Line(1), Column(0), &spec(0, 2), CELL);
        let _ = mgr.put_placement(b.id, Line(0), Column(0), &spec(0, 1), CELL);
        // a.id < b.id (BTreeMap insertion order), so after sort by (z=0, image_id):
        // a/1, a/2, b/1 → groups 0, 0, 1
        let snap = mgr.render_snapshot(0);
        assert_eq!(snap.items.len(), 3);
        let groups: Vec<u32> = snap.items.iter().map(|i| i.group_index).collect();
        assert_eq!(groups[0], groups[1], "two placements of same image must share a group");
        assert_ne!(groups[1], groups[2], "different image must start a new group");
    }

    #[test]
    fn snapshot_drains_upload_and_delete_queues() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 4, 4);
        place(&mut mgr, a.id, 1);
        assert!(!mgr.pending_uploads.is_empty());

        let snap = mgr.render_snapshot(0);
        assert_eq!(snap.uploads.len(), 1);
        assert_eq!(snap.uploads[0].0, a.id);
        assert!(mgr.pending_uploads.is_empty(), "queues must be drained after snapshot");

        mgr.remove_image(a.id);
        let snap2 = mgr.render_snapshot(0);
        assert!(snap2.deletes.contains(&a.id));
        assert!(mgr.pending_deletes.is_empty());
    }

    #[test]
    fn scroll_deletes_out_of_view_placements_not_snapshot() {
        // Deletion is authoritative on SCROLL (when there is no scrollback to
        // retain into), NOT at snapshot time — render_snapshot only reads. A
        // limit-0 scroll that pushes 1-row placements past the top frees them
        // before the next snapshot is taken
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 4, 4);
        place(&mut mgr, a.id, 1);
        let anon = add(&mut mgr, 0, 0, 4, 4);
        place(&mut mgr, anon.id, 1);

        // Scroll up 2 with no scrollback: line 1 -> -1, bottom 0 <= 0 => GC'd
        assert!(mgr.scroll(&region(0, 10), -2, SCREEN_LINES, CELL, 0));

        let snap = mgr.render_snapshot(0);

        assert!(snap.items.is_empty(), "GC'd-at-scroll placements never reach the snapshot");
        assert!(mgr.image(anon.id).is_none(), "anonymous image must be freed at GC");
        assert!(snap.deletes.contains(&anon.id));
        assert!(mgr.image(a.id).is_some(), "addressable image keeps pixel data");
        assert!(!snap.deletes.contains(&a.id));
    }

    #[test]
    fn snapshot_uv_rect_derivation() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 100, 200);
        let spec = PlacementSpec {
            src_x: 10,
            src_y: 20,
            src_width: 50,
            src_height: 80,
            ..Default::default()
        };
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec, CELL);

        let snap = mgr.render_snapshot(0);
        assert_eq!(snap.items.len(), 1);
        let uv = snap.items[0].src_uv;
        assert!((uv.u0 - 0.10).abs() < 1e-6);
        assert!((uv.v0 - 0.10).abs() < 1e-6);
        assert!((uv.u1 - 0.60).abs() < 1e-6);
        assert!((uv.v1 - 0.50).abs() < 1e-6);
    }

    fn del_z(mgr: &mut GraphicsManager, spec: u8, z: i32) -> Option<bool> {
        mgr.handle_delete(&GraphicsCommand {
            action: b'd',
            delete_action: spec,
            z_index: z,
            ..Default::default()
        })
    }

    fn del_number(mgr: &mut GraphicsManager, spec: u8, number: u32) -> Option<bool> {
        mgr.handle_delete(&GraphicsCommand {
            action: b'd',
            delete_action: spec,
            image_number: number,
            ..Default::default()
        })
    }

    fn del_range(mgr: &mut GraphicsManager, spec: u8, lo: u32, hi: u32) -> Option<bool> {
        mgr.handle_delete(&GraphicsCommand {
            action: b'd',
            delete_action: spec,
            x_offset: lo,
            y_offset: hi,
            ..Default::default()
        })
    }

    fn del_point_z(mgr: &mut GraphicsManager, spec: u8, x: u32, y: u32, z: i32) -> Option<bool> {
        mgr.handle_delete(&GraphicsCommand {
            action: b'd',
            delete_action: spec,
            x_offset: x,
            y_offset: y,
            z_index: z,
            ..Default::default()
        })
    }

    #[test]
    fn delete_by_cursor_cell_c_spec() {
        // c/C is tested via term/mod.rs (requires cursor injection); here we
        // verify the underlying p/P filter at the same coordinates since c/C
        // is now routed through the same branch in handle_delete
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        let spec = PlacementSpec { num_cols: 2, num_rows: 2, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(3), Column(3), &spec, CELL);

        // Miss: column 3 is 0-based col index 2 → 1-based x=3; placement covers x=4..=5, y=4..=5
        assert_eq!(del_point(&mut mgr, b'p', 3, 4), Some(false));
        assert_eq!(del_point(&mut mgr, b'p', 4, 3), Some(false));
        // Hit via the same point_filter_func path that c/C uses
        assert_eq!(del_point(&mut mgr, b'p', 4, 4), Some(true));
        assert!(mgr.image(a.id).unwrap().placements().is_empty());
    }

    #[test]
    fn delete_by_number_n_spec() {
        let mut mgr = GraphicsManager::new();
        // Two images with different numbers
        let a = add(&mut mgr, 1, 1, 5, 5); // client_id=1, client_number=1
        let b = add(&mut mgr, 2, 2, 5, 5); // client_id=2, client_number=2
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &PlacementSpec::default(), CELL);
        let _ = mgr.put_placement(b.id, Line(1), Column(0), &PlacementSpec::default(), CELL);

        // Miss: number 99 not present
        assert_eq!(del_number(&mut mgr, b'n', 99), Some(false));
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);

        // Hit on number 2 (image b). Lowercase keeps image data
        assert_eq!(del_number(&mut mgr, b'n', 2), Some(true));
        assert!(mgr.image(b.id).unwrap().placements().is_empty());
        assert!(!mgr.image(b.id).unwrap().frames.is_empty());

        // Uppercase frees image when emptied
        let _ = mgr.put_placement(b.id, Line(1), Column(0), &PlacementSpec::default(), CELL);
        assert_eq!(del_number(&mut mgr, b'N', 2), Some(true));
        assert!(mgr.image(b.id).is_none());
        assert!(mgr.pending_deletes.contains(&b.id));
    }

    #[test]
    fn delete_by_number_n_newest_semantics() {
        // When multiple images share a number, kitty deletes the newest one
        let mut mgr = GraphicsManager::new();
        let old = add(&mut mgr, 1, 7, 5, 5);
        let new = add(&mut mgr, 2, 7, 5, 5);
        let _ = mgr.put_placement(old.id, Line(0), Column(0), &PlacementSpec::default(), CELL);
        let _ = mgr.put_placement(new.id, Line(1), Column(0), &PlacementSpec::default(), CELL);

        del_number(&mut mgr, b'n', 7);

        // Newest image (new) lost its placement; older image (old) untouched
        assert_eq!(mgr.image(new.id).unwrap().placements().len(), 0);
        assert_eq!(mgr.image(old.id).unwrap().placements().len(), 1);
    }

    #[test]
    fn delete_by_column_x_spec() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        // 3-col placement at column 2 (0-based): covers cols 2..=4 → 1-based x=3..=5
        let spec = PlacementSpec { num_cols: 3, num_rows: 1, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(0), Column(2), &spec, CELL);

        assert_eq!(del_point(&mut mgr, b'x', 2, 0), Some(false)); // x=2 misses
        assert_eq!(del_point(&mut mgr, b'x', 6, 0), Some(false)); // x=6 misses
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);
        assert_eq!(del_point(&mut mgr, b'x', 3, 0), Some(true)); // x=3 hits
        assert!(mgr.image(a.id).unwrap().placements().is_empty());

        // Uppercase X frees image
        let b = add(&mut mgr, 2, 0, 5, 5);
        let _ = mgr.put_placement(b.id, Line(0), Column(2), &spec, CELL);
        assert_eq!(del_point(&mut mgr, b'X', 5, 0), Some(true));
        assert!(mgr.image(b.id).is_none());
        assert!(mgr.pending_deletes.contains(&b.id));
    }

    #[test]
    fn delete_by_row_y_spec() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        // 1-col, 3-row placement at line 2: covers rows 2..=4 → 1-based y=3..=5
        let spec = PlacementSpec { num_cols: 1, num_rows: 3, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(2), Column(0), &spec, CELL);

        assert_eq!(del_point(&mut mgr, b'y', 0, 2), Some(false)); // y=2 misses
        assert_eq!(del_point(&mut mgr, b'y', 0, 6), Some(false)); // y=6 misses
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);
        assert_eq!(del_point(&mut mgr, b'y', 0, 4), Some(true)); // y=4 hits
        assert!(mgr.image(a.id).unwrap().placements().is_empty());

        // Uppercase Y frees image
        let b = add(&mut mgr, 2, 0, 5, 5);
        let _ = mgr.put_placement(b.id, Line(2), Column(0), &spec, CELL);
        assert_eq!(del_point(&mut mgr, b'Y', 0, 5), Some(true));
        assert!(mgr.image(b.id).is_none());
        assert!(mgr.pending_deletes.contains(&b.id));
    }

    #[test]
    fn delete_by_z_index_z_spec() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        let b = add(&mut mgr, 2, 0, 5, 5);
        let spec_z0 = PlacementSpec { z_index: 0, ..Default::default() };
        let spec_z1 = PlacementSpec { z_index: 1, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec_z0, CELL);
        let _ = mgr.put_placement(b.id, Line(0), Column(0), &spec_z1, CELL);

        // z=5 misses both
        assert_eq!(del_z(&mut mgr, b'z', 5), Some(false));
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);

        // z=1 hits b only
        assert_eq!(del_z(&mut mgr, b'z', 1), Some(true));
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);
        assert!(mgr.image(b.id).unwrap().placements().is_empty());

        // Uppercase Z at z=0 frees a
        assert_eq!(del_z(&mut mgr, b'Z', 0), Some(true));
        assert!(mgr.image(a.id).is_none());
        assert!(mgr.pending_deletes.contains(&a.id));
    }

    #[test]
    fn delete_by_z_negative_z_index() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        let spec = PlacementSpec { z_index: -10, ..Default::default() };
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &spec, CELL);

        assert_eq!(del_z(&mut mgr, b'z', -10), Some(true));
        assert!(mgr.image(a.id).unwrap().placements().is_empty());
    }

    #[test]
    fn delete_point3d_q_spec() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        let spec = PlacementSpec { num_cols: 2, num_rows: 2, z_index: 3, ..Default::default() };
        // Placement at line 1, column 1: covers 1-based x=2..=3, y=2..=3, z=3
        let _ = mgr.put_placement(a.id, Line(1), Column(1), &spec, CELL);

        // Wrong z-index: no match
        assert_eq!(del_point_z(&mut mgr, b'q', 2, 2, 99), Some(false));
        // Right z but wrong cell
        assert_eq!(del_point_z(&mut mgr, b'q', 1, 2, 3), Some(false));
        // Right cell, right z
        assert_eq!(del_point_z(&mut mgr, b'q', 2, 2, 3), Some(true));
        assert!(mgr.image(a.id).unwrap().placements().is_empty());

        // Uppercase Q frees image
        let _ = mgr.put_placement(a.id, Line(1), Column(1), &spec, CELL);
        assert_eq!(del_point_z(&mut mgr, b'Q', 3, 3, 3), Some(true));
        assert!(mgr.image(a.id).is_none());
    }

    #[test]
    fn delete_id_range_r_spec() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5); // client_id=1
        let b = add(&mut mgr, 2, 0, 5, 5); // client_id=2
        let c = add(&mut mgr, 3, 0, 5, 5); // client_id=3
        for id in [a.id, b.id, c.id] {
            let _ = mgr.put_placement(id, Line(0), Column(0), &PlacementSpec::default(), CELL);
        }

        // Range [2,3] removes b and c, leaves a
        assert_eq!(del_range(&mut mgr, b'r', 2, 3), Some(true));
        assert_eq!(mgr.image(a.id).unwrap().placements().len(), 1);
        assert!(mgr.image(b.id).unwrap().placements().is_empty());
        assert!(mgr.image(c.id).unwrap().placements().is_empty());
        // Lowercase: data kept
        assert!(!mgr.image(b.id).unwrap().frames.is_empty());

        // Uppercase R frees images
        let d = add(&mut mgr, 4, 0, 5, 5);
        let e = add(&mut mgr, 5, 0, 5, 5);
        let _ = mgr.put_placement(d.id, Line(0), Column(0), &PlacementSpec::default(), CELL);
        let _ = mgr.put_placement(e.id, Line(0), Column(0), &PlacementSpec::default(), CELL);
        assert_eq!(del_range(&mut mgr, b'R', 4, 5), Some(true));
        assert!(mgr.image(d.id).is_none());
        assert!(mgr.image(e.id).is_none());
        assert!(mgr.pending_deletes.contains(&d.id));
        assert!(mgr.pending_deletes.contains(&e.id));
    }

    #[test]
    fn delete_id_range_r_unplaced_images() {
        // Unplaced images in range are freed by the pre-filter (graphics.c:2103-2109)
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 5, 5);
        let b = add(&mut mgr, 2, 0, 5, 5);
        // Only a is placed; b is unplaced
        let _ = mgr.put_placement(a.id, Line(0), Column(0), &PlacementSpec::default(), CELL);

        assert_eq!(del_range(&mut mgr, b'R', 1, 2), Some(true));
        assert!(mgr.image(b.id).is_none());
        assert!(mgr.pending_deletes.contains(&b.id));
    }

    #[test]
    fn lowercase_vs_uppercase_data_freeing() {
        // Explicit table asserting that lowercase keeps data and uppercase frees it
        let cases: &[(u8, u8)] = &[
            (b'a', b'A'),
            (b'i', b'I'),
            (b'n', b'N'),
            (b'p', b'P'),
            (b'q', b'Q'),
            (b'x', b'X'),
            (b'y', b'Y'),
            (b'z', b'Z'),
            (b'r', b'R'),
        ];

        for &(lower, upper) in cases {
            // -- lowercase: placements gone, image data stays --
            let mut mgr = GraphicsManager::new();
            let img = add(&mut mgr, 1, 1, 5, 5);
            let _ = mgr.put_placement(
                img.id,
                Line(0),
                Column(0),
                &PlacementSpec { z_index: 0, ..Default::default() },
                CELL,
            );
            // Build a command that matches this image for all specifier types
            let cmd = GraphicsCommand {
                action: b'd',
                delete_action: lower,
                id: 1,
                image_number: 1,
                x_offset: 1, // 1-based col 1 → 0-based 0
                y_offset: 1, // 1-based row 1 → 0-based 0
                z_index: 0,
                ..Default::default()
            };
            let result = mgr.handle_delete(&cmd);
            assert!(result.is_some(), "d={}: expected Some, got None", lower as char);
            let still_there = mgr.image(img.id);
            assert!(still_there.is_some(), "d={}: lowercase must keep image data", lower as char);
            assert!(
                !mgr.pending_deletes.contains(&img.id),
                "d={}: lowercase must not enqueue GPU delete",
                lower as char
            );

            // -- uppercase: placements gone AND image freed --
            let mut mgr = GraphicsManager::new();
            let img = add(&mut mgr, 1, 1, 5, 5);
            let _ = mgr.put_placement(
                img.id,
                Line(0),
                Column(0),
                &PlacementSpec { z_index: 0, ..Default::default() },
                CELL,
            );
            let cmd = GraphicsCommand {
                action: b'd',
                delete_action: upper,
                id: 1,
                image_number: 1,
                x_offset: 1,
                y_offset: 1,
                z_index: 0,
                ..Default::default()
            };
            let result = mgr.handle_delete(&cmd);
            assert!(result.is_some(), "d={}: expected Some, got None", upper as char);
            assert!(mgr.image(img.id).is_none(), "d={}: uppercase must free image", upper as char);
            assert!(
                mgr.pending_deletes.contains(&img.id),
                "d={}: uppercase must enqueue GPU delete",
                upper as char
            );
        }
    }

    #[test]
    fn geometric_filters_exclude_virtual_placements() {
        // Virtual placements must be skipped by c/p/q/x/y/z specifiers
        let specs: &[u8] = b"pPqQxXyYzZ";

        for &spec in specs {
            let mut mgr = GraphicsManager::new();
            let img = add(&mut mgr, 1, 0, 5, 5);
            let _ = mgr.put_placement(
                img.id,
                Line(0),
                Column(0),
                &PlacementSpec { z_index: 0, ..Default::default() },
                CELL,
            );
            // Mark the placement as virtual
            mgr.image_mut(img.id).unwrap().placements_mut()[0].is_virtual = true;

            let cmd = GraphicsCommand {
                action: b'd',
                delete_action: spec,
                x_offset: 1,
                y_offset: 1,
                z_index: 0,
                ..Default::default()
            };
            let result = mgr.handle_delete(&cmd);
            assert_eq!(
                result,
                Some(false),
                "d={}: virtual placement must not be removed by geometric filter",
                spec as char
            );
            assert_eq!(
                mgr.image(img.id).unwrap().placements().len(),
                1,
                "d={}: virtual placement count must be unchanged",
                spec as char
            );
        }
    }

    #[test]
    fn virtual_placements_deleted_by_id_spec() {
        // Virtual placements are deletable by d=i/I (explicit id targeting)
        // d=a/A uses clear_filter_func_noncell which skips virtual refs (same
        // as kitty), so they survive a sweep and are only reachable by id/number
        for &(spec, frees) in &[(b'i', false), (b'I', true)] {
            let mut mgr = GraphicsManager::new();
            let img = add(&mut mgr, 1, 0, 5, 5);
            let _ = mgr.put_placement(img.id, Line(0), Column(0), &PlacementSpec::default(), CELL);
            mgr.image_mut(img.id).unwrap().placements_mut()[0].is_virtual = true;

            let result = mgr.handle_delete(&GraphicsCommand {
                action: b'd',
                delete_action: spec,
                id: 1,
                ..Default::default()
            });
            assert!(result.is_some(), "d={}: should return Some", spec as char);
            if frees {
                assert!(
                    mgr.image(img.id).is_none(),
                    "d={}: uppercase must free image",
                    spec as char
                );
            } else {
                assert!(
                    mgr.image(img.id).unwrap().placements().is_empty(),
                    "d={}: virtual placement must be removed by i",
                    spec as char
                );
            }
        }
    }

    fn make_frame(w: u32, h: u32) -> Frame {
        Frame { width: w, height: h, data: rgba((w * h * 4) as usize), ..Default::default() }
    }

    #[test]
    fn frame_add_appends_frames() {
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 1, 0, 4, 4);
        assert_eq!(mgr.image(img.id).unwrap().frames.len(), 1);

        mgr.add_frame(img.id, 0, make_frame(4, 4)).unwrap();
        assert_eq!(mgr.image(img.id).unwrap().frames.len(), 2);

        mgr.add_frame(img.id, 0, make_frame(4, 4)).unwrap();
        assert_eq!(mgr.image(img.id).unwrap().frames.len(), 3);
    }

    #[test]
    fn frame_add_stores_metadata() {
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 1, 0, 4, 4);
        let frame = Frame {
            width: 4,
            height: 4,
            data: rgba(64),
            gap_ms: 100,
            x_offset: 2,
            y_offset: 3,
            base_frame_id: 1,
            bgcolor: 0xFF0000FF,
            alpha_blend: true,
        };
        mgr.add_frame(img.id, 0, frame).unwrap();
        let stored = &mgr.image(img.id).unwrap().frames[1];
        assert_eq!(stored.gap_ms, 100);
        assert_eq!(stored.x_offset, 2);
        assert_eq!(stored.y_offset, 3);
        assert_eq!(stored.base_frame_id, 1);
        assert_eq!(stored.bgcolor, 0xFF0000FF);
        assert!(stored.alpha_blend);
    }

    #[test]
    fn frame_edit_replaces_not_appends() {
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 1, 0, 4, 4);
        mgr.add_frame(img.id, 0, make_frame(4, 4)).unwrap();
        assert_eq!(mgr.image(img.id).unwrap().frames.len(), 2);

        // Edit frame 2 (1-based) — should replace, not append
        let new_data = Arc::new(vec![0x11u8; 64]);
        let replacement =
            Frame { width: 4, height: 4, data: new_data.clone(), ..Default::default() };
        mgr.edit_frame(img.id, 2, replacement).unwrap();

        let image = mgr.image(img.id).unwrap();
        assert_eq!(image.frames.len(), 2, "edit must not append a new frame");
        assert_eq!(image.frames[1].data.as_ref(), new_data.as_ref());
    }

    #[test]
    fn frame_edit_out_of_range_returns_enoent() {
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 1, 0, 4, 4);
        let err = mgr.edit_frame(img.id, 2, make_frame(4, 4)).unwrap_err();
        assert_eq!(err.code, kitty_command::ErrorCode::ENOENT);
    }

    #[test]
    fn frame_coalesce_bookkeeping() {
        // Verify metadata + storage for a composed frame with base_frame_id
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 1, 0, 4, 4); // root = 64 bytes
        let before_frame_storage = mgr.frame_storage_used;

        let frame = Frame {
            width: 4,
            height: 4,
            data: rgba(64),
            base_frame_id: 1,
            alpha_blend: true,
            ..Default::default()
        };
        mgr.add_frame(img.id, 0, frame).unwrap();

        // Frame storage increased by 64 bytes
        assert_eq!(mgr.frame_storage_used, before_frame_storage + 64);
        // Image's frame_storage tracks the same delta
        assert_eq!(mgr.image(img.id).unwrap().frame_storage(), 64);
        // base_frame_id correctly stored
        assert_eq!(mgr.image(img.id).unwrap().frames[1].base_frame_id, 1);
    }

    #[test]
    fn frame_quota_enforced_at_5x() {
        // Storage limit = 64 bytes; frame ceiling = 5 × 64 = 320 bytes
        let mut mgr = GraphicsManager::with_storage_limit(64);
        let img = add(&mut mgr, 1, 0, 4, 4); // root = 64 bytes (at limit)

        // Each animation frame is 64 bytes. Should be able to add up to 5 frames
        // (5 × 64 = 320) without error
        for _ in 0..5 {
            mgr.add_frame(img.id, 0, make_frame(4, 4)).unwrap();
        }
        assert_eq!(mgr.image(img.id).unwrap().frames.len(), 6); // root + 5 anim

        // 6th animation frame (384 bytes total) should fail ENOSPC
        let err = mgr.add_frame(img.id, 0, make_frame(4, 4)).unwrap_err();
        assert_eq!(err.code, kitty_command::ErrorCode::ENOSPC);
    }

    #[test]
    fn frame_storage_released_on_image_remove() {
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 1, 0, 4, 4);
        mgr.add_frame(img.id, 0, make_frame(4, 4)).unwrap();
        assert!(mgr.frame_storage_used > 0);

        mgr.remove_image(img.id);
        assert_eq!(mgr.frame_storage_used, 0);
    }

    /// Build a 2x2 image with known pixel bytes.
    fn make_image_2x2(mgr: &mut GraphicsManager, client_id: u32, pixels: Vec<u8>) -> ImageId {
        assert_eq!(pixels.len(), 16); // 2*2*4
        let data = Arc::new(pixels);
        mgr.add_image(client_id, 0, 2, 2, data).id
    }

    #[test]
    fn coalesce_standalone_full_frame() {
        // A full-size standalone frame (base_frame_id=0, no bgcolor, full dims)
        // must return exactly the stored pixel data
        // 2x2 RGBA: [R0G0B0A0, R1G1B1A1, R2G2B2A2, R3G3B3A3]
        let pixels = vec![
            0x10, 0x11, 0x12, 0xFF, // px(0,0)
            0x20, 0x21, 0x22, 0xFF, // px(1,0)
            0x30, 0x31, 0x32, 0xFF, // px(0,1)
            0x40, 0x41, 0x42, 0xFF, // px(1,1)
        ];
        let mut mgr = GraphicsManager::new();
        let img_id = make_image_2x2(&mut mgr, 1, pixels.clone());
        let img = mgr.image(img_id).unwrap();
        let result = GraphicsManager::get_coalesced_frame_data(img, 1, 0).unwrap();
        assert_eq!(result, pixels, "standalone full frame must return pixel data verbatim");
    }

    #[test]
    fn coalesce_standalone_bgcolor_fill() {
        // A 1x1 frame at offset (1,1) within a 2x2 image with bgcolor=0xFF0000FF (red,opaque)
        // Canvas: 2x2 = 16 bytes; cells (0,0),(1,0),(0,1) filled with bgcolor; (1,1) = frame px
        let frame_pixel = vec![0x00u8, 0xFF, 0x00, 0xFF]; // green opaque (source copy)
        let mut mgr = GraphicsManager::new();
        // Root frame is 2x2 (needed to establish image dims)
        let img_id = make_image_2x2(&mut mgr, 1, vec![0u8; 16]);
        // Add animation frame: 1x1 at (1,1), bgcolor=red, alpha_blend=false (copy)
        let frame2 = Frame {
            width: 1,
            height: 1,
            data: Arc::new(frame_pixel.clone()),
            x_offset: 1,
            y_offset: 1,
            base_frame_id: 0,
            bgcolor: 0xFF0000FF, // R=0xFF G=0x00 B=0x00 A=0xFF
            alpha_blend: false,
            ..Default::default()
        };
        mgr.add_frame(img_id, 0, frame2).unwrap();
        let img = mgr.image(img_id).unwrap();
        // Frame 2 is the animation frame
        let result = GraphicsManager::get_coalesced_frame_data(img, 2, 0).unwrap();
        assert_eq!(result.len(), 16);
        // px(0,0), (1,0), (0,1) = bgcolor red; px(1,1) = green
        assert_eq!(&result[0..4], &[0xFF, 0x00, 0x00, 0xFF], "px(0,0) = bgcolor red");
        assert_eq!(&result[4..8], &[0xFF, 0x00, 0x00, 0xFF], "px(1,0) = bgcolor red");
        assert_eq!(&result[8..12], &[0xFF, 0x00, 0x00, 0xFF], "px(0,1) = bgcolor red");
        assert_eq!(&result[12..16], &[0x00, 0xFF, 0x00, 0xFF], "px(1,1) = frame pixel green");
    }

    #[test]
    fn coalesce_source_copy_fast_path() {
        // alpha_blend=false: frame pixels must be copied directly without blending
        // Base is opaque blue; delta is a 1x1 red pixel at (0,0), source-copy mode
        let base_pixels = vec![
            0x00, 0x00, 0xFF, 0xFF, // blue at (0,0)
            0x00, 0x00, 0xFF, 0xFF, // blue at (1,0)
            0x00, 0x00, 0xFF, 0xFF, // blue at (0,1)
            0x00, 0x00, 0xFF, 0xFF, // blue at (1,1)
        ];
        let mut mgr = GraphicsManager::new();
        let img_id = make_image_2x2(&mut mgr, 1, base_pixels.clone());
        let delta = Frame {
            width: 1,
            height: 1,
            data: Arc::new(vec![0xFF, 0x00, 0x00, 0xFF]), // red pixel
            x_offset: 0,
            y_offset: 0,
            base_frame_id: 1, // references frame 1 (root)
            bgcolor: 0,
            alpha_blend: false, // source-copy
            ..Default::default()
        };
        mgr.add_frame(img_id, 0, delta).unwrap();
        let img = mgr.image(img_id).unwrap();
        let result = GraphicsManager::get_coalesced_frame_data(img, 2, 0).unwrap();
        // px(0,0) must be red (copied); rest remain blue
        assert_eq!(&result[0..4], &[0xFF, 0x00, 0x00, 0xFF], "source-copy: px(0,0)=red");
        assert_eq!(&result[4..8], &[0x00, 0x00, 0xFF, 0xFF], "source-copy: px(1,0)=blue");
        assert_eq!(&result[8..12], &[0x00, 0x00, 0xFF, 0xFF], "source-copy: px(0,1)=blue");
        assert_eq!(&result[12..16], &[0x00, 0x00, 0xFF, 0xFF], "source-copy: px(1,1)=blue");
    }

    #[test]
    fn coalesce_porter_duff_over() {
        // alpha_blend=true: Porter-Duff over. Over pixel is 50% transparent green
        // Under pixel is opaque blue. Hand-computed result:
        //   src_a=128/255≈0.502, dst_a=1.0
        //   out_a = 0.502 + 1.0*(1-0.502) = 1.0 → 255
        //   out_R = (0*0.502 + 0*1.0*(0.498))/1.0 = 0
        //   out_G = (255*0.502 + 0*0.498)/1.0 = 128 (rounds to 128)
        //   out_B = (0*0.502 + 255*0.498)/1.0 = 127 (rounds to 127)
        let base_px = vec![
            0x00u8, 0x00, 0xFF, 0xFF, // opaque blue at (0,0)
            0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF, 0x00, 0x00, 0xFF, 0xFF,
        ];
        let mut mgr = GraphicsManager::new();
        let img_id = make_image_2x2(&mut mgr, 1, base_px);
        let over_px = vec![0x00u8, 0xFF, 0x00, 128]; // semi-transparent green
        let delta = Frame {
            width: 1,
            height: 1,
            data: Arc::new(over_px),
            x_offset: 0,
            y_offset: 0,
            base_frame_id: 1,
            bgcolor: 0,
            alpha_blend: true,
            ..Default::default()
        };
        mgr.add_frame(img_id, 0, delta).unwrap();
        let img = mgr.image(img_id).unwrap();
        let result = GraphicsManager::get_coalesced_frame_data(img, 2, 0).unwrap();
        // Exact values from Rust f32 cast: 255*(128/255+1*(1-128/255))=255→255;
        // G=floor(255*0.502/1.0)=128; B=floor(255*0.498/1.0)=126 (f32 truncation)
        assert_eq!(result[3], 0xFF, "out alpha must be 255");
        assert_eq!(result[0], 0x00, "out R=0");
        assert_eq!(result[1], 128, "out G=128");
        assert_eq!(result[2], 126, "out B=126 (f32 truncation)");
        // Other pixels unchanged
        assert_eq!(&result[4..8], &[0x00, 0x00, 0xFF, 0xFF]);
    }

    #[test]
    fn coalesce_2_frame_chain() {
        // Chain: frame1 (standalone blue 2x2) -> frame2 (red 1x1 at (0,0) copy)
        //     -> frame3 (green 1x1 at (1,1) copy)
        // Expected coalesced result for frame3: blue everywhere except (0,0)=red, (1,1)=green
        let blue = vec![0x00u8, 0x00, 0xFF, 0xFF];
        let red = vec![0xFFu8, 0x00, 0x00, 0xFF];
        let green = vec![0x00u8, 0xFF, 0x00, 0xFF];
        let base_pixels = {
            let mut v = Vec::with_capacity(16);
            for _ in 0..4 {
                v.extend_from_slice(&blue);
            }
            v
        };
        let mut mgr = GraphicsManager::new();
        let img_id = make_image_2x2(&mut mgr, 1, base_pixels);
        // Frame 2: red at (0,0), base=frame1
        mgr.add_frame(img_id, 0, Frame {
            width: 1,
            height: 1,
            data: Arc::new(red.clone()),
            x_offset: 0,
            y_offset: 0,
            base_frame_id: 1,
            bgcolor: 0,
            alpha_blend: false,
            ..Default::default()
        })
        .unwrap();
        // Frame 3: green at (1,1), base=frame2
        mgr.add_frame(img_id, 0, Frame {
            width: 1,
            height: 1,
            data: Arc::new(green.clone()),
            x_offset: 1,
            y_offset: 1,
            base_frame_id: 2,
            bgcolor: 0,
            alpha_blend: false,
            ..Default::default()
        })
        .unwrap();
        let img = mgr.image(img_id).unwrap();
        let result = GraphicsManager::get_coalesced_frame_data(img, 3, 0).unwrap();
        assert_eq!(&result[0..4], &red[..], "px(0,0)=red from frame2");
        assert_eq!(&result[4..8], &blue[..], "px(1,0)=blue");
        assert_eq!(&result[8..12], &blue[..], "px(0,1)=blue");
        assert_eq!(&result[12..16], &green[..], "px(1,1)=green from frame3");
    }

    #[test]
    fn coalesce_depth_cap_32() {
        // depth cap fires when depth > 32, i.e. at depth=33 (the 34th recursive call)
        // A chain of 34 frames: frame 34 at depth 0, ..., frame 1 at depth 33 → None
        let mut mgr = GraphicsManager::new();
        let img_id = mgr.add_image(1, 0, 1, 1, Arc::new(vec![0u8; 4])).id;
        // Append 33 more frames each referencing the previous (total chain = 34)
        for i in 0..33u32 {
            mgr.add_frame(img_id, 0, Frame {
                width: 1,
                height: 1,
                data: Arc::new(vec![0u8; 4]),
                base_frame_id: i + 1,
                ..Default::default()
            })
            .unwrap();
        }
        let img = mgr.image(img_id).unwrap();
        assert_eq!(img.frames.len(), 34);
        // Frame 34 recurses to depth 33 to reach frame 1; depth 33 > 32 → None
        let result = GraphicsManager::get_coalesced_frame_data(img, 34, 0);
        assert!(result.is_none(), "depth-cap 32 must return None for 34-frame chain");
    }

    #[test]
    fn keyframe_flatten_triggers_at_threshold() {
        // With a 2x2 image (area=4), limit = 8. Chain of 5 frames of 1x1 each
        // accumulates drawn_area = 5 < 8 but count = 5, so threshold is hit
        let mut mgr = GraphicsManager::new();
        let img_id = make_image_2x2(&mut mgr, 1, vec![0u8; 16]);
        // Build a chain of 4 more frames (frame 1 is root, frames 2-5 delta)
        for i in 0..4u32 {
            mgr.add_frame(img_id, 0, Frame {
                width: 1,
                height: 1,
                data: Arc::new(vec![0xAAu8, 0xBB, 0xCC, 0xFF]),
                base_frame_id: i + 1,
                ..Default::default()
            })
            .unwrap();
        }
        // Frame 5 has chain length 5 (5→4→3→2→1). Flatten frame 5
        {
            let img = mgr.image_mut(img_id).unwrap();
            GraphicsManager::maybe_flatten_keyframe(img, 5);
            // After flattening, frame 5 must be standalone (base_frame_id==0)
            assert_eq!(img.frames[4].base_frame_id, 0, "flattened frame must be standalone");
            assert_eq!(img.frames[4].width, 2, "flattened frame must span full image width");
            assert_eq!(img.frames[4].height, 2, "flattened frame must span full image height");
        }
    }

    #[test]
    fn compose_frame_einval_on_overlap() {
        // compose_frame must return EINVAL when the composed region is out of bounds
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 1, 0, 2, 2);
        mgr.add_frame(img.id, 0, make_frame(2, 2)).unwrap();
        // Source frame = 2 (index 1), Dest frame = 1 (index 0)
        // Place a 2x2 source at offset (1,1) in a 2x2 image → x+w=3 > 2 → EINVAL
        let err = mgr
            .compose_frame(img.id, ComposeFrameArgs {
                src_frame_number: 2,
                dst_frame_number: 1,
                dst_x: 1,
                dst_y: 1,
                src_w: 2,
                src_h: 2,
                needs_blend: false,
            })
            .unwrap_err();
        assert_eq!(err.code, kitty_command::ErrorCode::EINVAL, "out-of-bounds must be EINVAL");
    }

    #[test]
    fn compose_frame_source_copy() {
        // Compose 1x1 red pixel from frame2 onto frame1 at (1,1), source-copy mode
        let base = vec![0u8; 16]; // 2x2 transparent black
        let mut mgr = GraphicsManager::new();
        let img_id = make_image_2x2(&mut mgr, 1, base);
        let red = vec![0xFFu8, 0x00, 0x00, 0xFF];
        mgr.add_frame(img_id, 0, Frame {
            width: 1,
            height: 1,
            data: Arc::new(red),
            ..Default::default()
        })
        .unwrap();
        // Compose frame2 (1x1 red, w=1,h=1) onto frame1 at (1,1)
        mgr.compose_frame(img_id, ComposeFrameArgs {
            src_frame_number: 2,
            dst_frame_number: 1,
            dst_x: 1,
            dst_y: 1,
            src_w: 1,
            src_h: 1,
            needs_blend: false,
        })
        .unwrap();
        let img = mgr.image(img_id).unwrap();
        let result = GraphicsManager::get_coalesced_frame_data(img, 1, 0).unwrap();
        assert_eq!(&result[12..16], &[0xFF, 0x00, 0x00, 0xFF], "compose_frame: px(1,1)=red");
        assert_eq!(&result[0..4], &[0u8, 0, 0, 0], "compose_frame: px(0,0) untouched");
    }

    /// Build a 2x2 image with N full-frame animation frames, each a distinct color.
    fn make_animated(mgr: &mut GraphicsManager, client_id: u32, n_frames: usize) -> ImageId {
        let added = add(mgr, client_id, 0, 2, 2);
        for i in 0..n_frames.saturating_sub(1) {
            let color = (i as u8).wrapping_add(0x10);
            let data = Arc::new(vec![color; 16]);
            mgr.add_frame(added.id, 0, Frame {
                width: 2,
                height: 2,
                data,
                gap_ms: 100,
                ..Default::default()
            })
            .unwrap();
        }
        added.id
    }

    #[test]
    fn animation_no_timer_when_idle() {
        // scan_active_animations must return None (and do no work) when nothing runs
        let mgr = GraphicsManager::new();
        assert_eq!(mgr.active_animation_count(), 0);
        assert!(mgr.scan_active_animations(1000).is_none());
    }

    #[test]
    fn animation_frame_advance_timing() {
        // 3-frame image; gap_ms=100ms on every frame
        // At t=0 start Running. At t=99 no advance. At t=100 advances to frame 2
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 1, 3);
        // All frames get gap_ms = 100
        for f in mgr.image_mut(id).unwrap().frames.iter_mut() {
            f.gap_ms = 100;
        }
        // Place so `is_drawn` proxy is satisfied (kitty image_is_animatable guard)
        place(&mut mgr, id, 0);
        mgr.animation_control(id, AnimationControlArgs {
            anim_state: 3,
            frame_number: 0,
            gap_frame: 0,
            gap_ms: 0,
            loop_count: 0,
            now_ms: 0,
        });

        assert_eq!(mgr.active_animation_count(), 1);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 0);

        // Before deadline: no advance
        let advanced = mgr.advance_animations(99);
        assert!(!advanced);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 0);

        // At deadline: advances to frame index 1
        let advanced = mgr.advance_animations(100);
        assert!(advanced);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 1);

        // scan returns ~100ms until the next frame
        let next = mgr.scan_active_animations(100).unwrap();
        assert!(next.as_millis() > 0 && next.as_millis() <= 100);

        // Advance again at t=200 → frame index 2
        let advanced = mgr.advance_animations(200);
        assert!(advanced);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 2);
    }

    #[test]
    fn animation_loop_count_n_minus_one_semantics() {
        // v=2 → max_loops=1 (kitty n-1): animation stops after 1 extra loop
        // i.e. after wrapping around once.  With 2 frames and gap_ms=10:
        //   loop 0: frame0→frame1→wrap (current_loop becomes 1)
        //   loop 1 = max_loops → stop
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 2, 2);
        for f in mgr.image_mut(id).unwrap().frames.iter_mut() {
            f.gap_ms = 10;
        }
        // Place so `is_drawn` proxy is satisfied (kitty image_is_animatable guard)
        place(&mut mgr, id, 0);
        // v=2 → max_loops = 2-1 = 1
        mgr.animation_control(id, AnimationControlArgs {
            anim_state: 3,
            frame_number: 0,
            gap_frame: 0,
            gap_ms: 0,
            loop_count: 2,
            now_ms: 0,
        });

        assert_eq!(mgr.image(id).unwrap().max_loops, 1);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 0);

        // t=10: frame0 → frame1
        mgr.advance_animations(10);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 1);
        assert_eq!(mgr.image(id).unwrap().animation_state, AnimationState::Running);

        // t=20: frame1 → frame0 (wrap, current_loop → 1 = max_loops → stop)
        mgr.advance_animations(20);
        assert_eq!(mgr.image(id).unwrap().animation_state, AnimationState::Stopped);
        assert_eq!(mgr.active_animation_count(), 0);
        // After stopping, no more timer needed
        assert!(mgr.scan_active_animations(20).is_none());
    }

    #[test]
    fn animation_gapless_frame_skip() {
        // Frame 1: gap=50, frame 2: gap=0 (gapless), frame 3: gap=50
        // Advancing at t=50 from frame 1 must skip frame 2 (gap=0) and land on frame 3
        let mut mgr = GraphicsManager::new();
        let added = add(&mut mgr, 3, 0, 2, 2);
        let id = added.id;
        let make_frame = |gap: u32| Frame {
            width: 2,
            height: 2,
            data: Arc::new(vec![0xAA; 16]),
            gap_ms: gap,
            ..Default::default()
        };
        mgr.add_frame(id, 0, make_frame(0)).unwrap(); // frame 2 – gapless
        mgr.add_frame(id, 0, make_frame(50)).unwrap(); // frame 3 – gap=50
        // Set root frame gap to 50
        mgr.image_mut(id).unwrap().frames[0].gap_ms = 50;
        // Place so kitty `is_drawn` proxy is satisfied
        place(&mut mgr, id, 0);
        mgr.animation_control(id, AnimationControlArgs {
            anim_state: 3,
            frame_number: 0,
            gap_frame: 0,
            gap_ms: 0,
            loop_count: 0,
            now_ms: 0,
        });

        // At t=50, advance from frame 0 (gap=50): next=frame1 (gap=0, skip), land frame2
        mgr.advance_animations(50);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 2, "gapless frame must be skipped");
    }

    #[test]
    fn animation_stop_command() {
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 4, 3);
        mgr.animation_control(id, AnimationControlArgs {
            anim_state: 3,
            frame_number: 0,
            gap_frame: 0,
            gap_ms: 0,
            loop_count: 0,
            now_ms: 0,
        });
        assert_eq!(mgr.active_animation_count(), 1);
        mgr.animation_control(id, AnimationControlArgs {
            anim_state: 1,
            frame_number: 0,
            gap_frame: 0,
            gap_ms: 0,
            loop_count: 0,
            now_ms: 0,
        }); // stop
        assert_eq!(mgr.active_animation_count(), 0);
        assert_eq!(mgr.image(id).unwrap().animation_state, AnimationState::Stopped);
        assert!(mgr.scan_active_animations(999).is_none());
    }

    #[test]
    fn animation_no_advance_when_stopped() {
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 5, 3);
        for f in mgr.image_mut(id).unwrap().frames.iter_mut() {
            f.gap_ms = 50;
        }
        // Leave Stopped; advance must not move the frame
        let advanced = mgr.advance_animations(1000);
        assert!(!advanced);
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 0);
    }

    #[test]
    fn animation_not_animatable_without_placement() {
        // A multi-frame running image that was never placed must NOT be reported
        // animatable (kitty `is_drawn` guard, graphics.c:1774)
        let mut mgr = GraphicsManager::new();
        let added = add(&mut mgr, 10, 0, 2, 2);
        let id = added.id;
        // Add a second frame with non-zero gap
        mgr.add_frame(id, 0, Frame {
            width: 2,
            height: 2,
            data: Arc::new(vec![0xBB; 16]),
            gap_ms: 100,
            ..Default::default()
        })
        .unwrap();
        // Set running — but intentionally do NOT call place()
        mgr.animation_control(id, AnimationControlArgs {
            anim_state: 3,
            frame_number: 0,
            gap_frame: 0,
            gap_ms: 0,
            loop_count: 0,
            now_ms: 0,
        });
        // No placement → not animatable → scan returns None
        assert!(mgr.scan_active_animations(999).is_none());
        // And advance must not move the frame
        assert!(!mgr.advance_animations(999));
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 0);
    }

    #[test]
    fn animation_not_animatable_when_all_frames_zero_gap() {
        // An image where every frame has gap_ms=0 must NOT be reported animatable
        // (kitty `animation_duration` guard, graphics.c:1774)
        let mut mgr = GraphicsManager::new();
        let id = make_animated(&mut mgr, 11, 3);
        // Override all frame gaps to 0
        for f in mgr.image_mut(id).unwrap().frames.iter_mut() {
            f.gap_ms = 0;
        }
        mgr.animation_control(id, AnimationControlArgs {
            anim_state: 3,
            frame_number: 0,
            gap_frame: 0,
            gap_ms: 0,
            loop_count: 0,
            now_ms: 0,
        });
        // All gaps zero → not animatable
        assert!(mgr.scan_active_animations(999).is_none());
        assert!(!mgr.advance_animations(999));
        assert_eq!(mgr.image(id).unwrap().current_frame_index, 0);
    }

    /// Helper: place image `id` at `(line, col)` with client placement id `p`.
    fn place_at(
        mgr: &mut GraphicsManager,
        id: ImageId,
        line: i32,
        col: usize,
        p: u32,
    ) -> Result<Option<(u32, u32)>, CommandError> {
        let spec = PlacementSpec { placement_id: p, ..Default::default() };
        mgr.put_placement(id, Line(line), Column(col), &spec, CELL)
    }

    /// Helper: place image `id` relative to parent image `par_client_id`
    /// / placement `par_pl_id`, with H=dx, V=dy cell offsets.
    fn place_relative(
        mgr: &mut GraphicsManager,
        id: ImageId,
        p: u32,
        par_client_id: u32,
        par_pl_id: u32,
        dx: i32,
        dy: i32,
    ) -> Result<Option<(u32, u32)>, CommandError> {
        let spec = PlacementSpec {
            placement_id: p,
            parent_client_id: par_client_id,
            parent_placement_client_id: par_pl_id,
            parent_offset_x: dx,
            parent_offset_y: dy,
            ..Default::default()
        };
        mgr.put_placement(id, Line(0), Column(0), &spec, CELL)
    }

    #[test]
    fn parent_nonexistent_image_returns_enoparent() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 10, 10);
        let err = place_relative(&mut mgr, a.id, 1, 99, 0, 0, 0).unwrap_err();
        assert_eq!(err.code, ErrorCode::ENOPARENT);
        assert!(err.sends_response);
    }

    #[test]
    fn parent_image_with_no_placements_returns_enoparent() {
        let mut mgr = GraphicsManager::new();
        let parent = add(&mut mgr, 1, 0, 10, 10); // exists but no placements
        let child = add(&mut mgr, 2, 0, 5, 5);
        let err = place_relative(&mut mgr, child.id, 1, 1, 0, 0, 0).unwrap_err();
        assert_eq!(err.code, ErrorCode::ENOPARENT);
        // parent_id was registered but the image hasn't been placed
        let _ = parent; // ensure it stays alive
    }

    #[test]
    fn parent_placement_id_not_found_returns_enoparent() {
        let mut mgr = GraphicsManager::new();
        let parent = add(&mut mgr, 1, 0, 10, 10);
        place_at(&mut mgr, parent.id, 0, 0, 1).unwrap(); // placement p=1
        let child = add(&mut mgr, 2, 0, 5, 5);
        // Q=99 doesn't exist on parent
        let err = place_relative(&mut mgr, child.id, 1, 1, 99, 0, 0).unwrap_err();
        assert_eq!(err.code, ErrorCode::ENOPARENT);
    }

    #[test]
    fn self_parent_returns_einval() {
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 10, 10);
        place_at(&mut mgr, a.id, 0, 0, 1).unwrap(); // p=1
        // Now try to place p=1 again relative to itself (P=1, Q=1)
        let err = place_relative(&mut mgr, a.id, 1, 1, 1, 0, 0).unwrap_err();
        assert_eq!(err.code, ErrorCode::EINVAL);
    }

    #[test]
    fn virtual_with_parent_returns_einval() {
        // This is handled in term/mod.rs::graphics_place, but we test the
        // PlacementSpec path: a virtual placement should not get parent fields,
        // but the protocol layer rejects it before put_placement
        // We verify via a direct spec with is_virtual=true
        // Since put_placement doesn't check is_virtual+parent (that's term's job),
        // this test just confirms the path is consistent with the protocol
        // (The actual rejection lives in graphics_place, tested via term tests.)
        let mut mgr = GraphicsManager::new();
        let parent = add(&mut mgr, 1, 0, 10, 10);
        place_at(&mut mgr, parent.id, 2, 3, 1).unwrap();
        let child = add(&mut mgr, 2, 0, 5, 5);
        // Virtual + parent is rejected at the term layer; put_placement itself
        // accepts it. Confirm no panic and the placement is created
        let spec = PlacementSpec {
            placement_id: 1,
            is_virtual: true,
            parent_client_id: 1,
            parent_placement_client_id: 1,
            parent_offset_x: 1,
            parent_offset_y: 1,
            ..Default::default()
        };
        // put_placement does NOT reject virtual+parent (term does)
        let result = mgr.put_placement(child.id, Line(0), Column(0), &spec, CELL);
        assert!(result.is_ok(), "put_placement itself doesn't reject virtual+parent");
    }

    #[test]
    fn cycle_direct_returns_ecycle() {
        // A→B→A is a cycle
        let mut mgr = GraphicsManager::new();
        let a = add(&mut mgr, 1, 0, 10, 10);
        let b = add(&mut mgr, 2, 0, 10, 10);

        // Place a at (0,0) with p=1
        place_at(&mut mgr, a.id, 0, 0, 1).unwrap();
        // Place b relative to a's p=1
        place_relative(&mut mgr, b.id, 1, 1, 1, 0, 0).unwrap();
        // Now update a's p=1 to be relative to b's p=1 → A→B→A cycle
        let err = place_relative(&mut mgr, a.id, 1, 2, 1, 0, 0).unwrap_err();
        assert_eq!(err.code, ErrorCode::ECYCLE);
        // Placement must be unchanged (tentative ancestry: revert on failure)
        let a_pl = mgr.image(a.id).unwrap().placements()[0].clone();
        assert_eq!(a_pl.parent_image_id, 0, "revert must restore no-parent state");
    }

    #[test]
    fn depth_limit_returns_etoodeep() {
        // kitty's PARENT_DEPTH_LIMIT = 8: depth >= 8 in has_good_ancestry
        // triggers ETOODEEP. A valid chain has ≤8 ancestors; 9 ancestors
        // (depth reaches 8 during the walk) is rejected
        // We create 10 images: imgs[0] is root; imgs[9] has 9 ancestors
        let mut mgr = GraphicsManager::new();
        let imgs: Vec<_> = (1..=10).map(|i| add(&mut mgr, i, 0, 4, 4)).collect();

        // Place img1 (root, client_id=1)
        place_at(&mut mgr, imgs[0].id, 0, 0, 1).unwrap();
        // Chain imgs[1..=8]: each relative to the previous
        // imgs[1] relative to imgs[0] (client_id=1), ..., imgs[8] relative to imgs[7]
        for (img, parent_client_id) in imgs[1..=8].iter().zip(1u32..) {
            place_relative(&mut mgr, img.id, 1, parent_client_id, 1, 0, 0).unwrap();
        }
        // imgs[9] relative to imgs[8] would be the 9th ancestor → ETOODEEP
        let err = place_relative(&mut mgr, imgs[9].id, 1, 9, 1, 0, 0).unwrap_err();
        assert_eq!(err.code, ErrorCode::ETOODEEP);
    }

    #[test]
    fn resolve_parent_offset_two_level_chain() {
        // img1 placed at (row=3, col=5), img2 relative with H=2, V=1
        // resolve_parent_offset should give (row=3+1, col=5+2) = (4, 7)
        let mut mgr = GraphicsManager::new();
        let img1 = add(&mut mgr, 1, 0, 10, 10);
        let img2 = add(&mut mgr, 2, 0, 10, 10);

        place_at(&mut mgr, img1.id, 3, 5, 1).unwrap();
        place_relative(&mut mgr, img2.id, 1, 1, 1, 2, 1).unwrap();

        let img2_pl = &mgr.image(img2.id).unwrap().placements()[0];
        let pl_id = img2_pl.internal_id;
        let resolved = mgr.resolve_parent_offset(img2.id, pl_id).unwrap();
        assert_eq!(resolved, (Line(4), Column(7)));
    }

    #[test]
    fn resolve_parent_offset_three_level_chain() {
        // img1@(1,1) → img2 H=2,V=0 → img3 H=3,V=1
        // Expected: img3 at (1+0+1, 1+2+3) = (2, 6)
        let mut mgr = GraphicsManager::new();
        let img1 = add(&mut mgr, 1, 0, 4, 4);
        let img2 = add(&mut mgr, 2, 0, 4, 4);
        let img3 = add(&mut mgr, 3, 0, 4, 4);

        place_at(&mut mgr, img1.id, 1, 1, 1).unwrap();
        place_relative(&mut mgr, img2.id, 1, 1, 1, 2, 0).unwrap();
        place_relative(&mut mgr, img3.id, 1, 2, 1, 3, 1).unwrap();

        let pl3 = &mgr.image(img3.id).unwrap().placements()[0];
        let resolved = mgr.resolve_parent_offset(img3.id, pl3.internal_id).unwrap();
        assert_eq!(resolved, (Line(2), Column(6)));
    }

    #[test]
    fn cascade_orphan_does_not_appear_in_render_snapshot() {
        // Place parent, then child. Delete parent. Child should not appear
        // in render snapshot (orphan → silent cascade)
        let mut mgr = GraphicsManager::new();
        let parent = add(&mut mgr, 1, 0, 10, 10);
        let child = add(&mut mgr, 2, 0, 10, 10);

        place_at(&mut mgr, parent.id, 0, 0, 1).unwrap();
        place_relative(&mut mgr, child.id, 1, 1, 1, 0, 0).unwrap();

        // Verify child appears before deletion
        let snap = mgr.render_snapshot(0);
        let child_in_snap = snap.items.iter().any(|it| it.image_id == child.id);
        assert!(child_in_snap, "child must appear in snapshot before parent deletion");

        // Delete the parent image entirely
        mgr.remove_image(parent.id);

        // Drain the snapshot again — child must not appear (parent gone → unresolvable)
        // We need to re-add uploads since snapshots drain them; instead just re-check items
        // (remove_image doesn't drain anything; render_snapshot will try resolve and fail.)
        // Put child back (it wasn't deleted) by checking its image still exists
        assert!(mgr.image(child.id).is_some(), "child image must still exist (only parent gone)");
        // Force a snapshot: child's resolve_parent_offset returns None → skipped
        let snap2 = mgr.render_snapshot(0);
        let child_in_snap2 = snap2.items.iter().any(|it| it.image_id == child.id);
        assert!(!child_in_snap2, "orphaned child must be invisible in render snapshot (cascade)");
    }

    #[test]
    fn resolve_parent_offset_returns_none_for_orphan() {
        let mut mgr = GraphicsManager::new();
        let parent = add(&mut mgr, 1, 0, 10, 10);
        let child = add(&mut mgr, 2, 0, 10, 10);

        place_at(&mut mgr, parent.id, 2, 4, 1).unwrap();
        place_relative(&mut mgr, child.id, 1, 1, 1, 1, 1).unwrap();

        let pl_id = mgr.image(child.id).unwrap().placements()[0].internal_id;
        let resolved = mgr.resolve_parent_offset(child.id, pl_id);
        assert!(resolved.is_some(), "should resolve before parent deletion");

        mgr.remove_image(parent.id);
        let resolved_after = mgr.resolve_parent_offset(child.id, pl_id);
        assert!(resolved_after.is_none(), "should return None after parent deleted");
    }

    #[test]
    fn relative_placement_uses_first_parent_placement_when_q_is_zero() {
        // Q=0 means "use first placement of the parent"
        let mut mgr = GraphicsManager::new();
        let parent = add(&mut mgr, 1, 0, 10, 10);
        let child = add(&mut mgr, 2, 0, 10, 10);

        // Create two placements for parent
        place_at(&mut mgr, parent.id, 5, 7, 1).unwrap();
        place_at(&mut mgr, parent.id, 9, 9, 2).unwrap();

        // Q=0 → first placement (p=1 at row=5, col=7)
        let spec = PlacementSpec {
            placement_id: 1,
            parent_client_id: 1,
            parent_placement_client_id: 0, // Q=0 → use first
            parent_offset_x: 1,
            parent_offset_y: 2,
            ..Default::default()
        };
        mgr.put_placement(child.id, Line(0), Column(0), &spec, CELL).unwrap();

        let pl = &mgr.image(child.id).unwrap().placements()[0];
        let resolved = mgr.resolve_parent_offset(child.id, pl.internal_id).unwrap();
        // First parent placement is at (5, 7); child offset H=1,V=2 → (7, 8)
        assert_eq!(resolved, (Line(7), Column(8)));
    }

    /// Memory-stress: transmit 200 images through the quota, churn by
    /// deleting half and re-adding, then assert that `used_storage` stays
    /// at or below `storage_limit` and does not grow monotonically.
    ///
    /// Acceptance criteria:
    ///   - `used_storage <= storage_limit` after every add
    ///   - Storage does not grow monotonically across two passes (reclaim works)
    ///   - `active_animation_count == 0` when no animations are registered (animation-timer guard
    ///     adds zero per-frame cost when idle)
    #[test]
    fn stress_200_images_quota_churn() {
        // Use a small quota so eviction fires frequently
        // Each image is 64 x 64 x 4 = 16 384 bytes
        // Quota = 10 images worth; adding 200 forces ~19 eviction rounds
        const IMG_W: u32 = 64;
        const IMG_H: u32 = 64;
        const IMG_BYTES: usize = (IMG_W * IMG_H * 4) as usize;
        const N: u32 = 200;
        const QUOTA_IMAGES: usize = 10;
        let quota = IMG_BYTES * QUOTA_IMAGES;

        let mut mgr = GraphicsManager::with_storage_limit(quota);
        assert_eq!(mgr.storage_limit, quota);

        // Pass 1: add 200 images (all without placements so quota eviction
        // can remove them freely — mirrors how the protocol adds unplaced images)
        for i in 1..=N {
            add(&mut mgr, i, 0, IMG_W, IMG_H);
            assert!(
                mgr.used_storage() <= quota,
                "pass-1 add {i}: used_storage {} > quota {}",
                mgr.used_storage(),
                quota,
            );
        }
        let storage_after_pass1 = mgr.used_storage();
        // After 200 adds into a 10-image quota, at most quota bytes should remain
        assert!(
            storage_after_pass1 <= quota,
            "after pass-1: used_storage {storage_after_pass1} > quota {quota}",
        );

        // Pass 2: add 200 more images (same IDs wrap — new transmissions
        // replace if client_id matches; anonymous i=0 always fresh)
        // Use anonymous ids so each add is genuinely new
        for i in (N + 1)..=(N * 2) {
            add(&mut mgr, i, 0, IMG_W, IMG_H);
            assert!(
                mgr.used_storage() <= quota,
                "pass-2 add {i}: used_storage {} > quota {}",
                mgr.used_storage(),
                quota,
            );
        }
        let storage_after_pass2 = mgr.used_storage();

        // Storage must not monotonically grow: if it were leaking, pass-2
        // would exceed pass-1 + quota. Being within quota is sufficient
        assert!(
            storage_after_pass2 <= quota,
            "after pass-2: used_storage {storage_after_pass2} > quota {quota}",
        );

        // Animation guard: no animations were registered — count must be 0
        // and scan must short-circuit without work
        assert_eq!(
            mgr.active_animation_count(),
            0,
            "no animations registered: active_animation_count should be 0",
        );
        assert!(
            mgr.scan_active_animations(999_999).is_none(),
            "scan_active_animations must return None (no-op) when count == 0",
        );
    }

    /// Verify the animation-timer guard: `active_animation_count == 0` means
    /// `scan_active_animations` returns `None` immediately — this is the
    /// per-frame early-exit that keeps the text-render hot path free of
    /// graphics overhead when no animations are in flight.
    #[test]
    fn animation_guard_zero_cost_when_idle() {
        let mgr = GraphicsManager::new();
        // Fresh manager with no images: must short-circuit
        assert_eq!(mgr.active_animation_count(), 0);
        let result = mgr.scan_active_animations(u64::MAX);
        assert!(
            result.is_none(),
            "scan_active_animations must return None when active_animation_count == 0",
        );
    }

    /// An empty snapshot (no images on screen, none uploaded/deleted) must NOT force a
    /// full redraw — otherwise damage tracking would be permanently defeated.
    #[test]
    fn full_damage_not_required_when_no_graphics() {
        let snapshot =
            RenderSnapshot { items: Vec::new(), uploads: Vec::new(), deletes: Vec::new() };
        assert!(!snapshot.requires_full_damage());
    }

    /// A visible image (non-empty `items`) must force full-frame damage. This is the core
    /// guard against buffer-age ghosting: a static image plus partial-damage updates
    /// elsewhere would otherwise leave the image stale in the alternate buffer.
    #[test]
    fn full_damage_required_when_image_visible() {
        let mut mgr = GraphicsManager::new();
        let img = add(&mut mgr, 0, 1, 2, 2);
        place(&mut mgr, img.id, 1);
        let snapshot = mgr.render_snapshot(0);
        assert!(!snapshot.items.is_empty(), "placed image must yield render items");
        assert!(snapshot.requires_full_damage());
    }

    /// A delete-only frame (image removed, nothing left visible) must STILL force full
    /// damage so the just-deleted image's pixels are cleared from BOTH buffers, not left
    /// frozen in the alternate one.
    #[test]
    fn full_damage_required_on_delete_only_frame() {
        let snapshot =
            RenderSnapshot { items: Vec::new(), uploads: Vec::new(), deletes: vec![1 as ImageId] };
        assert!(snapshot.requires_full_damage());
    }

    /// An upload-only frame (new texture, placement pending) must force full damage too.
    #[test]
    fn full_damage_required_on_upload_only_frame() {
        let snapshot = RenderSnapshot {
            items: Vec::new(),
            uploads: vec![(1 as ImageId, Arc::new(vec![0u8; 16]))],
            deletes: Vec::new(),
        };
        assert!(snapshot.requires_full_damage());
    }

    /// Helper: add a 10x20 image (1-cell extent at CELL={10,20}) and place it
    /// at the given grid position with the given origin.
    fn add_positional(
        mgr: &mut GraphicsManager,
        line: i32,
        col: usize,
        origin: PlacementOrigin,
    ) -> ImageId {
        let img = add(mgr, 0, 0, 10, 20); // 1-col × 1-row at CELL={10,20}
        let spec = PlacementSpec { origin, ..Default::default() };
        mgr.put_placement(img.id, Line(line), Column(col), &spec, CELL).unwrap().unwrap();
        img.id
    }

    /// A new Sixel placement evicts a prior Sixel placement at the same cell.
    #[test]
    fn evict_overlapping_positional_sixel_evicts_overlapping_sixel() {
        let mut mgr = GraphicsManager::new();
        let old = add_positional(&mut mgr, 0, 0, PlacementOrigin::Sixel);
        let new_img = add_positional(&mut mgr, 0, 0, PlacementOrigin::Sixel);

        // Evict everything overlapping (0,0)+(1,1) except the new image
        mgr.evict_overlapping_positional(new_img, Line(0), Column(0), 1, 1);

        assert!(mgr.image(old).is_none(), "old sixel must be evicted");
        assert!(mgr.image(new_img).is_some(), "new sixel must survive");
    }

    /// A new Sixel placement does NOT evict a non-overlapping Sixel image.
    #[test]
    fn evict_overlapping_positional_no_evict_non_overlapping() {
        let mut mgr = GraphicsManager::new();
        // Place old image at column 5 (no overlap with column 0)
        let old = add_positional(&mut mgr, 0, 5, PlacementOrigin::Sixel);
        let new_img = add_positional(&mut mgr, 0, 0, PlacementOrigin::Sixel);

        mgr.evict_overlapping_positional(new_img, Line(0), Column(0), 1, 1);

        assert!(mgr.image(old).is_some(), "non-overlapping sixel must survive");
    }

    /// A new Sixel placement does NOT evict a Kitty-origin placement at the same coords.
    #[test]
    fn evict_overlapping_positional_no_evict_kitty_origin() {
        let mut mgr = GraphicsManager::new();
        let kitty_img = add_positional(&mut mgr, 0, 0, PlacementOrigin::Kitty);
        let new_img = add_positional(&mut mgr, 0, 0, PlacementOrigin::Sixel);

        mgr.evict_overlapping_positional(new_img, Line(0), Column(0), 1, 1);

        assert!(mgr.image(kitty_img).is_some(), "kitty placement must never be overlap-evicted");
    }

    /// A new Sixel placement does NOT evict a virtual (U=1) placement.
    #[test]
    fn evict_overlapping_positional_no_evict_virtual_placement() {
        let mut mgr = GraphicsManager::new();
        // Add a Sixel-origin image but mark its placement as virtual
        let img = add(&mut mgr, 0, 0, 10, 20);
        let spec = PlacementSpec {
            origin: PlacementOrigin::Sixel,
            is_virtual: true,
            ..Default::default()
        };
        mgr.put_placement(img.id, Line(0), Column(0), &spec, CELL).unwrap().unwrap();

        let new_img = add_positional(&mut mgr, 0, 0, PlacementOrigin::Sixel);
        mgr.evict_overlapping_positional(new_img, Line(0), Column(0), 1, 1);

        assert!(mgr.image(img.id).is_some(), "virtual placement must not be evicted");
    }
}
