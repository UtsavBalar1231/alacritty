//! Exports the `Term` type which is a high-level API for the Grid.

use std::ops::{Index, IndexMut, Range};
use std::sync::Arc;
use std::{cmp, mem, ptr, slice, str};

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as Base64;
use bitflags::bitflags;
use log::{debug, trace};
use unicode_width::UnicodeWidthChar;

use crate::event::{Event, EventListener};
use crate::graphics::kitty_command::{CommandError, ErrorCode, GraphicsCommand};
use crate::graphics::placeholder::{
    IMAGE_PLACEHOLDER_CHAR, PlaceholderCellData, PlaceholderRun, color_to_id, diacritic_to_num,
    fit_to_box, scan_placeholder_cells,
};
use crate::graphics::response::{self, ResponseEcho};
use crate::graphics::sixel::DcsBuilder;
use crate::graphics::transmission::TransmissionResult;
use crate::graphics::{
    self as graphics, AnimationControlArgs, ApcBuilder, CellRect, CellSize, ComposeFrameArgs,
    GraphicsManager, GraphicsOptions, ImageId, ImageRenderItem, PlacementSpec, RenderSnapshot,
    UvRect, ZBucket,
};
use crate::grid::{Dimensions, Grid, GridIterator, Scroll};
use crate::index::{self, Boundary, Column, Direction, Line, Point, Side};
use crate::selection::{Selection, SelectionRange, SelectionType};
use crate::term::cell::{Cell, Flags, LineLength};
use crate::term::color::Colors;
use crate::vi_mode::{ViModeCursor, ViMotion};
use crate::vte::Params;
use crate::vte::ansi::{
    self, Attr, CharsetIndex, Color, CursorShape, CursorStyle, Handler, Hyperlink, KeyboardModes,
    KeyboardModesApplyBehavior, NamedColor, NamedMode, NamedPrivateMode, PrivateMode, Rgb,
    StandardCharset,
};

pub mod cell;
pub mod color;
pub mod search;

/// Minimum number of columns.
///
/// A minimum of 2 is necessary to hold fullwidth unicode characters.
pub const MIN_COLUMNS: usize = 2;

/// Minimum number of visible lines.
pub const MIN_SCREEN_LINES: usize = 1;

/// Max size of the window title stack.
const TITLE_STACK_MAX_DEPTH: usize = 4096;

/// Default semantic escape characters.
pub const SEMANTIC_ESCAPE_CHARS: &str = ",│`|:\"' ()[]{}<>\t";

/// Max size of the keyboard modes.
const KEYBOARD_MODE_STACK_MAX_DEPTH: usize = TITLE_STACK_MAX_DEPTH;

/// Default tab interval, corresponding to terminfo `it` value.
const INITIAL_TABSTOPS: usize = 8;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct TermMode: u32 {
        const NONE                    = 0;
        const SHOW_CURSOR             = 1;
        const APP_CURSOR              = 1 << 1;
        const APP_KEYPAD              = 1 << 2;
        const MOUSE_REPORT_CLICK      = 1 << 3;
        const BRACKETED_PASTE         = 1 << 4;
        const SGR_MOUSE               = 1 << 5;
        const MOUSE_MOTION            = 1 << 6;
        const LINE_WRAP               = 1 << 7;
        const LINE_FEED_NEW_LINE      = 1 << 8;
        const ORIGIN                  = 1 << 9;
        const INSERT                  = 1 << 10;
        const FOCUS_IN_OUT            = 1 << 11;
        const ALT_SCREEN              = 1 << 12;
        const MOUSE_DRAG              = 1 << 13;
        const UTF8_MOUSE              = 1 << 14;
        const ALTERNATE_SCROLL        = 1 << 15;
        const VI                      = 1 << 16;
        const URGENCY_HINTS           = 1 << 17;
        const DISAMBIGUATE_ESC_CODES  = 1 << 18;
        const REPORT_EVENT_TYPES      = 1 << 19;
        const REPORT_ALTERNATE_KEYS   = 1 << 20;
        const REPORT_ALL_KEYS_AS_ESC  = 1 << 21;
        const REPORT_ASSOCIATED_TEXT  = 1 << 22;
        /// DECSDM (private mode 80): set = sixel image anchored at origin (no scroll);
        /// reset (default) = sixel scrolls with cursor.
        const SIXEL_DISPLAY           = 1 << 23;
        /// Private mode 1070: reset = shared palette across DCS sequences;
        /// set (default) = private palette per DCS sequence.
        const SIXEL_PRIV_PALETTE      = 1 << 24;
        /// Private mode 8452: set = cursor moves to right of image; reset = below.
        const SIXEL_CURSOR_TO_RIGHT   = 1 << 25;
        const MOUSE_MODE              = Self::MOUSE_REPORT_CLICK.bits() | Self::MOUSE_MOTION.bits() | Self::MOUSE_DRAG.bits();
        const KITTY_KEYBOARD_PROTOCOL = Self::DISAMBIGUATE_ESC_CODES.bits()
                                      | Self::REPORT_EVENT_TYPES.bits()
                                      | Self::REPORT_ALTERNATE_KEYS.bits()
                                      | Self::REPORT_ALL_KEYS_AS_ESC.bits()
                                      | Self::REPORT_ASSOCIATED_TEXT.bits();
         const ANY                    = u32::MAX;
    }
}

impl From<KeyboardModes> for TermMode {
    fn from(value: KeyboardModes) -> Self {
        let mut mode = Self::empty();

        let disambiguate_esc_codes = value.contains(KeyboardModes::DISAMBIGUATE_ESC_CODES);
        mode.set(TermMode::DISAMBIGUATE_ESC_CODES, disambiguate_esc_codes);

        let report_event_types = value.contains(KeyboardModes::REPORT_EVENT_TYPES);
        mode.set(TermMode::REPORT_EVENT_TYPES, report_event_types);

        let report_alternate_keys = value.contains(KeyboardModes::REPORT_ALTERNATE_KEYS);
        mode.set(TermMode::REPORT_ALTERNATE_KEYS, report_alternate_keys);

        let report_all_keys_as_esc = value.contains(KeyboardModes::REPORT_ALL_KEYS_AS_ESC);
        mode.set(TermMode::REPORT_ALL_KEYS_AS_ESC, report_all_keys_as_esc);

        let report_associated_text = value.contains(KeyboardModes::REPORT_ASSOCIATED_TEXT);
        mode.set(TermMode::REPORT_ASSOCIATED_TEXT, report_associated_text);

        mode
    }
}

impl Default for TermMode {
    fn default() -> TermMode {
        TermMode::SHOW_CURSOR
            | TermMode::LINE_WRAP
            | TermMode::ALTERNATE_SCROLL
            | TermMode::URGENCY_HINTS
            | TermMode::SIXEL_PRIV_PALETTE
    }
}

/// Convert a terminal point to a viewport relative point.
#[inline]
pub fn point_to_viewport(display_offset: usize, point: Point) -> Option<Point<usize>> {
    let viewport_line = point.line.0 + display_offset as i32;
    usize::try_from(viewport_line).ok().map(|line| Point::new(line, point.column))
}

/// Convert a viewport relative point to a terminal point.
#[inline]
pub fn viewport_to_point(display_offset: usize, point: Point<usize>) -> Point {
    let line = Line(point.line as i32) - display_offset;
    Point::new(line, point.column)
}

/// Clamp a classic image render item to the visible viewport `[0, screen_lines)`,
/// cropping whole rows off either edge and advancing the source UV proportionally.
///
/// Returns `false` when the item is fully outside the viewport (caller drops it).
/// `dest.line` is viewport-relative here (display_offset already applied). A
/// top-straddling item (`dest.line < 0`) MUST be cropped rather than drawn,
/// because the renderer has no scissor and `gl::Viewport` spans the whole window,
/// so a negative top-origin quad would overdraw the tab bar / padding above the
/// grid. Whole-row cropping mirrors the margin-scroll path's approximation
/// (subcell `cell_y_offset` is dropped on a top crop).
fn crop_item_to_viewport(item: &mut ImageRenderItem, screen_lines: i32) -> bool {
    let rows = item.dest.num_rows as i32;
    if rows <= 0 {
        return false;
    }
    let top = item.dest.line.0;
    let bottom = top + rows;

    // Fully above or below the viewport: cull.
    if bottom <= 0 || top >= screen_lines {
        return false;
    }

    // Source UV span per destination row (constant across the item).
    let v_per_row = (item.src_uv.v1 - item.src_uv.v0) / rows as f32;

    // Crop rows that fall above the viewport top.
    if top < 0 {
        let clipped = (-top) as u32;
        item.src_uv.v0 += v_per_row * clipped as f32;
        item.dest.num_rows -= clipped;
        item.dest.line = Line(0);
        item.dest.cell_y_offset = 0;
    }

    // Crop rows that fall below the viewport bottom.
    let new_bottom = item.dest.line.0 + item.dest.num_rows as i32;
    if new_bottom > screen_lines {
        let clipped = (new_bottom - screen_lines) as u32;
        item.src_uv.v1 -= v_per_row * clipped as f32;
        item.dest.num_rows -= clipped;
    }

    item.dest.num_rows > 0
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineDamageBounds {
    /// Damaged line number.
    pub line: usize,

    /// Leftmost damaged column.
    pub left: usize,

    /// Rightmost damaged column.
    pub right: usize,
}

impl LineDamageBounds {
    #[inline]
    pub fn new(line: usize, left: usize, right: usize) -> Self {
        Self { line, left, right }
    }

    #[inline]
    pub fn undamaged(line: usize, num_cols: usize) -> Self {
        Self { line, left: num_cols, right: 0 }
    }

    #[inline]
    pub fn reset(&mut self, num_cols: usize) {
        *self = Self::undamaged(self.line, num_cols);
    }

    #[inline]
    pub fn expand(&mut self, left: usize, right: usize) {
        self.left = cmp::min(self.left, left);
        self.right = cmp::max(self.right, right);
    }

    #[inline]
    pub fn is_damaged(&self) -> bool {
        self.left <= self.right
    }
}

/// Terminal damage information collected since the last [`Term::reset_damage`] call.
#[derive(Debug)]
pub enum TermDamage<'a> {
    /// The entire terminal is damaged.
    Full,

    /// Iterator over damaged lines in the terminal.
    Partial(TermDamageIterator<'a>),
}

/// Iterator over the terminal's viewport damaged lines.
#[derive(Clone, Debug)]
pub struct TermDamageIterator<'a> {
    line_damage: slice::Iter<'a, LineDamageBounds>,
    display_offset: usize,
}

impl<'a> TermDamageIterator<'a> {
    pub fn new(line_damage: &'a [LineDamageBounds], display_offset: usize) -> Self {
        let num_lines = line_damage.len();
        // Filter out invisible damage.
        let line_damage = &line_damage[..num_lines.saturating_sub(display_offset)];
        Self { display_offset, line_damage: line_damage.iter() }
    }
}

impl Iterator for TermDamageIterator<'_> {
    type Item = LineDamageBounds;

    fn next(&mut self) -> Option<Self::Item> {
        self.line_damage.find_map(|line| {
            line.is_damaged().then_some(LineDamageBounds::new(
                line.line + self.display_offset,
                line.left,
                line.right,
            ))
        })
    }
}

/// State of the terminal damage.
struct TermDamageState {
    /// Hint whether terminal should be damaged entirely regardless of the actual damage changes.
    full: bool,

    /// Information about damage on terminal lines.
    lines: Vec<LineDamageBounds>,

    /// Old terminal cursor point.
    last_cursor: Point,
}

impl TermDamageState {
    fn new(num_cols: usize, num_lines: usize) -> Self {
        let lines =
            (0..num_lines).map(|line| LineDamageBounds::undamaged(line, num_cols)).collect();

        Self { full: true, lines, last_cursor: Default::default() }
    }

    #[inline]
    fn resize(&mut self, num_cols: usize, num_lines: usize) {
        // Reset point, so old cursor won't end up outside of the viewport.
        self.last_cursor = Default::default();
        self.full = true;

        self.lines.clear();
        self.lines.reserve(num_lines);
        for line in 0..num_lines {
            self.lines.push(LineDamageBounds::undamaged(line, num_cols));
        }
    }

    /// Damage point inside of the viewport.
    #[inline]
    fn damage_point(&mut self, point: Point<usize>) {
        self.damage_line(point.line, point.column.0, point.column.0);
    }

    /// Expand `line`'s damage to span at least `left` to `right` column.
    #[inline]
    fn damage_line(&mut self, line: usize, left: usize, right: usize) {
        self.lines[line].expand(left, right);
    }

    /// Reset information about terminal damage.
    fn reset(&mut self, num_cols: usize) {
        self.full = false;
        self.lines.iter_mut().for_each(|line| line.reset(num_cols));
    }
}

pub struct Term<T> {
    /// Terminal focus controlling the cursor shape.
    pub is_focused: bool,

    /// Cursor for keyboard selection.
    pub vi_mode_cursor: ViModeCursor,

    pub selection: Option<Selection>,

    /// Currently active grid.
    ///
    /// Tracks the screen buffer currently in use. While the alternate screen buffer is active,
    /// this will be the alternate grid. Otherwise it is the primary screen buffer.
    grid: Grid<Cell>,

    /// Currently inactive grid.
    ///
    /// Opposite of the active grid. While the alternate screen buffer is active, this will be the
    /// primary grid. Otherwise it is the alternate screen buffer.
    inactive_grid: Grid<Cell>,

    /// Index into `charsets`, pointing to what ASCII is currently being mapped to.
    active_charset: CharsetIndex,

    /// Tabstops.
    tabs: TabStops,

    /// Mode flags.
    mode: TermMode,

    /// Scroll region.
    ///
    /// Range going from top to bottom of the terminal, indexed from the top of the viewport.
    scroll_region: Range<Line>,

    /// Modified terminal colors.
    colors: Colors,

    /// Current style of the cursor.
    cursor_style: Option<CursorStyle>,

    /// Proxy for sending events to the event loop.
    event_proxy: T,

    /// Current title of the window.
    title: Option<String>,

    /// Stack of saved window titles. When a title is popped from this stack, the `title` for the
    /// term is set.
    title_stack: Vec<Option<String>>,

    /// The stack for the keyboard modes.
    keyboard_mode_stack: Vec<KeyboardModes>,

    /// Currently inactive keyboard mode stack.
    inactive_keyboard_mode_stack: Vec<KeyboardModes>,

    /// Information about damaged cells.
    damage: TermDamageState,

    /// Graphics attached to the currently active grid.
    graphics: GraphicsManager,

    /// Graphics attached to the currently inactive grid.
    ///
    /// Swapped together with the grids (kitty's `main_grman`/`alt_grman`).
    inactive_graphics: GraphicsManager,

    /// Accumulator for in-flight APC sequences (kitty graphics commands).
    apc_builder: ApcBuilder,

    /// Accumulator for in-flight Sixel DCS sequences.
    dcs_builder: DcsBuilder,

    /// Accumulator for iTerm2 multipart image transfers.
    iterm_multipart: crate::graphics::iterm::MultipartBuffer,

    /// Shared palette for Sixel mode-1070 palette sharing (mode 1070 reset = shared).
    sixel_shared_palette: Option<Vec<crate::vte::ansi::Rgb>>,

    /// Active color-register count for XTSMGRAPHICS Pi=1 queries.
    sixel_color_registers: u32,

    /// Cell dimensions in pixels, used for graphics placement math.
    graphics_cell_size: CellSize,

    /// Config directly for the terminal.
    config: Config,
}

/// Configuration options for the [`Term`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The maximum amount of scrolling history.
    pub scrolling_history: usize,

    /// Default cursor style to reset the cursor to.
    pub default_cursor_style: CursorStyle,

    /// Cursor style for Vi mode.
    pub vi_mode_cursor_style: Option<CursorStyle>,

    /// The characters which terminate semantic selection.
    ///
    /// The default value is [`SEMANTIC_ESCAPE_CHARS`].
    pub semantic_escape_chars: String,

    /// Whether to enable kitty keyboard protocol.
    pub kitty_keyboard: bool,

    /// OSC52 support mode.
    pub osc52: Osc52,

    /// Terminal graphics protocol options.
    pub graphics: GraphicsOptions,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            scrolling_history: 10000,
            semantic_escape_chars: SEMANTIC_ESCAPE_CHARS.to_owned(),
            default_cursor_style: Default::default(),
            vi_mode_cursor_style: Default::default(),
            kitty_keyboard: Default::default(),
            osc52: Default::default(),
            graphics: Default::default(),
        }
    }
}

/// OSC 52 behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize), serde(rename_all = "lowercase"))]
pub enum Osc52 {
    /// The handling of the escape sequence is disabled.
    Disabled,
    /// Only copy sequence is accepted.
    ///
    /// This option is the default as a compromise between entirely
    /// disabling it (the most secure) and allowing `paste` (the less secure).
    #[default]
    OnlyCopy,
    /// Only paste sequence is accepted.
    OnlyPaste,
    /// Both are accepted.
    CopyPaste,
}

impl<T> Term<T> {
    #[inline]
    pub fn scroll_display(&mut self, scroll: Scroll)
    where
        T: EventListener,
    {
        let old_display_offset = self.grid.display_offset();
        self.grid.scroll_display(scroll);
        self.event_proxy.send_event(Event::MouseCursorDirty);

        // Clamp vi mode cursor to the viewport.
        let viewport_start = -(self.grid.display_offset() as i32);
        let viewport_end = viewport_start + self.bottommost_line().0;
        let vi_cursor_line = &mut self.vi_mode_cursor.point.line.0;
        *vi_cursor_line = cmp::min(viewport_end, cmp::max(viewport_start, *vi_cursor_line));
        self.vi_mode_recompute_selection();

        // Damage everything if display offset changed.
        if old_display_offset != self.grid().display_offset() {
            self.mark_fully_damaged();
        }
    }

    pub fn new<D: Dimensions>(config: Config, dimensions: &D, event_proxy: T) -> Term<T> {
        let num_cols = dimensions.columns();
        let num_lines = dimensions.screen_lines();

        let history_size = config.scrolling_history;
        let grid = Grid::new(num_lines, num_cols, history_size);
        let inactive_grid = Grid::new(num_lines, num_cols, 0);

        let tabs = TabStops::new(grid.columns());

        let scroll_region = Line(0)..Line(grid.screen_lines() as i32);

        // Initialize terminal damage, covering the entire terminal upon launch.
        let damage = TermDamageState::new(num_cols, num_lines);

        let max_storage = config.graphics.max_storage;

        Term {
            inactive_grid,
            scroll_region,
            event_proxy,
            damage,
            config,
            grid,
            tabs,
            inactive_keyboard_mode_stack: Default::default(),
            keyboard_mode_stack: Default::default(),
            active_charset: Default::default(),
            vi_mode_cursor: Default::default(),
            cursor_style: Default::default(),
            colors: color::Colors::default(),
            title_stack: Default::default(),
            is_focused: Default::default(),
            selection: Default::default(),
            title: Default::default(),
            mode: Default::default(),
            graphics: GraphicsManager::with_storage_limit(max_storage),
            inactive_graphics: GraphicsManager::with_storage_limit(max_storage),
            apc_builder: Default::default(),
            dcs_builder: Default::default(),
            iterm_multipart: Default::default(),
            sixel_shared_palette: None,
            sixel_color_registers: crate::graphics::sixel::MAX_COLOR_REGISTERS as u32,
            // Placeholder until the display layer reports real cell metrics.
            graphics_cell_size: CellSize { width: 10, height: 20 },
        }
    }

    /// Collect the information about the changes in the lines, which
    /// could be used to minimize the amount of drawing operations.
    ///
    /// The user controlled elements, like `Vi` mode cursor and `Selection` are **not** part of the
    /// collected damage state. Those could easily be tracked by comparing their old and new
    /// value between adjacent frames.
    ///
    /// After reading damage [`reset_damage`] should be called.
    ///
    /// [`reset_damage`]: Self::reset_damage
    #[must_use]
    pub fn damage(&mut self) -> TermDamage<'_> {
        // Ensure the entire terminal is damaged after entering insert mode.
        // Leaving is handled in the ansi handler.
        if self.mode.contains(TermMode::INSERT) {
            self.mark_fully_damaged();
        }

        let previous_cursor = mem::replace(&mut self.damage.last_cursor, self.grid.cursor.point);

        if self.damage.full {
            return TermDamage::Full;
        }

        // Add information about old cursor position and new one if they are not the same, so we
        // cover everything that was produced by `Term::input`.
        if self.damage.last_cursor != previous_cursor {
            // Cursor coordinates are always inside viewport even if you have `display_offset`.
            let point = Point::new(previous_cursor.line.0 as usize, previous_cursor.column);
            self.damage.damage_point(point);
        }

        // Always damage current cursor.
        self.damage_cursor();

        // NOTE: damage which changes all the content when the display offset is non-zero (e.g.
        // scrolling) is handled via full damage.
        let display_offset = self.grid().display_offset();
        TermDamage::Partial(TermDamageIterator::new(&self.damage.lines, display_offset))
    }

    /// Resets the terminal damage information.
    pub fn reset_damage(&mut self) {
        self.damage.reset(self.columns());
    }

    #[inline]
    fn mark_fully_damaged(&mut self) {
        self.damage.full = true;
    }

    /// Set new options for the [`Term`].
    pub fn set_options(&mut self, options: Config)
    where
        T: EventListener,
    {
        let old_config = mem::replace(&mut self.config, options);

        let title_event = match &self.title {
            Some(title) => Event::Title(title.clone()),
            None => Event::ResetTitle,
        };

        self.event_proxy.send_event(title_event);

        if self.mode.contains(TermMode::ALT_SCREEN) {
            self.inactive_grid.update_history(self.config.scrolling_history);
        } else {
            self.grid.update_history(self.config.scrolling_history);
        }

        if self.config.kitty_keyboard != old_config.kitty_keyboard {
            self.keyboard_mode_stack = Vec::new();
            self.inactive_keyboard_mode_stack = Vec::new();
            self.mode.remove(TermMode::KITTY_KEYBOARD_PROTOCOL);
        }

        if self.config.graphics != old_config.graphics {
            // Disabling graphics drops all images immediately: in-flight
            // loads are aborted and both managers are emptied, enqueueing
            // GPU texture deletes for the renderer to drain.
            if !self.config.graphics.kitty_enabled() && old_config.graphics.kitty_enabled() {
                self.graphics.abort_load();
                self.inactive_graphics.abort_load();
                self.graphics.clear(true);
                self.inactive_graphics.clear(true);
            }

            // Apply the (possibly changed) storage quota, evicting if over.
            self.graphics.set_storage_limit(self.config.graphics.max_storage);
            self.inactive_graphics.set_storage_limit(self.config.graphics.max_storage);
        }

        // Damage everything on config updates.
        self.mark_fully_damaged();
    }

    /// Convert the active selection to a String.
    pub fn selection_to_string(&self) -> Option<String> {
        let selection_range = self.selection.as_ref().and_then(|s| s.to_range(self))?;
        let SelectionRange { start, end, .. } = selection_range;

        let mut res = String::new();

        match self.selection.as_ref() {
            Some(Selection { ty: SelectionType::Block, .. }) => {
                for line in (start.line.0..end.line.0).map(Line::from) {
                    res += self
                        .line_to_string(line, start.column..end.column, start.column.0 != 0)
                        .trim_end();
                    res += "\n";
                }

                res += self.line_to_string(end.line, start.column..end.column, true).trim_end();
            },
            Some(Selection { ty: SelectionType::Lines, .. }) => {
                res = self.bounds_to_string(start, end) + "\n";
            },
            _ => {
                res = self.bounds_to_string(start, end);
            },
        }

        Some(res)
    }

    /// Convert range between two points to a String.
    pub fn bounds_to_string(&self, start: Point, end: Point) -> String {
        let mut res = String::new();

        for line in (start.line.0..=end.line.0).map(Line::from) {
            let start_col = if line == start.line { start.column } else { Column(0) };
            let end_col = if line == end.line { end.column } else { self.last_column() };

            res += &self.line_to_string(line, start_col..end_col, line == end.line);
        }

        res.strip_suffix('\n').map(str::to_owned).unwrap_or(res)
    }

    /// Convert a single line in the grid to a String.
    fn line_to_string(
        &self,
        line: Line,
        mut cols: Range<Column>,
        include_wrapped_wide: bool,
    ) -> String {
        let mut text = String::new();

        let grid_line = &self.grid[line];
        let line_length = cmp::min(grid_line.line_length(), cols.end + 1);

        // Include wide char when trailing spacer is selected.
        if grid_line[cols.start].flags.contains(Flags::WIDE_CHAR_SPACER) {
            cols.start -= 1;
        }

        let mut tab_mode = false;
        for column in (cols.start.0..line_length.0).map(Column::from) {
            let cell = &grid_line[column];

            // Skip over cells until next tab-stop once a tab was found.
            if tab_mode {
                if self.tabs[column] || cell.c != ' ' {
                    tab_mode = false;
                } else {
                    continue;
                }
            }

            if cell.c == '\t' {
                tab_mode = true;
            }

            if !cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                // Push cells primary character.
                text.push(cell.c);

                // Push zero-width characters.
                for c in cell.zerowidth().into_iter().flatten() {
                    text.push(*c);
                }
            }
        }

        if cols.end >= self.columns() - 1
            && (line_length.0 == 0
                || !self.grid[line][line_length - 1].flags.contains(Flags::WRAPLINE))
        {
            text.push('\n');
        }

        // If wide char is not part of the selection, but leading spacer is, include it.
        if line_length == self.columns()
            && line_length.0 >= 2
            && grid_line[line_length - 1].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER)
            && include_wrapped_wide
        {
            text.push(self.grid[line - 1i32][Column(0)].c);
        }

        text
    }

    /// Terminal content required for rendering.
    #[inline]
    pub fn renderable_content(&self) -> RenderableContent<'_>
    where
        T: EventListener,
    {
        RenderableContent::new(self)
    }

    /// Access to the raw grid data structure.
    pub fn grid(&self) -> &Grid<Cell> {
        &self.grid
    }

    /// Mutable access to the raw grid data structure.
    pub fn grid_mut(&mut self) -> &mut Grid<Cell> {
        &mut self.grid
    }

    /// Resize terminal to new dimensions.
    pub fn resize<S: Dimensions>(&mut self, size: S) {
        let old_cols = self.columns();
        let old_lines = self.screen_lines();

        let num_cols = size.columns();
        let num_lines = size.screen_lines();

        if old_cols == num_cols && old_lines == num_lines {
            debug!("Term::resize dimensions unchanged");
            return;
        }

        debug!("New num_cols is {num_cols} and num_lines is {num_lines}");

        // Move vi mode cursor with the content.
        let history_size = self.history_size();
        let mut delta = num_lines as i32 - old_lines as i32;
        let min_delta = cmp::min(0, num_lines as i32 - self.grid.cursor.point.line.0 - 1);
        delta = cmp::min(cmp::max(delta, min_delta), history_size as i32);
        self.vi_mode_cursor.point.line += delta;

        // Content shift of the inactive grid, computed like `delta` above but
        // from the inactive grid's own cursor and history (the shift `Grid::
        // resize` will apply to its content).
        let inactive_delta = {
            let history_size = self.inactive_grid.history_size() as i32;
            let cursor_line = self.inactive_grid.cursor.point.line.0;
            let raw_delta = num_lines as i32 - old_lines as i32;
            let min_delta = cmp::min(0, num_lines as i32 - cursor_line - 1);
            cmp::min(cmp::max(raw_delta, min_delta), history_size)
        };

        let is_alt = self.mode.contains(TermMode::ALT_SCREEN);
        self.grid.resize(!is_alt, num_lines, num_cols);
        self.inactive_grid.resize(is_alt, num_lines, num_cols);

        // Invalidate selection and tabs only when necessary.
        if old_cols != num_cols {
            self.selection = None;

            // Recreate tabs list.
            self.tabs.resize(num_cols);
        } else if let Some(selection) = self.selection.take() {
            let max_lines = cmp::max(num_lines, old_lines) as i32;
            let range = Line(0)..Line(max_lines);
            self.selection = selection.rotate(self, &range, -delta);
        }

        // Graphics anchors move with the content like the vi cursor and
        // selection. Classic placements are SHIFTED (never dropped) on a
        // column-count reflow too, so `kitty +icat` images survive a resize and
        // keep tracking scrollback — matching kitty's grman_resize, which never
        // reflows and only shifts start_row (screen.c:572-575). `delta` always
        // applies to the active grid, `inactive_delta` to the inactive one; the
        // alt screen's `max_scroll_limit` is 0, so its images still GC at the
        // screen top (no scrollback there).
        let active_limit = self.grid.max_scroll_limit() as i32;
        let inactive_limit = self.inactive_grid.max_scroll_limit() as i32;
        self.graphics.shift_anchors(delta, active_limit);
        self.inactive_graphics.shift_anchors(inactive_delta, inactive_limit);

        // Clamp vi cursor to viewport.
        let vi_point = self.vi_mode_cursor.point;
        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let viewport_bottom = viewport_top + self.bottommost_line();
        self.vi_mode_cursor.point.line =
            cmp::max(cmp::min(vi_point.line, viewport_bottom), viewport_top);
        self.vi_mode_cursor.point.column = cmp::min(vi_point.column, self.last_column());

        // Reset scrolling region.
        self.scroll_region = Line(0)..Line(self.screen_lines() as i32);

        // Resize damage information.
        self.damage.resize(num_cols, num_lines);
    }

    /// Active terminal modes.
    #[inline]
    pub fn mode(&self) -> &TermMode {
        &self.mode
    }

    /// Graphics attached to the currently active grid.
    pub fn graphics(&self) -> &GraphicsManager {
        &self.graphics
    }

    /// Mutable access to the active grid's graphics.
    pub fn graphics_mut(&mut self) -> &mut GraphicsManager {
        &mut self.graphics
    }

    /// Update the cell dimensions used for graphics placement math.
    ///
    /// On a change (font size, DPR) all placements on both grids are rescaled
    /// like kitty's `grman_rescale` (screen.c:648-649): subcell offsets are
    /// clamped to the new cell and effective extents are recomputed. The UI
    /// layer must call this whenever the cell size changes.
    pub fn set_graphics_cell_size(&mut self, cell_size: CellSize) {
        if cell_size == self.graphics_cell_size {
            return;
        }
        self.graphics_cell_size = cell_size;

        let dirty = self.graphics.rescale(cell_size);
        let inactive_dirty = self.inactive_graphics.rescale(cell_size);
        if dirty || inactive_dirty {
            self.mark_fully_damaged();
        }
    }

    /// Produce a render snapshot for the active graphics manager.
    ///
    /// Calls GC, builds sorted `ImageRenderItem`s, and drains upload/delete queues.
    /// Must be called under the Term lock before `drop(terminal)` (display/mod.rs:841).
    /// `timestamp` is reserved for animation (Phase 8) and currently unused.
    pub fn render_snapshot(&mut self, timestamp: u64) -> RenderSnapshot {
        let mut snapshot = self.graphics.render_snapshot(timestamp);

        // Classic (non-virtual) placements come back with ABSOLUTE grid lines in
        // `dest.line`, but the renderer interprets `dest.line` as VIEWPORT-relative
        // (`py = line * cell_height`, top-origin, no display_offset added). Convert
        // grid -> viewport here by adding display_offset (matches `point_to_viewport`)
        // so a scrolled-back classic image (e.g. `kitty +icat`) tracks the scrollback
        // instead of staying pinned to a fixed screen row — the same "sticky image"
        // contract the placeholder scan already honors. Content scroll keeps classic
        // placements aligned via `placement.line += delta`, but VIEW scroll only
        // changes display_offset, which is applied right here. Placeholder items are
        // appended AFTER this shift because they are emitted viewport-relative already.
        let display_offset = self.grid.display_offset() as i32;
        if display_offset != 0 {
            for item in &mut snapshot.items {
                item.dest.line += display_offset;
            }
        }

        // Cull and crop classic items to the visible viewport `[0, screen_lines)`.
        // GC now retains placements that scrolled into history (so they re-render
        // when scrolled back), so a placement can sit fully or partially outside
        // the viewport. The renderer has no scissor and `gl::Viewport` spans the
        // whole window, so a top-straddling image (`dest.line < 0`) would otherwise
        // overdraw the tab bar/padding above the grid. Crop whole rows off either
        // edge, advancing the source UV proportionally; drop fully-offscreen items.
        // Placeholder items are appended AFTER this and are already
        // viewport-relative and box-bounded, so they are left untouched.
        let screen_lines = self.grid.screen_lines() as i32;
        snapshot.items.retain_mut(|item| crop_item_to_viewport(item, screen_lines));

        self.append_placeholder_items(&mut snapshot.items);
        snapshot
    }

    /// Scan visible rows flagged with `has_image_placeholders`, decode each
    /// placeholder cell, and append ephemeral cell-image render items to `out`.
    ///
    /// These items are recreated every call and never persisted.
    fn append_placeholder_items(&self, out: &mut Vec<ImageRenderItem>) {
        let cell = self.graphics_cell_size;
        let screen_lines = self.grid.screen_lines() as i32;
        let display_offset = self.grid.display_offset() as i32;

        for row_idx in 0..screen_lines {
            // The grid lookup needs the absolute, scrollback-aware line
            // (`row_idx - display_offset`), but `CellRect.dest.line` is
            // VIEWPORT-relative by contract (the renderer maps it straight to
            // `line * cell_height` with a top-origin viewport and no
            // display_offset). `row_idx` already IS the viewport-relative line,
            // so emit that for the dest — otherwise a scrolled-back image stays
            // pinned to a fixed screen row instead of tracking the scrollback
            // (the "sticky image" bug; classic placements track scroll because
            // their viewport-relative `line` is shifted by `delta` on scroll).
            let grid_line = Line(row_idx - display_offset);
            let row = &self.grid[grid_line];
            if !row.has_image_placeholders() {
                continue;
            }
            // `row_idx` IS the viewport-relative dest line; compute it only for
            // rows that actually carry a placeholder so the common text-only
            // scan row stays a flag-check no-op.
            let viewport_line = Line(row_idx);

            // Collect pre-decoded placeholder cell data for this row.
            let mut ph_cells: Vec<PlaceholderCellData> = Vec::new();
            for (col_idx, grid_cell) in row[..].iter().enumerate() {
                if grid_cell.c != IMAGE_PLACEHOLDER_CHAR {
                    continue;
                }
                let id_lo = color_to_id(grid_cell.fg);
                let placement_id = grid_cell.underline_color().map(color_to_id).unwrap_or(0);

                // Combining diacritics are stored in the zerowidth vec.
                let zw = grid_cell.zerowidth().unwrap_or(&[]);
                let img_row = zw.first().map(|&c| diacritic_to_num(c as u32)).unwrap_or(0);
                let img_col = zw.get(1).map(|&c| diacritic_to_num(c as u32)).unwrap_or(0);
                let id_hi = zw.get(2).map(|&c| diacritic_to_num(c as u32)).unwrap_or(0);

                ph_cells.push(PlaceholderCellData {
                    screen_col: col_idx as u32,
                    id_lo,
                    placement_id,
                    img_row,
                    img_col,
                    id_hi,
                });
            }

            if ph_cells.is_empty() {
                continue;
            }

            let runs: Vec<PlaceholderRun> = scan_placeholder_cells(&ph_cells);

            for run in runs {
                let img = match self.graphics.image_by_client_id(run.image_id) {
                    Some(img) => img,
                    None => continue,
                };

                // Find the virtual placement; if placement_id is 0, use the first one.
                let placement = if run.placement_id != 0 {
                    img.placements()
                        .iter()
                        .find(|p| p.is_virtual && p.client_id == run.placement_id)
                } else {
                    img.placements().iter().find(|p| p.is_virtual)
                };
                let placement = match placement {
                    Some(p) => p,
                    None => continue,
                };

                // Box dimensions: from virtual placement, or auto-compute.
                let box_cols = if placement.num_cols > 0 {
                    placement.num_cols
                } else {
                    img.width.div_ceil(cell.width)
                };
                let box_rows = if placement.num_rows > 0 {
                    placement.num_rows
                } else {
                    img.height.div_ceil(cell.height)
                };

                // Clamp the run to the live placement's box width. `scan_placeholder_cells`
                // groups consecutive placeholder cells by L-to-R image-column inheritance
                // and has NO knowledge of `box_cols`, so a STALE placeholder cell that
                // survives a delete-all-bypassing path (a cell-shifting DCH/ICH, an
                // ECH/clear that leaves the row flag set, or a narrower next preview whose
                // shared row keeps an old right-hand cell) can extend a fresh run past the
                // image's actual column span. Without this clamp the over-long run makes
                // `fit_to_box` sample a too-wide source rect and the emitted item paints the
                // image across unrelated text cells — the whole-row "smear" / TUI-corruption
                // facet. This is defense-in-depth at the render boundary: it neutralises the
                // width corruption from EVERY stale-cell source, not just the delete-all path
                // that `tear_down_placeholder_cells` covers. Skip the run if nothing of it
                // falls inside the box.
                let run_length = run.run_length.min(box_cols.saturating_sub(run.img_col_start));
                if run_length == 0 {
                    continue;
                }

                let fit = match fit_to_box(
                    img.width,
                    img.height,
                    box_cols,
                    box_rows,
                    cell.width,
                    cell.height,
                    run.img_col_start,
                    run.img_row,
                    run_length,
                ) {
                    Some(f) => f,
                    None => continue,
                };

                let iw = img.width as f32;
                let ih = img.height as f32;
                let src_uv = UvRect {
                    u0: fit.src_x as f32 / iw,
                    v0: fit.src_y as f32 / ih,
                    u1: (fit.src_x + fit.src_w) as f32 / iw,
                    v1: (fit.src_y + fit.src_h) as f32 / ih,
                };

                out.push(ImageRenderItem {
                    image_id: img.id(),
                    placement_id: placement.id(),
                    z_index: -1,
                    z_bucket: ZBucket::BetweenBgAndText,
                    src_uv,
                    dest: CellRect {
                        line: viewport_line,
                        column: Column(run.screen_col as usize),
                        num_cols: run_length,
                        num_rows: 1,
                        cell_x_offset: fit.cell_x_offset,
                        cell_y_offset: fit.cell_y_offset,
                    },
                    group_index: 0,
                });
            }
        }
    }

    /// Swap primary and alternate screen buffer.
    pub fn swap_alt(&mut self) {
        if !self.mode.contains(TermMode::ALT_SCREEN) {
            // Set alt screen cursor to the current primary screen cursor.
            self.inactive_grid.cursor = self.grid.cursor.clone();

            // Drop information about the primary screens saved cursor.
            self.grid.saved_cursor = self.grid.cursor.clone();

            // Reset alternate screen contents.
            self.inactive_grid.reset_region(..);

            // Entering the alt screen clears its graphics, mirroring kitty's
            // 1049 handling (screen.c:1629-1632); leaving does not clear.
            self.inactive_graphics.clear(true);
        }

        mem::swap(&mut self.keyboard_mode_stack, &mut self.inactive_keyboard_mode_stack);
        let keyboard_mode =
            self.keyboard_mode_stack.last().copied().unwrap_or(KeyboardModes::NO_MODE).into();
        self.set_keyboard_mode(keyboard_mode, KeyboardModesApplyBehavior::Replace);

        mem::swap(&mut self.grid, &mut self.inactive_grid);
        mem::swap(&mut self.graphics, &mut self.inactive_graphics);
        self.mode ^= TermMode::ALT_SCREEN;
        self.selection = None;
        self.mark_fully_damaged();
    }

    /// Scroll screen down.
    ///
    /// Text moves down; clear at bottom
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_down_relative(&mut self, origin: Line, mut lines: usize) {
        trace!("Scrolling down relative: origin={origin}, lines={lines}");

        lines = cmp::min(lines, (self.scroll_region.end - self.scroll_region.start).0 as usize);
        lines = cmp::min(lines, (self.scroll_region.end - origin).0 as usize);

        let region = origin..self.scroll_region.end;

        // Scroll selection.
        self.selection =
            self.selection.take().and_then(|s| s.rotate(self, &region, -(lines as i32)));

        // Scroll vi mode cursor.
        let line = &mut self.vi_mode_cursor.point.line;
        if region.start <= *line && region.end > *line {
            *line = cmp::min(*line + lines, region.end - 1);
        }

        // Scroll graphics placements (kitty grman_scroll_images).
        // Hoist the empty-images guard to the call site so the text-only scroll
        // hot path pays nothing (no function call, no screen_lines() setup) when
        // no graphics are registered. scroll() already no-ops when empty, so this
        // is behavior-identical.
        if !self.graphics.is_empty() {
            let screen_lines = self.screen_lines();
            // Only a region anchored at the screen top feeds scrollback; a
            // margin region (origin > 0) discards scrolled-out lines, so
            // placements pushed past the region top must be hard-deleted
            // (limit 0), mirroring Grid::scroll_up's history-save condition.
            let scrollback_limit =
                if region.start == Line(0) { self.grid.max_scroll_limit() as i32 } else { 0 };
            self.graphics.scroll(
                &region,
                lines as i32,
                screen_lines,
                self.graphics_cell_size,
                scrollback_limit,
            );
        }

        // Scroll between origin and bottom
        self.grid.scroll_down(&region, lines);
        self.mark_fully_damaged();
    }

    /// Scroll screen up
    ///
    /// Text moves up; clear at top
    /// Expects origin to be in scroll range.
    #[inline]
    fn scroll_up_relative(&mut self, origin: Line, mut lines: usize) {
        trace!("Scrolling up relative: origin={origin}, lines={lines}");

        lines = cmp::min(lines, (self.scroll_region.end - self.scroll_region.start).0 as usize);

        let region = origin..self.scroll_region.end;

        // Scroll selection.
        self.selection = self.selection.take().and_then(|s| s.rotate(self, &region, lines as i32));

        // Scroll graphics placements (kitty grman_scroll_images).
        // Hoist the empty-images guard to the call site so the text-only scroll
        // hot path pays nothing (no function call, no screen_lines() setup) when
        // no graphics are registered. scroll() already no-ops when empty, so this
        // is behavior-identical.
        if !self.graphics.is_empty() {
            let screen_lines = self.screen_lines();
            // Only a region anchored at the screen top feeds scrollback; a
            // margin region (origin > 0) discards scrolled-out lines, so
            // placements pushed past the region top must be hard-deleted
            // (limit 0), mirroring Grid::scroll_up's history-save condition.
            let scrollback_limit =
                if region.start == Line(0) { self.grid.max_scroll_limit() as i32 } else { 0 };
            self.graphics.scroll(
                &region,
                -(lines as i32),
                screen_lines,
                self.graphics_cell_size,
                scrollback_limit,
            );
        }

        self.grid.scroll_up(&region, lines);

        // Scroll vi mode cursor.
        let viewport_top = Line(-(self.grid.display_offset() as i32));
        let top = if region.start == 0 { viewport_top } else { region.start };
        let line = &mut self.vi_mode_cursor.point.line;
        if (top <= *line) && region.end > *line {
            *line = cmp::max(*line - lines, top);
        }
        self.mark_fully_damaged();
    }

    fn deccolm(&mut self)
    where
        T: EventListener,
    {
        // Setting 132 column font makes no sense, but run the other side effects.
        // Clear scrolling region.
        self.set_scrolling_region(1, None);

        // Clear grid.
        self.grid.reset_region(..);
        self.mark_fully_damaged();
    }

    #[inline]
    pub fn exit(&mut self)
    where
        T: EventListener,
    {
        self.event_proxy.send_event(Event::Exit);
    }

    /// Toggle the vi mode.
    #[inline]
    pub fn toggle_vi_mode(&mut self)
    where
        T: EventListener,
    {
        self.mode ^= TermMode::VI;

        if self.mode.contains(TermMode::VI) {
            let display_offset = self.grid.display_offset() as i32;
            if self.grid.cursor.point.line > self.bottommost_line() - display_offset {
                // Move cursor to top-left if terminal cursor is not visible.
                let point = Point::new(Line(-display_offset), Column(0));
                self.vi_mode_cursor = ViModeCursor::new(point);
            } else {
                // Reset vi mode cursor position to match primary cursor.
                self.vi_mode_cursor = ViModeCursor::new(self.grid.cursor.point);
            }
        }

        // Update UI about cursor blinking state changes.
        self.event_proxy.send_event(Event::CursorBlinkingChange);
    }

    /// Move vi mode cursor.
    #[inline]
    pub fn vi_motion(&mut self, motion: ViMotion)
    where
        T: EventListener,
    {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Move cursor.
        self.vi_mode_cursor = self.vi_mode_cursor.motion(self, motion);
        self.vi_mode_recompute_selection();
    }

    /// Move vi cursor to a point in the grid.
    #[inline]
    pub fn vi_goto_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        // Move viewport to make point visible.
        self.scroll_to_point(point);

        // Move vi cursor to the point.
        self.vi_mode_cursor.point = point;

        self.vi_mode_recompute_selection();
    }

    /// Update the active selection to match the vi mode cursor position.
    #[inline]
    fn vi_mode_recompute_selection(&mut self) {
        // Require vi mode to be active.
        if !self.mode.contains(TermMode::VI) {
            return;
        }

        // Update only if non-empty selection is present.
        if let Some(selection) = self.selection.as_mut().filter(|s| !s.is_empty()) {
            selection.update(self.vi_mode_cursor.point, Side::Left);
            selection.include_all();
        }
    }

    /// Scroll display to point if it is outside of viewport.
    pub fn scroll_to_point(&mut self, point: Point)
    where
        T: EventListener,
    {
        let display_offset = self.grid.display_offset() as i32;
        let screen_lines = self.grid.screen_lines() as i32;

        if point.line < -display_offset {
            let lines = point.line + display_offset;
            self.scroll_display(Scroll::Delta(-lines.0));
        } else if point.line >= (screen_lines - display_offset) {
            let lines = point.line + display_offset - screen_lines + 1i32;
            self.scroll_display(Scroll::Delta(-lines.0));
        }
    }

    /// Jump to the end of a wide cell.
    pub fn expand_wide(&self, mut point: Point, direction: Direction) -> Point {
        let flags = self.grid[point.line][point.column].flags;

        match direction {
            Direction::Right if flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) => {
                point.column = Column(1);
                point.line += 1;
            },
            Direction::Right if flags.contains(Flags::WIDE_CHAR) => {
                point.column = cmp::min(point.column + 1, self.last_column());
            },
            Direction::Left if flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) => {
                if flags.contains(Flags::WIDE_CHAR_SPACER) {
                    point.column -= 1;
                }

                let prev = point.sub(self, Boundary::Grid, 1);
                if self.grid[prev].flags.contains(Flags::LEADING_WIDE_CHAR_SPACER) {
                    point = prev;
                }
            },
            _ => (),
        }

        point
    }

    #[inline]
    pub fn semantic_escape_chars(&self) -> &str {
        &self.config.semantic_escape_chars
    }

    #[cfg(test)]
    pub(crate) fn set_semantic_escape_chars(&mut self, semantic_escape_chars: &str) {
        self.config.semantic_escape_chars = semantic_escape_chars.into();
    }

    /// Active terminal cursor style.
    ///
    /// While vi mode is active, this will automatically return the vi mode cursor style.
    #[inline]
    pub fn cursor_style(&self) -> CursorStyle {
        let cursor_style = self.cursor_style.unwrap_or(self.config.default_cursor_style);

        if self.mode.contains(TermMode::VI) {
            self.config.vi_mode_cursor_style.unwrap_or(cursor_style)
        } else {
            cursor_style
        }
    }

    pub fn colors(&self) -> &Colors {
        &self.colors
    }

    /// Insert a linebreak at the current cursor position.
    #[inline]
    fn wrapline(&mut self)
    where
        T: EventListener,
    {
        if !self.mode.contains(TermMode::LINE_WRAP) {
            return;
        }

        trace!("Wrapping input");

        self.grid.cursor_cell().flags.insert(Flags::WRAPLINE);

        if self.grid.cursor.point.line + 1 >= self.scroll_region.end {
            self.linefeed();
        } else {
            self.damage_cursor();
            self.grid.cursor.point.line += 1;
        }

        self.grid.cursor.point.column = Column(0);
        self.grid.cursor.input_needs_wrap = false;
        self.damage_cursor();
    }

    /// Write `c` to the cell at the cursor position.
    #[inline(always)]
    fn write_at_cursor(&mut self, c: char) {
        let c = self.grid.cursor.charsets[self.active_charset].map(c);
        let fg = self.grid.cursor.template.fg;
        let bg = self.grid.cursor.template.bg;
        let flags = self.grid.cursor.template.flags;
        let extra = self.grid.cursor.template.extra.clone();

        let mut cursor_cell = self.grid.cursor_cell();

        // Clear all related cells when overwriting a fullwidth cell.
        if cursor_cell.flags.intersects(Flags::WIDE_CHAR | Flags::WIDE_CHAR_SPACER) {
            // Remove wide char and spacer.
            let wide = cursor_cell.flags.contains(Flags::WIDE_CHAR);
            let point = self.grid.cursor.point;
            if wide && point.column < self.last_column() {
                self.grid[point.line][point.column + 1].flags.remove(Flags::WIDE_CHAR_SPACER);
            } else if point.column > 0 {
                self.grid[point.line][point.column - 1].clear_wide();
            }

            // Remove leading spacers.
            if point.column <= 1 && point.line != self.topmost_line() {
                let column = self.last_column();
                self.grid[point.line - 1i32][column].flags.remove(Flags::LEADING_WIDE_CHAR_SPACER);
            }

            cursor_cell = self.grid.cursor_cell();
        }

        cursor_cell.c = c;
        cursor_cell.fg = fg;
        cursor_cell.bg = bg;
        cursor_cell.flags = flags;
        cursor_cell.extra = extra;

        // Set the per-row image-placeholder flag when the placeholder character is written.
        // This keeps the render-snapshot scan near-zero cost for plain-text rows.
        if c == '\u{10EEEE}' {
            let line = self.grid.cursor.point.line;
            self.grid[line].set_image_placeholders(true);
        }
    }

    #[inline]
    fn damage_cursor(&mut self) {
        // The normal cursor coordinates are always in viewport.
        let point =
            Point::new(self.grid.cursor.point.line.0 as usize, self.grid.cursor.point.column);
        self.damage.damage_point(point);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, mode: TermMode, apply: KeyboardModesApplyBehavior) {
        let active_mode = self.mode & TermMode::KITTY_KEYBOARD_PROTOCOL;
        self.mode &= !TermMode::KITTY_KEYBOARD_PROTOCOL;
        let new_mode = match apply {
            KeyboardModesApplyBehavior::Replace => mode,
            KeyboardModesApplyBehavior::Union => active_mode.union(mode),
            KeyboardModesApplyBehavior::Difference => active_mode.difference(mode),
        };
        trace!("Setting keyboard mode to {new_mode:?}");
        self.mode |= new_mode;
    }
}

impl<T> Dimensions for Term<T> {
    #[inline]
    fn columns(&self) -> usize {
        self.grid.columns()
    }

    #[inline]
    fn screen_lines(&self) -> usize {
        self.grid.screen_lines()
    }

    #[inline]
    fn total_lines(&self) -> usize {
        self.grid.total_lines()
    }
}

/// Sixel DCS dispatch.
impl<T: EventListener> Term<T> {
    fn sixel_dcs_command(&mut self, payload: Vec<u8>, p2: u16, overflowed: bool) {
        if !self.config.graphics.sixel_enabled() {
            return;
        }

        if overflowed {
            debug!("Sixel DCS payload truncated, discarding");
            return;
        }

        // Mode 1070: SIXEL_PRIV_PALETTE set (default) → private palette per DCS;
        // reset → shared palette persists across DCS sequences.
        let shared_palette = if self.mode.contains(TermMode::SIXEL_PRIV_PALETTE) {
            None
        } else {
            Some(self.sixel_shared_palette.take().unwrap_or_default())
        };

        let transparent_bg = p2 == 1;
        let mut parser =
            crate::graphics::sixel::Parser::new_with_p2(transparent_bg, shared_palette);

        for byte in &payload {
            if let Err(e) = parser.put(*byte) {
                debug!("Sixel parse error: {e}");
                return;
            }
        }

        let priv_palette = self.mode.contains(TermMode::SIXEL_PRIV_PALETTE);
        let (width, height, rgba, palette) = match parser.finish() {
            Ok(v) => v,
            Err(e) => {
                debug!("Sixel finish error: {e}");
                return;
            },
        };

        if !priv_palette {
            self.sixel_shared_palette = Some(palette);
        }

        if width == 0 || height == 0 {
            return;
        }

        let added = self.graphics.add_image(0, 0, width, height, rgba);

        let cursor = self.grid.cursor.point;
        let spec =
            PlacementSpec { origin: crate::graphics::PlacementOrigin::Sixel, ..Default::default() };
        let extent = self
            .graphics
            .put_placement(added.id, cursor.line, cursor.column, &spec, self.graphics_cell_size)
            .ok()
            .flatten();

        // Evict prior overlapping Sixel/Iterm2 placements (yazi stacking bug fix).
        if let Some((columns, lines)) = extent {
            self.graphics.evict_overlapping_positional(
                added.id,
                cursor.line,
                cursor.column,
                columns,
                lines,
            );
        }

        if let Some((columns, lines)) = extent {
            let cursor_to_right = self.mode.contains(TermMode::SIXEL_CURSOR_TO_RIGHT);
            if cursor_to_right {
                let new_x = (cursor.column.0 + columns as usize).min(self.columns() - 1);
                self.grid.cursor.point = Point::new(Line(cursor.line.0), Column(new_x));
                self.grid.cursor.input_needs_wrap = false;
            } else {
                self.advance_cursor_after_image(cursor, columns, lines);
            }
        }

        self.mark_fully_damaged();
    }

    /// Decode and display one iTerm2 inline image.
    ///
    /// `args` carry the parsed `File=` arguments; `b64` is the raw base64
    /// payload.  Returns immediately (silent skip) on any error so that
    /// malformed or unsupported sequences never panic or produce garbage output.
    fn iterm_display(&mut self, args: crate::graphics::iterm::FileArgs, b64: &[u8]) {
        use crate::graphics::iterm::{CellMetrics, decode_png, resolve_dimensions};

        if !args.inline {
            // inline=0 means "download" — not implemented in v1.
            return;
        }

        let raw = match Base64.decode(b64) {
            Ok(v) => v,
            Err(e) => {
                debug!("[iterm2] base64 decode error: {e}");
                return;
            },
        };

        // v1: PNG only.  Non-PNG formats are silently skipped.
        let (img_w, img_h, rgba) = match decode_png(&raw) {
            Some(v) => v,
            None => {
                debug!("[iterm2] non-PNG or corrupt image data — skipping (v1 PNG-only)");
                return;
            },
        };

        if img_w == 0 || img_h == 0 {
            return;
        }

        let cell = self.graphics_cell_size;
        let metrics = CellMetrics {
            cell_w: cell.width,
            cell_h: cell.height,
            cols: self.columns() as u32,
            rows: self.screen_lines() as u32,
        };

        let (disp_w, disp_h) = resolve_dimensions(
            args.width,
            args.height,
            img_w,
            img_h,
            args.preserve_aspect_ratio,
            metrics,
        );

        if disp_w == 0 || disp_h == 0 {
            return;
        }

        let added = self.graphics.add_image(0, 0, img_w, img_h, rgba);

        let cursor = self.grid.cursor.point;
        let num_cols = disp_w.div_ceil(cell.width.max(1));
        let num_rows = disp_h.div_ceil(cell.height.max(1));
        let spec = PlacementSpec {
            num_cols,
            num_rows,
            origin: crate::graphics::PlacementOrigin::Iterm2,
            ..Default::default()
        };
        let extent = self
            .graphics
            .put_placement(added.id, cursor.line, cursor.column, &spec, self.graphics_cell_size)
            .ok()
            .flatten();

        // Evict prior overlapping Sixel/Iterm2 placements (yazi stacking bug fix).
        if let Some((columns, lines)) = extent {
            self.graphics.evict_overlapping_positional(
                added.id,
                cursor.line,
                cursor.column,
                columns,
                lines,
            );
        }

        // doNotMoveCursor=1: cursor stays put; no scroll on last line (WezTerm #3266 fix).
        if let Some((columns, lines)) = extent.filter(|_| !args.do_not_move_cursor) {
            self.advance_cursor_after_image(cursor, columns, lines);
        }

        self.mark_fully_damaged();
    }

    /// Advance the cursor after placing a non-kitty inline image (sixel non-right, iTerm2).
    ///
    /// Wraps `new_x` to the next row when it overflows, scrolls up in normal screen if the
    /// cursor would pass the bottom margin (in alt-screen, clamps instead — scrollback does not
    /// exist there and `scroll_up_relative` would clobber the TUI), then clamps both axes to
    /// the grid bounds. This matches foot/xterm behavior for tall images.
    fn advance_cursor_after_image(&mut self, cursor: Point<Line>, columns: u32, lines: u32) {
        let new_x = cursor.column.0 + columns as usize;
        let new_y = if lines > 0 { cursor.line.0 + lines as i32 - 1 } else { cursor.line.0 };
        let (new_x, new_y) =
            if new_x >= self.columns() { (0usize, new_y + 1) } else { (new_x, new_y) };
        let margin_bottom = self.scroll_region.end.0.saturating_sub(1);
        let new_y = if new_y > margin_bottom {
            // Alt-screen has no scrollback; clamping avoids clobbering the TUI.
            // Normal screen scrolls up so the cursor stays inside the margin.
            if !self.mode.contains(TermMode::ALT_SCREEN) {
                let overflow = (new_y - margin_bottom) as usize;
                self.scroll_up_relative(self.scroll_region.start, overflow);
            }
            margin_bottom
        } else {
            new_y
        };
        let new_x = new_x.min(self.columns() - 1);
        let new_y = new_y.max(0).min(self.screen_lines() as i32 - 1);
        self.grid.cursor.point = Point::new(Line(new_y), Column(new_x));
        self.grid.cursor.input_needs_wrap = false;
    }
}

/// Kitty graphics command dispatch.
///
/// Port of kitty's `grman_handle_command` (`kitty/graphics.c`). Commands
/// arrive as APC payloads via the `apc_start`/`apc_put`/`apc_end` handler
/// callbacks; responses are emitted synchronously through
/// [`Event::PtyWrite`], the same path DA1 uses, so a graphics query response
/// is structurally guaranteed to precede a later DA1 response.
impl<T: EventListener> Term<T> {
    /// Handle a complete `G`-prefixed APC sequence; `body` excludes the `G`.
    fn kitty_graphics_command(&mut self, body: &[u8], overflowed: bool) {
        // The APC sequence is consumed either way; with the protocol
        // disabled nothing is stored or placed, but commands still get an
        // error response so clients can detect the missing support.
        if !self.config.graphics.kitty_enabled() {
            self.graphics_disabled_response(body);
            return;
        }

        if overflowed {
            let error = CommandError {
                code: ErrorCode::EFBIG,
                message: "APC sequence too long".into(),
                sends_response: true,
            };
            self.send_graphics_response(&response::scan_echo_keys(body), Some(&error), false);
            return;
        }

        let cmd = match GraphicsCommand::parse(body) {
            Ok(cmd) => cmd,
            Err(error) => {
                // Kitty answers post-parse validation errors (i= plus I=) but
                // drops malformed control blocks silently.
                if error.sends_response {
                    let echo = response::scan_echo_keys(body);
                    self.send_graphics_response(&echo, Some(&error), false);
                } else {
                    debug!("Ignoring malformed graphics command: {error}");
                }
                return;
            },
        };

        match cmd.action {
            0 | b't' | b'T' | b'q' => self.graphics_add_command(cmd),
            b'p' => self.graphics_put_command(cmd),
            b'd' => self.graphics_delete_command(&cmd),
            b'f' => self.graphics_frame_command(cmd),
            b'c' => self.graphics_compose_command(cmd),
            b'a' => self.graphics_animation_command(cmd),
            // Kitty's unknown-action path: log, no response.
            _ => debug!("Ignoring unsupported graphics command action: {}", cmd.action as char),
        }
    }

    /// Handle `a=t`/`a=T`/`a=q` (and the implicit default action):
    /// transmission, optional display, and queries.
    fn graphics_add_command(&mut self, cmd: GraphicsCommand) {
        // Queries without an image id (I=-only, no i=) are rejected with a debug log
        // and no response — kitty logs REPORT_ERROR and breaks (graphics.c:2212).
        if cmd.action == b'q' && cmd.id == 0 {
            debug!("Ignoring graphics query without image id");
            return;
        }

        let chunk_quiet = cmd.quiet;
        let chunk_echo = ResponseEcho {
            id: cmd.id,
            image_number: cmd.image_number,
            placement_id: cmd.placement_id,
            quiet: cmd.quiet,
            num_lines: 0,
        };

        // Identity of an in-flight chunked load this command continues, if
        // any; continuation chunks usually carry no identity keys of their
        // own and errors must echo the saved first-chunk keys.
        let transmission_type =
            if cmd.transmission_type == 0 { b'd' } else { cmd.transmission_type };
        let loading_echo = if transmission_type == b'd' {
            self.graphics.loading().map(|load| ResponseEcho {
                id: load.start.id,
                image_number: load.start.image_number,
                placement_id: load.start.placement_id,
                quiet: load.start.quiet,
                num_lines: 0,
            })
        } else {
            None
        };

        let load = match self.graphics.handle_transmission(cmd) {
            Err(error) => {
                let mut echo = loading_echo.unwrap_or(chunk_echo);
                if chunk_quiet != 0 {
                    echo.quiet = chunk_quiet;
                }
                self.send_graphics_response(&echo, Some(&error), false);
                return;
            },
            // Chunk accepted mid-stream: no `OK` until the final chunk.
            Ok(TransmissionResult::MoreDataNeeded) => return,
            Ok(TransmissionResult::Complete(load)) => load,
        };

        // `start` is the saved first-chunk command; identity, action and
        // placement keys come from it (kitty semantics). A `q=` on the final
        // chunk overrides the saved suppression level (graphics.c:2216).
        let start = load.start;
        let quiet = if chunk_quiet != 0 { chunk_quiet } else { start.quiet };

        if start.action == b'q' {
            // Fully synchronous query: the data was validated and decoded,
            // but is dropped without storing or uploading anything. Kitty
            // echoes only `i=` for queries.
            let echo = ResponseEcho { id: start.id, quiet, ..Default::default() };
            self.send_graphics_response(&echo, None, true);
            return;
        }

        let added = self.graphics.add_image(
            start.id,
            start.image_number,
            load.width,
            load.height,
            load.data,
        );
        let echo = ResponseEcho {
            id: added.client_id,
            image_number: start.image_number,
            placement_id: start.placement_id,
            quiet,
            num_lines: 0,
        };

        // Kitty builds the response before the a=T display step, so
        // placement failures after a successful load are not reported.
        self.send_graphics_response(&echo, None, true);

        if start.action == b'T' {
            self.graphics_place(added.id, &start);
        }
    }

    /// Handle `a=p`: place a previously transmitted image at the cursor.
    fn graphics_put_command(&mut self, cmd: GraphicsCommand) {
        if cmd.id == 0 && cmd.image_number == 0 {
            debug!("Ignoring graphics put command without image id or number");
            return;
        }

        let mut echo = ResponseEcho {
            id: cmd.id,
            image_number: cmd.image_number,
            placement_id: cmd.placement_id,
            quiet: cmd.quiet,
            num_lines: 0,
        };

        let image = if cmd.id != 0 {
            self.graphics.image_by_client_id(cmd.id)
        } else {
            self.graphics.image_by_client_number(cmd.image_number)
        };

        match image.map(|image| (image.id(), image.client_id)) {
            None => {
                let error = CommandError {
                    code: ErrorCode::ENOENT,
                    message: format!(
                        "Put command refers to non-existent image with id: {} and number: {}",
                        cmd.id, cmd.image_number
                    ),
                    sends_response: true,
                };
                self.send_graphics_response(&echo, Some(&error), false);
            },
            Some((image_id, client_id)) => {
                // Kitty echoes the image's client id, also when the command
                // addressed it by number (graphics.c:2257).
                echo.id = client_id;
                if let Some(error) = self.graphics_place(image_id, &cmd) {
                    self.send_graphics_response(&echo, Some(&error), false);
                } else {
                    self.send_graphics_response(&echo, None, true);
                }
            },
        }
    }

    /// Handle `a=d`: delete placements and optionally free image data.
    ///
    /// Deletes never produce a response in kitty, success or failure
    /// (`grman_handle_command` leaves the response empty for `case 'd'`,
    /// graphics.c:2261-2263), and unknown specifiers are log-only
    /// (graphics.c:2155-2157); `f/F` are stubbed (Phase 8).
    fn graphics_delete_command(&mut self, cmd: &GraphicsCommand) {
        // d=c/C uses the cursor position. Kitty sets x/y from the cursor
        // before calling point_filter_func (graphics.c:2126-2129), so we
        // synthesise a modified command here rather than threading the cursor
        // into GraphicsManager.
        let patched;
        let effective_cmd = if cmd.delete_action == b'c' || cmd.delete_action == b'C' {
            let cur = self.grid.cursor.point;
            patched = GraphicsCommand {
                x_offset: cur.column.0 as u32 + 1,
                y_offset: cur.line.0 as u32 + 1,
                ..cmd.clone()
            };
            &patched
        } else {
            cmd
        };
        let result = self.graphics.handle_delete(effective_cmd);

        // On delete-all, tear down placeholder cells regardless of `handle_delete`
        // result: yazi's virtual placements (`U=1`) are excluded from `d=a`/`d=A`
        // (kitty parity, graphics/mod.rs:2229-2231), so the delete is a model
        // no-op, yet stale U+10EEEE cells must still be blanked to prevent them from
        // reviving against a reused client id on the next preview (the stacking bug).
        let cleared_placeholders = matches!(effective_cmd.delete_action, 0 | b'a' | b'A')
            && self.tear_down_placeholder_cells();

        match result {
            Some(true) => self.mark_fully_damaged(),
            Some(false) => {
                if cleared_placeholders {
                    self.mark_fully_damaged();
                }
            },
            None => debug!(
                "Ignoring unsupported graphics delete specifier: {}",
                cmd.delete_action as char
            ),
        }
    }

    /// Blank every Unicode image-placeholder (U+10EEEE) cell in the entire grid
    /// (viewport + scrollback) and clear the per-row placeholder flag.
    /// Returns `true` if any cell was torn down.
    ///
    /// Cells must be blanked, not just the flag: a narrower next preview re-flags a
    /// shared row while a stale right-hand cell survives, extending the scan run past
    /// the new box. History rows are included so stale cells in scrollback cannot
    /// resurface on scroll-up. Scoped to delete-all; targeted deletes (`d=i`/`d=I`)
    /// never call this.
    fn tear_down_placeholder_cells(&mut self) -> bool {
        let top = self.grid.topmost_line().0;
        let bottom = self.grid.bottommost_line().0;
        let mut cleared = false;
        for line in top..=bottom {
            let row = &mut self.grid[Line(line)];
            if !row.has_image_placeholders() {
                continue;
            }
            let cols = row.len();
            for col in 0..cols {
                let cell = &mut row[Column(col)];
                if cell.c == IMAGE_PLACEHOLDER_CHAR {
                    *cell = Cell { bg: cell.bg, ..Cell::default() };
                }
            }
            row.set_image_placeholders(false);
            cleared = true;
        }
        cleared
    }

    /// Handle `a=f`: transmit/store an animation frame for an existing image.
    ///
    /// Protocol semantics (kitty graphics.c handle_animation_frame_load_command):
    /// - `i=`/`I=` identifies the target image (must already exist).
    /// - `r=` (frame_number): 1-based target slot; 0 = append after last frame.
    /// - `c=` (other_frame_number): base frame for composition (stored, not computed here).
    /// - `z=` (gap): gap in ms; `Y=` (bgcolor); `C=` (compose mode / alpha_blend).
    /// - Chunked frames require `a=f` on every chunk (m=1 continuation).
    /// - Deep composition (Porter-Duff, chain flattening, keyframe flattening) is implemented via
    ///   `GraphicsManager::get_coalesced_frame_data` (Task 32).
    fn graphics_frame_command(&mut self, cmd: GraphicsCommand) {
        let chunk_echo = ResponseEcho {
            id: cmd.id,
            image_number: cmd.image_number,
            placement_id: cmd.placement_id,
            quiet: cmd.quiet,
            num_lines: cmd.num_lines,
        };

        // Kitty allows continuation chunks of a chunked a=f to omit i=/I=;
        // the in-flight load identity is used as a fallback
        // (graphics.c:2227: `!self->currently_loading.loading_for.image_id`).
        let loading_echo = if cmd.id == 0 && cmd.image_number == 0 {
            self.graphics.loading().map(|load| ResponseEcho {
                id: load.start.id,
                image_number: load.start.image_number,
                placement_id: load.start.placement_id,
                quiet: load.start.quiet,
                num_lines: cmd.num_lines,
            })
        } else {
            None
        };

        let echo = loading_echo.unwrap_or(chunk_echo);

        if echo.id == 0 && echo.image_number == 0 {
            // No in-flight load and no identity keys on this chunk: genuine error.
            let error = CommandError {
                code: ErrorCode::EINVAL,
                message: "a=f requires i= or I= to identify the target image".into(),
                sends_response: true,
            };
            self.send_graphics_response(&echo, Some(&error), false);
            return;
        }

        // Resolve target image using the resolved identity (may come from in-flight load).
        let image_id = if echo.id != 0 {
            self.graphics.image_by_client_id(echo.id).map(|img| img.id())
        } else {
            self.graphics.image_by_client_number(echo.image_number).map(|img| img.id())
        };
        let image_id = match image_id {
            Some(id) => id,
            None => {
                let error = CommandError {
                    code: ErrorCode::ENOENT,
                    message: format!(
                        "a=f: no image with id={} number={}",
                        echo.id, echo.image_number
                    ),
                    sends_response: true,
                };
                self.send_graphics_response(&echo, Some(&error), false);
                return;
            },
        };

        // Decode the pixel data via the normal transmission path. For `a=f`
        // with chunked data (m=1) each chunk must re-assert a=f — mirroring
        // the chunked root-image path where each chunk reasserts its action.
        let load = match self.graphics.handle_transmission(cmd) {
            Err(error) => {
                self.send_graphics_response(&echo, Some(&error), false);
                return;
            },
            Ok(TransmissionResult::MoreDataNeeded) => return,
            Ok(TransmissionResult::Complete(load)) => load,
        };

        let start = load.start;
        let quiet = if echo.quiet != 0 { echo.quiet } else { start.quiet };
        let final_echo =
            ResponseEcho { id: echo.id, image_number: start.image_number, quiet, ..echo };

        let frame_number = start.frame_number();
        let base_frame_id = start.other_frame_number();
        // gap(): i32 from z=; negative values treated as 0 per kitty.
        let gap_ms = start.gap().max(0) as u32;
        let bgcolor = start.bgcolor();
        // C=0 = Porter-Duff over; C=1 = source-copy (kitty default is over).
        let alpha_blend = start.compose_mode() == 0;

        let new_frame = graphics::Frame {
            width: load.width,
            height: load.height,
            data: load.data,
            gap_ms,
            x_offset: start.x_offset,
            y_offset: start.y_offset,
            base_frame_id,
            bgcolor,
            alpha_blend,
        };

        // Determine whether to append or edit based on frame_number.
        let result = if frame_number == 0
            || frame_number as usize
                >= self.graphics.image(image_id).map(|img| img.frames.len()).unwrap_or(0)
        {
            self.graphics.add_frame(image_id, frame_number, new_frame).map(|_| ())
        } else {
            self.graphics.edit_frame(image_id, frame_number, new_frame)
        };

        match result {
            Ok(()) => self.send_graphics_response(&final_echo, None, true),
            Err(error) => self.send_graphics_response(&final_echo, Some(&error), false),
        }
    }

    /// Handle `a=c` (compose): composite source frame region onto target frame.
    ///
    /// `c=` (other_frame_number) is source; `r=` (frame_number) is destination.
    /// `x=`/`y=` are the placement offset on the destination; `w=`/`h=` clip the
    /// source region (0 = full frame). `C=0` = Porter-Duff over, `C=1` = copy.
    /// Responds `EINVAL` when any frame is missing or the region is out of bounds.
    fn graphics_compose_command(&mut self, cmd: GraphicsCommand) {
        let echo = ResponseEcho {
            id: cmd.id,
            image_number: cmd.image_number,
            placement_id: cmd.placement_id,
            quiet: cmd.quiet,
            num_lines: 0,
        };
        if cmd.id == 0 && cmd.image_number == 0 {
            let error = CommandError {
                code: ErrorCode::EINVAL,
                message: "a=c requires i= or I= to identify the target image".into(),
                sends_response: true,
            };
            self.send_graphics_response(&echo, Some(&error), false);
            return;
        }
        let src_frame = cmd.other_frame_number();
        let dst_frame = cmd.frame_number();
        if src_frame == 0 || dst_frame == 0 {
            let error = CommandError {
                code: ErrorCode::EINVAL,
                message: "a=c requires c= (source frame) and r= (dest frame)".into(),
                sends_response: true,
            };
            self.send_graphics_response(&echo, Some(&error), false);
            return;
        }
        let image_id = if cmd.id != 0 {
            self.graphics.image_by_client_id(cmd.id).map(|img| img.id())
        } else {
            self.graphics.image_by_client_number(cmd.image_number).map(|img| img.id())
        };
        let image_id = match image_id {
            Some(id) => id,
            None => {
                let error = CommandError {
                    code: ErrorCode::ENOENT,
                    message: format!(
                        "a=c: no image with id={} number={}",
                        cmd.id, cmd.image_number
                    ),
                    sends_response: true,
                };
                self.send_graphics_response(&echo, Some(&error), false);
                return;
            },
        };
        let needs_blend = cmd.compose_mode() == 0;
        let result = self.graphics.compose_frame(image_id, ComposeFrameArgs {
            src_frame_number: src_frame,
            dst_frame_number: dst_frame,
            dst_x: cmd.x_offset,
            dst_y: cmd.y_offset,
            src_w: cmd.width,
            src_h: cmd.height,
            needs_blend,
        });
        match result {
            Ok(()) => self.send_graphics_response(&echo, None, true),
            Err(error) => self.send_graphics_response(&echo, Some(&error), false),
        }
    }

    /// Handle `a=a` (animation control). Never sends a response (kitty parity).
    ///
    /// Port of kitty `handle_animation_control_command` (graphics.c:1729).
    /// `s=` → animation state; `c=` → jump to frame; `r=`+`z=` → edit frame
    /// gap; `v=` → loop count (n-1 semantics). Silently ignored for unknown
    /// images so that `a=a` is always response-free.
    fn graphics_animation_command(&mut self, cmd: GraphicsCommand) {
        if cmd.id == 0 && cmd.image_number == 0 {
            return;
        }
        let image_id = if cmd.id != 0 {
            self.graphics.image_by_client_id(cmd.id).map(|img| img.id())
        } else {
            self.graphics.image_by_client_number(cmd.image_number).map(|img| img.id())
        };
        let image_id = match image_id {
            Some(id) => id,
            None => return, // unknown image — silently ignored, no response.
        };
        let now_ms =
            std::time::SystemTime::UNIX_EPOCH.elapsed().map(|d| d.as_millis() as u64).unwrap_or(0);
        self.graphics.animation_control(image_id, AnimationControlArgs {
            anim_state: cmd.animation_state(),
            frame_number: cmd.other_frame_number(), // c= jump-to-frame
            gap_frame: cmd.frame_number(),          // r= frame whose gap to edit
            gap_ms: cmd.gap(),                      // z= new gap in ms
            loop_count: cmd.loop_count(),           // v= (n-1 semantics)
            now_ms,
        });
    }

    /// Create a placement at the cursor and move the cursor over it.
    ///
    /// Storage half lives in [`GraphicsManager::put_placement`]; this adds
    /// kitty's cursor movement policy (`C=`) and full damage.
    ///
    /// # Cursor movement algorithm (mirrors kitty screen.c:1607-1612)
    ///
    /// Four coordinate concepts that must NOT be confused:
    /// - `c=/r=` (`num_cells`/`num_lines`): requested display size in terminal cells
    /// - `X=/Y=` (`cell_x_offset`/`cell_y_offset`): sub-cell pixel offset within the first cell
    /// - `x=/y=/w=/h=` (`x_offset`/`y_offset`/`width`/`height`): source-image crop in pixels
    /// - `(Line, Column)` anchor: viewport-relative cell position of the top-left corner
    ///
    /// When `C=0` (default, no-move suppressed):
    ///   1. `x += effective_num_cols` — may exceed terminal width
    ///   2. `y += effective_num_rows - 1` — rows–1 because the image top-left is the current row
    ///   3. If `x >= columns`: wrap to next line (`x = 0`, `y += 1`)
    ///   4. If `y > scroll_region.end - 1`: scroll up by the overflow, clamp `y`
    ///   5. Clamp final `(x, y)` to screen bounds
    ///
    /// When `C=1`: cursor is NOT moved (image is placed but cursor stays put).
    fn graphics_place(&mut self, image_id: ImageId, cmd: &GraphicsCommand) -> Option<CommandError> {
        // Virtual placement + parent → EINVAL (graphics.c:1064-1066).
        if cmd.unicode_placement && cmd.parent_id != 0 {
            return Some(CommandError {
                code: ErrorCode::EINVAL,
                message: "Put command creating a virtual placement cannot refer to a parent".into(),
                sends_response: true,
            });
        }

        // x=/y= are source-image crop origins; w=/h= are crop dimensions.
        // X=/Y= are sub-cell pixel offsets into the first cell (already in cmd).
        // c=/r= requested display cells — effective extent computed by put_placement.
        // H=/V= are CELL offsets from the parent's anchor (not pixels).
        let spec = PlacementSpec {
            placement_id: cmd.placement_id,
            src_x: cmd.x_offset,
            src_y: cmd.y_offset,
            src_width: cmd.width,
            src_height: cmd.height,
            cell_x_offset: cmd.cell_x_offset,
            cell_y_offset: cmd.cell_y_offset,
            num_cols: cmd.num_cells,
            num_rows: cmd.num_lines,
            z_index: cmd.z_index,
            is_virtual: cmd.unicode_placement,
            parent_client_id: cmd.parent_id,
            parent_placement_client_id: cmd.parent_placement_id,
            parent_offset_x: cmd.offset_from_parent_x,
            parent_offset_y: cmd.offset_from_parent_y,
            ..Default::default()
        };

        // Virtual placements (U=1) have no screen anchor and must not move
        // the cursor (kitty graphics.c:is_virtual_ref semantics).
        if cmd.unicode_placement {
            let result = self.graphics.put_placement(
                image_id,
                Line(0),
                Column(0),
                &spec,
                self.graphics_cell_size,
            );
            if let Err(e) = result {
                return Some(e);
            }
            self.mark_fully_damaged();
            return None;
        }

        // Relative placement (P= nonzero): do not advance the cursor
        // (graphics.c:1145-1151).
        let has_parent = cmd.parent_id != 0;

        let cursor = self.grid.cursor.point;
        let result = self.graphics.put_placement(
            image_id,
            cursor.line,
            cursor.column,
            &spec,
            self.graphics_cell_size,
        );

        let extent = match result {
            Err(e) => return Some(e),
            Ok(e) => e,
        };

        if let Some((columns, lines)) = extent {
            // C=1 or parented placement: cursor does not move
            // (graphics.c:1147-1151: cursor only advances when no parent and
            //  not virtual).
            if cmd.cursor_movement != 1 && !has_parent {
                // Step 1+2: advance x by cols, y by rows-1.
                let new_x = cursor.column.0 + columns as usize;
                let new_y =
                    if lines > 0 { cursor.line.0 + lines as i32 - 1 } else { cursor.line.0 };

                // Step 3: wrap at right margin (kitty: if cursor->x >= columns).
                let (new_x, new_y) =
                    if new_x >= self.columns() { (0usize, new_y + 1) } else { (new_x, new_y) };

                // Step 4: scroll if past bottom of scroll region
                // (kitty: if cursor->y > margin_bottom → screen_scroll).
                // scroll_region.end is exclusive; margin_bottom is inclusive.
                let margin_bottom = self.scroll_region.end.0 - 1;
                let new_y = if new_y > margin_bottom {
                    let overflow = (new_y - margin_bottom) as usize;
                    self.scroll_up_relative(self.scroll_region.start, overflow);
                    margin_bottom
                } else {
                    new_y
                };

                // Step 5: clamp to screen bounds (kitty: screen_ensure_bounds).
                let new_y = new_y.max(0).min(self.screen_lines() as i32 - 1);
                let new_x = new_x.min(self.columns() - 1);

                self.grid.cursor.point = Point::new(Line(new_y), Column(new_x));
                self.grid.cursor.input_needs_wrap = false;
            }

            self.mark_fully_damaged();
        }

        None
    }

    /// Answer a graphics command arriving while the protocol is disabled.
    ///
    /// Kitty has no disabled mode to mirror, so the closest spec-legal
    /// behavior is used: every command that would normally respond gets an
    /// `EPERM` error ("operation not permitted" — the terminal refuses to
    /// act, the command itself was well-formed). Queries (`a=q`) are the
    /// feature-detection probe (`a=q` followed by DA1) and must never be
    /// silent, so for them even `q=2` suppression is overridden. Deletes
    /// and malformed commands stay silent, exactly as when enabled.
    fn graphics_disabled_response(&mut self, body: &[u8]) {
        let cmd = match GraphicsCommand::parse(body) {
            Ok(cmd) => cmd,
            Err(error) => {
                debug!("Ignoring graphics command while disabled: {error}");
                return;
            },
        };

        // Deletes never produce a response (kitty parity).
        if cmd.action == b'd' {
            return;
        }

        let echo = if cmd.action == b'q' {
            // Queries echo only `i=` and override suppression; a query
            // without an image id remains silent (nothing to correlate).
            ResponseEcho { id: cmd.id, ..Default::default() }
        } else {
            ResponseEcho {
                id: cmd.id,
                image_number: cmd.image_number,
                placement_id: cmd.placement_id,
                quiet: cmd.quiet,
                num_lines: 0,
            }
        };

        let error = CommandError {
            code: ErrorCode::EPERM,
            message: "Graphics support has been disabled in the configuration".into(),
            sends_response: true,
        };
        self.send_graphics_response(&echo, Some(&error), false);
    }

    /// Emit a graphics response over the PTY, honoring suppression rules.
    fn send_graphics_response(
        &mut self,
        echo: &ResponseEcho,
        error: Option<&CommandError>,
        data_loaded: bool,
    ) {
        if let Some(text) = response::build_response(echo, error, data_loaded) {
            self.event_proxy.send_event(Event::PtyWrite(text));
        }
    }
}

impl<T: EventListener> Handler for Term<T> {
    /// A character to be displayed.
    #[inline(never)]
    fn input(&mut self, c: char) {
        // Number of cells the char will occupy.
        let width = match c.width() {
            Some(width) => width,
            None => return,
        };

        // Handle zero-width characters.
        if width == 0 {
            // Get previous column.
            let mut column = self.grid.cursor.point.column;
            if !self.grid.cursor.input_needs_wrap {
                column.0 = column.saturating_sub(1);
            }

            // Put zerowidth characters over first fullwidth character cell.
            let line = self.grid.cursor.point.line;
            if self.grid[line][column].flags.contains(Flags::WIDE_CHAR_SPACER) {
                column.0 = column.saturating_sub(1);
            }

            self.grid[line][column].push_zerowidth(c);
            return;
        }

        // Move cursor to next line.
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
        }

        // If in insert mode, first shift cells to the right.
        let columns = self.columns();
        if self.mode.contains(TermMode::INSERT) && self.grid.cursor.point.column + width < columns {
            let line = self.grid.cursor.point.line;
            let col = self.grid.cursor.point.column;
            let row = &mut self.grid[line][..];

            for col in (col.0..(columns - width)).rev() {
                row.swap(col + width, col);
            }
        }

        if width == 1 {
            self.write_at_cursor(c);
        } else {
            if self.grid.cursor.point.column + 1 >= columns {
                if self.mode.contains(TermMode::LINE_WRAP) {
                    // Insert placeholder before wide char if glyph does not fit in this row.
                    self.grid.cursor.template.flags.insert(Flags::LEADING_WIDE_CHAR_SPACER);
                    self.write_at_cursor(' ');
                    self.grid.cursor.template.flags.remove(Flags::LEADING_WIDE_CHAR_SPACER);
                    self.wrapline();
                } else {
                    // Prevent out of bounds crash when linewrapping is disabled.
                    self.grid.cursor.input_needs_wrap = true;
                    return;
                }
            }

            // Write full width glyph to current cursor cell.
            self.grid.cursor.template.flags.insert(Flags::WIDE_CHAR);
            self.write_at_cursor(c);
            self.grid.cursor.template.flags.remove(Flags::WIDE_CHAR);

            // Write spacer to cell following the wide glyph.
            self.grid.cursor.point.column += 1;
            self.grid.cursor.template.flags.insert(Flags::WIDE_CHAR_SPACER);
            self.write_at_cursor(' ');
            self.grid.cursor.template.flags.remove(Flags::WIDE_CHAR_SPACER);
        }

        if self.grid.cursor.point.column + 1 < columns {
            self.grid.cursor.point.column += 1;
        } else {
            self.grid.cursor.input_needs_wrap = true;
        }
    }

    #[inline]
    fn decaln(&mut self) {
        trace!("Decalnning");

        for line in (0..self.screen_lines()).map(Line::from) {
            for column in 0..self.columns() {
                let cell = &mut self.grid[line][Column(column)];
                *cell = Cell::default();
                cell.c = 'E';
            }
        }

        self.mark_fully_damaged();
    }

    #[inline]
    fn goto(&mut self, line: i32, col: usize) {
        let line = Line(line);
        let col = Column(col);

        trace!("Going to: line={line}, col={col}");
        let (y_offset, max_y) = if self.mode.contains(TermMode::ORIGIN) {
            (self.scroll_region.start, self.scroll_region.end - 1)
        } else {
            (Line(0), self.bottommost_line())
        };

        self.damage_cursor();
        self.grid.cursor.point.line = cmp::max(cmp::min(line + y_offset, max_y), Line(0));
        self.grid.cursor.point.column = cmp::min(col, self.last_column());
        self.damage_cursor();
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn goto_line(&mut self, line: i32) {
        trace!("Going to line: {line}");
        self.goto(line, self.grid.cursor.point.column.0)
    }

    #[inline]
    fn goto_col(&mut self, col: usize) {
        trace!("Going to column: {col}");
        self.goto(self.grid.cursor.point.line.0, col)
    }

    #[inline]
    fn insert_blank(&mut self, count: usize) {
        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;

        // Ensure inserting within terminal bounds
        let count = cmp::min(count, self.columns() - cursor.point.column.0);

        let source = cursor.point.column;
        let destination = cursor.point.column.0 + count;
        let num_cells = self.columns() - destination;

        let line = cursor.point.line;
        self.damage.damage_line(line.0 as usize, 0, self.columns() - 1);

        let row = &mut self.grid[line][..];

        for offset in (0..num_cells).rev() {
            row.swap(destination + offset, source.0 + offset);
        }

        // Cells were just moved out toward the end of the line;
        // fill in between source and dest with blanks.
        for cell in &mut row[source.0..destination] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn move_up(&mut self, lines: usize) {
        trace!("Moving up: {lines}");

        let line = self.grid.cursor.point.line - lines;
        let column = self.grid.cursor.point.column;
        self.goto(line.0, column.0)
    }

    #[inline]
    fn move_down(&mut self, lines: usize) {
        trace!("Moving down: {lines}");

        let line = self.grid.cursor.point.line + lines;
        let column = self.grid.cursor.point.column;
        self.goto(line.0, column.0)
    }

    #[inline]
    fn move_forward(&mut self, cols: usize) {
        trace!("Moving forward: {cols}");
        let last_column = cmp::min(self.grid.cursor.point.column + cols, self.last_column());

        let cursor_line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(cursor_line, self.grid.cursor.point.column.0, last_column.0);

        self.grid.cursor.point.column = last_column;
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn move_backward(&mut self, cols: usize) {
        trace!("Moving backward: {cols}");
        let column = self.grid.cursor.point.column.saturating_sub(cols);

        let cursor_line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(cursor_line, column, self.grid.cursor.point.column.0);

        self.grid.cursor.point.column = Column(column);
        self.grid.cursor.input_needs_wrap = false;
    }

    #[inline]
    fn identify_terminal(&mut self, intermediate: Option<char>) {
        match intermediate {
            None => {
                trace!("Reporting primary device attributes");
                // ESC[?62;4;c advertises sixel (4) when enabled; bare ESC[?62;c otherwise.
                // Both payloads are ≥5 chars, satisfying kitten's len>3 detection check.
                let text = if self.config.graphics.sixel_enabled() {
                    String::from("\x1b[?62;4;c")
                } else {
                    String::from("\x1b[?62;c")
                };
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            Some('>') => {
                trace!("Reporting secondary device attributes");
                let version = version_number(env!("CARGO_PKG_VERSION"));
                let text = format!("\x1b[>0;{version};1c");
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            _ => debug!("Unsupported device attributes intermediate"),
        }
    }

    #[inline]
    fn report_keyboard_mode(&mut self) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Reporting active keyboard mode");
        let current_mode =
            self.keyboard_mode_stack.last().unwrap_or(&KeyboardModes::NO_MODE).bits();
        let text = format!("\x1b[?{current_mode}u");
        self.event_proxy.send_event(Event::PtyWrite(text));
    }

    #[inline]
    fn push_keyboard_mode(&mut self, mode: KeyboardModes) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Pushing `{mode:?}` keyboard mode into the stack");

        if self.keyboard_mode_stack.len() >= KEYBOARD_MODE_STACK_MAX_DEPTH {
            let removed = self.title_stack.remove(0);
            trace!(
                "Removing '{removed:?}' from bottom of keyboard mode stack that exceeds its \
                 maximum depth"
            );
        }

        self.keyboard_mode_stack.push(mode);
        self.set_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    #[inline]
    fn pop_keyboard_modes(&mut self, to_pop: u16) {
        if !self.config.kitty_keyboard {
            return;
        }

        trace!("Attempting to pop {to_pop} keyboard modes from the stack");
        let new_len = self.keyboard_mode_stack.len().saturating_sub(to_pop as usize);
        self.keyboard_mode_stack.truncate(new_len);

        // Reload active mode.
        let mode = self.keyboard_mode_stack.last().copied().unwrap_or(KeyboardModes::NO_MODE);
        self.set_keyboard_mode(mode.into(), KeyboardModesApplyBehavior::Replace);
    }

    #[inline]
    fn set_keyboard_mode(&mut self, mode: KeyboardModes, apply: KeyboardModesApplyBehavior) {
        if !self.config.kitty_keyboard {
            return;
        }

        self.set_keyboard_mode(mode.into(), apply);
    }

    #[inline]
    fn device_status(&mut self, arg: usize) {
        trace!("Reporting device status: {arg}");
        match arg {
            5 => {
                let text = String::from("\x1b[0n");
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            6 => {
                let pos = self.grid.cursor.point;
                let text = format!("\x1b[{};{}R", pos.line + 1, pos.column + 1);
                self.event_proxy.send_event(Event::PtyWrite(text));
            },
            _ => debug!("unknown device status query: {arg}"),
        };
    }

    #[inline]
    fn graphics_attribute(&mut self, pi: u16, pa: u16, pv: u32) {
        use crate::graphics::sixel::{MAX_COLOR_REGISTERS, MAX_SIXEL_DIM};

        trace!("XTSMGRAPHICS Pi={pi} Pa={pa} Pv={pv}");

        let text = match pi {
            1 => {
                let max = MAX_COLOR_REGISTERS as u32;
                match pa {
                    1 => format!("\x1b[?1;0;{}S", self.sixel_color_registers),
                    2 => {
                        self.sixel_color_registers = max;
                        format!("\x1b[?1;0;{max}S")
                    },
                    3 => {
                        if pv == 0 || pv > max {
                            format!("\x1b[?1;3;{max}S")
                        } else {
                            self.sixel_color_registers = pv;
                            format!("\x1b[?1;0;{pv}S")
                        }
                    },
                    4 => format!("\x1b[?1;0;{max}S"),
                    _ => String::from("\x1b[?1;2;0S"),
                }
            },
            2 => {
                let max_dim = MAX_SIXEL_DIM as u32;
                let cell = &self.graphics_cell_size;
                let cols = self.columns() as u32;
                let rows = self.screen_lines() as u32;
                let cur_w = cols * cell.width;
                let cur_h = rows * cell.height;
                match pa {
                    1 => format!("\x1b[?2;0;{cur_w};{cur_h}S"),
                    2 => format!("\x1b[?2;0;{cur_w};{cur_h}S"),
                    3 => String::from("\x1b[?2;3;0S"),
                    4 => format!("\x1b[?2;0;{max_dim};{max_dim}S"),
                    _ => String::from("\x1b[?2;2;0S"),
                }
            },
            _ => format!("\x1b[?{pi};1;0S"),
        };
        self.event_proxy.send_event(Event::PtyWrite(text));
    }

    #[inline]
    fn move_down_and_cr(&mut self, lines: usize) {
        trace!("Moving down and cr: {lines}");

        let line = self.grid.cursor.point.line + lines;
        self.goto(line.0, 0)
    }

    #[inline]
    fn move_up_and_cr(&mut self, lines: usize) {
        trace!("Moving up and cr: {lines}");

        let line = self.grid.cursor.point.line - lines;
        self.goto(line.0, 0)
    }

    /// Insert tab at cursor position.
    #[inline]
    fn put_tab(&mut self, mut count: u16) {
        // A tab after the last column is the same as a linebreak.
        if self.grid.cursor.input_needs_wrap {
            self.wrapline();
            return;
        }

        while self.grid.cursor.point.column < self.columns() && count != 0 {
            count -= 1;

            let c = self.grid.cursor.charsets[self.active_charset].map('\t');
            let cell = self.grid.cursor_cell();
            if cell.c == ' ' {
                cell.c = c;
            }

            loop {
                if (self.grid.cursor.point.column + 1) == self.columns() {
                    break;
                }

                self.grid.cursor.point.column += 1;

                if self.tabs[self.grid.cursor.point.column] {
                    break;
                }
            }
        }
    }

    /// Backspace.
    #[inline]
    fn backspace(&mut self) {
        trace!("Backspace");

        if self.grid.cursor.point.column > Column(0) {
            let line = self.grid.cursor.point.line.0 as usize;
            let column = self.grid.cursor.point.column.0;
            self.grid.cursor.point.column -= 1;
            self.grid.cursor.input_needs_wrap = false;
            self.damage.damage_line(line, column - 1, column);
        }
    }

    /// Carriage return.
    #[inline]
    fn carriage_return(&mut self) {
        trace!("Carriage return");
        let new_col = 0;
        let line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(line, new_col, self.grid.cursor.point.column.0);
        self.grid.cursor.point.column = Column(new_col);
        self.grid.cursor.input_needs_wrap = false;
    }

    /// Linefeed.
    #[inline]
    fn linefeed(&mut self) {
        trace!("Linefeed");
        let next = self.grid.cursor.point.line + 1;
        if next == self.scroll_region.end {
            self.scroll_up(1);
        } else if next < self.screen_lines() {
            self.damage_cursor();
            self.grid.cursor.point.line += 1;
            self.damage_cursor();
        }
    }

    /// Set current position as a tabstop.
    #[inline]
    fn bell(&mut self) {
        trace!("Bell");
        self.event_proxy.send_event(Event::Bell);
    }

    #[inline]
    fn substitute(&mut self) {
        trace!("[unimplemented] Substitute");
    }

    /// Run LF/NL.
    ///
    /// LF/NL mode has some interesting history. According to ECMA-48 4th
    /// edition, in LINE FEED mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// > (FF), LINE TABULATION (VT) cause only movement of the active position in
    /// > the direction of the line progression.
    ///
    /// In NEW LINE mode,
    ///
    /// > The execution of the formatter functions LINE FEED (LF), FORM FEED
    /// > (FF), LINE TABULATION (VT) cause movement to the line home position on
    /// > the following line, the following form, etc. In the case of LF this is
    /// > referred to as the New Line (NL) option.
    ///
    /// Additionally, ECMA-48 4th edition says that this option is deprecated.
    /// ECMA-48 5th edition only mentions this option (without explanation)
    /// saying that it's been removed.
    ///
    /// As an emulator, we need to support it since applications may still rely
    /// on it.
    #[inline]
    fn newline(&mut self) {
        self.linefeed();

        if self.mode.contains(TermMode::LINE_FEED_NEW_LINE) {
            self.carriage_return();
        }
    }

    #[inline]
    fn set_horizontal_tabstop(&mut self) {
        trace!("Setting horizontal tabstop");
        self.tabs[self.grid.cursor.point.column] = true;
    }

    #[inline]
    fn scroll_up(&mut self, lines: usize) {
        let origin = self.scroll_region.start;
        self.scroll_up_relative(origin, lines);
    }

    #[inline]
    fn scroll_down(&mut self, lines: usize) {
        let origin = self.scroll_region.start;
        self.scroll_down_relative(origin, lines);
    }

    #[inline]
    fn insert_blank_lines(&mut self, lines: usize) {
        trace!("Inserting blank {lines} lines");

        let origin = self.grid.cursor.point.line;
        if self.scroll_region.contains(&origin) {
            self.scroll_down_relative(origin, lines);
        }
    }

    #[inline]
    fn delete_lines(&mut self, lines: usize) {
        let origin = self.grid.cursor.point.line;
        let lines = cmp::min(self.screen_lines() - origin.0 as usize, lines);

        trace!("Deleting {lines} lines");

        if lines > 0 && self.scroll_region.contains(&origin) {
            self.scroll_up_relative(origin, lines);
        }
    }

    #[inline]
    fn erase_chars(&mut self, count: usize) {
        let cursor = &self.grid.cursor;

        trace!("Erasing chars: count={}, col={}", count, cursor.point.column);

        let start = cursor.point.column;
        let end = cmp::min(start + count, Column(self.columns()));

        // Cleared cells have current background color set.
        let bg = self.grid.cursor.template.bg;
        let line = cursor.point.line;
        self.damage.damage_line(line.0 as usize, start.0, end.0);
        let row = &mut self.grid[line];
        for cell in &mut row[start..end] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn delete_chars(&mut self, count: usize) {
        let columns = self.columns();
        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;

        // Ensure deleting within terminal bounds.
        let count = cmp::min(count, columns);

        let start = cursor.point.column.0;
        let end = cmp::min(start + count, columns - 1);
        let num_cells = columns - end;

        let line = cursor.point.line;
        self.damage.damage_line(line.0 as usize, 0, self.columns() - 1);
        let row = &mut self.grid[line][..];

        for offset in 0..num_cells {
            row.swap(start + offset, end + offset);
        }

        // Clear last `count` cells in the row. If deleting 1 char, need to delete
        // 1 cell.
        let end = columns - count;
        for cell in &mut row[end..] {
            *cell = bg.into();
        }
    }

    #[inline]
    fn move_backward_tabs(&mut self, count: u16) {
        trace!("Moving backward {count} tabs");

        let old_col = self.grid.cursor.point.column.0;
        for _ in 0..count {
            let mut col = self.grid.cursor.point.column;

            if col == 0 {
                break;
            }

            for i in (0..(col.0)).rev() {
                if self.tabs[index::Column(i)] {
                    col = index::Column(i);
                    break;
                }
            }
            self.grid.cursor.point.column = col;
        }

        let line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(line, self.grid.cursor.point.column.0, old_col);
    }

    #[inline]
    fn move_forward_tabs(&mut self, count: u16) {
        trace!("Moving forward {count} tabs");

        let num_cols = self.columns();
        let old_col = self.grid.cursor.point.column.0;
        for _ in 0..count {
            let mut col = self.grid.cursor.point.column;

            if col == num_cols - 1 {
                break;
            }

            for i in col.0 + 1..num_cols {
                col = index::Column(i);
                if self.tabs[col] {
                    break;
                }
            }

            self.grid.cursor.point.column = col;
        }

        let line = self.grid.cursor.point.line.0 as usize;
        self.damage.damage_line(line, old_col, self.grid.cursor.point.column.0);
    }

    #[inline]
    fn save_cursor_position(&mut self) {
        trace!("Saving cursor position");

        self.grid.saved_cursor = self.grid.cursor.clone();
    }

    #[inline]
    fn restore_cursor_position(&mut self) {
        trace!("Restoring cursor position");

        self.damage_cursor();
        self.grid.cursor = self.grid.saved_cursor.clone();
        self.damage_cursor();
    }

    #[inline]
    fn clear_line(&mut self, mode: ansi::LineClearMode) {
        trace!("Clearing line: {mode:?}");

        let cursor = &self.grid.cursor;
        let bg = cursor.template.bg;
        let point = cursor.point;

        let (left, right) = match mode {
            ansi::LineClearMode::Right if cursor.input_needs_wrap => return,
            ansi::LineClearMode::Right => (point.column, Column(self.columns())),
            ansi::LineClearMode::Left => (Column(0), point.column + 1),
            ansi::LineClearMode::All => (Column(0), Column(self.columns())),
        };

        self.damage.damage_line(point.line.0 as usize, left.0, right.0 - 1);

        let row = &mut self.grid[point.line];
        for cell in &mut row[left..right] {
            *cell = bg.into();
        }

        let range = self.grid.cursor.point.line..=self.grid.cursor.point.line;
        self.selection = self.selection.take().filter(|s| !s.intersects_range(range));
    }

    /// Set the indexed color value.
    #[inline]
    fn set_color(&mut self, index: usize, color: Rgb) {
        trace!("Setting color[{index}] = {color:?}");

        // Damage terminal if the color changed and it's not the cursor.
        if index != NamedColor::Cursor as usize && self.colors[index] != Some(color) {
            self.mark_fully_damaged();
        }

        self.colors[index] = Some(color);
    }

    /// Respond to a color query escape sequence.
    #[inline]
    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        trace!("Requested write of escape sequence for color code {prefix}: color[{index}]");

        let terminator = terminator.to_owned();
        self.event_proxy.send_event(Event::ColorRequest(
            index,
            Arc::new(move |color| {
                format!(
                    "\x1b]{};rgb:{1:02x}{1:02x}/{2:02x}{2:02x}/{3:02x}{3:02x}{4}",
                    prefix, color.r, color.g, color.b, terminator
                )
            }),
        ));
    }

    /// Reset the indexed color to original value.
    #[inline]
    fn reset_color(&mut self, index: usize) {
        trace!("Resetting color[{index}]");

        // Damage terminal if the color changed and it's not the cursor.
        if index != NamedColor::Cursor as usize && self.colors[index].is_some() {
            self.mark_fully_damaged();
        }

        self.colors[index] = None;
    }

    /// Store data into clipboard.
    #[inline]
    fn clipboard_store(&mut self, clipboard: u8, base64: &[u8]) {
        if !matches!(self.config.osc52, Osc52::OnlyCopy | Osc52::CopyPaste) {
            debug!("Denied osc52 store");
            return;
        }

        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        if let Ok(bytes) = Base64.decode(base64)
            && let Ok(text) = String::from_utf8(bytes)
        {
            self.event_proxy.send_event(Event::ClipboardStore(clipboard_type, text));
        }
    }

    /// Load data from clipboard.
    #[inline]
    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        if !matches!(self.config.osc52, Osc52::OnlyPaste | Osc52::CopyPaste) {
            debug!("Denied osc52 load");
            return;
        }

        let clipboard_type = match clipboard {
            b'c' => ClipboardType::Clipboard,
            b'p' | b's' => ClipboardType::Selection,
            _ => return,
        };

        let terminator = terminator.to_owned();

        self.event_proxy.send_event(Event::ClipboardLoad(
            clipboard_type,
            Arc::new(move |text| {
                let base64 = Base64.encode(text);
                format!("\x1b]52;{};{}{}", clipboard as char, base64, terminator)
            }),
        ));
    }

    #[inline]
    fn clear_screen(&mut self, mode: ansi::ClearMode) {
        trace!("Clearing screen: {mode:?}");
        let bg = self.grid.cursor.template.bg;

        let screen_lines = self.screen_lines();

        match mode {
            ansi::ClearMode::Above => {
                let cursor = self.grid.cursor.point;

                // If clearing more than one line.
                if cursor.line > 1 {
                    // Fully clear all lines before the current line.
                    self.grid.reset_region(..cursor.line);
                }

                // Clear up to the current column in the current line.
                let end = cmp::min(cursor.column + 1, Column(self.columns()));
                for cell in &mut self.grid[cursor.line][..end] {
                    *cell = bg.into();
                }

                let range = Line(0)..=cursor.line;
                self.selection = self.selection.take().filter(|s| !s.intersects_range(range));
            },
            ansi::ClearMode::Below => {
                let cursor = self.grid.cursor.point;
                for cell in &mut self.grid[cursor.line][cursor.column..] {
                    *cell = bg.into();
                }

                if (cursor.line.0 as usize) < screen_lines - 1 {
                    self.grid.reset_region((cursor.line + 1)..);
                }

                let range = cursor.line..Line(screen_lines as i32);
                self.selection = self.selection.take().filter(|s| !s.intersects_range(range));
            },
            ansi::ClearMode::All => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.grid.reset_region(..);
                } else {
                    let old_offset = self.grid.display_offset();

                    self.grid.clear_viewport();

                    // Compute number of lines scrolled by clearing the viewport.
                    let lines = self.grid.display_offset().saturating_sub(old_offset);

                    self.vi_mode_cursor.point.line =
                        (self.vi_mode_cursor.point.line - lines).grid_clamp(self, Boundary::Grid);
                }

                self.selection = None;

                // ED 2 deletes graphics intersecting the screen, like kitty
                // (screen.c:2604 calls `grman_clear` with `all=false`).
                self.graphics.clear(false);
            },
            ansi::ClearMode::Saved if self.history_size() > 0 => {
                self.grid.clear_history();

                self.vi_mode_cursor.point.line =
                    self.vi_mode_cursor.point.line.grid_clamp(self, Boundary::Cursor);

                self.selection = self.selection.take().filter(|s| !s.intersects_range(..Line(0)));

                self.graphics.clear_scrollback();
            },
            // We have no history to clear.
            ansi::ClearMode::Saved => {
                self.graphics.clear_scrollback();
            },
        }

        self.mark_fully_damaged();
    }

    #[inline]
    fn clear_tabs(&mut self, mode: ansi::TabulationClearMode) {
        trace!("Clearing tabs: {mode:?}");
        match mode {
            ansi::TabulationClearMode::Current => {
                self.tabs[self.grid.cursor.point.column] = false;
            },
            ansi::TabulationClearMode::All => {
                self.tabs.clear_all();
            },
        }
    }

    /// Reset all important fields in the term struct.
    #[inline]
    fn reset_state(&mut self) {
        if self.mode.contains(TermMode::ALT_SCREEN) {
            mem::swap(&mut self.grid, &mut self.inactive_grid);
            mem::swap(&mut self.graphics, &mut self.inactive_graphics);
        }

        // Clear graphics on both screens and drop any in-flight chunked
        // transmission (kitty clears both managers in `screen_reset`,
        // screen.c:203-204).
        self.graphics.clear(true);
        self.graphics.abort_load();
        self.inactive_graphics.clear(true);
        self.inactive_graphics.abort_load();

        self.active_charset = Default::default();
        self.cursor_style = None;
        self.grid.reset();
        self.inactive_grid.reset();
        self.scroll_region = Line(0)..Line(self.screen_lines() as i32);
        self.tabs = TabStops::new(self.columns());
        self.title_stack = Vec::new();
        self.title = None;
        self.selection = None;
        self.vi_mode_cursor = Default::default();
        self.keyboard_mode_stack = Default::default();
        self.inactive_keyboard_mode_stack = Default::default();

        // Preserve vi mode across resets.
        self.mode &= TermMode::VI;
        self.mode.insert(TermMode::default());

        self.event_proxy.send_event(Event::CursorBlinkingChange);
        self.mark_fully_damaged();
    }

    #[inline]
    fn reverse_index(&mut self) {
        trace!("Reversing index");
        // If cursor is at the top.
        if self.grid.cursor.point.line == self.scroll_region.start {
            self.scroll_down(1);
        } else {
            self.damage_cursor();
            self.grid.cursor.point.line = cmp::max(self.grid.cursor.point.line - 1, Line(0));
            self.damage_cursor();
        }
    }

    #[inline]
    fn set_hyperlink(&mut self, hyperlink: Option<Hyperlink>) {
        trace!("Setting hyperlink: {hyperlink:?}");
        self.grid.cursor.template.set_hyperlink(hyperlink.map(|e| e.into()));
    }

    /// Set a terminal attribute.
    #[inline]
    fn terminal_attribute(&mut self, attr: Attr) {
        trace!("Setting attribute: {attr:?}");
        let cursor = &mut self.grid.cursor;
        match attr {
            Attr::Foreground(color) => cursor.template.fg = color,
            Attr::Background(color) => cursor.template.bg = color,
            Attr::UnderlineColor(color) => cursor.template.set_underline_color(color),
            Attr::Reset => {
                cursor.template.fg = Color::Named(NamedColor::Foreground);
                cursor.template.bg = Color::Named(NamedColor::Background);
                cursor.template.flags = Flags::empty();
                cursor.template.set_underline_color(None);
            },
            Attr::Reverse => cursor.template.flags.insert(Flags::INVERSE),
            Attr::CancelReverse => cursor.template.flags.remove(Flags::INVERSE),
            Attr::Bold => cursor.template.flags.insert(Flags::BOLD),
            Attr::CancelBold => cursor.template.flags.remove(Flags::BOLD),
            Attr::Dim => cursor.template.flags.insert(Flags::DIM),
            Attr::CancelBoldDim => cursor.template.flags.remove(Flags::BOLD | Flags::DIM),
            Attr::Italic => cursor.template.flags.insert(Flags::ITALIC),
            Attr::CancelItalic => cursor.template.flags.remove(Flags::ITALIC),
            Attr::Underline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::UNDERLINE);
            },
            Attr::DoubleUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DOUBLE_UNDERLINE);
            },
            Attr::Undercurl => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::UNDERCURL);
            },
            Attr::DottedUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DOTTED_UNDERLINE);
            },
            Attr::DashedUnderline => {
                cursor.template.flags.remove(Flags::ALL_UNDERLINES);
                cursor.template.flags.insert(Flags::DASHED_UNDERLINE);
            },
            Attr::CancelUnderline => cursor.template.flags.remove(Flags::ALL_UNDERLINES),
            Attr::Hidden => cursor.template.flags.insert(Flags::HIDDEN),
            Attr::CancelHidden => cursor.template.flags.remove(Flags::HIDDEN),
            Attr::Strike => cursor.template.flags.insert(Flags::STRIKEOUT),
            Attr::CancelStrike => cursor.template.flags.remove(Flags::STRIKEOUT),
            _ => {
                debug!("Term got unhandled attr: {attr:?}");
            },
        }
    }

    #[inline]
    fn set_private_mode(&mut self, mode: PrivateMode) {
        let mode = match mode {
            PrivateMode::Named(mode) => mode,
            PrivateMode::Unknown(80) => {
                self.mode.insert(TermMode::SIXEL_DISPLAY);
                return;
            },
            PrivateMode::Unknown(1070) => {
                self.mode.insert(TermMode::SIXEL_PRIV_PALETTE);
                return;
            },
            PrivateMode::Unknown(8452) => {
                self.mode.insert(TermMode::SIXEL_CURSOR_TO_RIGHT);
                return;
            },
            PrivateMode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in set_private_mode");
                return;
            },
        };

        trace!("Setting private mode: {mode:?}");
        match mode {
            NamedPrivateMode::UrgencyHints => self.mode.insert(TermMode::URGENCY_HINTS),
            NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                if !self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            },
            NamedPrivateMode::ShowCursor => self.mode.insert(TermMode::SHOW_CURSOR),
            NamedPrivateMode::CursorKeys => self.mode.insert(TermMode::APP_CURSOR),
            // Mouse protocols are mutually exclusive.
            NamedPrivateMode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MODE);
                self.mode.insert(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportFocusInOut => self.mode.insert(TermMode::FOCUS_IN_OUT),
            NamedPrivateMode::BracketedPaste => self.mode.insert(TermMode::BRACKETED_PASTE),
            // Mouse encodings are mutually exclusive.
            NamedPrivateMode::SgrMouse => {
                self.mode.remove(TermMode::UTF8_MOUSE);
                self.mode.insert(TermMode::SGR_MOUSE);
            },
            NamedPrivateMode::Utf8Mouse => {
                self.mode.remove(TermMode::SGR_MOUSE);
                self.mode.insert(TermMode::UTF8_MOUSE);
            },
            NamedPrivateMode::AlternateScroll => self.mode.insert(TermMode::ALTERNATE_SCROLL),
            NamedPrivateMode::LineWrap => self.mode.insert(TermMode::LINE_WRAP),
            NamedPrivateMode::Origin => {
                self.mode.insert(TermMode::ORIGIN);
                self.goto(0, 0);
            },
            NamedPrivateMode::ColumnMode => self.deccolm(),
            NamedPrivateMode::BlinkingCursor => {
                let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
                style.blinking = true;
                self.event_proxy.send_event(Event::CursorBlinkingChange);
            },
            NamedPrivateMode::SyncUpdate => (),
        }
    }

    #[inline]
    fn unset_private_mode(&mut self, mode: PrivateMode) {
        let mode = match mode {
            PrivateMode::Named(mode) => mode,
            PrivateMode::Unknown(80) => {
                self.mode.remove(TermMode::SIXEL_DISPLAY);
                return;
            },
            PrivateMode::Unknown(1070) => {
                self.mode.remove(TermMode::SIXEL_PRIV_PALETTE);
                return;
            },
            PrivateMode::Unknown(8452) => {
                self.mode.remove(TermMode::SIXEL_CURSOR_TO_RIGHT);
                return;
            },
            PrivateMode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in unset_private_mode");
                return;
            },
        };

        trace!("Unsetting private mode: {mode:?}");
        match mode {
            NamedPrivateMode::UrgencyHints => self.mode.remove(TermMode::URGENCY_HINTS),
            NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                if self.mode.contains(TermMode::ALT_SCREEN) {
                    self.swap_alt();
                }
            },
            NamedPrivateMode::ShowCursor => self.mode.remove(TermMode::SHOW_CURSOR),
            NamedPrivateMode::CursorKeys => self.mode.remove(TermMode::APP_CURSOR),
            NamedPrivateMode::ReportMouseClicks => {
                self.mode.remove(TermMode::MOUSE_REPORT_CLICK);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportCellMouseMotion => {
                self.mode.remove(TermMode::MOUSE_DRAG);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportAllMouseMotion => {
                self.mode.remove(TermMode::MOUSE_MOTION);
                self.event_proxy.send_event(Event::MouseCursorDirty);
            },
            NamedPrivateMode::ReportFocusInOut => self.mode.remove(TermMode::FOCUS_IN_OUT),
            NamedPrivateMode::BracketedPaste => self.mode.remove(TermMode::BRACKETED_PASTE),
            NamedPrivateMode::SgrMouse => self.mode.remove(TermMode::SGR_MOUSE),
            NamedPrivateMode::Utf8Mouse => self.mode.remove(TermMode::UTF8_MOUSE),
            NamedPrivateMode::AlternateScroll => self.mode.remove(TermMode::ALTERNATE_SCROLL),
            NamedPrivateMode::LineWrap => self.mode.remove(TermMode::LINE_WRAP),
            NamedPrivateMode::Origin => self.mode.remove(TermMode::ORIGIN),
            NamedPrivateMode::ColumnMode => self.deccolm(),
            NamedPrivateMode::BlinkingCursor => {
                let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
                style.blinking = false;
                self.event_proxy.send_event(Event::CursorBlinkingChange);
            },
            NamedPrivateMode::SyncUpdate => (),
        }
    }

    #[inline]
    fn report_private_mode(&mut self, mode: PrivateMode) {
        trace!("Reporting private mode {mode:?}");
        let state = match mode {
            PrivateMode::Named(mode) => match mode {
                NamedPrivateMode::CursorKeys => self.mode.contains(TermMode::APP_CURSOR).into(),
                NamedPrivateMode::Origin => self.mode.contains(TermMode::ORIGIN).into(),
                NamedPrivateMode::LineWrap => self.mode.contains(TermMode::LINE_WRAP).into(),
                NamedPrivateMode::BlinkingCursor => {
                    let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
                    style.blinking.into()
                },
                NamedPrivateMode::ShowCursor => self.mode.contains(TermMode::SHOW_CURSOR).into(),
                NamedPrivateMode::ReportMouseClicks => {
                    self.mode.contains(TermMode::MOUSE_REPORT_CLICK).into()
                },
                NamedPrivateMode::ReportCellMouseMotion => {
                    self.mode.contains(TermMode::MOUSE_DRAG).into()
                },
                NamedPrivateMode::ReportAllMouseMotion => {
                    self.mode.contains(TermMode::MOUSE_MOTION).into()
                },
                NamedPrivateMode::ReportFocusInOut => {
                    self.mode.contains(TermMode::FOCUS_IN_OUT).into()
                },
                NamedPrivateMode::Utf8Mouse => self.mode.contains(TermMode::UTF8_MOUSE).into(),
                NamedPrivateMode::SgrMouse => self.mode.contains(TermMode::SGR_MOUSE).into(),
                NamedPrivateMode::AlternateScroll => {
                    self.mode.contains(TermMode::ALTERNATE_SCROLL).into()
                },
                NamedPrivateMode::UrgencyHints => {
                    self.mode.contains(TermMode::URGENCY_HINTS).into()
                },
                NamedPrivateMode::SwapScreenAndSetRestoreCursor => {
                    self.mode.contains(TermMode::ALT_SCREEN).into()
                },
                NamedPrivateMode::BracketedPaste => {
                    self.mode.contains(TermMode::BRACKETED_PASTE).into()
                },
                NamedPrivateMode::SyncUpdate => ModeState::Reset,
                NamedPrivateMode::ColumnMode => ModeState::NotSupported,
            },
            PrivateMode::Unknown(_) => ModeState::NotSupported,
        };

        self.event_proxy.send_event(Event::PtyWrite(format!(
            "\x1b[?{};{}$y",
            mode.raw(),
            state as u8,
        )));
    }

    #[inline]
    fn set_mode(&mut self, mode: ansi::Mode) {
        let mode = match mode {
            ansi::Mode::Named(mode) => mode,
            ansi::Mode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in set_mode");
                return;
            },
        };

        trace!("Setting public mode: {mode:?}");
        match mode {
            NamedMode::Insert => self.mode.insert(TermMode::INSERT),
            NamedMode::LineFeedNewLine => self.mode.insert(TermMode::LINE_FEED_NEW_LINE),
        }
    }

    #[inline]
    fn unset_mode(&mut self, mode: ansi::Mode) {
        let mode = match mode {
            ansi::Mode::Named(mode) => mode,
            ansi::Mode::Unknown(mode) => {
                debug!("Ignoring unknown mode {mode} in unset_mode");
                return;
            },
        };

        trace!("Setting public mode: {mode:?}");
        match mode {
            NamedMode::Insert => {
                self.mode.remove(TermMode::INSERT);
                self.mark_fully_damaged();
            },
            NamedMode::LineFeedNewLine => self.mode.remove(TermMode::LINE_FEED_NEW_LINE),
        }
    }

    #[inline]
    fn report_mode(&mut self, mode: ansi::Mode) {
        trace!("Reporting mode {mode:?}");
        let state = match mode {
            ansi::Mode::Named(mode) => match mode {
                NamedMode::Insert => self.mode.contains(TermMode::INSERT).into(),
                NamedMode::LineFeedNewLine => {
                    self.mode.contains(TermMode::LINE_FEED_NEW_LINE).into()
                },
            },
            ansi::Mode::Unknown(_) => ModeState::NotSupported,
        };

        self.event_proxy.send_event(Event::PtyWrite(format!(
            "\x1b[{};{}$y",
            mode.raw(),
            state as u8,
        )));
    }

    #[inline]
    fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>) {
        // Fallback to the last line as default.
        let bottom = bottom.unwrap_or_else(|| self.screen_lines());

        if top >= bottom {
            debug!("Invalid scrolling region: ({top};{bottom})");
            return;
        }

        // Bottom should be included in the range, but range end is not
        // usually included. One option would be to use an inclusive
        // range, but instead we just let the open range end be 1
        // higher.
        let start = Line(top as i32 - 1);
        let end = Line(bottom as i32);

        trace!("Setting scrolling region: ({start};{end})");

        let screen_lines = Line(self.screen_lines() as i32);
        self.scroll_region.start = cmp::min(start, screen_lines);
        self.scroll_region.end = cmp::min(end, screen_lines);
        self.goto(0, 0);
    }

    #[inline]
    fn set_keypad_application_mode(&mut self) {
        trace!("Setting keypad application mode");
        self.mode.insert(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn unset_keypad_application_mode(&mut self) {
        trace!("Unsetting keypad application mode");
        self.mode.remove(TermMode::APP_KEYPAD);
    }

    #[inline]
    fn configure_charset(&mut self, index: CharsetIndex, charset: StandardCharset) {
        trace!("Configuring charset {index:?} as {charset:?}");
        self.grid.cursor.charsets[index] = charset;
    }

    #[inline]
    fn set_active_charset(&mut self, index: CharsetIndex) {
        trace!("Setting active charset {index:?}");
        self.active_charset = index;
    }

    #[inline]
    fn set_cursor_style(&mut self, style: Option<CursorStyle>) {
        trace!("Setting cursor style {style:?}");
        self.cursor_style = style;

        // Notify UI about blinking changes.
        self.event_proxy.send_event(Event::CursorBlinkingChange);
    }

    #[inline]
    fn set_cursor_shape(&mut self, shape: CursorShape) {
        trace!("Setting cursor shape {shape:?}");

        let style = self.cursor_style.get_or_insert(self.config.default_cursor_style);
        style.shape = shape;
    }

    #[inline]
    fn set_title(&mut self, title: Option<String>) {
        trace!("Setting title to '{title:?}'");

        self.title.clone_from(&title);

        let title_event = match title {
            Some(title) => Event::Title(title),
            None => Event::ResetTitle,
        };

        self.event_proxy.send_event(title_event);
    }

    #[inline]
    fn push_title(&mut self) {
        trace!("Pushing '{:?}' onto title stack", self.title);

        if self.title_stack.len() >= TITLE_STACK_MAX_DEPTH {
            let removed = self.title_stack.remove(0);
            trace!(
                "Removing '{removed:?}' from bottom of title stack that exceeds its maximum depth"
            );
        }

        self.title_stack.push(self.title.clone());
    }

    #[inline]
    fn pop_title(&mut self) {
        trace!("Attempting to pop title from stack...");

        if let Some(popped) = self.title_stack.pop() {
            trace!("Title '{popped:?}' popped from stack");
            self.set_title(popped);
        }
    }

    #[inline]
    fn text_area_size_pixels(&mut self) {
        self.event_proxy.send_event(Event::TextAreaSizeRequest(Arc::new(move |window_size| {
            let height = window_size.num_lines * window_size.cell_height;
            let width = window_size.num_cols * window_size.cell_width;
            format!("\x1b[4;{height};{width}t")
        })));
    }

    #[inline]
    fn text_area_size_chars(&mut self) {
        let text = format!("\x1b[8;{};{}t", self.screen_lines(), self.columns());
        self.event_proxy.send_event(Event::PtyWrite(text));
    }

    #[inline]
    fn apc_start(&mut self) {
        self.apc_builder.start();
    }

    #[inline]
    fn apc_put(&mut self, bytes: &[u8]) {
        self.apc_builder.put(bytes);
    }

    fn apc_end(&mut self) {
        // A CAN/SUB abort also surfaces as `apc_end`; the partial payload
        // then fails to parse and is dropped silently, like any malformed
        // command.
        if let Some((payload, overflowed)) = self.apc_builder.end()
            && let Some((b'G', body)) = payload.split_first()
        {
            self.kitty_graphics_command(body, overflowed);
        }
    }

    fn dcs_hook(&mut self, params: &Params, _intermediates: &[u8], _ignore: bool, action: char) {
        self.dcs_builder.reset();
        if action == 'q' {
            let mut iter = params.iter();
            // P1 (aspect ratio) and P3 (grid size) are ignored; only P2 (transparency) is used.
            let p2 = iter.nth(1).and_then(|s| s.iter().next().copied()).unwrap_or(0);
            self.dcs_builder.start(p2);
        }
    }

    #[inline]
    fn dcs_put(&mut self, byte: u8) {
        self.dcs_builder.put(byte);
    }

    fn dcs_unhook(&mut self) {
        if let Some((payload, p2, overflowed)) = self.dcs_builder.end() {
            self.sixel_dcs_command(payload, p2, overflowed);
        }
    }

    fn osc_1337_raw(&mut self, payload: &[u8]) {
        use crate::graphics::iterm::{FileArgs, split_osc1337};

        if !self.config.graphics.iterm2_enabled() {
            return;
        }

        let (keyword, header, b64) = match split_osc1337(payload) {
            Some(v) => v,
            None => {
                debug!("[iterm2] malformed OSC 1337 payload");
                return;
            },
        };

        match keyword {
            b"File" => {
                let args = FileArgs::parse(header);
                self.iterm_display(args, b64);
            },
            b"MultipartFile" => {
                let args = FileArgs::parse(header);
                self.iterm_multipart.start(args, b64);
            },
            b"FilePart" => {
                self.iterm_multipart.append(b64);
            },
            b"FileEnd" => {
                if let Some((args, full_b64)) = self.iterm_multipart.finish(b64) {
                    self.iterm_display(args, &full_b64);
                }
            },
            _ => {
                debug!("[iterm2] unknown OSC 1337 keyword: {:?}", keyword);
            },
        }
    }
}

/// The state of the [`Mode`] and [`PrivateMode`].
#[repr(u8)]
#[derive(Debug, Clone, Copy)]
enum ModeState {
    /// The mode is not supported.
    NotSupported = 0,
    /// The mode is currently set.
    Set = 1,
    /// The mode is currently not set.
    Reset = 2,
}

impl From<bool> for ModeState {
    fn from(value: bool) -> Self {
        if value { Self::Set } else { Self::Reset }
    }
}

/// Terminal version for escape sequence reports.
///
/// This returns the current terminal version as a unique number based on alacritty_terminal's
/// semver version. The different versions are padded to ensure that a higher semver version will
/// always report a higher version number.
fn version_number(mut version: &str) -> usize {
    if let Some(separator) = version.rfind('-') {
        version = &version[..separator];
    }

    let mut version_number = 0;

    let semver_versions = version.split('.');
    for (i, semver_version) in semver_versions.rev().enumerate() {
        let semver_number = semver_version.parse::<usize>().unwrap_or(0);
        version_number += usize::pow(100, i as u32) * semver_number;
    }

    version_number
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardType {
    Clipboard,
    Selection,
}

struct TabStops {
    tabs: Vec<bool>,
}

impl TabStops {
    #[inline]
    fn new(columns: usize) -> TabStops {
        TabStops { tabs: (0..columns).map(|i| i % INITIAL_TABSTOPS == 0).collect() }
    }

    /// Remove all tabstops.
    #[inline]
    fn clear_all(&mut self) {
        unsafe {
            ptr::write_bytes(self.tabs.as_mut_ptr(), 0, self.tabs.len());
        }
    }

    /// Increase tabstop capacity.
    #[inline]
    fn resize(&mut self, columns: usize) {
        let mut index = self.tabs.len();
        self.tabs.resize_with(columns, || {
            let is_tabstop = index.is_multiple_of(INITIAL_TABSTOPS);
            index += 1;
            is_tabstop
        });
    }
}

impl Index<Column> for TabStops {
    type Output = bool;

    fn index(&self, index: Column) -> &bool {
        &self.tabs[index.0]
    }
}

impl IndexMut<Column> for TabStops {
    fn index_mut(&mut self, index: Column) -> &mut bool {
        self.tabs.index_mut(index.0)
    }
}

/// Terminal cursor rendering information.
#[derive(Copy, Clone, PartialEq, Eq)]
pub struct RenderableCursor {
    pub shape: CursorShape,
    pub point: Point,
}

impl RenderableCursor {
    fn new<T>(term: &Term<T>) -> Self {
        // Cursor position.
        let vi_mode = term.mode().contains(TermMode::VI);
        let mut point = if vi_mode { term.vi_mode_cursor.point } else { term.grid.cursor.point };
        if term.grid[point].flags.contains(Flags::WIDE_CHAR_SPACER) {
            point.column -= 1;
        }

        // Cursor shape.
        let shape = if !vi_mode && !term.mode().contains(TermMode::SHOW_CURSOR) {
            CursorShape::Hidden
        } else {
            term.cursor_style().shape
        };

        Self { shape, point }
    }
}

/// Visible terminal content.
///
/// This contains all content required to render the current terminal view.
pub struct RenderableContent<'a> {
    pub display_iter: GridIterator<'a, Cell>,
    pub selection: Option<SelectionRange>,
    pub cursor: RenderableCursor,
    pub display_offset: usize,
    pub colors: &'a color::Colors,
    pub mode: TermMode,
}

impl<'a> RenderableContent<'a> {
    fn new<T>(term: &'a Term<T>) -> Self {
        Self {
            display_iter: term.grid().display_iter(),
            display_offset: term.grid().display_offset(),
            cursor: RenderableCursor::new(term),
            selection: term.selection.as_ref().and_then(|s| s.to_range(term)),
            colors: &term.colors,
            mode: *term.mode(),
        }
    }
}

/// Terminal test helpers.
pub mod test {
    use super::*;

    #[cfg(feature = "serde")]
    use serde::{Deserialize, Serialize};

    use crate::event::VoidListener;

    #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
    pub struct TermSize {
        pub columns: usize,
        pub screen_lines: usize,
    }

    impl TermSize {
        pub fn new(columns: usize, screen_lines: usize) -> Self {
            Self { columns, screen_lines }
        }
    }

    impl Dimensions for TermSize {
        fn total_lines(&self) -> usize {
            self.screen_lines()
        }

        fn screen_lines(&self) -> usize {
            self.screen_lines
        }

        fn columns(&self) -> usize {
            self.columns
        }
    }

    /// Construct a terminal from its content as string.
    ///
    /// A `\n` will break line and `\r\n` will break line without wrapping.
    ///
    /// # Examples
    ///
    /// ```rust
    /// use alacritty_terminal::term::test::mock_term;
    ///
    /// // Create a terminal with the following cells:
    /// //
    /// // [h][e][l][l][o] <- WRAPLINE flag set
    /// // [:][)][ ][ ][ ]
    /// // [t][e][s][t][ ]
    /// mock_term(
    ///     "\
    ///     hello\n:)\r\ntest",
    /// );
    /// ```
    pub fn mock_term(content: &str) -> Term<VoidListener> {
        let lines: Vec<&str> = content.split('\n').collect();
        let num_cols = lines
            .iter()
            .map(|line| line.chars().filter(|c| *c != '\r').map(|c| c.width().unwrap()).sum())
            .max()
            .unwrap_or(0);

        // Create terminal with the appropriate dimensions.
        let size = TermSize::new(num_cols, lines.len());
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Fill terminal with content.
        for (line, text) in lines.iter().enumerate() {
            let line = Line(line as i32);
            if !text.ends_with('\r') && line + 1 != lines.len() {
                term.grid[line][Column(num_cols - 1)].flags.insert(Flags::WRAPLINE);
            }

            let mut index = 0;
            for c in text.chars().take_while(|c| *c != '\r') {
                term.grid[line][Column(index)].c = c;

                // Handle fullwidth characters.
                let width = c.width().unwrap();
                if width == 2 {
                    term.grid[line][Column(index)].flags.insert(Flags::WIDE_CHAR);
                    term.grid[line][Column(index + 1)].flags.insert(Flags::WIDE_CHAR_SPACER);
                }

                index += width;
            }
        }

        term
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::mem;

    use crate::event::VoidListener;
    use crate::grid::{Grid, Scroll};
    use crate::index::{Column, Point, Side};
    use crate::selection::{Selection, SelectionType};
    use crate::term::cell::{Cell, Flags};
    use crate::term::test::TermSize;
    use crate::vte::ansi::{self, CharsetIndex, Handler, StandardCharset};

    #[test]
    fn scroll_display_page_up() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Scrollable amount to top is 11.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-1), Column(0)));
        assert_eq!(term.grid.display_offset(), 10);

        // Scrollable amount to top is 1.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-2), Column(0)));
        assert_eq!(term.grid.display_offset(), 11);

        // Scrollable amount to top is 0.
        term.scroll_display(Scroll::PageUp);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-2), Column(0)));
        assert_eq!(term.grid.display_offset(), 11);
    }

    #[test]
    fn scroll_display_page_down() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Change display_offset to topmost.
        term.grid_mut().scroll_display(Scroll::Top);
        term.vi_mode_cursor = ViModeCursor::new(Point::new(Line(-11), Column(0)));

        // Scrollable amount to bottom is 11.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-1), Column(0)));
        assert_eq!(term.grid.display_offset(), 1);

        // Scrollable amount to bottom is 1.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(0)));
        assert_eq!(term.grid.display_offset(), 0);

        // Scrollable amount to bottom is 0.
        term.scroll_display(Scroll::PageDown);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(0)));
        assert_eq!(term.grid.display_offset(), 0);
    }

    #[test]
    fn simple_selection_works() {
        let size = TermSize::new(5, 5);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let grid = term.grid_mut();
        for i in 0..4 {
            if i == 1 {
                continue;
            }

            grid[Line(i)][Column(0)].c = '"';

            for j in 1..4 {
                grid[Line(i)][Column(j)].c = 'a';
            }

            grid[Line(i)][Column(4)].c = '"';
        }
        grid[Line(2)][Column(0)].c = ' ';
        grid[Line(2)][Column(4)].c = ' ';
        grid[Line(2)][Column(4)].flags.insert(Flags::WRAPLINE);
        grid[Line(3)][Column(0)].c = ' ';

        // Multiple lines contain an empty line.
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point { line: Line(0), column: Column(0) },
            Side::Left,
        ));
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(2), column: Column(4) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\"aaa\"\n\n aaa ")));

        // A wrapline.
        term.selection = Some(Selection::new(
            SelectionType::Simple,
            Point { line: Line(2), column: Column(0) },
            Side::Left,
        ));
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(4) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from(" aaa  aaa\"")));
    }

    #[test]
    fn semantic_selection_works() {
        let size = TermSize::new(5, 3);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut grid: Grid<Cell> = Grid::new(3, 5, 0);
        for i in 0..5 {
            for j in 0..2 {
                grid[Line(j)][Column(i)].c = 'a';
            }
        }
        grid[Line(0)][Column(0)].c = '"';
        grid[Line(0)][Column(3)].c = '"';
        grid[Line(1)][Column(2)].c = '"';
        grid[Line(0)][Column(4)].flags.insert(Flags::WRAPLINE);

        let mut escape_chars = String::from("\"");

        mem::swap(&mut term.grid, &mut grid);
        mem::swap(&mut term.config.semantic_escape_chars, &mut escape_chars);

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: Line(0), column: Column(1) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: Line(0), column: Column(4) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }

        {
            term.selection = Some(Selection::new(
                SelectionType::Semantic,
                Point { line: Line(1), column: Column(1) },
                Side::Left,
            ));
            assert_eq!(term.selection_to_string(), Some(String::from("aaa")));
        }
    }

    #[test]
    fn line_selection_works() {
        let size = TermSize::new(5, 1);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let mut grid: Grid<Cell> = Grid::new(1, 5, 0);
        for i in 0..5 {
            grid[Line(0)][Column(i)].c = 'a';
        }
        grid[Line(0)][Column(0)].c = '"';
        grid[Line(0)][Column(3)].c = '"';

        mem::swap(&mut term.grid, &mut grid);

        term.selection = Some(Selection::new(
            SelectionType::Lines,
            Point { line: Line(0), column: Column(3) },
            Side::Left,
        ));
        assert_eq!(term.selection_to_string(), Some(String::from("\"aa\"a\n")));
    }

    #[test]
    fn block_selection_works() {
        let size = TermSize::new(5, 5);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let grid = term.grid_mut();
        for i in 1..4 {
            grid[Line(i)][Column(0)].c = '"';

            for j in 1..4 {
                grid[Line(i)][Column(j)].c = 'a';
            }

            grid[Line(i)][Column(4)].c = '"';
        }
        grid[Line(2)][Column(2)].c = ' ';
        grid[Line(2)][Column(4)].flags.insert(Flags::WRAPLINE);
        grid[Line(3)][Column(4)].c = ' ';

        term.selection = Some(Selection::new(
            SelectionType::Block,
            Point { line: Line(0), column: Column(3) },
            Side::Left,
        ));

        // The same column.
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(3) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\na\na\na")));

        // The first column.
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(0) }, Side::Left);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\n\"aa\n\"a\n\"aa")));

        // The last column.
        if let Some(s) = term.selection.as_mut() {
            s.update(Point { line: Line(3), column: Column(4) }, Side::Right);
        }
        assert_eq!(term.selection_to_string(), Some(String::from("\na\"\na\"\na")));
    }

    /// Check that the grid can be serialized back and forth losslessly.
    ///
    /// This test is in the term module as opposed to the grid since we want to
    /// test this property with a T=Cell.
    #[test]
    #[cfg(feature = "serde")]
    fn grid_serde() {
        let grid: Grid<Cell> = Grid::new(24, 80, 0);
        let serialized = serde_json::to_string(&grid).expect("ser");
        let deserialized = serde_json::from_str::<Grid<Cell>>(&serialized).expect("de");

        assert_eq!(deserialized, grid);
    }

    #[test]
    fn input_line_drawing_character() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let cursor = Point::new(Line(0), Column(0));
        term.configure_charset(CharsetIndex::G0, StandardCharset::SpecialCharacterAndLineDrawing);
        term.input('a');

        assert_eq!(term.grid()[cursor].c, '▒');
    }

    #[test]
    fn clearing_viewport_keeps_history_position() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Change the display area.
        term.scroll_display(Scroll::Top);

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the viewport.
        term.clear_screen(ansi::ClearMode::All);

        assert_eq!(term.grid.display_offset(), 10);
    }

    #[test]
    fn clearing_viewport_with_vi_mode_keeps_history_position() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        // Change the display area and the vi cursor position.
        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point = Point::new(Line(-5), Column(3));

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the viewport.
        term.clear_screen(ansi::ClearMode::All);

        assert_eq!(term.grid.display_offset(), 10);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(-5), Column(3)));
    }

    #[test]
    fn clearing_scrollback_resets_display_offset() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Change the display area.
        term.scroll_display(Scroll::Top);

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the scrollback buffer.
        term.clear_screen(ansi::ClearMode::Saved);

        assert_eq!(term.grid.display_offset(), 0);
    }

    #[test]
    fn clearing_scrollback_sets_vi_cursor_into_viewport() {
        let size = TermSize::new(10, 20);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..29 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        // Change the display area and the vi cursor position.
        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point = Point::new(Line(-5), Column(3));

        assert_eq!(term.grid.display_offset(), 10);

        // Clear the scrollback buffer.
        term.clear_screen(ansi::ClearMode::Saved);

        assert_eq!(term.grid.display_offset(), 0);
        assert_eq!(term.vi_mode_cursor.point, Point::new(Line(0), Column(3)));
    }

    #[test]
    fn clear_saved_lines() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Add one line of scrollback.
        term.grid.scroll_up(&(Line(0)..Line(1)), 1);

        // Clear the history.
        term.clear_screen(ansi::ClearMode::Saved);

        // Make sure that scrolling does not change the grid.
        let mut scrolled_grid = term.grid.clone();
        scrolled_grid.scroll_display(Scroll::Top);

        // Truncate grids for comparison.
        scrolled_grid.truncate();
        term.grid.truncate();

        assert_eq!(term.grid, scrolled_grid);
    }

    #[test]
    fn vi_cursor_keep_pos_on_scrollback_buffer() {
        let size = TermSize::new(5, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 11 lines of scrollback.
        for _ in 0..20 {
            term.newline();
        }

        // Enable vi mode.
        term.toggle_vi_mode();

        term.scroll_display(Scroll::Top);
        term.vi_mode_cursor.point.line = Line(-11);

        term.linefeed();
        assert_eq!(term.vi_mode_cursor.point.line, Line(-12));
    }

    #[test]
    fn grow_lines_updates_active_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.screen_lines = 30;
        term.resize(size);

        assert_eq!(term.history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn grow_lines_updates_inactive_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        // Increase visible lines.
        size.screen_lines = 30;
        term.resize(size);

        // Leave alt screen.
        term.unset_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        assert_eq!(term.history_size(), 0);
        assert_eq!(term.grid.cursor.point, Point::new(Line(19), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_active_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Increase visible lines.
        size.screen_lines = 5;
        term.resize(size);

        assert_eq!(term.history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn shrink_lines_updates_inactive_cursor_pos() {
        let mut size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Create 10 lines of scrollback.
        for _ in 0..19 {
            term.newline();
        }
        assert_eq!(term.history_size(), 10);
        assert_eq!(term.grid.cursor.point, Point::new(Line(9), Column(0)));

        // Enter alt screen.
        term.set_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        // Increase visible lines.
        size.screen_lines = 5;
        term.resize(size);

        // Leave alt screen.
        term.unset_private_mode(NamedPrivateMode::SwapScreenAndSetRestoreCursor.into());

        assert_eq!(term.history_size(), 15);
        assert_eq!(term.grid.cursor.point, Point::new(Line(4), Column(0)));
    }

    #[test]
    fn damage_public_usage() {
        let size = TermSize::new(10, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        // Reset terminal for partial damage tests since it's initialized as fully damaged.
        term.reset_damage();

        // Test that we damage input form [`Term::input`].

        let left = term.grid.cursor.point.column.0;
        term.input('d');
        term.input('a');
        term.input('m');
        term.input('a');
        term.input('g');
        term.input('e');
        let right = term.grid.cursor.point.column.0;

        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(damaged_lines.next(), Some(LineDamageBounds { line: 0, left, right }));
        assert_eq!(damaged_lines.next(), None);
        term.reset_damage();

        // Create scrollback.
        for _ in 0..20 {
            term.newline();
        }

        match term.damage() {
            TermDamage::Full => (),
            TermDamage::Partial(_) => panic!("Expected Full damage, however got Partial "),
        };
        term.reset_damage();

        term.scroll_display(Scroll::Delta(10));
        term.reset_damage();

        // No damage when scrolled into viewport.
        for idx in 0..term.columns() {
            term.goto(idx as i32, idx);
        }
        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(damaged_lines.next(), None);

        // Scroll back into the viewport, so we have 2 visible lines which terminal can write
        // to.
        term.scroll_display(Scroll::Delta(-2));
        term.reset_damage();

        term.goto(0, 0);
        term.goto(1, 0);
        term.goto(2, 0);
        let display_offset = term.grid().display_offset();
        let mut damaged_lines = match term.damage() {
            TermDamage::Full => panic!("Expected partial damage, however got Full"),
            TermDamage::Partial(damaged_lines) => damaged_lines,
        };
        assert_eq!(
            damaged_lines.next(),
            Some(LineDamageBounds { line: display_offset, left: 0, right: 0 })
        );
        assert_eq!(
            damaged_lines.next(),
            Some(LineDamageBounds { line: display_offset + 1, left: 0, right: 0 })
        );
        assert_eq!(damaged_lines.next(), None);
    }

    #[test]
    fn damage_cursor_movements() {
        let size = TermSize::new(10, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);
        let num_cols = term.columns();
        // Reset terminal for partial damage tests since it's initialized as fully damaged.
        term.reset_damage();

        term.goto(1, 1);

        // NOTE While we can use `[Term::damage]` to access terminal damage information, in the
        // following tests we will be accessing `term.damage.lines` directly to avoid adding extra
        // damage information (like cursor and Vi cursor), which we're not testing.

        assert_eq!(term.damage.lines[0], LineDamageBounds { line: 0, left: 0, right: 0 });
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 1, right: 1 });
        term.damage.reset(num_cols);

        term.move_forward(3);
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 1, right: 4 });
        term.damage.reset(num_cols);

        term.move_backward(8);
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 0, right: 4 });
        term.goto(5, 5);
        term.damage.reset(num_cols);

        term.backspace();
        term.backspace();
        assert_eq!(term.damage.lines[5], LineDamageBounds { line: 5, left: 3, right: 5 });
        term.damage.reset(num_cols);

        term.move_up(1);
        assert_eq!(term.damage.lines[5], LineDamageBounds { line: 5, left: 3, right: 3 });
        assert_eq!(term.damage.lines[4], LineDamageBounds { line: 4, left: 3, right: 3 });
        term.damage.reset(num_cols);

        term.move_down(1);
        term.move_down(1);
        assert_eq!(term.damage.lines[4], LineDamageBounds { line: 4, left: 3, right: 3 });
        assert_eq!(term.damage.lines[5], LineDamageBounds { line: 5, left: 3, right: 3 });
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 3, right: 3 });
        term.damage.reset(num_cols);

        term.wrapline();
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 3, right: 3 });
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 0 });
        term.move_forward(3);
        term.move_up(1);
        term.damage.reset(num_cols);

        term.linefeed();
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 3, right: 3 });
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 3, right: 3 });
        term.damage.reset(num_cols);

        term.carriage_return();
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 3 });
        term.damage.reset(num_cols);

        term.erase_chars(5);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 5 });
        term.damage.reset(num_cols);

        term.delete_chars(3);
        let right = term.columns() - 1;
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right });
        term.move_forward(term.columns());
        term.damage.reset(num_cols);

        term.move_backward_tabs(1);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right });
        term.save_cursor_position();
        term.goto(1, 1);
        term.damage.reset(num_cols);

        term.restore_cursor_position();
        assert_eq!(term.damage.lines[1], LineDamageBounds { line: 1, left: 1, right: 1 });
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right: 8 });
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::All);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right });
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::Left);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 0, right: 8 });
        term.damage.reset(num_cols);

        term.clear_line(ansi::LineClearMode::Right);
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right });
        term.damage.reset(num_cols);

        term.reverse_index();
        assert_eq!(term.damage.lines[7], LineDamageBounds { line: 7, left: 8, right: 8 });
        assert_eq!(term.damage.lines[6], LineDamageBounds { line: 6, left: 8, right: 8 });
    }

    #[test]
    fn full_damage() {
        let size = TermSize::new(100, 10);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        assert!(term.damage.full);
        for _ in 0..20 {
            term.newline();
        }
        term.reset_damage();

        term.clear_screen(ansi::ClearMode::Above);
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_display(Scroll::Top);
        assert!(term.damage.full);
        term.reset_damage();

        // Sequential call to scroll display without doing anything shouldn't damage.
        term.scroll_display(Scroll::Top);
        assert!(!term.damage.full);
        term.reset_damage();

        term.set_options(Config::default());
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_down_relative(Line(5), 2);
        assert!(term.damage.full);
        term.reset_damage();

        term.scroll_up_relative(Line(3), 2);
        assert!(term.damage.full);
        term.reset_damage();

        term.deccolm();
        assert!(term.damage.full);
        term.reset_damage();

        term.decaln();
        assert!(term.damage.full);
        term.reset_damage();

        term.set_mode(NamedMode::Insert.into());
        // Just setting `Insert` mode shouldn't mark terminal as damaged.
        assert!(!term.damage.full);
        term.reset_damage();

        let color_index = 257;
        term.set_color(color_index, Rgb::default());
        assert!(term.damage.full);
        term.reset_damage();

        // Setting the same color once again shouldn't trigger full damage.
        term.set_color(color_index, Rgb::default());
        assert!(!term.damage.full);

        term.reset_color(color_index);
        assert!(term.damage.full);
        term.reset_damage();

        // We shouldn't trigger fully damage when cursor gets update.
        term.set_color(NamedColor::Cursor as usize, Rgb::default());
        assert!(!term.damage.full);

        // However requesting terminal damage should mark terminal as fully damaged in `Insert`
        // mode.
        let _ = term.damage();
        assert!(term.damage.full);
        term.reset_damage();

        term.unset_mode(NamedMode::Insert.into());
        assert!(term.damage.full);
        term.reset_damage();

        // Keep this as a last check, so we don't have to deal with restoring from alt-screen.
        term.swap_alt();
        assert!(term.damage.full);
        term.reset_damage();

        let size = TermSize::new(10, 10);
        term.resize(size);
        assert!(term.damage.full);
    }

    #[test]
    fn window_title() {
        let size = TermSize::new(7, 17);
        let mut term = Term::new(Config::default(), &size, VoidListener);

        // Title None by default.
        assert_eq!(term.title, None);

        // Title can be set.
        term.set_title(Some("Test".into()));
        assert_eq!(term.title, Some("Test".into()));

        // Title can be pushed onto stack.
        term.push_title();
        term.set_title(Some("Next".into()));
        assert_eq!(term.title, Some("Next".into()));
        assert_eq!(term.title_stack.first().unwrap(), &Some("Test".into()));

        // Title can be popped from stack and set as the window title.
        term.pop_title();
        assert_eq!(term.title, Some("Test".into()));
        assert!(term.title_stack.is_empty());

        // Title stack doesn't grow infinitely.
        for _ in 0..4097 {
            term.push_title();
        }
        assert_eq!(term.title_stack.len(), 4096);

        // Title and title stack reset when terminal state is reset.
        term.push_title();
        term.reset_state();
        assert_eq!(term.title, None);
        assert!(term.title_stack.is_empty());

        // Title stack pops back to default.
        term.title = None;
        term.push_title();
        term.set_title(Some("Test".into()));
        term.pop_title();
        assert_eq!(term.title, None);

        // Title can be reset to default.
        term.title = Some("Test".into());
        term.set_title(None);
        assert_eq!(term.title, None);
    }

    #[test]
    fn parse_cargo_version() {
        assert!(version_number(env!("CARGO_PKG_VERSION")) >= 10_01);
        assert_eq!(version_number("0.0.1-dev"), 1);
        assert_eq!(version_number("0.1.2-dev"), 1_02);
        assert_eq!(version_number("1.2.3-dev"), 1_02_03);
        assert_eq!(version_number("999.99.99"), 9_99_99_99);
    }

    /// Kitty graphics protocol integration tests: raw APC byte streams are
    /// fed through `Processor::advance` and the exact PTY responses are
    /// asserted.
    mod graphics {
        use std::cell::RefCell;
        use std::rc::Rc;

        use super::*;

        #[derive(Default, Clone)]
        struct PtyWriteCapture {
            written: Rc<RefCell<Vec<String>>>,
        }

        impl EventListener for PtyWriteCapture {
            fn send_event(&self, event: Event) {
                if let Event::PtyWrite(text) = event {
                    self.written.borrow_mut().push(text);
                }
            }
        }

        struct GraphicsTerm {
            term: Term<PtyWriteCapture>,
            parser: ansi::Processor,
            written: Rc<RefCell<Vec<String>>>,
        }

        impl GraphicsTerm {
            fn new() -> Self {
                Self::with_config(Config::default())
            }

            fn with_config(config: Config) -> Self {
                let listener = PtyWriteCapture::default();
                let written = listener.written.clone();
                let term = Term::new(config, &TermSize::new(20, 10), listener);
                Self { term, parser: ansi::Processor::new(), written }
            }

            fn advance(&mut self, bytes: &[u8]) {
                self.parser.advance(&mut self.term, bytes);
            }

            fn responses(&self) -> Vec<String> {
                self.written.borrow().clone()
            }
        }

        /// 1x1 RGBA pixel: `f=32,s=1,v=1` plus the base64 of 4 bytes.
        const PIXEL: &str = "f=32,s=1,v=1;/wAA/w==";

        #[test]
        fn explicit_id_success() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=31,{PIXEL}\x1b\\").as_bytes());

            assert_eq!(t.responses(), vec!["\x1b_Gi=31;OK\x1b\\".to_string()]);
            assert_eq!(t.term.graphics().len(), 1);
        }

        #[test]
        fn image_number_echoes_assigned_id() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_GI=5,{PIXEL}\x1b\\").as_bytes());

            // The smallest free client id (1) is assigned and echoed with I=.
            assert_eq!(t.responses(), vec!["\x1b_Gi=1,I=5;OK\x1b\\".to_string()]);
        }

        #[test]
        fn quiet_one_suppresses_ok_but_not_errors() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=2,q=1,{PIXEL}\x1b\\").as_bytes());
            assert_eq!(t.responses(), Vec::<String>::new());

            // Unknown format => EINVAL, still reported with q=1.
            t.advance(b"\x1b_Gi=3,q=1,f=7,s=1,v=1;AAAA\x1b\\");
            let responses = t.responses();
            assert_eq!(responses.len(), 1);
            assert!(responses[0].starts_with("\x1b_Gi=3;EINVAL:"), "{responses:?}");
        }

        #[test]
        fn quiet_two_suppresses_everything() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=2,q=2,{PIXEL}\x1b\\").as_bytes());
            t.advance(b"\x1b_Gi=3,q=2,f=7,s=1,v=1;AAAA\x1b\\");

            assert_eq!(t.responses(), Vec::<String>::new());
        }

        #[test]
        fn anonymous_command_is_silent() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_G{PIXEL}\x1b\\").as_bytes());

            // Loaded, but no id/number => no response.
            assert_eq!(t.responses(), Vec::<String>::new());
            assert_eq!(t.term.graphics().len(), 1);
        }

        #[test]
        fn chunked_transmission_responds_only_on_final_chunk() {
            let mut t = GraphicsTerm::new();
            // 1x2 RGBA image, 8 bytes split into two 4-byte chunks.
            t.advance(b"\x1b_Gi=7,f=32,s=1,v=2,m=1;/wAA/w==\x1b\\");
            assert_eq!(t.responses(), Vec::<String>::new());

            t.advance(b"\x1b_Gm=0;AP8A/w==\x1b\\");
            assert_eq!(t.responses(), vec!["\x1b_Gi=7;OK\x1b\\".to_string()]);
            assert_eq!(t.term.graphics().image_by_client_id(7).unwrap().height, 2);
        }

        #[test]
        fn parse_errors_are_silent() {
            let mut t = GraphicsTerm::new();
            // `j` is not a valid key.
            t.advance(b"\x1b_Gj=1,i=9;AAAA\x1b\\");
            // A CAN abort surfaces as a partial payload; truncated base64
            // (dangling sextet) is a parse error and stays silent.
            t.advance(b"\x1b_Gi=9,f=32,s=1,v=1;/wAA/\x18");

            assert_eq!(t.responses(), Vec::<String>::new());
        }

        #[test]
        fn id_and_number_together_is_einval() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b_Gi=1,I=2,f=32,s=1,v=1;/wAA/w==\x1b\\");

            assert_eq!(t.responses(), vec![
                "\x1b_Gi=1,I=2;EINVAL:Must not specify both image id and image number\x1b\\"
                    .to_string()
            ]);
        }

        #[test]
        fn query_success_stores_nothing() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Ga=q,i=31,{PIXEL}\x1b\\").as_bytes());

            assert_eq!(t.responses(), vec!["\x1b_Gi=31;OK\x1b\\".to_string()]);
            assert!(t.term.graphics().is_empty());
            assert!(t.term.graphics().pending_uploads.is_empty());
        }

        #[test]
        fn notcurses_self_rgba_probe_answers_ok() {
            // The exact capability-detection probe notcurses sends at startup:
            // a 1x1 24-bit RGB pixel (f=24, 3 bytes -> base64 "AAAA") with the
            // query action. notcurses expects `ESC _ G i=1 ; OK ESC \` back to
            // conclude the terminal supports the kitty graphics protocol.
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b_Gi=1,a=q,s=1,v=1,f=24;AAAA\x1b\\");

            assert_eq!(t.responses(), vec!["\x1b_Gi=1;OK\x1b\\".to_string()]);
            // A query must not persist the probe image.
            assert!(t.term.graphics().is_empty());
        }

        #[test]
        fn query_error_is_reported() {
            let mut t = GraphicsTerm::new();
            // Declared 1x2 but only one pixel transmitted => ENODATA.
            t.advance(b"\x1b_Ga=q,i=32,f=32,s=1,v=2;/wAA/w==\x1b\\");

            let responses = t.responses();
            assert_eq!(responses.len(), 1);
            assert!(responses[0].starts_with("\x1b_Gi=32;ENODATA:"), "{responses:?}");
            assert!(t.term.graphics().is_empty());
        }

        #[test]
        fn query_without_id_is_silent() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Ga=q,{PIXEL}\x1b\\").as_bytes());

            assert_eq!(t.responses(), Vec::<String>::new());
        }

        #[test]
        fn query_response_precedes_da1_in_one_batch() {
            let mut t = GraphicsTerm::new();
            // notcurses-style detection: a=q followed by DA1 in one batch;
            // it hangs if the DA1 response overtakes the graphics response.
            t.advance(format!("\x1b_Ga=q,i=44,{PIXEL}\x1b\\\x1b[c").as_bytes());

            assert_eq!(t.responses(), vec![
                "\x1b_Gi=44;OK\x1b\\".to_string(),
                "\x1b[?62;4;c".to_string()
            ]);
        }

        #[test]
        fn transmit_and_display_places_and_moves_cursor() {
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.term.reset_damage();
            t.advance(b"\x1b_Ga=T,i=8,p=3,f=32,s=1,v=1;/wAA/w==\x1b\\");

            assert_eq!(t.responses(), vec!["\x1b_Gi=8,p=3;OK\x1b\\".to_string()]);
            let image = t.term.graphics().image_by_client_id(8).unwrap();
            assert_eq!(image.placements().len(), 1);
            // 1x1 image in 1x1 px cells covers one cell; rows - 1 == 0.
            assert_eq!(t.term.grid.cursor.point, Point::new(Line(0), Column(1)));
            assert!(t.term.damage.full);
        }

        #[test]
        fn put_places_existing_image() {
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=4,{PIXEL}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=4,p=9\x1b\\");

            assert_eq!(t.responses(), vec![
                "\x1b_Gi=4;OK\x1b\\".to_string(),
                "\x1b_Gi=4,p=9;OK\x1b\\".to_string()
            ]);
            let image = t.term.graphics().image_by_client_id(4).unwrap();
            assert_eq!(image.placements().len(), 1);
        }

        #[test]
        fn put_missing_image_is_enoent() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b_Ga=p,i=77\x1b\\");

            assert_eq!(t.responses(), vec![
                "\x1b_Gi=77;ENOENT:Put command refers to non-existent image with id: 77 and \
                 number: 0\x1b\\"
                    .to_string()
            ]);
        }

        #[test]
        fn oversized_apc_is_efbig() {
            let mut t = GraphicsTerm::new();
            t.term.apc_builder.limit = 16;
            t.advance(format!("\x1b_Gi=6,{}AAAA\x1b\\", PIXEL).as_bytes());

            let responses = t.responses();
            assert_eq!(responses.len(), 1);
            assert!(responses[0].starts_with("\x1b_Gi=6;EFBIG:"), "{responses:?}");
        }

        #[test]
        fn unicode_placeholder_put_creates_virtual_placement() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=4,{PIXEL}\x1b\\").as_bytes());
            let cursor_before = t.term.grid.cursor.point;
            t.advance(b"\x1b_Ga=p,i=4,U=1\x1b\\");

            // Virtual placement: OK response, cursor unchanged.
            let responses = t.responses();
            assert_eq!(responses.len(), 2, "expected transmit OK + place OK");
            assert_eq!(responses[1], "\x1b_Gi=4;OK\x1b\\", "U=1 must succeed");
            assert_eq!(t.term.grid.cursor.point, cursor_before, "U=1 must not move cursor");

            // Placement exists and is marked virtual.
            let img = t.term.graphics().image_by_client_id(4).unwrap();
            assert_eq!(img.placements().len(), 1);
            assert!(img.placements()[0].is_virtual, "U=1 placement must be virtual");
        }

        impl GraphicsTerm {
            /// Transmit and display a 1x1 image as `i=`/`p=` with 1px cells.
            fn place(&mut self, image: u32, placement: u32) {
                self.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
                self.advance(format!("\x1b_Ga=T,i={image},p={placement},{PIXEL}\x1b\\").as_bytes());
            }
        }

        #[test]
        fn deletes_never_respond() {
            let mut t = GraphicsTerm::new();
            t.place(1, 1);
            assert_eq!(t.responses().len(), 1);

            // Successful, no-op, and unsupported-specifier deletes alike are
            // silent (kitty's `case 'd'` never builds a response).
            t.advance(b"\x1b_Ga=d,d=i,i=1\x1b\\");
            t.advance(b"\x1b_Ga=d,d=I,i=99\x1b\\");
            t.advance(b"\x1b_Ga=d,d=X,i=1,x=1\x1b\\");
            assert_eq!(t.responses().len(), 1);
        }

        #[test]
        fn delete_by_id_lowercase_keeps_data_uppercase_frees() {
            let mut t = GraphicsTerm::new();
            t.place(1, 1);

            t.advance(b"\x1b_Ga=d,d=i,i=1\x1b\\");
            let image = t.term.graphics().image_by_client_id(1).unwrap();
            assert!(image.placements().is_empty());
            let internal = image.id();

            t.advance(b"\x1b_Ga=d,d=I,i=1\x1b\\");
            assert!(t.term.graphics().image_by_client_id(1).is_none());
            assert!(t.term.graphics().pending_deletes.contains(&internal));
        }

        #[test]
        fn delete_marks_damage() {
            let mut t = GraphicsTerm::new();
            t.place(1, 1);
            t.term.reset_damage();

            t.advance(b"\x1b_Ga=d,d=a\x1b\\");
            assert!(t.term.damage.full);
            assert!(t.term.graphics().image_by_client_id(1).unwrap().placements().is_empty());
        }

        #[test]
        fn yazi_style_delete_loop_leaves_no_stale_placements() {
            let mut t = GraphicsTerm::new();
            for _ in 0..3 {
                t.place(1, 1);
                t.advance(b"\x1b_Ga=d,d=i,i=1\x1b\\");
            }
            t.place(1, 1);

            let image = t.term.graphics().image_by_client_id(1).unwrap();
            assert_eq!(image.placements().len(), 1, "stale placement after delete + re-place");
            assert_eq!(t.term.graphics().len(), 1);
        }

        #[test]
        fn delete_point_uses_one_based_cell() {
            let mut t = GraphicsTerm::new();
            // Place a single-cell image at line 2, column 3 (CUP is 1-based).
            t.advance(b"\x1b[3;4H");
            t.place(1, 1);

            // Adjacent cell: no match.
            t.advance(b"\x1b_Ga=d,d=p,x=5,y=3\x1b\\");
            assert_eq!(t.term.graphics().image_by_client_id(1).unwrap().placements().len(), 1);

            // The covered cell; `d=p` keeps the image data.
            t.advance(b"\x1b_Ga=d,d=p,x=4,y=3\x1b\\");
            assert!(t.term.graphics().image_by_client_id(1).unwrap().placements().is_empty());
        }

        #[test]
        fn delete_aborts_chunked_load() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b_Gi=7,f=32,s=1,v=2,m=1;/wAA/w==\x1b\\");
            assert!(t.term.graphics().loading().is_some());

            t.advance(b"\x1b_Ga=d,d=a\x1b\\");
            assert!(t.term.graphics().loading().is_none());
        }

        #[test]
        fn ed2_clears_graphics_including_unplaced_images() {
            let mut t = GraphicsTerm::new();
            t.place(1, 1);
            // Stored but never placed: `grman_clear` frees these too.
            t.advance(format!("\x1b_Gi=2,{PIXEL}\x1b\\").as_bytes());

            t.advance(b"\x1b[2J");
            assert!(t.term.graphics().is_empty());
        }

        #[test]
        fn ed3_clears_only_scrollback_placements() {
            let mut t = GraphicsTerm::new();
            t.place(1, 1);
            t.place(2, 1);
            // Simulate a placement scrolled entirely into history.
            let image = t.term.graphics_mut().image_by_client_id(2).unwrap().id();
            t.term.graphics_mut().image_mut(image).unwrap().placements_mut()[0].line = Line(-3);

            t.advance(b"\x1b[3J");
            assert!(t.term.graphics().image_by_client_id(2).is_none());
            assert_eq!(t.term.graphics().image_by_client_id(1).unwrap().placements().len(), 1);
        }

        #[test]
        fn el_and_ed_01_preserve_graphics() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[3;4H");
            t.place(1, 1);

            // EL 0/1/2 and ED 0/1 never touch graphics (kitty only clears
            // them for ED 2/3/22, screen.c:2590-2604).
            t.advance(b"\x1b[K\x1b[1K\x1b[2K\x1b[J\x1b[1J");
            assert_eq!(t.term.graphics().image_by_client_id(1).unwrap().placements().len(), 1);
            assert_eq!(t.term.graphics().len(), 1);
        }

        #[test]
        fn ris_clears_both_graphics_managers() {
            let mut t = GraphicsTerm::new();
            t.place(1, 1);
            t.advance(b"\x1b[?1049h");
            t.place(2, 1);
            // In-flight chunked load on the alt manager.
            t.advance(b"\x1b_Gi=7,f=32,s=1,v=2,m=1;/wAA/w==\x1b\\");

            t.advance(b"\x1bc");
            assert!(!t.term.mode().contains(TermMode::ALT_SCREEN));
            assert!(t.term.graphics().is_empty());
            assert!(t.term.inactive_graphics.is_empty());
            assert!(t.term.graphics().loading().is_none());
            assert!(t.term.inactive_graphics.loading().is_none());
        }

        #[test]
        fn alt_screen_swap_clears_alt_graphics_on_entry_only() {
            let mut t = GraphicsTerm::new();
            t.place(1, 1);
            t.advance(b"\x1b[?1049h");
            t.place(2, 1);

            // Leaving keeps the alt graphics around (kitty clears only on
            // entry, screen.c:1629-1632)...
            t.advance(b"\x1b[?1049l");
            assert_eq!(t.term.inactive_graphics.len(), 1);
            // ...and the main screen placement survived the round trip.
            assert_eq!(t.term.graphics().image_by_client_id(1).unwrap().placements().len(), 1);

            // Re-entering clears the stale alt graphics.
            t.advance(b"\x1b[?1049h");
            assert!(t.term.graphics().is_empty());
        }

        /// Anchor line of the only placement of image `image`.
        fn anchor(t: &GraphicsTerm, image: u32) -> Line {
            let img = t.term.graphics().image_by_client_id(image).unwrap();
            assert_eq!(img.placements().len(), 1);
            img.placements()[0].line
        }

        #[test]
        fn scroll_rotates_anchors_and_retains_in_scrollback() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[6;1H");
            t.place(1, 1);
            assert_eq!(anchor(&t, 1), Line(5));

            t.term.scroll_up_relative(Line(0), 2);
            assert_eq!(anchor(&t, 1), Line(3));

            t.term.scroll_down_relative(Line(0), 1);
            assert_eq!(anchor(&t, 1), Line(4));

            // A top-anchored scroll feeds scrollback, so the 1-row placement
            // pushed above the viewport is RETAINED (re-renders when scrolled
            // back), not hard-deleted. Default history is ample.
            t.term.scroll_up_relative(Line(0), 5);
            assert_eq!(anchor(&t, 1), Line(-1), "retained one line into scrollback");
            assert!(!t.term.graphics().image_by_client_id(1).unwrap().frames.is_empty());
        }

        #[test]
        fn scroll_hard_deletes_once_past_scrollback_capacity() {
            let config = Config { scrolling_history: 3, ..Config::default() };
            let mut t = GraphicsTerm::with_config(config);
            t.advance(b"\x1b[6;1H");
            t.place(1, 1);
            assert_eq!(anchor(&t, 1), Line(5));

            // Within the 3-line scrollback: retained above the viewport.
            t.term.scroll_up_relative(Line(0), 7); // Line(5) -> Line(-2).
            assert_eq!(anchor(&t, 1), Line(-2));

            // Past the scrollback capacity: the 1-row placement is GC'd, while
            // the addressable image data is kept.
            t.term.scroll_up_relative(Line(0), 2); // Line(-2) -> Line(-4).
            let img = t.term.graphics().image_by_client_id(1).unwrap();
            assert!(img.placements().is_empty());
            assert!(!img.frames.is_empty());
        }

        #[test]
        fn margin_scroll_respects_scroll_region() {
            let mut t = GraphicsTerm::new();
            // DECSTBM rows 3..8: region Line(2)..Line(8).
            t.advance(b"\x1b[3;8r");
            t.advance(b"\x1b[1;1H");
            t.place(1, 1); // Line(0), outside the region.
            t.advance(b"\x1b[5;1H");
            t.place(2, 1); // Line(4), inside the region.

            t.term.scroll_up_relative(Line(2), 2);
            assert_eq!(anchor(&t, 1), Line(0), "outside the region must not move");
            assert_eq!(anchor(&t, 2), Line(2));

            // Another scroll up pushes the 1-row placement outside the region
            // top: removed, not moved into the lines above the region.
            t.term.scroll_up_relative(Line(2), 3);
            assert_eq!(anchor(&t, 1), Line(0));
            assert!(t.term.graphics().image_by_client_id(2).unwrap().placements().is_empty());
        }

        #[test]
        fn resize_shifts_anchors_like_the_vi_cursor() {
            let mut t = GraphicsTerm::new();
            // 14 newlines on a 10-line screen: 5 lines of history.
            t.advance("\r\n".repeat(14).as_bytes());
            t.advance(b"\x1b[5;1H");
            t.place(1, 1);
            assert_eq!(anchor(&t, 1), Line(4));

            // Growing by 3 pulls 3 lines from history: content (and the vi
            // cursor) moves down by 3.
            t.term.resize(TermSize::new(20, 13));
            assert_eq!(anchor(&t, 1), Line(7));

            // Shrinking with the cursor at the bottom scrolls content up by
            // the same amount the cursor moves.
            t.advance(b"\x1b[13;1H");
            t.term.resize(TermSize::new(20, 8));
            assert_eq!(anchor(&t, 1), Line(2));

            // Placements pushed above the viewport by the shrink are retained
            // in scrollback (reachable by scrolling back), not hard-deleted.
            t.advance(b"\x1b[7;1H");
            t.place(2, 1); // Line(6).
            t.advance(b"\x1b[8;1H");
            t.term.resize(TermSize::new(20, 4));
            // Cursor at the bottom: content scrolled up by 4.
            assert_eq!(anchor(&t, 1), Line(-2), "pushed into scrollback, retained");
            assert_eq!(anchor(&t, 2), Line(2));
        }

        #[test]
        fn column_reflow_keeps_primary_placements_and_alt_survives() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[3;1H");
            t.place(1, 1); // Primary grid, Line(2).
            t.advance(b"\x1b[?1049h");
            t.advance(b"\x1b[4;1H");
            t.place(2, 1); // Alt grid, Line(3).

            // Column change: the primary grid reflows. Images are anchored to
            // absolute grid lines and shift like the vi cursor, so they SURVIVE
            // the reflow (D3 fix) instead of being dropped — a kitty/icat image
            // is no longer lost on a tiling-WM column resize. A pure column
            // change with no row delta leaves the anchor in place.
            t.term.resize(TermSize::new(30, 10));
            assert_eq!(anchor(&t, 2), Line(3));
            let primary = t.term.inactive_graphics.image_by_client_id(1).unwrap();
            assert_eq!(primary.placements().len(), 1, "primary placement survives column reflow");
            assert_eq!(primary.placements()[0].line, Line(2), "anchor unchanged by pure reflow");
            assert!(!primary.frames.is_empty());
        }

        #[test]
        fn classic_image_renders_after_column_resize() {
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(b"\x1b[1;1H");
            t.place(1, 1); // active primary grid, Line(0).
            assert_eq!(t.term.render_snapshot(0).items.len(), 1);

            // Column-count change (tiling-WM resize). Before the D3 fix this
            // dropped the placement and the kitty/icat image vanished.
            t.term.resize(TermSize::new(30, 10));
            assert_eq!(
                t.term.render_snapshot(0).items.len(),
                1,
                "image survives and re-renders after a column resize",
            );
        }

        #[test]
        fn classic_image_survives_content_scroll_into_history_and_rerenders() {
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(b"\x1b[1;1H");
            t.place(1, 1);
            assert_eq!(anchor(&t, 1), Line(0));
            assert_eq!(t.term.render_snapshot(0).items.len(), 1, "renders in active view");

            // Top-anchored content scroll pushes it 6 lines into scrollback.
            t.term.scroll_up_relative(Line(0), 6);
            assert_eq!(anchor(&t, 1), Line(-6), "retained in history");

            // Out of view: culled from the snapshot while viewing the active area.
            assert!(t.term.render_snapshot(0).items.is_empty(), "culled when out of view");

            // Scroll the VIEW back to it: it re-renders inside the viewport
            // (A3 — image tracks scrollback instead of vanishing).
            t.term.scroll_display(Scroll::Delta(6));
            let snap = t.term.render_snapshot(0);
            let item = snap.items.first().expect("scrolled-back image re-renders");
            assert_eq!(item.dest.line, Line(0), "re-rendered at the viewport top");
        }

        #[test]
        fn crop_item_to_viewport_crops_culls_and_passes_through() {
            // Build a real classic render item, then exercise the crop math by
            // overwriting the viewport-relevant fields.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(b"\x1b[1;1H");
            t.place(1, 1);
            let base = t.term.render_snapshot(0).items.remove(0);
            let screen_lines = 10;

            // Fully visible: untouched, kept.
            let mut it = base.clone();
            it.dest.line = Line(2);
            it.dest.num_rows = 3;
            it.src_uv.v0 = 0.0;
            it.src_uv.v1 = 1.0;
            assert!(crop_item_to_viewport(&mut it, screen_lines));
            assert_eq!((it.dest.line, it.dest.num_rows), (Line(2), 3));
            assert_eq!((it.src_uv.v0, it.src_uv.v1), (0.0, 1.0));

            // Top straddle: line -2, 5 rows -> 2 rows cropped off the top,
            // anchor snaps to 0, subcell offset cleared, v0 advanced by 2/5.
            let mut it = base.clone();
            it.dest.line = Line(-2);
            it.dest.num_rows = 5;
            it.dest.cell_y_offset = 7;
            it.src_uv.v0 = 0.0;
            it.src_uv.v1 = 1.0;
            assert!(crop_item_to_viewport(&mut it, screen_lines));
            assert_eq!(it.dest.line, Line(0));
            assert_eq!(it.dest.num_rows, 3);
            assert_eq!(it.dest.cell_y_offset, 0);
            assert!((it.src_uv.v0 - 0.4).abs() < 1e-6, "v0 advanced by 2/5");
            assert!((it.src_uv.v1 - 1.0).abs() < 1e-6);

            // Bottom straddle: line 8, 5 rows -> bottom 13 > 10, 3 rows cropped,
            // v1 pulled back by 3/5.
            let mut it = base.clone();
            it.dest.line = Line(8);
            it.dest.num_rows = 5;
            it.src_uv.v0 = 0.0;
            it.src_uv.v1 = 1.0;
            assert!(crop_item_to_viewport(&mut it, screen_lines));
            assert_eq!((it.dest.line, it.dest.num_rows), (Line(8), 2));
            assert!((it.src_uv.v1 - 0.4).abs() < 1e-6, "v1 pulled back by 3/5");

            // Fully above the viewport: culled.
            let mut it = base.clone();
            it.dest.line = Line(-5);
            it.dest.num_rows = 3; // bottom -2 <= 0
            assert!(!crop_item_to_viewport(&mut it, screen_lines));

            // Fully below the viewport: culled.
            let mut it = base.clone();
            it.dest.line = Line(10);
            it.dest.num_rows = 3;
            assert!(!crop_item_to_viewport(&mut it, screen_lines));

            // Taller than the viewport: straddles BOTH edges. line -2, 15 rows
            // -> top crop 2 (line 0, v0 += 2/15), bottom crop 3 (v1 -= 3/15),
            // leaving exactly the 10 visible rows.
            let mut it = base.clone();
            it.dest.line = Line(-2);
            it.dest.num_rows = 15;
            it.src_uv.v0 = 0.0;
            it.src_uv.v1 = 1.0;
            assert!(crop_item_to_viewport(&mut it, screen_lines));
            assert_eq!((it.dest.line, it.dest.num_rows), (Line(0), 10));
            assert!((it.src_uv.v0 - 2.0 / 15.0).abs() < 1e-6, "v0 advanced by 2/15");
            assert!((it.src_uv.v1 - (1.0 - 3.0 / 15.0)).abs() < 1e-6, "v1 pulled back by 3/15");
        }

        #[test]
        fn cell_size_change_rescales_placements() {
            let mut t = GraphicsTerm::new();
            // 1x2 px image placed with a 1x1 cell: effective extent (1, 2).
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(b"\x1b_Ga=T,i=1,f=32,s=1,v=2;/wAA/wD/AP8=\x1b\\");
            let img = t.term.graphics().image_by_client_id(1).unwrap();
            assert_eq!(img.placements()[0].effective_num_rows, 2);
            let id = img.id();
            // Simulate a stale subcell offset beyond the next cell size.
            t.term.graphics_mut().image_mut(id).unwrap().placements_mut()[0].cell_y_offset = 5;

            t.term.reset_damage();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 2 });

            let p = &t.term.graphics().image_by_client_id(1).unwrap().placements()[0];
            assert_eq!(p.effective_num_rows, 1, "extent recomputed for the new cell");
            assert_eq!(p.cell_y_offset, 1, "subcell offset clamped to cell - 1");
            assert!(t.term.damage.full, "rescale must damage the viewport");

            // Setting the same size again is a no-op.
            t.term.reset_damage();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 2 });
            assert!(!t.term.damage.full);
        }

        fn disabled_config() -> Config {
            let mut config = Config::default();
            config.graphics.enabled = false;
            config
        }

        #[test]
        fn disabled_query_gets_eperm_error() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            t.advance(format!("\x1b_Ga=q,i=31,{PIXEL}\x1b\\").as_bytes());

            let responses = t.responses();
            assert_eq!(responses.len(), 1);
            assert!(responses[0].starts_with("\x1b_Gi=31;EPERM:"), "{responses:?}");
            assert!(t.term.graphics().is_empty());
            assert!(t.term.graphics().pending_uploads.is_empty());
        }

        #[test]
        fn disabled_kitty_toggle_alone_disables_queries() {
            let mut config = Config::default();
            config.graphics.kitty_protocol = false;
            let mut t = GraphicsTerm::with_config(config);
            t.advance(format!("\x1b_Ga=q,i=31,{PIXEL}\x1b\\").as_bytes());

            let responses = t.responses();
            assert_eq!(responses.len(), 1);
            assert!(responses[0].starts_with("\x1b_Gi=31;EPERM:"), "{responses:?}");
        }

        #[test]
        fn disabled_query_overrides_quiet_suppression() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            // Detection must never be silent, even with q=2.
            t.advance(format!("\x1b_Ga=q,i=31,q=2,{PIXEL}\x1b\\").as_bytes());

            let responses = t.responses();
            assert_eq!(responses.len(), 1);
            assert!(responses[0].starts_with("\x1b_Gi=31;EPERM:"), "{responses:?}");
        }

        #[test]
        fn disabled_query_precedes_da1() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            t.advance(format!("\x1b_Ga=q,i=44,{PIXEL}\x1b\\\x1b[c").as_bytes());

            let responses = t.responses();
            assert_eq!(responses.len(), 2, "{responses:?}");
            assert!(responses[0].starts_with("\x1b_Gi=44;EPERM:"), "{responses:?}");
            assert_eq!(responses[1], "\x1b[?62;c");
        }

        #[test]
        fn da1_contains_4_when_sixel_enabled() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[c");
            assert_eq!(t.responses(), vec!["\x1b[?62;4;c".to_string()]);
        }

        #[test]
        fn da1_omits_4_when_sixel_disabled() {
            let mut config = Config::default();
            config.graphics.sixel = false;
            let mut t = GraphicsTerm::with_config(config);
            t.advance(b"\x1b[c");
            assert_eq!(t.responses(), vec!["\x1b[?62;c".to_string()]);
        }

        #[test]
        fn da1_omits_4_when_graphics_master_disabled() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            t.advance(b"\x1b[c");
            assert_eq!(t.responses(), vec!["\x1b[?62;c".to_string()]);
        }

        #[test]
        fn disabled_transmit_errors_and_stores_nothing() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            t.advance(format!("\x1b_Gi=31,{PIXEL}\x1b\\").as_bytes());

            let responses = t.responses();
            assert_eq!(responses.len(), 1);
            assert!(responses[0].starts_with("\x1b_Gi=31;EPERM:"), "{responses:?}");
            assert!(t.term.graphics().is_empty());
            assert!(t.term.graphics().pending_uploads.is_empty());
        }

        #[test]
        fn disabled_transmit_honors_quiet_two() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            // Non-query commands keep the normal suppression rules.
            t.advance(format!("\x1b_Gi=31,q=2,{PIXEL}\x1b\\").as_bytes());

            assert_eq!(t.responses(), Vec::<String>::new());
            assert!(t.term.graphics().is_empty());
        }

        #[test]
        fn disabled_delete_is_silent() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            t.advance(b"\x1b_Ga=d,d=A,i=31\x1b\\");

            assert_eq!(t.responses(), Vec::<String>::new());
        }

        #[test]
        fn disabling_graphics_on_reload_drops_all_images() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Ga=T,i=1,{PIXEL}\x1b\\").as_bytes());
            t.advance(b"\x1b[?1049h");
            t.advance(format!("\x1b_Ga=T,i=2,{PIXEL}\x1b\\").as_bytes());
            assert_eq!(t.term.graphics().len(), 1);
            assert_eq!(t.term.inactive_graphics.len(), 1);
            t.term.reset_damage();

            t.term.set_options(disabled_config());

            // Both managers are emptied and GPU deletes are enqueued.
            assert!(t.term.graphics().is_empty());
            assert!(t.term.inactive_graphics.is_empty());
            assert!(!t.term.graphics().pending_deletes.is_empty());
            assert!(!t.term.inactive_graphics.pending_deletes.is_empty());
            assert!(t.term.damage.full);

            // Disabled at runtime: new transmissions are rejected loudly.
            t.advance(format!("\x1b_Ga=q,i=9,{PIXEL}\x1b\\").as_bytes());
            let responses = t.responses();
            assert!(responses.last().unwrap().starts_with("\x1b_Gi=9;EPERM:"), "{responses:?}");
        }

        #[test]
        fn reenabling_graphics_on_reload_restores_support() {
            let mut t = GraphicsTerm::with_config(disabled_config());
            t.term.set_options(Config::default());
            t.advance(format!("\x1b_Gi=5,{PIXEL}\x1b\\").as_bytes());

            assert_eq!(t.responses(), vec!["\x1b_Gi=5;OK\x1b\\".to_string()]);
            assert_eq!(t.term.graphics().len(), 1);
        }

        #[test]
        fn custom_storage_quota_reaches_managers() {
            let mut config = Config::default();
            config.graphics.max_storage = 7;
            let mut t = GraphicsTerm::with_config(config);
            assert_eq!(t.term.graphics().storage_limit, 7);
            assert_eq!(t.term.inactive_graphics.storage_limit, 7);

            // Two 4-byte pixels exceed the 7-byte quota: the unplaced older
            // image is evicted, the newly added one is kept.
            t.advance(format!("\x1b_Gi=1,{PIXEL}\x1b\\").as_bytes());
            t.advance(format!("\x1b_Gi=2,{PIXEL}\x1b\\").as_bytes());
            assert_eq!(t.term.graphics().len(), 1);
            assert!(t.term.graphics().image_by_client_id(2).is_some());
        }

        #[test]
        fn reload_storage_quota_evicts_down() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=1,{PIXEL}\x1b\\").as_bytes());
            t.advance(format!("\x1b_Gi=2,{PIXEL}\x1b\\").as_bytes());
            assert_eq!(t.term.graphics().len(), 2);

            let mut config = Config::default();
            config.graphics.max_storage = 4;
            t.term.set_options(config);

            assert_eq!(t.term.graphics().storage_limit, 4);
            assert_eq!(t.term.inactive_graphics.storage_limit, 4);
            // 8 bytes stored > 4 bytes quota: unplaced images are evicted.
            assert!(t.term.graphics().used_storage() <= 4);
            assert!(!t.term.graphics().pending_deletes.is_empty());
        }

        // ── Task-19: C= cursor movement policy ──────────────────────────────
        //
        // The GraphicsTerm default is 20 cols × 10 lines.
        // All cursor-movement tests use 1×1 px cells (set_graphics_cell_size) so
        // that a 1×1 px image = 1 col × 1 row.  We force specific extents via
        // `c=` / `r=` in the placement command — that is the correct way to
        // decouple cell math from image size when testing cursor logic.
        //
        // Four coordinate concepts (per the algorithm comment in graphics_place):
        //   c=/r=  — requested display columns/rows (effective extent comes from
        //             PlacementSpec.num_cols / num_rows and is used for cursor move)
        //   X=/Y=  — sub-cell pixel offset in the first cell (not tested here)
        //   x=/y=/w=/h= — source-image crop in pixels (not tested here)
        //   (Line, Column) anchor — set as the cursor position before the command

        #[test]
        fn cursor_default_advances_right_by_cols_down_by_rows_minus_one() {
            // 1×1 image at (0,0) with explicit c=1,r=1: x += 1 → Col(1), y += 0.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=1,{PIXEL}\x1b\\").as_bytes());
            t.term.grid.cursor.point = Point::new(Line(0), Column(0));
            t.advance(b"\x1b_Ga=p,i=1,c=1,r=1\x1b\\");
            assert_eq!(
                t.term.grid.cursor.point,
                Point::new(Line(0), Column(1)),
                "1×1 image: x+1, y unchanged (rows-1=0)"
            );
        }

        #[test]
        fn cursor_multi_row_advances_down_by_rows_minus_one() {
            // Place with c=3,r=4 at (Line(2), Col(3)):
            // x = 3+3=6 (no wrap, < 20), y = 2+3 = 5.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=2,{PIXEL}\x1b\\").as_bytes());
            t.term.grid.cursor.point = Point::new(Line(2), Column(3));
            t.advance(b"\x1b_Ga=p,i=2,c=3,r=4\x1b\\");
            assert_eq!(
                t.term.grid.cursor.point,
                Point::new(Line(5), Column(6)),
                "multi-row: y advances by rows-1=3"
            );
        }

        #[test]
        fn cursor_c1_no_move() {
            // C=1: cursor stays at (0, 0) regardless of image size.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=3,{PIXEL}\x1b\\").as_bytes());
            t.term.grid.cursor.point = Point::new(Line(3), Column(7));
            t.advance(b"\x1b_Ga=p,i=3,c=5,r=3,C=1\x1b\\");
            assert_eq!(
                t.term.grid.cursor.point,
                Point::new(Line(3), Column(7)),
                "C=1: cursor must not move"
            );
        }

        #[test]
        fn cursor_wrap_at_right_margin() {
            // c=2 at Column(19) in a 20-col terminal:
            // x = 19+2 = 21 >= 20 → wrap: x=0, y = 0+1 = 1 (r=1, rows-1=0, then y++).
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=4,{PIXEL}\x1b\\").as_bytes());
            t.term.grid.cursor.point = Point::new(Line(0), Column(19));
            t.advance(b"\x1b_Ga=p,i=4,c=2,r=1\x1b\\");
            // x=19+2=21 >= 20 → x=0, y=0+1=1; scroll_region.end=10, no scroll.
            assert_eq!(
                t.term.grid.cursor.point,
                Point::new(Line(1), Column(0)),
                "wrap: column overflow wraps to col=0, line+1"
            );
        }

        #[test]
        fn cursor_wrap_multi_row_then_scroll() {
            // c=2 at Col(19), r=3 at Line(8) in a 10-line terminal:
            // x = 19+2=21 >= 20 → x=0, y = 8 + (3-1) = 10.  Then y++ = 11.
            // Wait: wrap happens after adding rows-1, not before.
            // Algorithm: step1: x=19+2=21, y=8+2=10.
            //            step2: x>=20 → x=0, y=11.
            //            step3: y=11 > margin_bottom=9 → scroll(2), y clamped to 9.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=5,{PIXEL}\x1b\\").as_bytes());
            t.term.grid.cursor.point = Point::new(Line(8), Column(19));
            t.advance(b"\x1b_Ga=p,i=5,c=2,r=3\x1b\\");
            assert_eq!(
                t.term.grid.cursor.point,
                Point::new(Line(9), Column(0)),
                "wrap+scroll: col wraps, row overflows → scroll"
            );
        }

        #[test]
        fn cursor_scroll_at_bottom_of_scroll_region() {
            // r=4 at Line(8): y = 8 + 3 = 11 > margin_bottom(9) → scroll(2), cursor at 9.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=6,{PIXEL}\x1b\\").as_bytes());
            t.term.grid.cursor.point = Point::new(Line(8), Column(0));
            t.advance(b"\x1b_Ga=p,i=6,c=1,r=4\x1b\\");
            // y=8+3=11 > 9 → scroll(2), cursor at 9; x=0+1=1.
            assert_eq!(
                t.term.grid.cursor.point,
                Point::new(Line(9), Column(1)),
                "bottom scroll: cursor clamped to margin_bottom"
            );
        }

        // ── Task-19: I= image-number flows ──────────────────────────────────

        #[test]
        fn put_by_image_number_echoes_both_i_and_capital_i() {
            // Transmit with I=7 → auto-assigned i=1.
            // Put with I=7 → must echo both i=1 and I=7 in the response.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_GI=7,{PIXEL}\x1b\\").as_bytes());
            let r0 = t.responses();
            assert_eq!(r0.len(), 1);
            // Transmission echoes both.
            assert_eq!(r0[0], "\x1b_Gi=1,I=7;OK\x1b\\");

            // Place by number.
            t.advance(b"\x1b_Ga=p,I=7\x1b\\");
            let r1 = t.responses();
            assert_eq!(r1.len(), 2, "exactly two responses");
            // a=p with I= must echo BOTH i= (resolved client id) AND I= (number).
            // Exact bytes: "\x1b_Gi=1,I=7;OK\x1b\\"
            assert_eq!(r1[1], "\x1b_Gi=1,I=7;OK\x1b\\", "a=p by I= must echo both i= and I=");
        }

        #[test]
        fn put_by_image_number_newest_wins() {
            // Two images with client_number=3; the second (newer) must be placed.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            // First image: I=3 → i=1.
            t.advance(format!("\x1b_GI=3,{PIXEL}\x1b\\").as_bytes());
            // Second image: I=3 → i=2 (newest).
            t.advance(format!("\x1b_GI=3,{PIXEL}\x1b\\").as_bytes());

            t.advance(b"\x1b_Ga=p,I=3\x1b\\");
            let responses = t.responses();
            // The newest image has client_id=2 (assigned in order).
            assert!(
                responses.last().unwrap().starts_with("\x1b_Gi=2,I=3;"),
                "newest-wins: response must echo i=2 (newest), got: {:?}",
                responses.last()
            );
        }

        // ── Task-19: a=T transmit+place with C=1 ────────────────────────────

        #[test]
        fn transmit_and_place_c1_no_move() {
            // a=T with C=1: image stored and placed, cursor unmoved.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.term.grid.cursor.point = Point::new(Line(2), Column(5));
            t.advance(format!("\x1b_Ga=T,i=9,C=1,{PIXEL}\x1b\\").as_bytes());
            assert_eq!(
                t.term.grid.cursor.point,
                Point::new(Line(2), Column(5)),
                "a=T with C=1: cursor must not move"
            );
        }

        // ── Task-19: x=/y=/w=/h= source crop (unit test in mod.rs side) ─────
        // The src-crop storage is tested in graphics/mod.rs (src_rect_clamped_to_image);
        // here we verify the full path: crop fields from cmd flow into the placement.

        #[test]
        fn placement_src_crop_fields_flow_from_command() {
            // Transmit a 4×4 image (16 pixels = 64 bytes).
            use base64::Engine as _;
            let raw: Vec<u8> = vec![255u8; 4 * 4 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            let cmd = format!("\x1b_Gi=10,f=32,s=4,v=4;{payload}\x1b\\");
            t.advance(cmd.as_bytes());

            // Place with x=1,y=1,w=2,h=2 crop.
            t.advance(b"\x1b_Ga=p,i=10,x=1,y=1,w=2,h=2\x1b\\");

            let img = t.term.graphics().image_by_client_id(10).unwrap();
            let p = &img.placements()[0];
            assert_eq!(p.src_x, 1, "x= crop origin");
            assert_eq!(p.src_y, 1, "y= crop origin");
            assert_eq!(p.src_width, 2, "w= crop width");
            assert_eq!(p.src_height, 2, "h= crop height");
        }

        // ── Task-21: virtual placements (U=1) ───────────────────────────────

        #[test]
        fn virtual_placement_survives_geometric_deletes() {
            // d=c/p/q/x/y/z must not remove a U=1 virtual placement.
            for &spec in b"cpqxyz" {
                let mut t = GraphicsTerm::new();
                t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
                t.advance(format!("\x1b_Gi=1,{PIXEL}\x1b\\").as_bytes());
                t.advance(b"\x1b_Ga=p,i=1,U=1\x1b\\");
                let before = t.term.graphics().image_by_client_id(1).unwrap().placements().len();

                // Uppercase variants too.
                let upper = spec.to_ascii_uppercase();
                for &del in &[spec, upper] {
                    t.advance(
                        format!("\x1b_Ga=d,d={},i=1,x=1,y=1,z=0\x1b\\", del as char).as_bytes(),
                    );
                    let count = t.term.graphics().image_by_client_id(1).unwrap().placements().len();
                    assert_eq!(
                        count, before,
                        "d={}: virtual placement must survive geometric delete",
                        del as char
                    );
                }
            }
        }

        #[test]
        fn virtual_placement_deleted_by_id() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=2,{PIXEL}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=2,U=1\x1b\\");
            assert_eq!(t.term.graphics().image_by_client_id(2).unwrap().placements().len(), 1);

            t.advance(b"\x1b_Ga=d,d=i,i=2\x1b\\");
            assert_eq!(
                t.term.graphics().image_by_client_id(2).unwrap().placements().len(),
                0,
                "d=i must remove virtual placement"
            );
        }

        #[test]
        fn virtual_placement_deleted_by_all() {
            // d=a uses clear_filter_func_noncell which checks !is_virtual in kitty,
            // so virtual placements survive d=a (same as kitty behaviour).
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=3,{PIXEL}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=3,U=1\x1b\\");

            t.advance(b"\x1b_Ga=d,d=a\x1b\\");
            let count = t.term.graphics().image_by_client_id(3).unwrap().placements().len();
            assert_eq!(count, 1, "d=a must NOT remove virtual placements (kitty parity)");
        }

        #[test]
        fn virtual_placement_excluded_from_render_snapshot() {
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            t.advance(format!("\x1b_Gi=5,{PIXEL}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=5,U=1\x1b\\");

            let snap = t.term.render_snapshot(0);
            assert!(snap.items.is_empty(), "virtual placement must not appear in render snapshot");
        }

        #[test]
        fn virtual_placement_q2_no_response() {
            let mut t = GraphicsTerm::new();
            t.advance(format!("\x1b_Gi=6,{PIXEL}\x1b\\").as_bytes());
            let before = t.responses().len();

            t.advance(b"\x1b_Ga=p,i=6,U=1,q=2\x1b\\");
            assert_eq!(t.responses().len(), before, "U=1,q=2 must emit no response");
        }

        #[test]
        fn virtual_placement_transmit_and_place_a_t() {
            // a=T,U=1: transmit + virtual place in one command.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            let cursor_before = t.term.grid.cursor.point;
            t.advance(format!("\x1b_Ga=T,i=7,U=1,{PIXEL}\x1b\\").as_bytes());

            // Cursor must not move.
            assert_eq!(t.term.grid.cursor.point, cursor_before, "a=T,U=1 must not move cursor");

            // Placement exists and is virtual.
            let img = t.term.graphics().image_by_client_id(7).unwrap();
            assert_eq!(img.placements().len(), 1);
            assert!(img.placements()[0].is_virtual, "a=T,U=1 placement must be virtual");

            // Not in render snapshot.
            let snap = t.term.render_snapshot(0);
            assert!(snap.items.is_empty(), "a=T,U=1 must not appear in render snapshot");
        }

        // ── Task-22: Unicode placeholder cell scanner ────────────────────────

        #[test]
        fn placeholder_flag_set_on_write_cleared_on_reset() {
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });
            let line = t.term.grid.cursor.point.line;
            assert!(!t.term.grid[line].has_image_placeholders(), "no flag before write");

            // Write U+10EEEE via VTE (ESC sequence sets a character).
            // Use a raw cell write to bypass charset mapping complexity.
            t.term.write_at_cursor('\u{10EEEE}');
            assert!(t.term.grid[line].has_image_placeholders(), "flag set after U+10EEEE write");

            // Reset the row via scroll (scroll_up resets the top line).
            t.term.grid[line].reset(&crate::term::cell::Cell::default());
            assert!(!t.term.grid[line].has_image_placeholders(), "flag cleared after row reset");
        }

        #[test]
        fn placeholder_scan_produces_cell_image_render_items() {
            let mut t = GraphicsTerm::new();
            // 10×20 cells, 10×20 image — fits exactly 1×1 box.
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });

            // Transmit a 10×20 RGBA image with client id 42.
            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);
            t.advance(format!("\x1b_Gi=42,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            // Create virtual placement: 1 col × 1 row.
            t.advance(b"\x1b_Ga=p,i=42,U=1,c=1,r=1\x1b\\");

            // Write a placeholder cell at (0,0): fg = index 42, no underline.
            // Set fg to Color::Indexed(42) via ESC[38;5;42m, then write the char.
            // Diacritics U+0305 (row=1, 1-based) + U+0305 (col=1, 1-based).
            {
                let line = t.term.grid.cursor.point.line;
                let col = t.term.grid.cursor.point.column;
                use crate::vte::ansi::Color;
                t.term.grid.cursor.template.fg = Color::Indexed(42);
                t.term.write_at_cursor('\u{10EEEE}');
                // Push diacritics as zerowidth chars.
                t.term.grid[line][col].push_zerowidth('\u{0305}'); // row diacritic → 1
                t.term.grid[line][col].push_zerowidth('\u{0305}'); // col diacritic → 1
            }

            let snap = t.term.render_snapshot(0);
            // Must have exactly one cell-image render item.
            let cell_items: Vec<_> = snap.items.iter().filter(|item| item.z_index == -1).collect();
            assert_eq!(cell_items.len(), 1, "expected one cell-image item from placeholder scan");
            let item = cell_items[0];
            assert_eq!(item.dest.num_cols, 1);
            assert_eq!(item.dest.num_rows, 1);
        }

        #[test]
        fn placeholder_cell_revives_against_reused_image_id_after_delete_all() {
            // Regression for the yazi image-preview "new image renders on top of
            // previous" stacking bug. yazi reuses ONE client id (i=877974) across
            // every preview and emits `a=d,d=A` before each. That delete removes
            // the graphics-model image + NON-virtual placements but NEVER touches
            // the U+10EEEE *grid cells* (they are text). When the next preview is
            // shorter / narrower / repositioned, the previous preview's
            // placeholder cells that the new preview does not overwrite remain in
            // the grid. After the image is re-transmitted under the SAME client id
            // (add_image replaces in place), those stale cells resolve via
            // `image_by_client_id` to the NEW image and render at stale positions
            // — the visible stacking.
            //
            // The existing `yazi_scroll_lifecycle_no_placement_stacking` test used
            // NON-virtual placements (graphics/mod.rs `place()` helper hardcodes
            // `is_virtual: false`), so it never exercised this U=1 grid-scan path.
            // That is why the bug was not caught.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });

            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);

            // Preview A: transmit id=42, anonymous virtual placement, one cell.
            t.advance(format!("\x1b_Gi=42,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=42,U=1,c=1,r=1\x1b\\");
            let line = t.term.grid.cursor.point.line;
            let col = t.term.grid.cursor.point.column;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(42);
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col].push_zerowidth('\u{0305}');
            t.term.grid[line][col].push_zerowidth('\u{0305}');
            let a_items =
                t.term.render_snapshot(0).items.iter().filter(|i| i.z_index == -1).count();
            assert_eq!(a_items, 1, "preview A must render its placeholder cell");

            // yazi switches preview: clear-all, then re-transmit under the SAME
            // client id WITHOUT rewriting the old cell (models a shorter/narrower
            // next preview that does not cover the previous cell).
            t.advance(b"\x1b_Ga=d,d=A\x1b\\");
            t.advance(format!("\x1b_Gi=42,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=42,U=1,c=1,r=1\x1b\\");

            // The stale cell must NOT revive against the reused image id.
            let stale = t.term.render_snapshot(0).items.iter().filter(|i| i.z_index == -1).count();
            assert_eq!(
                stale, 0,
                "stale placeholder cell revived against reused image id (stacking bug)"
            );
        }

        #[test]
        fn placeholder_narrower_preview_drops_stale_columns() {
            // The per-row flag-clear handles a SHORTER next preview (stale rows
            // never get re-flagged). This test exercises the harder NARROWER
            // case: the next preview reuses the SAME row(s) but fewer columns.
            // Writing the new preview's left cell re-sets the row flag, and the
            // previous preview's stale right-hand cell — still present as text in
            // that row — merges into the scan run (its img_col is absent, so the
            // L-to-R inheritance rule continues the run). With `num_cols =
            // run.run_length` (term/mod.rs) the stale column renders beyond the
            // new 1-col box: visible stacking. Clearing only the per-row flag is
            // insufficient here.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });
            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);

            // Preview A: 2-col wide box, two placeholder cells in row 0.
            t.advance(format!("\x1b_Gi=42,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=42,U=1,c=2,r=1\x1b\\");
            let line = t.term.grid.cursor.point.line;
            let col0 = t.term.grid.cursor.point.column;
            let col1 = col0 + 1;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(42);
            // Cell 0: row + col diacritics (img_col = 1). write_at_cursor does NOT
            // advance the cursor, so position each cell explicitly.
            t.term.grid.cursor.point.column = col0;
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col0].push_zerowidth('\u{0305}');
            t.term.grid[line][col0].push_zerowidth('\u{0305}');
            // Cell 1: row diacritic only — col inherits via L-to-R (img_col = 2).
            t.term.grid.cursor.point.column = col1;
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col1].push_zerowidth('\u{0305}');
            let a_cols: u32 = t
                .term
                .render_snapshot(0)
                .items
                .iter()
                .filter(|i| i.z_index == -1)
                .map(|i| i.dest.num_cols)
                .sum();
            assert_eq!(a_cols, 2, "preview A renders a 2-col run");

            // yazi switches to a NARROWER preview: delete-all, re-transmit under
            // the SAME id, place a 1-col box, and write ONLY the left cell.
            t.advance(b"\x1b_Ga=d,d=A\x1b\\");
            t.advance(format!("\x1b_Gi=42,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=42,U=1,c=1,r=1\x1b\\");
            // Re-write the left cell (col0) — re-sets the row flag. col1 is stale.
            t.term.grid.cursor.point.line = line;
            t.term.grid.cursor.point.column = col0;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(42);
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col0].push_zerowidth('\u{0305}');
            t.term.grid[line][col0].push_zerowidth('\u{0305}');

            // Only the new 1-col preview must render. The stale col1 must not
            // extend the run beyond the new box.
            let cols: u32 = t
                .term
                .render_snapshot(0)
                .items
                .iter()
                .filter(|i| i.z_index == -1)
                .map(|i| i.dest.num_cols)
                .sum();
            assert_eq!(
                cols, 1,
                "stale right-hand placeholder column revived beyond the narrower box (stacking)"
            );
        }

        #[test]
        fn placeholder_run_clamped_to_box_without_teardown() {
            // Defense-in-depth for the "entire TUI gets corrupted" smear facet.
            // `tear_down_placeholder_cells` only removes stale cells on the
            // delete-all path. A stale placeholder cell can ALSO survive a path that
            // never issues delete-all — a cell-shifting DCH/ICH, an ECH/clear that
            // leaves the row flag set, or a wider previous preview whose right-hand
            // cell is left behind when the box shrinks under the SAME image id.
            // `scan_placeholder_cells` groups consecutive cells by L-to-R image-column
            // inheritance with NO knowledge of the live placement's `box_cols`, so the
            // stale cell extends the run one column past the image's real span. The
            // render-time clamp in `append_placeholder_items` must drop the out-of-box
            // column even though `tear_down` never ran; without it the over-long run
            // makes `fit_to_box` sample a too-wide source rect and paints the image
            // across an unrelated cell (the whole-row smear).
            //
            // Crucially this test issues NO `a=d,d=A` — that isolates the clamp from
            // tear_down. The sibling `placeholder_narrower_preview_drops_stale_columns`
            // relies on tear_down blanking the stale cell; this one proves the render
            // boundary holds on its own.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });
            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);

            // Transmit id=42 and place a 1-COLUMN virtual box (c=1,r=1).
            t.advance(format!("\x1b_Gi=42,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=42,U=1,c=1,r=1\x1b\\");

            let line = t.term.grid.cursor.point.line;
            let col0 = t.term.grid.cursor.point.column;
            let col1 = col0 + 1;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(42);

            // Legit left cell (img_col = 1). write_at_cursor does NOT advance the
            // cursor, so position each cell explicitly.
            t.term.grid.cursor.point.column = col0;
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col0].push_zerowidth('\u{0305}'); // row diacritic → 1
            t.term.grid[line][col0].push_zerowidth('\u{0305}'); // col diacritic → 1

            // Stale right cell that was never torn down: row diacritic only, so the
            // scan's L-to-R rule continues the run (img_col inherits to 2) — one
            // column PAST the 1-col box.
            t.term.grid.cursor.point.column = col1;
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col1].push_zerowidth('\u{0305}'); // row → 1, col inherits

            let cols: u32 = t
                .term
                .render_snapshot(0)
                .items
                .iter()
                .filter(|i| i.z_index == -1)
                .map(|i| i.dest.num_cols)
                .sum();
            assert_eq!(
                cols, 1,
                "render-time clamp must bound the placeholder run to the 1-col box even though \
                 delete-all/tear_down never ran (stale-cell smear / TUI corruption)"
            );
        }

        #[test]
        fn placeholder_rewritten_after_delete_all_still_renders() {
            // Tearing down placeholder cells on delete-all must NOT permanently
            // suppress rendering: a FRESH placeholder cell written after the delete
            // renders normally (guards against over-suppression / false negative).
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });
            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);

            // Preview A.
            t.advance(format!("\x1b_Gi=9,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=9,U=1,c=1,r=1\x1b\\");
            let line = t.term.grid.cursor.point.line;
            let col = t.term.grid.cursor.point.column;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(9);
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col].push_zerowidth('\u{0305}');
            t.term.grid[line][col].push_zerowidth('\u{0305}');

            // Delete-all tears the cell down (flag cleared, char blanked).
            t.advance(b"\x1b_Ga=d,d=A\x1b\\");
            assert!(
                !t.term.grid[line].has_image_placeholders(),
                "delete-all must clear the per-row placeholder flag"
            );
            assert_ne!(
                t.term.grid[line][col].c, '\u{10EEEE}',
                "delete-all must blank the orphaned placeholder cell"
            );

            // Re-transmit, re-place, and the app writes a FRESH placeholder cell.
            t.advance(format!("\x1b_Gi=9,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=9,U=1,c=1,r=1\x1b\\");
            t.term.grid.cursor.point.line = line;
            t.term.grid.cursor.point.column = col;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(9);
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line][col].push_zerowidth('\u{0305}');
            t.term.grid[line][col].push_zerowidth('\u{0305}');

            let items = t.term.render_snapshot(0).items.iter().filter(|i| i.z_index == -1).count();
            assert_eq!(items, 1, "freshly-written placeholder after delete-all must render");
        }

        #[test]
        fn placeholder_scrollback_torn_down_by_delete_all() {
            // R3: a placeholder cell that has scrolled into history must also be
            // torn down by delete-all, so scrolling back up after a preview switch
            // does not re-ghost a stale cell against the re-transmitted image.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });
            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);

            t.advance(format!("\x1b_Gi=11,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=11,U=1,c=1,r=1\x1b\\");
            let start_line = t.term.grid.cursor.point.line;
            let col = t.term.grid.cursor.point.column;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(11);
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[start_line][col].push_zerowidth('\u{0305}');
            t.term.grid[start_line][col].push_zerowidth('\u{0305}');

            // Scroll the placeholder row off the top into scrollback history.
            let lines = t.term.screen_lines();
            for _ in 0..lines + 2 {
                t.advance(b"\r\n");
            }
            // Locate the placeholder cell in history (negative line index).
            let history_line = (0..=t.term.history_size())
                .map(|h| Line(-(h as i32)))
                .find(|&l| t.term.grid[l].has_image_placeholders());
            let history_line = history_line.expect("placeholder must have scrolled into history");
            assert!(history_line.0 < 0, "placeholder row must be in scrollback, not viewport");

            // Delete-all must reach into history and tear the cell down.
            t.advance(b"\x1b_Ga=d,d=A\x1b\\");
            assert!(
                !t.term.grid[history_line].has_image_placeholders(),
                "delete-all must clear the placeholder flag in scrollback history"
            );
            assert_ne!(
                t.term.grid[history_line][col].c, '\u{10EEEE}',
                "delete-all must blank the orphaned placeholder cell in scrollback history"
            );
        }

        #[test]
        fn placeholder_image_tracks_scrollback_not_sticky() {
            // Regression for the "sticky image" bug: in kitty a Unicode-placeholder
            // image scrolls WITH the scrollback; in this fork the image stayed
            // pinned to a fixed screen row while the text scrolled out from under
            // it. Root cause: `append_placeholder_items` emitted the *absolute*
            // grid line as `CellRect.dest.line`, but that field is VIEWPORT-relative
            // by contract — the renderer maps it straight to `line * cell_height`
            // with a top-origin viewport and no display_offset. Classic placements
            // track scroll because their viewport-relative `line` is shifted by
            // `delta` on scroll; the placeholder scan must emit the viewport line
            // directly. With display_offset == 0 the two coincide (why normal use
            // looked fine); the bug only shows once the user scrolls the scrollback.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });

            // Build BLANK scrollback history first (no image involved), so the
            // view can be scrolled without physically scrolling the grid — that
            // keeps the virtual placement alive and isolates the coordinate bug
            // (a content scroll would shift/evict the placement, a separate path).
            let lines = t.term.screen_lines() as i32;
            for _ in 0..lines + 3 {
                t.advance(b"\r\n");
            }
            assert!(t.term.history_size() >= 3, "need scrollback to scroll the view");

            // Transmit + virtual-place an image AFTER history exists.
            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);
            t.advance(format!("\x1b_Gi=55,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=55,U=1,c=1,r=1\x1b\\");

            // Write the placeholder cell at the TOP active line (grid line 0).
            let active_line = Line(0);
            let col = Column(0);
            t.term.grid.cursor.point.line = active_line;
            t.term.grid.cursor.point.column = col;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(55);
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[active_line][col].push_zerowidth('\u{0305}');
            t.term.grid[active_line][col].push_zerowidth('\u{0305}');

            // Baseline (display_offset == 0): the image sits at its viewport row 0.
            let base_line = {
                let snap = t.term.render_snapshot(0);
                snap.items
                    .iter()
                    .find(|i| i.z_index == -1)
                    .expect("placeholder must render at baseline")
                    .dest
                    .line
            };
            assert_eq!(
                base_line, active_line,
                "baseline dest line must equal the cell's viewport row"
            );

            // Scroll the VIEW up (display_offset only; content/placement unmoved).
            t.term.scroll_display(Scroll::Delta(3));
            let display_offset = t.term.grid.display_offset() as i32;
            assert!(display_offset > 0, "view must have scrolled into history");

            let snap = t.term.render_snapshot(0);
            let item = snap
                .items
                .iter()
                .find(|i| i.z_index == -1)
                .expect("scrolled-back placeholder must still render while visible");

            // dest.line MUST be viewport-relative: grid_line + display_offset.
            // The placeholder cell is at grid line 0, so after scrolling the view
            // down by `display_offset` it must render at viewport row
            // `0 + display_offset`. Under the bug dest.line stayed pinned at the
            // absolute grid line 0 (the "sticky" image) while the text scrolled.
            let expected_viewport_line = active_line.0 + display_offset;
            assert_eq!(
                item.dest.line.0, expected_viewport_line,
                "placeholder image must track scrollback view (viewport-relative dest line); got \
                 {} expected {}",
                item.dest.line.0, expected_viewport_line
            );
            assert!(
                (0..t.term.screen_lines() as i32).contains(&item.dest.line.0),
                "a visible placeholder must map to an on-screen viewport row, not a \
                 sticky/off-screen one (got dest.line {})",
                item.dest.line.0,
            );
        }

        #[test]
        fn classic_placement_tracks_scrollback_not_sticky() {
            // Sibling of `placeholder_image_tracks_scrollback_not_sticky`, but for
            // the CLASSIC (non-virtual) placement path used by `kitty +icat`.
            // `graphics.render_snapshot()` emits each classic placement's *absolute
            // grid* line as `CellRect.dest.line`, but that field is VIEWPORT-relative
            // by contract (renderer: `py = line * cell_height`, no display_offset).
            // Content scroll keeps classic placements aligned via `placement.line +=
            // delta`, but VIEW scroll only moves display_offset — so a scrolled-back
            // `kitty +icat` image stayed pinned to a fixed screen row. The Term-level
            // `render_snapshot` wrapper now adds display_offset to convert grid ->
            // viewport. With display_offset == 0 the two coincide (normal use looked
            // fine); the bug only shows once the scrollback is scrolled.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });

            // Blank scrollback first so the VIEW can scroll without a content
            // scroll (which would shift the placement via `placement.line += delta`
            // and confound the coordinate check).
            let lines = t.term.screen_lines() as i32;
            for _ in 0..lines + 3 {
                t.advance(b"\r\n");
            }
            assert!(t.term.history_size() >= 3, "need scrollback to scroll the view");

            // Transmit + classic-place (a=T, no U=1) a 1x1-cell image at the TOP
            // active line (grid line 0). The placement records cursor line/column.
            let active_line = Line(0);
            t.term.grid.cursor.point.line = active_line;
            t.term.grid.cursor.point.column = Column(0);
            let raw: Vec<u8> = vec![128u8; 10 * 20 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);
            t.advance(format!("\x1b_Ga=T,i=55,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());

            // Exactly one (classic) render item; placeholder scan adds none here.
            let base_line = {
                let snap = t.term.render_snapshot(0);
                assert_eq!(snap.items.len(), 1, "expected one classic placement item");
                snap.items[0].dest.line
            };
            assert_eq!(
                base_line, active_line,
                "baseline dest line must equal the placement's viewport row"
            );

            // Scroll the VIEW up (display_offset only; placement.line unmoved).
            t.term.scroll_display(Scroll::Delta(3));
            let display_offset = t.term.grid.display_offset() as i32;
            assert!(display_offset > 0, "view must have scrolled into history");

            let snap = t.term.render_snapshot(0);
            let item = snap
                .items
                .first()
                .expect("scrolled-back classic placement must still render while visible");

            // dest.line MUST be viewport-relative: grid_line + display_offset. The
            // placement is at grid line 0, so it must render at viewport row
            // `0 + display_offset`. Under the bug dest.line stayed pinned at the
            // absolute grid line 0 (the "sticky" image) while the text scrolled.
            let expected_viewport_line = active_line.0 + display_offset;
            assert_eq!(
                item.dest.line.0, expected_viewport_line,
                "classic image must track scrollback view (viewport-relative dest line); got {} \
                 expected {}",
                item.dest.line.0, expected_viewport_line
            );
            assert!(
                (0..t.term.screen_lines() as i32).contains(&item.dest.line.0),
                "a visible classic image must map to an on-screen viewport row, not a \
                 sticky/off-screen one (got dest.line {})",
                item.dest.line.0,
            );
        }

        #[test]
        fn placeholder_multirow_preview_lower_rows_dont_revive() {
            // All other placeholder regression tests use a single image row
            // (r=1). This covers a TALLER preview (r=2, two stacked placeholder
            // rows) switching to a SHORTER one (r=1): the lower stale row must be
            // torn down by the grid-wide sweep so it cannot revive against the
            // re-transmitted image.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 10, height: 20 });
            // 10x40 image → exactly 1 col × 2 rows at this cell size.
            let raw: Vec<u8> = vec![128u8; 10 * 40 * 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);

            // Preview A: 2-row-tall box, one placeholder cell per row.
            t.advance(format!("\x1b_Gi=77,f=32,s=10,v=40;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=77,U=1,c=1,r=2\x1b\\");
            let line0 = t.term.grid.cursor.point.line;
            let col = t.term.grid.cursor.point.column;
            let line1 = line0 + 1;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(77);
            // Row 0: img_row = 1 (U+0305), img_col = 1 (U+0305).
            t.term.grid.cursor.point.line = line0;
            t.term.grid.cursor.point.column = col;
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line0][col].push_zerowidth('\u{0305}');
            t.term.grid[line0][col].push_zerowidth('\u{0305}');
            // Row 1: img_row = 2 (U+030D), img_col = 1 (U+0305).
            t.term.grid.cursor.point.line = line1;
            t.term.grid.cursor.point.column = col;
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line1][col].push_zerowidth('\u{030d}');
            t.term.grid[line1][col].push_zerowidth('\u{0305}');
            let a_rows: u32 = t
                .term
                .render_snapshot(0)
                .items
                .iter()
                .filter(|i| i.z_index == -1)
                .map(|i| i.dest.num_rows)
                .sum();
            assert_eq!(a_rows, 2, "preview A renders two stacked placeholder rows");

            // yazi switches to a SHORTER 1-row preview: delete-all, re-transmit
            // under the SAME id, place a 1-row box, write ONLY the top row.
            t.advance(b"\x1b_Ga=d,d=A\x1b\\");
            t.advance(format!("\x1b_Gi=77,f=32,s=10,v=20;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=77,U=1,c=1,r=1\x1b\\");
            t.term.grid.cursor.point.line = line0;
            t.term.grid.cursor.point.column = col;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(77);
            t.term.write_at_cursor('\u{10EEEE}');
            t.term.grid[line0][col].push_zerowidth('\u{0305}');
            t.term.grid[line0][col].push_zerowidth('\u{0305}');

            // The stale lower row must not revive.
            let rows: u32 = t
                .term
                .render_snapshot(0)
                .items
                .iter()
                .filter(|i| i.z_index == -1)
                .map(|i| i.dest.num_rows)
                .sum();
            assert_eq!(rows, 1, "stale lower placeholder row revived (multi-row stacking)");
        }

        #[test]
        fn placeholder_and_classic_both_survive_column_reflow() {
            // After the D3 fix, classic placements survive a column resize
            // (anchored to grid lines, shifted like the vi cursor) AND
            // placeholder U+10EEEE cells survive because they flow as text.
            let mut t = GraphicsTerm::new();
            t.term.set_graphics_cell_size(CellSize { width: 1, height: 1 });
            let raw: Vec<u8> = vec![0xffu8; 4];
            let payload = base64::engine::general_purpose::STANDARD.encode(&raw);

            // Classic placement.
            t.advance(format!("\x1b_Gi=1,f=32,s=1,v=1;{payload}\x1b\\").as_bytes());
            t.advance(b"\x1b_Ga=p,i=1\x1b\\");
            assert_eq!(
                t.term.graphics().image_by_client_id(1).unwrap().placements().len(),
                1,
                "classic placement exists before resize"
            );

            // Write a placeholder cell.
            let line = t.term.grid.cursor.point.line;
            t.term.grid.cursor.template.fg = crate::vte::ansi::Color::Indexed(1);
            t.term.write_at_cursor('\u{10EEEE}');
            assert!(t.term.grid[line].has_image_placeholders(), "flag set");

            // Resize columns — triggers reflow.
            t.term.resize(TermSize::new(15, 10));

            // Classic placement survives the reflow (D3 fix).
            let classic_placements = t
                .term
                .graphics()
                .image_by_client_id(1)
                .map(|img| img.placements().len())
                .unwrap_or(0);
            assert_eq!(classic_placements, 1, "classic placement survives column resize");

            // Placeholder char is still present in the grid (it flows with text).
            let found = (0..t.term.screen_lines() as i32).any(|r| {
                let row = &t.term.grid[Line(r)];
                row.has_image_placeholders()
            });
            assert!(found, "placeholder cells survive reflow as text");
        }

        // ── Task-27: Sixel modes + XTSMGRAPHICS ─────────────────────────────

        #[test]
        fn sixel_priv_palette_on_by_default() {
            let t = GraphicsTerm::new();
            assert!(
                t.term.mode().contains(TermMode::SIXEL_PRIV_PALETTE),
                "SIXEL_PRIV_PALETTE must be ON by default"
            );
        }

        #[test]
        fn sixel_mode_80_set_reset() {
            let mut t = GraphicsTerm::new();
            assert!(!t.term.mode().contains(TermMode::SIXEL_DISPLAY));
            t.advance(b"\x1b[?80h");
            assert!(t.term.mode().contains(TermMode::SIXEL_DISPLAY), "mode 80 set");
            t.advance(b"\x1b[?80l");
            assert!(!t.term.mode().contains(TermMode::SIXEL_DISPLAY), "mode 80 reset");
        }

        #[test]
        fn sixel_mode_1070_set_reset() {
            let mut t = GraphicsTerm::new();
            // Default ON.
            assert!(t.term.mode().contains(TermMode::SIXEL_PRIV_PALETTE));
            // Reset (CSI ? 1070 l) → shared palette.
            t.advance(b"\x1b[?1070l");
            assert!(!t.term.mode().contains(TermMode::SIXEL_PRIV_PALETTE), "mode 1070 reset");
            // Set (CSI ? 1070 h) → back to private palette.
            t.advance(b"\x1b[?1070h");
            assert!(t.term.mode().contains(TermMode::SIXEL_PRIV_PALETTE), "mode 1070 set");
        }

        #[test]
        fn sixel_mode_8452_set_reset() {
            let mut t = GraphicsTerm::new();
            assert!(!t.term.mode().contains(TermMode::SIXEL_CURSOR_TO_RIGHT));
            t.advance(b"\x1b[?8452h");
            assert!(t.term.mode().contains(TermMode::SIXEL_CURSOR_TO_RIGHT), "mode 8452 set");
            t.advance(b"\x1b[?8452l");
            assert!(!t.term.mode().contains(TermMode::SIXEL_CURSOR_TO_RIGHT), "mode 8452 reset");
        }

        #[test]
        fn xtsmgraphics_color_registers_read() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?1;1S");
            assert_eq!(t.responses(), vec!["\x1b[?1;0;1024S"]);
        }

        #[test]
        fn xtsmgraphics_color_registers_reset() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?1;2S");
            assert_eq!(t.responses(), vec!["\x1b[?1;0;1024S"]);
        }

        #[test]
        fn xtsmgraphics_color_registers_set_valid() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?1;3;256S");
            assert_eq!(t.responses(), vec!["\x1b[?1;0;256S"]);
            // Read back confirms value was stored.
            t.advance(b"\x1b[?1;1S");
            let all = t.responses();
            assert_eq!(all[1], "\x1b[?1;0;256S");
        }

        #[test]
        fn xtsmgraphics_color_registers_set_out_of_range() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?1;3;9999S");
            assert_eq!(t.responses(), vec!["\x1b[?1;3;1024S"]);
        }

        #[test]
        fn xtsmgraphics_color_registers_read_max() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?1;4S");
            assert_eq!(t.responses(), vec!["\x1b[?1;0;1024S"]);
        }

        #[test]
        fn xtsmgraphics_color_registers_bad_pa() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?1;99S");
            assert_eq!(t.responses(), vec!["\x1b[?1;2;0S"]);
        }

        #[test]
        fn xtsmgraphics_geometry_read() {
            let mut t = GraphicsTerm::new();
            // Default cell size 10×20 (set in Term::new), 20 cols, 10 rows.
            t.advance(b"\x1b[?2;1S");
            let resp = t.responses();
            assert_eq!(resp.len(), 1);
            // Width = 20*10 = 200, Height = 10*20 = 200.
            assert_eq!(resp[0], "\x1b[?2;0;200;200S");
        }

        #[test]
        fn xtsmgraphics_geometry_reset() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?2;2S");
            let resp = t.responses();
            assert_eq!(resp.len(), 1);
            assert!(resp[0].starts_with("\x1b[?2;0;"), "geometry reset returns Ps=0: {resp:?}");
        }

        #[test]
        fn xtsmgraphics_geometry_set_rejected() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?2;3;800S");
            assert_eq!(t.responses(), vec!["\x1b[?2;3;0S"]);
        }

        #[test]
        fn xtsmgraphics_geometry_read_max() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?2;4S");
            assert_eq!(t.responses(), vec!["\x1b[?2;0;4096;4096S"]);
        }

        #[test]
        fn xtsmgraphics_bad_pi() {
            let mut t = GraphicsTerm::new();
            t.advance(b"\x1b[?99;1S");
            assert_eq!(t.responses(), vec!["\x1b[?99;1;0S"]);
        }

        #[test]
        fn plain_s_scroll_up_still_works() {
            let mut t = GraphicsTerm::new();
            // CSI S without ? marker = scroll up.  No PtyWrite response expected.
            t.advance(b"\x1b[1S");
            assert_eq!(t.responses(), Vec::<String>::new());
        }

        /// Bug (a): chunked `a=f` continuation chunks may omit `i=`/`I=`.
        ///
        /// Kitty's `grman_handle_command` (graphics.c:2227) accepts a chunk
        /// with no image identity when there is an in-flight load:
        ///   `if (!g->id && !g->image_number &&
        ///        !self->currently_loading.loading_for.image_id)`
        /// Only the absence of ALL THREE means "no target" → EINVAL.
        #[test]
        fn chunked_animation_frame_continuation_may_omit_image_id() {
            let mut t = GraphicsTerm::new();

            // Transmit a base image with i=5.
            t.advance(format!("\x1b_Ga=t,i=5,{PIXEL}\x1b\\").as_bytes());
            let n_before = t.responses().len();

            // First chunk of a new animation frame: carries i=5, m=1 (more to come).
            t.advance("\x1b_Ga=f,i=5,f=32,s=1,v=1,m=1;/wAA\x1b\\".as_bytes());
            // Mid-stream: no new response (m=1 suppresses OK until final chunk).
            assert_eq!(
                t.responses().len(),
                n_before,
                "first a=f chunk (m=1) should produce no response mid-stream"
            );

            // Final chunk: omits i= entirely (only m=0 needed per kitty spec).
            t.advance(b"\x1b_Gm=0;/w==\x1b\\");
            let responses = t.responses();
            // Should have received exactly one new OK (not EINVAL) for the completed frame.
            assert_eq!(
                responses.len(),
                n_before + 1,
                "final chunk should produce exactly one OK response, not EINVAL"
            );
            let last = &responses[responses.len() - 1];
            assert!(
                last.contains(";OK"),
                "expected OK for chunked a=f with omitted i= on final chunk, got: {last}"
            );
            assert!(
                !last.contains("EINVAL"),
                "chunked a=f continuation must not produce EINVAL: {last}"
            );
        }

        // ── Sixel stacking + alt-screen regression tests (Facet 1 & 2) ────────

        /// A minimal valid sixel DCS that produces a 1-wide, 1-sixel-band-tall
        /// (1×6 px) image: define color register 1 as white, draw one `~` band.
        fn sixel_1x6() -> Vec<u8> {
            // ESC P q #1;2;100;100;100 #1~ ESC \
            b"\x1bPq#1;2;100;100;100#1~\x1b\\".to_vec()
        }

        /// A sixel DCS that produces an image tall enough to exceed 10 screen
        /// rows (cell height 20px, so need >200px; 40 sixel bands = 240px).
        fn sixel_tall() -> Vec<u8> {
            // One band per `-`, 40 bands total = 240 px > 10 rows × 20px.
            let mut v = b"\x1bPq#1;2;100;100;100#1~".to_vec();
            for _ in 0..39 {
                v.extend_from_slice(b"-#1~");
            }
            v.extend_from_slice(b"\x1b\\");
            v
        }

        /// Emitting two overlapping sixels leaves exactly ONE live image/placement
        /// (stacking eviction works).
        #[test]
        fn sixel_stacking_eviction_leaves_one_image() {
            let mut t = GraphicsTerm::new();
            // Cursor starts at (0,0). Emit first sixel.
            t.advance(&sixel_1x6());
            let count_after_first = t.term.graphics().len();
            assert_eq!(count_after_first, 1, "first sixel: expect 1 image");

            // Reset cursor to (0,0) so second sixel overlaps first.
            t.advance(b"\x1b[H");
            t.advance(&sixel_1x6());

            assert_eq!(
                t.term.graphics().len(),
                1,
                "after two overlapping sixels, exactly 1 image must remain (stacking evicted)"
            );
        }

        /// In ALT_SCREEN, a sixel taller than the screen must NOT scroll the
        /// buffer. A sentinel character written at row 0 before the image must
        /// still be there afterward, and the cursor must be clamped at the
        /// bottom margin (not past it).
        #[test]
        fn sixel_tall_alt_screen_no_scroll() {
            let mut t = GraphicsTerm::new();

            // Enter alt screen.
            t.advance(b"\x1b[?1049h");
            assert!(t.term.mode().contains(TermMode::ALT_SCREEN));

            // Write a sentinel character 'Z' at row 0, col 0.
            t.advance(b"\x1b[H"); // cursor to (0,0)
            t.advance(b"Z");

            // Move cursor back to (0,0) and emit a tall sixel.
            t.advance(b"\x1b[H");
            t.advance(&sixel_tall());

            // The sentinel at (0,0) must still be 'Z' — scroll would have
            // moved it off screen.
            let cell = &t.term.grid()[Line(0)][Column(0)];
            assert_eq!(
                cell.c, 'Z',
                "sentinel at row 0 must survive: alt-screen sixel must not scroll"
            );

            // Cursor must be clamped at screen_lines-1 = 9, not past the bottom.
            let cursor_line = t.term.grid().cursor.point.line.0;
            assert!(
                cursor_line < t.term.screen_lines() as i32,
                "cursor must be clamped at bottom margin, got line {cursor_line}"
            );
        }

        /// On the PRIMARY screen, a tall sixel DOES scroll (existing behavior).
        #[test]
        fn sixel_tall_primary_screen_does_scroll() {
            let mut t = GraphicsTerm::new();
            // Primary screen (default). Write sentinel at row 0.
            t.advance(b"\x1b[H");
            t.advance(b"Z");
            // Capture scrollback depth before.
            let history_before = t.term.history_size();

            // Move to row 0 and emit tall sixel.
            t.advance(b"\x1b[H");
            t.advance(&sixel_tall());

            // The primary screen should have scrolled: scrollback grew.
            let history_after = t.term.history_size();
            assert!(
                history_after > history_before,
                "primary screen: tall sixel must scroll (history_before={history_before}, \
                 history_after={history_after})"
            );
        }

        /// Bug (b): `a=f` response must echo `r=` (num_lines/frame slot) when set.
        ///
        /// Kitty `finish_command_response` (graphics.c:805):
        ///   `if (g->num_lines && (g->action == 'f' || g->action == 'a'))
        ///        print(",r=%u", g->num_lines);`
        #[test]
        fn animation_frame_response_echoes_r_field() {
            let mut t = GraphicsTerm::new();

            // Transmit a base image.
            t.advance(format!("\x1b_Ga=t,i=7,{PIXEL}\x1b\\").as_bytes());
            let n_after_load = t.responses().len();

            // Add frame with r=3 (target frame slot 3).
            t.advance(format!("\x1b_Ga=f,i=7,r=3,{PIXEL}\x1b\\").as_bytes());
            let responses = t.responses();
            assert_eq!(
                responses.len(),
                n_after_load + 1,
                "a=f should produce exactly one response"
            );
            let resp = &responses[responses.len() - 1];
            assert!(resp.contains(",r=3"), "a=f response must echo r=3 (num_lines), got: {resp}");
        }
    }
}
