//! Sixel DCS parser — adapted from ayosec commit 3d658d2e (see SIXEL_VENDORED.md).
//! Produces `(width, height, Arc<Vec<u8>>)` RGBA output for the shared graphics pipeline.
//! Supports: P2 transparency, raster attributes, RGB/HLS color registers (up to 1024),
//! repeat `!`, `$`/`-`, OR-mode accumulation, private/shared palettes.

use std::cmp::max;
use std::sync::Arc;
use std::{fmt, mem};

use log::trace;
use vte::Params;

use crate::vte::ansi::Rgb;

/// Maximum image dimension (width or height) in pixels.
pub const MAX_SIXEL_DIM: usize = 4096;

/// Maximum number of color registers.
pub const MAX_COLOR_REGISTERS: usize = 1024;

/// Maximum Sixel DCS payload size (same generous cap as the APC builder).
pub const MAX_DCS_LEN: usize = 32 * 1024 * 1024; // 32 MiB

/// Color-register index.
#[derive(Copy, Clone, Default, Debug, PartialEq, Eq)]
struct ColorRegister(u16);

/// Sentinel value meaning "transparent".
const REG_TRANSPARENT: ColorRegister = ColorRegister(u16::MAX);

/// Maximum parameters per Sixel command.
const MAX_CMD_PARAMS: usize = 5;

/// Error from the Sixel parser.
#[derive(Debug)]
pub enum Error {
    TooBigImage { width: usize, height: usize },
    InvalidColorComponent { register: u16, component_value: u16 },
    InvalidColorCoordinateSystem { register: u16, coordinate_system: u16 },
}

/// Output of [`Parser::finish`]: `(width, height, rgba_bytes, palette)`.
pub type SixelOutput = (u32, u32, Arc<Vec<u8>>, Vec<Rgb>);

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::TooBigImage { width, height } => {
                write!(f, "sixel image too large ({width}×{height})")
            },
            Error::InvalidColorComponent { register, component_value } => {
                write!(f, "invalid color component {component_value} for register {register}")
            },
            Error::InvalidColorCoordinateSystem { register, coordinate_system } => {
                write!(
                    f,
                    "invalid color coordinate system {coordinate_system} for register {register}"
                )
            },
        }
    }
}

// ── Internal command parser ───────────────────────────────────────────────────

#[derive(Debug)]
enum SixelCommand {
    RepeatIntroducer,
    SetRasterAttributes,
    ColorIntroducer,
    CarriageReturn,
    NextLine,
}

#[derive(Debug)]
struct CommandParser {
    command: SixelCommand,
    params: [u16; MAX_CMD_PARAMS],
    pos: usize,
}

impl CommandParser {
    fn new(command: SixelCommand) -> Self {
        Self { command, params: [0; MAX_CMD_PARAMS], pos: 0 }
    }

    fn put(&mut self, byte: u8) {
        if self.pos < MAX_CMD_PARAMS {
            match byte {
                b'0'..=b'9' => {
                    self.params[self.pos] = self.params[self.pos]
                        .saturating_mul(10)
                        .saturating_add((byte - b'0') as u16);
                },
                b';' => {
                    self.pos += 1;
                },
                _ => {},
            }
        }
    }

    fn finish(self, parser: &mut Parser) -> Result<(), Error> {
        match self.command {
            SixelCommand::RepeatIntroducer => {
                parser.repeat_count = self.params[0] as usize;
            },

            SixelCommand::SetRasterAttributes => {
                if self.pos >= 3 {
                    let w = self.params[2] as usize;
                    let h = self.params[3] as usize;
                    parser.ensure_size(w, h)?;
                }
            },

            SixelCommand::ColorIntroducer => {
                let reg = ColorRegister(self.params[0]);

                if self.pos >= 4 {
                    macro_rules! component {
                        ($i:expr, $limit:expr) => {
                            match self.params[$i] {
                                x if x <= $limit => x,
                                x => {
                                    return Err(Error::InvalidColorComponent {
                                        register: reg.0,
                                        component_value: x,
                                    });
                                },
                            }
                        };
                        ($i:expr) => {
                            component!($i, 100)
                        };
                    }

                    let color = match self.params[1] {
                        1 => hls_to_rgb(component!(2, 360), component!(3), component!(4)),
                        2 => scale_rgb(component!(2), component!(3), component!(4)),
                        cs => {
                            return Err(Error::InvalidColorCoordinateSystem {
                                register: reg.0,
                                coordinate_system: cs,
                            });
                        },
                    };
                    parser.set_color_register(reg, color);
                }

                if (reg.0 as usize) < MAX_COLOR_REGISTERS {
                    parser.selected = reg;
                }
            },

            SixelCommand::CarriageReturn => {
                parser.x = 0;
            },

            SixelCommand::NextLine => {
                parser.x = 0;
                parser.y += 6;
            },
        }

        Ok(())
    }
}

// ── Sixel band value ──────────────────────────────────────────────────────────

struct Sixel(u8);

impl Sixel {
    #[inline]
    fn new(byte: u8) -> Self {
        debug_assert!((0x3F..=0x7E).contains(&byte));
        Sixel(byte - 0x3F)
    }

    #[inline]
    fn height(&self) -> usize {
        8 - self.0.leading_zeros() as usize
    }

    #[inline]
    fn dots(&self) -> impl Iterator<Item = bool> {
        let v = self.0;
        (0..6).map(move |i| v & (1 << i) != 0)
    }
}

// ── Public parser ─────────────────────────────────────────────────────────────

/// Stream parser for a single Sixel DCS payload.
///
/// Feed bytes via [`Parser::put`], then call [`Parser::finish`] to obtain the
/// decoded `(width, height, rgba_bytes)` and the final palette (for
/// palette-sharing across images).
#[derive(Default, Debug)]
pub struct Parser {
    command_parser: Option<CommandParser>,
    width: usize,
    height: usize,
    pixels: Vec<ColorRegister>,
    background: ColorRegister,
    color_registers: Vec<Rgb>,
    selected: ColorRegister,
    repeat_count: usize,
    x: usize,
    y: usize,
}

impl Parser {
    /// Create a parser from DCS `params` (P1, P2, P3).
    ///
    /// Pass `shared_palette = Some(vec)` for mode-1070 palette sharing, or
    /// `None` for a private palette seeded with the VT-340 defaults.
    pub fn new(params: &Params, shared_palette: Option<Vec<Rgb>>) -> Self {
        let ps2 = params.iter().nth(1).and_then(|sub| sub.iter().next().copied()).unwrap_or(0);
        Self::new_with_p2(ps2 == 1, shared_palette)
    }

    /// Create a parser from already-decoded P2 and palette.
    ///
    /// `transparent_bg`: P2 = 1 → transparent background.
    /// `shared_palette`: mode-1070 shared palette; `None` = private VT-340 seed.
    pub fn new_with_p2(transparent_bg: bool, shared_palette: Option<Vec<Rgb>>) -> Self {
        trace!("sixel parser start");

        let background = if transparent_bg { REG_TRANSPARENT } else { ColorRegister(0) };
        let mut p = Self { background, ..Self::default() };

        match shared_palette {
            Some(pal) => p.color_registers = pal,
            None => init_vt340_palette(&mut p),
        }

        p
    }

    /// Feed one byte of the DCS payload.
    pub fn put(&mut self, byte: u8) -> Result<(), Error> {
        match byte {
            b'!' => self.start_command(SixelCommand::RepeatIntroducer)?,
            b'"' => self.start_command(SixelCommand::SetRasterAttributes)?,
            b'#' => self.start_command(SixelCommand::ColorIntroducer)?,
            b'$' => self.start_command(SixelCommand::CarriageReturn)?,
            b'-' => self.start_command(SixelCommand::NextLine)?,

            b'0'..=b'9' | b';' => {
                if let Some(cp) = &mut self.command_parser {
                    cp.put(byte);
                }
            },

            0x3F..=0x7E => self.add_sixel(Sixel::new(byte))?,

            _ => {
                self.finish_command()?;
            },
        }

        Ok(())
    }

    #[inline]
    fn start_command(&mut self, command: SixelCommand) -> Result<(), Error> {
        self.finish_command()?;
        self.command_parser = Some(CommandParser::new(command));
        Ok(())
    }

    #[inline]
    fn finish_command(&mut self) -> Result<(), Error> {
        if let Some(cp) = self.command_parser.take() {
            cp.finish(self)?;
        }
        Ok(())
    }

    fn set_color_register(&mut self, reg: ColorRegister, color: Rgb) {
        let idx = reg.0 as usize;
        if idx >= MAX_COLOR_REGISTERS {
            return;
        }
        if self.color_registers.len() <= idx {
            self.color_registers.resize(idx + 1, Rgb::default());
        }
        self.color_registers[idx] = color;
    }

    fn ensure_size(&mut self, width: usize, height: usize) -> Result<(), Error> {
        if self.width >= width && self.height >= height {
            return Ok(());
        }

        if width > MAX_SIXEL_DIM || height > MAX_SIXEL_DIM {
            return Err(Error::TooBigImage { width, height });
        }

        trace!("sixel resize to {}×{}", max(self.width, width), max(self.height, height));

        if self.pixels.is_empty() {
            self.width = width;
            self.height = height;
            self.pixels = vec![self.background; width * height];
            return Ok(());
        }

        if self.width >= width {
            self.pixels.resize(height * self.width, self.background);
            self.height = height;
            return Ok(());
        }

        let height = usize::max(height, self.height);
        self.pixels.resize(height * width, self.background);
        for row in (0..self.height).rev() {
            for col in (0..self.width).rev() {
                let old = row * self.width + col;
                let new = row * width + col;
                self.pixels.swap(old, new);
            }
        }
        self.width = width;
        self.height = height;
        Ok(())
    }

    fn add_sixel(&mut self, sixel: Sixel) -> Result<(), Error> {
        self.finish_command()?;

        let repeat = max(1, mem::take(&mut self.repeat_count));
        self.ensure_size(self.x + repeat, self.y + sixel.height())?;

        if sixel.0 != 0 {
            let mut row_start = self.width * self.y + self.x;
            for dot in sixel.dots() {
                if dot {
                    for px in &mut self.pixels[row_start..row_start + repeat] {
                        *px = self.selected;
                    }
                }
                row_start += self.width;
            }
        }

        self.x += repeat;
        Ok(())
    }

    /// Finalise parsing and return `(width, height, rgba_data, palette)`.
    ///
    /// `rgba_data` is 8-bit pre-multiplied RGBA, row-major, 4 bytes per pixel.
    /// `palette` is the final color-register table for palette sharing.
    pub fn finish(mut self) -> Result<SixelOutput, Error> {
        self.finish_command()?;

        trace!(
            "sixel finish: {}×{}, {} registers",
            self.width,
            self.height,
            self.color_registers.len()
        );

        let mut rgba = Vec::with_capacity(self.pixels.len() * 4);

        for &reg in &self.pixels {
            let pixel: [u8; 4] = if reg == REG_TRANSPARENT {
                [0, 0, 0, 0]
            } else {
                match self.color_registers.get(reg.0 as usize) {
                    None => [0, 0, 0, 255],
                    Some(c) => [c.r, c.g, c.b, 255],
                }
            };
            rgba.extend_from_slice(&pixel);
        }

        let w = self.width as u32;
        let h = self.height as u32;
        Ok((w, h, Arc::new(rgba), self.color_registers))
    }
}

// ── Color conversion ──────────────────────────────────────────────────────────

/// HLS → RGB.  Components are in 0–360 (hue) / 0–100 (lightness, saturation).
///
/// Port of libsixel's `hls_to_rgb`.  The 120° rotation (`(hue + 240) % 360`)
/// matches the DEC / libsixel convention.
fn hls_to_rgb(hue: u16, lum: u16, sat: u16) -> Rgb {
    if sat == 0 {
        return scale_rgb(lum, lum, lum);
    }

    let lum = lum as f64;
    let c0 = if lum > 50.0 { (lum * 4.0 / 100.0) - 1.0 } else { -(2.0 * (lum / 100.0) - 1.0) };
    let c = sat as f64 * (1.0 - c0) / 2.0;
    let high = lum + c;
    let low = lum - c;

    let hue = (hue + 240) % 360;
    let h = hue as f64;

    let (r, g, b) = match hue / 60 {
        0 => (high, low + (high - low) * (h / 60.0), low),
        1 => (low + (high - low) * ((120.0 - h) / 60.0), high, low),
        2 => (low, high, low + (high - low) * ((h - 120.0) / 60.0)),
        3 => (low, low + (high - low) * ((240.0 - h) / 60.0), high),
        4 => (low + (high - low) * ((h - 240.0) / 60.0), low, high),
        5 => (high, low, low + (high - low) * ((360.0 - h) / 60.0)),
        _ => (0.0, 0.0, 0.0),
    };

    #[inline]
    fn clamp(x: f64) -> u8 {
        (x * 255.0 / 100.0).round().clamp(0.0, 255.0) as u8
    }

    Rgb { r: clamp(r), g: clamp(g), b: clamp(b) }
}

/// Scale RGB components from 0–100 range to 0–255.
#[inline]
fn scale_rgb(r: u16, g: u16, b: u16) -> Rgb {
    let scale = |v: u16| -> u8 { ((v as u32 * 255 + 50) / 100) as u8 };
    Rgb { r: scale(r), g: scale(g), b: scale(b) }
}

/// Seed a parser with the VT-340 default palette.
fn init_vt340_palette(p: &mut Parser) {
    let regs: &[(u16, u16, u16, u16, u16)] = &[
        (0, 2, 0, 0, 0),
        (1, 2, 20, 20, 80),
        (2, 2, 80, 13, 13),
        (3, 2, 20, 80, 20),
        (4, 2, 80, 20, 80),
        (5, 2, 20, 80, 80),
        (6, 2, 80, 80, 20),
        (7, 2, 53, 53, 53),
        (8, 2, 26, 26, 26),
        (9, 2, 33, 33, 60),
        (10, 2, 60, 26, 26),
        (11, 2, 33, 60, 33),
        (12, 2, 60, 33, 60),
        (13, 2, 33, 60, 60),
        (14, 2, 60, 60, 33),
        (15, 2, 80, 80, 80),
    ];
    for &(idx, cs, a, b, c) in regs {
        let color = if cs == 1 { hls_to_rgb(a, b, c) } else { scale_rgb(a, b, c) };
        p.set_color_register(ColorRegister(idx), color);
    }
}

// ── DCS accumulator (mirrors ApcBuilder for DCS sequences) ───────────────────

/// Accumulator for in-flight Sixel DCS payload bytes (mirrors [`super::ApcBuilder`]).
/// `start` on `dcs_hook` (final `q`), `put` per byte, `end` on `dcs_unhook`.
/// P1/P3 (aspect ratio / grid size) are intentionally absent — sizing comes from raster attributes.
#[derive(Debug, Default)]
pub struct DcsBuilder {
    buf: Vec<u8>,
    active: bool,
    overflowed: bool,
    /// DCS P2 parameter (background transparency).
    pub p2: u16,
}

impl DcsBuilder {
    /// Called from `dcs_hook` when the final char is `q` (Sixel).
    pub fn start(&mut self, p2: u16) {
        self.buf.clear();
        self.active = true;
        self.overflowed = false;
        self.p2 = p2;
    }

    /// Discard any in-flight sequence (e.g. non-Sixel DCS).
    pub fn reset(&mut self) {
        self.active = false;
        self.buf.clear();
    }

    /// Feed a payload byte.
    pub fn put(&mut self, byte: u8) {
        if !self.active {
            return;
        }
        if self.buf.len() >= MAX_DCS_LEN {
            self.overflowed = true;
            return;
        }
        self.buf.push(byte);
    }

    /// Finish and return the payload.  Returns `None` if no sequence was
    /// active.  `overflowed` signals the payload was truncated.
    pub fn end(&mut self) -> Option<(Vec<u8>, u16, bool)> {
        if !self.active {
            return None;
        }
        self.active = false;
        Some((mem::take(&mut self.buf), self.p2, self.overflowed))
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! feed {
        ($parser:expr, $data:expr) => {
            for &b in $data.as_bytes() {
                $parser.put(b).unwrap();
            }
        };
    }

    #[test]
    fn hls_rotation_values() {
        macro_rules! assert_color {
            ($h:expr, $l:expr, $s:expr => $r:expr, $g:expr, $b:expr) => {
                let got = hls_to_rgb($h, $l, $s);
                let want = Rgb { r: $r, g: $g, b: $b };
                assert!(
                    got.r.abs_diff(want.r) < 4
                        && got.g.abs_diff(want.g) < 4
                        && got.b.abs_diff(want.b) < 4,
                    "hls({},{},{}) → {:?}, expected {:?}",
                    $h,
                    $l,
                    $s,
                    got,
                    want
                );
            };
        }

        assert_color!(282, 33, 87 =>  10, 156, 112);
        assert_color!( 45, 36, 78 => 128,  18, 163);
        assert_color!(279,  9, 93 =>   0,  43,  28);
        assert_color!(186, 27, 54 =>  97, 105,  31);
        assert_color!( 93, 66, 75 => 107, 230, 173);
        assert_color!( 60, 51, 90 => 125, 133, 125);
        assert_color!(141, 39, 78 => 176,  74,  20);
        assert_color!(273, 30, 48 =>  38, 112,  79);
        assert_color!(270, 15, 57 =>  15,  59,  38);
        assert_color!( 84, 21, 99 => 105,   0,  64);
        assert_color!(162, 81, 93 =>  60, 148, 255);
        assert_color!( 96, 30, 72 => 130,  20,  64);
        assert_color!(222, 21, 90 =>  33,  99,   5);
        assert_color!(306, 33, 39 =>  51, 110, 115);
        assert_color!(144, 30, 72 => 130,  64,  20);
        assert_color!( 27,  0, 42 =>   0,   0,   0);
        assert_color!(123, 10,  0 =>  26,  26,  26);
        assert_color!(279,  6, 93 =>   0,  28,  18);
        assert_color!(270, 45, 69 =>  33, 194, 115);
        assert_color!(225, 39, 45 =>  77, 143,  54);
    }

    #[test]
    fn p2_transparency() {
        // `@` (0x40) = sixel value 1 = 0b000001: sets row 0, height 1.
        // Use it to grow the canvas, then check the background of an unset pixel.
        // P2=0 → opaque background (register 0 = black from VT-340 palette).
        let mut parser = Parser::new(&vte::Params::default(), None);
        // Select register 1 (blue in VT-340) and paint column 0; column 1 stays background.
        parser.selected = ColorRegister(1);
        parser.put(0x40).unwrap(); // col 0, color 1
        parser.put(0x3F).unwrap(); // col 1, zero sixel — canvas already exists, x advances
        let (w, h, rgba, _) = parser.finish().unwrap();
        assert!(w > 0 && h > 0, "canvas must be non-empty");
        // col 1, row 0: background = register 0 = (0,0,0,255).
        assert_eq!(&rgba[4..8], &[0, 0, 0, 255]);

        // P2=1 → transparent background.
        let mut parser2 = Parser::new_with_p2(true, None);
        parser2.selected = ColorRegister(1);
        parser2.put(0x40).unwrap(); // col 0, color 1
        parser2.put(0x3F).unwrap(); // col 1, zero sixel
        let (_, _, rgba2, _) = parser2.finish().unwrap();
        assert_eq!(&rgba2[4..8], &[0, 0, 0, 0]); // transparent
    }

    #[test]
    fn private_palette_default() {
        // No shared palette → private palette seeded with VT-340 defaults.
        let params = vte::Params::default();
        let parser = Parser::new(&params, None);
        assert_eq!(parser.color_registers.len(), 16);
        // Register 0 = black (0,0,0).
        assert_eq!(parser.color_registers[0], Rgb { r: 0, g: 0, b: 0 });
    }

    #[test]
    fn shared_palette_used() {
        let shared = vec![Rgb { r: 255, g: 0, b: 0 }, Rgb { r: 0, g: 255, b: 0 }];
        let params = vte::Params::default();
        let parser = Parser::new(&params, Some(shared.clone()));
        assert_eq!(parser.color_registers[0], shared[0]);
        assert_eq!(parser.color_registers[1], shared[1]);
    }

    #[test]
    fn color_registers_rgb_and_hls() {
        let mut parser = Parser::default();
        // #1;2;30;100;0 — RGB, r=30 g=100 b=0 (0–100 scale)
        // #200;1;20;75;50 — HLS, hue=20 lum=75 sat=50
        feed!(parser, "#1;2;30;100;0#200;1;20;75;50.");
        assert!(parser.color_registers.len() >= 201);
        assert_eq!(parser.color_registers[1], Rgb { r: 77, g: 255, b: 0 });
        let got = parser.color_registers[200];
        let want = hls_to_rgb(20, 75, 50);
        assert!(
            got.r.abs_diff(want.r) < 4 && got.g.abs_diff(want.g) < 4 && got.b.abs_diff(want.b) < 4,
            "register 200: got {got:?}, want {want:?}"
        );
    }

    /// Bug regression: HLS clamp used `% 256.0` instead of `.clamp(0.0, 255.0)`.
    /// When a channel value in the 0-100 scale exceeds 100, scaling to 0-255 gives
    /// a value > 255 which must be clamped, not wrapped via modulo.
    #[test]
    fn hls_clamp_not_modulo() {
        // hls(0, 100, 100): lum=100 > 50 → c0=3.0, c=-100, high=0, low=200.
        // hue 0+240=240, sector 4: r=200, g=200, b=0 (in 0-100 scale).
        // Scaled: r=200*255/100=510 → clamp→255; modulo bug gives 254.
        let got = hls_to_rgb(0, 100, 100);
        assert_eq!(
            got.r, 255,
            "hls(0,100,100) r: expected 255, got {} (modulo bug gives 254)",
            got.r
        );
        assert_eq!(
            got.g, 255,
            "hls(0,100,100) g: expected 255, got {} (modulo bug gives 254)",
            got.g
        );
        assert_eq!(got.b, 0, "hls(0,100,100) b: expected 0, got {}", got.b);

        // hls(162, 81, 93): blue channel (low=138.66 in 0-100 scale) → 353.58 scaled.
        // Correct: clamp→255. Modulo bug gives 98.
        let got2 = hls_to_rgb(162, 81, 93);
        assert_eq!(
            got2.b, 255,
            "hls(162,81,93) b: expected 255, got {} (modulo bug gives 98)",
            got2.b
        );
    }

    /// Bug regression: color-register selection of an out-of-range index (>= MAX_COLOR_REGISTERS)
    /// whose value saturates to u16::MAX (== REG_TRANSPARENT sentinel) must NOT poison
    /// `parser.selected`. Before the fix the assignment was unconditional, so `#65535`
    /// caused all subsequent sixel pixels to be rendered as transparent.
    #[test]
    fn out_of_range_register_does_not_set_transparent() {
        // Verify that `selected` is not REG_TRANSPARENT after a bare `#65535` introducer.
        let mut parser = Parser::default();
        feed!(parser, "#65535");
        parser.put(0x40).unwrap(); // finish command + draw a sixel
        assert_ne!(
            parser.selected, REG_TRANSPARENT,
            "selecting register 65535 must not set parser.selected to REG_TRANSPARENT"
        );

        // Verify that pixels drawn after `#65535` are not forced transparent.
        let mut parser2 = Parser::default();
        init_vt340_palette(&mut parser2);
        // Select register 1 (blue in VT-340 palette).
        feed!(parser2, "#1");
        parser2.put(b'.').unwrap(); // flush command
        // Attempt to select out-of-range register; selected must remain 1.
        feed!(parser2, "#65535");
        parser2.put(b'.').unwrap(); // flush command
        // Draw a sixel — should use the last valid selected register (1).
        parser2.put(0x40).unwrap(); // '@' = sixel value 1 = sets row 0
        let (_, _, rgba, _) = parser2.finish().unwrap();
        assert_eq!(
            rgba[3], 255,
            "pixel drawn after #65535 must be opaque (alpha=255), got {}",
            rgba[3]
        );
    }

    #[test]
    fn or_mode_accumulation() {
        // OR-mode: each band only writes pixels whose bit is 1; others are unchanged.
        // 0x40 = sixel value 1 = 0b000001 → sets row 0 only (height=1).
        let mut parser = Parser::default();
        init_vt340_palette(&mut parser);
        parser.set_color_register(ColorRegister(1), Rgb { r: 255, g: 0, b: 0 }); // red
        parser.set_color_register(ColorRegister(2), Rgb { r: 0, g: 0, b: 255 }); // blue

        // Band 1: col 0 → red (0x40 sets row 0, x advances to 1).
        parser.selected = ColorRegister(1);
        parser.add_sixel(Sixel::new(0x40)).unwrap();

        // Carriage return, then band 2 with color 2.
        parser.x = 0;
        parser.selected = ColorRegister(2);
        // Skip col 0 with a zero sixel (no bits set, does not overwrite col 0).
        parser.add_sixel(Sixel::new(0x3F)).unwrap(); // zero sixel at x=0, x→1
        parser.add_sixel(Sixel::new(0x40)).unwrap(); // blue at x=1

        let (_, _, rgba, _) = parser.finish().unwrap();
        // Buffer is (width × height × 4): width=2, height=1, so 8 bytes.
        assert_eq!(rgba.len(), 8, "2×1 image = 8 bytes");
        assert_eq!(&rgba[0..4], &[255, 0, 0, 255], "col 0 row 0 = red");
        assert_eq!(&rgba[4..8], &[0, 0, 255, 255], "col 1 row 0 = blue");
    }

    #[test]
    fn raster_attributes_resize() {
        let mut parser = Parser { background: REG_TRANSPARENT, ..Parser::default() };
        feed!(parser, "\"1;1;30;20.");
        assert_eq!(parser.width, 30);
        assert_eq!(parser.height, 20);
        assert_eq!(parser.pixels.len(), 30 * 20);
        assert!(parser.pixels.iter().all(|&p| p == REG_TRANSPARENT));
    }

    #[test]
    fn repeat_introducer() {
        let mut parser = Parser::default();
        init_vt340_palette(&mut parser);
        parser.selected = ColorRegister(1);
        // `!5A` → repeat 5 times, sixel `A`
        feed!(parser, "!5A");
        assert_eq!(parser.x, 5);
    }

    #[test]
    fn fixture_corpus() {
        use std::fs;
        use std::path::Path;

        let dir = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/sixel"));
        let names = ["testimage_im6", "testimage_libsixel", "testimage_ppmtosixel"];

        for name in &names {
            let mut sixel_data = fs::read(dir.join(name).with_extension("sixel"))
                .unwrap_or_else(|e| panic!("read {name}.sixel: {e}"));

            // Strip the DCS header up to and including the `q` byte.
            let q_pos = sixel_data
                .iter()
                .position(|&b| b == b'q')
                .unwrap_or_else(|| panic!("{name}: no `q` byte"));
            sixel_data.drain(..=q_pos);

            // Strip ST (ESC \ or 0x9C).
            if let Some(pos) = sixel_data.iter().position(|&b| b == 0x1B || b == 0x9C) {
                sixel_data.truncate(pos);
            }

            let mut parser = Parser::default();
            for b in &sixel_data {
                parser.put(*b).expect("parse error");
            }
            let (w, h, rgba, _) = parser.finish().expect("finish error");

            assert_eq!(w, 64, "{name}: width");
            assert_eq!(h, 64, "{name}: height");

            let expected = fs::read(dir.join(name).with_extension("rgba"))
                .unwrap_or_else(|e| panic!("read {name}.rgba: {e}"));

            assert_eq!(*rgba, expected, "{name}: RGBA mismatch");
        }
    }
}
