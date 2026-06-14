//! Parser for kitty graphics protocol APC commands.
//!
//! A kitty graphics command arrives as an APC sequence of the form
//! `ESC _ G <key>=<value>,... ; <base64 payload> ESC \`. This module parses
//! the bytes *after* the leading `G` into a [`GraphicsCommand`].
//!
//! The parser is a faithful port of kitty's generated parser
//! (`kitty/parse-graphics-command.h`):
//!
//! * Keys are single bytes; unknown keys are a hard error.
//! * Integer values are decimal, at most 10 digits, and must fit in a `u32` (an 11th digit is
//!   rejected as a stray character after the value).
//! * Signed keys (`z`, `H`, `V`) accept a single leading `-`; the magnitude is parsed as a `u32`
//!   and cast to `i32` with wrapping, exactly like kitty's `(int32_t)` cast.
//! * Flag values (`a=`, `d=`, `t=`, `o=`) are a single byte from a per-key set of accepted
//!   characters.
//! * A trailing comma after a value is accepted (kitty quirk).
//! * Everything after the first `;` is a base64 payload, decoded eagerly.
//!
//! Additionally, `i=` and `I=` both non-zero is rejected with `EINVAL`,
//! mirroring the first check in kitty's `grman_handle_command`
//! (`kitty/graphics.c`).

use std::fmt;

/// Maximum number of decimal digits kitty reads for an integer value.
const MAX_INT_DIGITS: usize = 10;

/// Error codes used by the kitty graphics protocol.
///
/// These are the codes kitty embeds in `ESC _ G ... ;ERRCODE:message ESC \`
/// responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum ErrorCode {
    EINVAL,
    ENOENT,
    EBADF,
    /// Kitty's code for libpng decode failures (`kitty/png-reader.c`).
    EBADPNG,
    ENOMEM,
    ENOSPC,
    ENODATA,
    EFBIG,
    EPERM,
    EILSEQ,
    ECYCLE,
    ETOODEEP,
    ENOPARENT,
}

impl ErrorCode {
    /// The code name as it appears in a kitty `;ERRCODE:message` response.
    fn as_str(self) -> &'static str {
        match self {
            ErrorCode::EINVAL => "EINVAL",
            ErrorCode::ENOENT => "ENOENT",
            ErrorCode::EBADF => "EBADF",
            ErrorCode::EBADPNG => "EBADPNG",
            ErrorCode::ENOMEM => "ENOMEM",
            ErrorCode::ENOSPC => "ENOSPC",
            ErrorCode::ENODATA => "ENODATA",
            ErrorCode::EFBIG => "EFBIG",
            ErrorCode::EPERM => "EPERM",
            ErrorCode::EILSEQ => "EILSEQ",
            ErrorCode::ECYCLE => "ECYCLE",
            ErrorCode::ETOODEEP => "ETOODEEP",
            ErrorCode::ENOPARENT => "ENOPARENT",
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Error produced while parsing or validating a graphics command.
///
/// [`fmt::Display`] renders as `CODE:message`, the exact form used after the
/// image id keys in a kitty error response
/// (`ESC _ G i=<id>;CODE:message ESC \`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandError {
    /// Kitty-compatible error code.
    pub code: ErrorCode,
    /// Human readable message, suitable for the `;CODE:message` response body.
    pub message: String,
    /// Whether kitty would send an `;ERR` APC response for this error.
    ///
    /// Malformed control blocks (bad key, bad integer, ...) are logged and
    /// dropped silently by kitty's parser; only errors raised at the command
    /// handling layer (like specifying both `i=` and `I=`) produce a
    /// response.
    pub sends_response: bool,
}

impl CommandError {
    fn parse(message: String) -> Self {
        Self { code: ErrorCode::EINVAL, message, sends_response: false }
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.code, self.message)
    }
}

impl std::error::Error for CommandError {}

/// A parsed kitty graphics protocol command.
///
/// Field names and types mirror kitty's `GraphicsCommand` struct
/// (`kitty/graphics.h`). Kitty overloads several keys depending on the
/// action (`a=`); the C struct expresses this with unions. Here the primary
/// (graphics) name is the field and the animation/composition aliases are
/// provided as accessor methods.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct GraphicsCommand {
    /// `a=`: action. One of `t`, `T`, `p`, `q`, `d`, `f`, `a`, `c`, or `0`
    /// when absent (kitty treats `0` like `t`).
    pub action: u8,
    /// `t=`: transmission medium. One of `d`, `f`, `t`, `s`, or `0`.
    pub transmission_type: u8,
    /// `o=`: compression. Only `z` (zlib) or `0`.
    pub compressed: u8,
    /// `d=`: delete specifier. One of `aAcCfFiInNpPqQrRxXyYzZ`, or `0`.
    pub delete_action: u8,
    /// `f=`: pixel format (`24`, `32`, or `100` for PNG).
    pub format: u32,
    /// `m=`: non-zero when more chunks follow.
    pub more: u32,
    /// `i=`: client image id.
    pub id: u32,
    /// `I=`: client image number.
    pub image_number: u32,
    /// `S=`: size of data to read from a file/shm transmission.
    pub data_sz: u32,
    /// `O=`: offset into a file/shm transmission.
    pub data_offset: u32,
    /// `p=`: placement id.
    pub placement_id: u32,
    /// `q=`: response suppression (`1`: suppress OK, `2`: suppress all).
    pub quiet: u32,
    /// `P=`: parent image id for relative placements.
    pub parent_id: u32,
    /// `Q=`: parent placement id for relative placements.
    pub parent_placement_id: u32,
    /// `w=`: width of the source rectangle to display.
    pub width: u32,
    /// `h=`: height of the source rectangle to display.
    pub height: u32,
    /// `x=`: left edge of the source rectangle to display.
    pub x_offset: u32,
    /// `y=`: top edge of the source rectangle to display.
    pub y_offset: u32,
    /// `C=`: when non-zero the cursor is not moved after a put. Overloaded as
    /// [`compose_mode`](Self::compose_mode) for animation composition.
    pub cursor_movement: u32,
    /// `X=`: horizontal offset within the first cell, in pixels.
    pub cell_x_offset: u32,
    /// `Y=`: vertical offset within the first cell, in pixels. Overloaded as
    /// [`bgcolor`](Self::bgcolor) for animation frames.
    pub cell_y_offset: u32,
    /// `s=`: image data width in pixels. Overloaded as
    /// [`animation_state`](Self::animation_state).
    pub data_width: u32,
    /// `v=`: image data height in pixels. Overloaded as
    /// [`loop_count`](Self::loop_count).
    pub data_height: u32,
    /// `r=`: number of lines to display over. Overloaded as
    /// [`frame_number`](Self::frame_number).
    pub num_lines: u32,
    /// `c=`: number of columns to display over. Overloaded as
    /// [`other_frame_number`](Self::other_frame_number).
    pub num_cells: u32,
    /// `z=`: z-index of the placement. Overloaded as [`gap`](Self::gap) for
    /// animation frames.
    pub z_index: i32,
    /// `U=`: create a virtual placement for Unicode placeholders.
    ///
    /// Kitty stores this as a C `bool`; any non-zero value parses as `true`.
    pub unicode_placement: bool,
    /// `H=`: horizontal offset from the parent placement, in pixels.
    pub offset_from_parent_x: i32,
    /// `V=`: vertical offset from the parent placement, in pixels.
    pub offset_from_parent_y: i32,
    /// Base64-decoded payload bytes.
    ///
    /// For `o=z` the payload is still zlib-compressed after decoding;
    /// decompression happens at the transmission layer.
    pub payload: Vec<u8>,
}

impl GraphicsCommand {
    /// `C=` reinterpreted as the composition mode for `a=f`/`a=c`.
    #[inline]
    pub fn compose_mode(&self) -> u32 {
        self.cursor_movement
    }

    /// `Y=` reinterpreted as the background color for animation frames.
    #[inline]
    pub fn bgcolor(&self) -> u32 {
        self.cell_y_offset
    }

    /// `s=` reinterpreted as the animation state for `a=a`.
    #[inline]
    pub fn animation_state(&self) -> u32 {
        self.data_width
    }

    /// `v=` reinterpreted as the animation loop count for `a=a`.
    #[inline]
    pub fn loop_count(&self) -> u32 {
        self.data_height
    }

    /// `r=` reinterpreted as the frame number for `a=f`/`a=a`/`a=c`.
    #[inline]
    pub fn frame_number(&self) -> u32 {
        self.num_lines
    }

    /// `c=` reinterpreted as the other frame number for `a=f`/`a=c`.
    #[inline]
    pub fn other_frame_number(&self) -> u32 {
        self.num_cells
    }

    /// `z=` reinterpreted as the frame gap in milliseconds for `a=f`/`a=a`.
    #[inline]
    pub fn gap(&self) -> i32 {
        self.z_index
    }

    /// Parse the body of a kitty graphics APC command.
    ///
    /// `buf` must contain the bytes *after* the leading `G`, i.e.
    /// `<key>=<value>,...;<base64 payload>`. An empty buffer parses to the
    /// default command, like in kitty.
    pub fn parse(buf: &[u8]) -> Result<GraphicsCommand, CommandError> {
        let mut g = GraphicsCommand::default();
        let mut pos = 0;
        let mut key = 0u8;

        #[derive(Clone, Copy, PartialEq, Eq)]
        enum State {
            Key,
            Equal,
            Uint,
            Int,
            Flag,
            AfterValue,
            Payload,
        }

        let mut value_state = State::Flag;
        // Kitty quirk: a command starting with `;` skips straight to the
        // payload separator handling.
        let mut state = if buf.first() == Some(&b';') { State::AfterValue } else { State::Key };

        while pos < buf.len() {
            match state {
                State::Key => {
                    key = buf[pos];
                    pos += 1;
                    value_state = match key {
                        b'a' | b'd' | b't' | b'o' => State::Flag,
                        b'f' | b'm' | b'i' | b'I' | b'p' | b'q' | b'w' | b'h' | b'x' | b'y'
                        | b'v' | b's' | b'S' | b'O' | b'c' | b'r' | b'X' | b'Y' | b'C' | b'U'
                        | b'P' | b'Q' => State::Uint,
                        b'z' | b'H' | b'V' => State::Int,
                        _ => {
                            return Err(CommandError::parse(format!(
                                "Malformed GraphicsCommand control block, invalid key character: \
                                 {key:#x}"
                            )));
                        },
                    };
                    state = State::Equal;
                },

                State::Equal => {
                    let b = buf[pos];
                    pos += 1;
                    if b != b'=' {
                        return Err(CommandError::parse(format!(
                            "Malformed GraphicsCommand control block, no = after key, found: \
                             {b:#x} instead"
                        )));
                    }
                    state = value_state;
                },

                State::Flag => {
                    let value = buf[pos];
                    pos += 1;
                    let (field, valid): (&mut u8, &[u8]) = match key {
                        b'a' => (&mut g.action, b"Tacdfpqt"),
                        b'd' => (&mut g.delete_action, b"ACFINPQRXYZacfinpqrxyz"),
                        b't' => (&mut g.transmission_type, b"dfst"),
                        b'o' => (&mut g.compressed, b"z"),
                        _ => unreachable!("flag state only entered for a/d/t/o"),
                    };
                    if !valid.contains(&value) {
                        let name = match key {
                            b'a' => "action",
                            b'd' => "delete_action",
                            b't' => "transmission_type",
                            _ => "compressed",
                        };
                        return Err(CommandError::parse(format!(
                            "Malformed GraphicsCommand control block, unknown flag value for \
                             {name}: {value:#x}"
                        )));
                    }
                    *field = value;
                    state = State::AfterValue;
                },

                State::Uint => {
                    let code = read_uint(buf, &mut pos, key)?;
                    match key {
                        b'f' => g.format = code,
                        b'm' => g.more = code,
                        b'i' => g.id = code,
                        b'I' => g.image_number = code,
                        b'p' => g.placement_id = code,
                        b'q' => g.quiet = code,
                        b'w' => g.width = code,
                        b'h' => g.height = code,
                        b'x' => g.x_offset = code,
                        b'y' => g.y_offset = code,
                        b'v' => g.data_height = code,
                        b's' => g.data_width = code,
                        b'S' => g.data_sz = code,
                        b'O' => g.data_offset = code,
                        b'c' => g.num_cells = code,
                        b'r' => g.num_lines = code,
                        b'X' => g.cell_x_offset = code,
                        b'Y' => g.cell_y_offset = code,
                        b'C' => g.cursor_movement = code,
                        b'U' => g.unicode_placement = code != 0,
                        b'P' => g.parent_id = code,
                        b'Q' => g.parent_placement_id = code,
                        _ => unreachable!("uint state only entered for uint keys"),
                    }
                    state = State::AfterValue;
                },

                State::Int => {
                    let is_negative = buf[pos] == b'-';
                    if is_negative {
                        pos += 1;
                    }
                    let code = read_uint(buf, &mut pos, key)?;
                    // Kitty casts the unsigned magnitude with `(int32_t)`,
                    // which wraps; mirror that exactly.
                    let value =
                        if is_negative { (code as i32).wrapping_neg() } else { code as i32 };
                    match key {
                        b'z' => g.z_index = value,
                        b'H' => g.offset_from_parent_x = value,
                        b'V' => g.offset_from_parent_y = value,
                        _ => unreachable!("int state only entered for z/H/V"),
                    }
                    state = State::AfterValue;
                },

                State::AfterValue => {
                    let b = buf[pos];
                    pos += 1;
                    match b {
                        b',' => state = State::Key,
                        b';' => state = State::Payload,
                        _ => {
                            return Err(CommandError::parse(format!(
                                "Malformed GraphicsCommand control block, expecting a , or \
                                 semi-colon after a value, found: {b:#x}"
                            )));
                        },
                    }
                },

                State::Payload => {
                    g.payload = decode_base64(&buf[pos..])?;
                    pos = buf.len();
                },
            }
        }

        match state {
            State::Equal => {
                return Err(CommandError::parse(
                    "Malformed GraphicsCommand control block, no = after key".into(),
                ));
            },
            State::Uint | State::Int => {
                return Err(CommandError::parse(
                    "Malformed GraphicsCommand control block, expecting an integer value".into(),
                ));
            },
            State::Flag => {
                return Err(CommandError::parse(
                    "Malformed GraphicsCommand control block, expecting a flag value".into(),
                ));
            },
            _ => (),
        }

        // First check of kitty's grman_handle_command (graphics.c): a client
        // must not address an image by both id and number. Unlike the parse
        // errors above, kitty answers this one with an error response.
        if g.id != 0 && g.image_number != 0 {
            return Err(CommandError {
                code: ErrorCode::EINVAL,
                message: "Must not specify both image id and image number".into(),
                sends_response: true,
            });
        }

        Ok(g)
    }
}

/// Read a decimal integer of at most [`MAX_INT_DIGITS`] digits as a `u32`.
///
/// Mirrors kitty's `READ_UINT`: at least one digit is required, the digit
/// window is capped at 10 (an 11th digit is left in the buffer, where the
/// `AFTER_VALUE` state rejects it), and values above `u32::MAX` are an error.
fn read_uint(buf: &[u8], pos: &mut usize, key: u8) -> Result<u32, CommandError> {
    let start = *pos;
    let end = buf.len().min(start + MAX_INT_DIGITS);
    let mut accumulator = 0u64;
    let mut i = start;
    while i < end {
        let b = buf[i];
        if !b.is_ascii_digit() {
            break;
        }
        accumulator = accumulator * 10 + u64::from(b - b'0');
        i += 1;
    }

    if i == start {
        return Err(CommandError::parse(format!(
            "Malformed GraphicsCommand control block, expecting an integer value for key: {}",
            key as char
        )));
    }
    *pos = i;

    if accumulator > u64::from(u32::MAX) {
        return Err(CommandError::parse(
            "Malformed GraphicsCommand control block, number is too large".into(),
        ));
    }

    Ok(accumulator as u32)
}

/// Internal engine behind [`decode_base64`].
///
/// Each payload chunk passed to [`push`](Self::push) is expected to be a
/// multiple of 4 base64 characters (one-shot decoding), but the carry logic
/// handles arbitrary boundaries correctly. Call [`finish`](Self::finish) to
/// flush any unpadded tail.
///
/// Accepts the standard alphabet (`A-Z a-z 0-9 + /`), with `=` padding
/// optional. Invalid characters and dangling single sextets are rejected with
/// `EINVAL`.
#[derive(Debug, Default, Clone)]
struct Base64Decoder {
    /// Sextet values of the current (incomplete) quantum.
    quantum: [u8; 4],
    /// Number of sextets buffered in `quantum`.
    len: u8,
    /// Number of padding bytes seen in the current quantum.
    pad: u8,
    /// Set once a padded quantum completed; no further input is allowed.
    done: bool,
}

impl Base64Decoder {
    const fn new() -> Self {
        Self { quantum: [0; 4], len: 0, pad: 0, done: false }
    }

    /// Decode a chunk of base64 input, appending decoded bytes to `out`.
    fn push(&mut self, input: &[u8], out: &mut Vec<u8>) -> Result<(), CommandError> {
        out.reserve(input.len() / 4 * 3 + 2);
        for &b in input {
            if self.done {
                return Err(invalid_base64());
            }

            if b == b'=' {
                match (self.len, self.pad) {
                    // `xx==`: two sextets decode to one byte.
                    (2, 0) => self.pad = 1,
                    (2, 1) => {
                        out.push(self.quantum[0] << 2 | self.quantum[1] >> 4);
                        self.reset_quantum();
                        self.done = true;
                    },
                    // `xxx=`: three sextets decode to two bytes.
                    (3, 0) => {
                        out.push(self.quantum[0] << 2 | self.quantum[1] >> 4);
                        out.push(self.quantum[1] << 4 | self.quantum[2] >> 2);
                        self.reset_quantum();
                        self.done = true;
                    },
                    _ => return Err(invalid_base64()),
                }
                continue;
            }

            // A sextet may not follow padding within a quantum.
            if self.pad != 0 {
                return Err(invalid_base64());
            }

            let sextet = match b {
                b'A'..=b'Z' => b - b'A',
                b'a'..=b'z' => b - b'a' + 26,
                b'0'..=b'9' => b - b'0' + 52,
                b'+' => 62,
                b'/' => 63,
                _ => return Err(invalid_base64()),
            };

            self.quantum[self.len as usize] = sextet;
            self.len += 1;
            if self.len == 4 {
                let q = self.quantum;
                out.push(q[0] << 2 | q[1] >> 4);
                out.push(q[1] << 4 | q[2] >> 2);
                out.push(q[2] << 6 | q[3]);
                self.reset_quantum();
            }
        }
        Ok(())
    }

    /// Flush an unpadded trailing quantum, completing the stream.
    ///
    /// The decoder is reset and may be reused for a new stream afterwards.
    fn finish(&mut self, out: &mut Vec<u8>) -> Result<(), CommandError> {
        let len = self.len;
        let q = self.quantum;
        self.reset_quantum();
        self.done = false;
        match len {
            0 => (),
            // A single trailing sextet carries fewer than 8 bits.
            1 => return Err(invalid_base64()),
            2 => out.push(q[0] << 2 | q[1] >> 4),
            3 => {
                out.push(q[0] << 2 | q[1] >> 4);
                out.push(q[1] << 4 | q[2] >> 2);
            },
            _ => unreachable!("quantum is flushed at 4 sextets"),
        }
        Ok(())
    }

    fn reset_quantum(&mut self) {
        self.quantum = [0; 4];
        self.len = 0;
        self.pad = 0;
    }
}

/// Decode a complete base64 buffer in one shot.
fn decode_base64(input: &[u8]) -> Result<Vec<u8>, CommandError> {
    let mut out = Vec::with_capacity(input.len() / 4 * 3 + 2);
    let mut decoder = Base64Decoder::new();
    decoder.push(input, &mut out)?;
    decoder.finish(&mut out)?;
    Ok(out)
}

fn invalid_base64() -> CommandError {
    CommandError::parse("invalid base64 data in graphics command payload".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    use base64::prelude::{BASE64_STANDARD, Engine};
    use proptest::prelude::*;

    fn parse(s: &str) -> Result<GraphicsCommand, CommandError> {
        GraphicsCommand::parse(s.as_bytes())
    }

    fn ok(s: &str) -> GraphicsCommand {
        parse(s).unwrap_or_else(|err| panic!("expected {s:?} to parse, got {err}"))
    }

    fn err(s: &str) -> CommandError {
        match parse(s) {
            Ok(g) => panic!("expected {s:?} to fail, got {g:?}"),
            Err(err) => err,
        }
    }

    /// Encode a command back into its `<key>=<value>,...;<payload>` form.
    fn encode(g: &GraphicsCommand) -> String {
        let mut parts = Vec::new();
        for (key, value) in [
            ('a', g.action),
            ('d', g.delete_action),
            ('t', g.transmission_type),
            ('o', g.compressed),
        ] {
            if value != 0 {
                parts.push(format!("{key}={}", value as char));
            }
        }
        for (key, value) in [
            ('f', g.format),
            ('m', g.more),
            ('i', g.id),
            ('I', g.image_number),
            ('p', g.placement_id),
            ('q', g.quiet),
            ('w', g.width),
            ('h', g.height),
            ('x', g.x_offset),
            ('y', g.y_offset),
            ('v', g.data_height),
            ('s', g.data_width),
            ('S', g.data_sz),
            ('O', g.data_offset),
            ('c', g.num_cells),
            ('r', g.num_lines),
            ('X', g.cell_x_offset),
            ('Y', g.cell_y_offset),
            ('C', g.cursor_movement),
            ('U', u32::from(g.unicode_placement)),
            ('P', g.parent_id),
            ('Q', g.parent_placement_id),
        ] {
            if value != 0 {
                parts.push(format!("{key}={value}"));
            }
        }
        for (key, value) in
            [('z', g.z_index), ('H', g.offset_from_parent_x), ('V', g.offset_from_parent_y)]
        {
            if value != 0 {
                parts.push(format!("{key}={value}"));
            }
        }
        let mut encoded = parts.join(",");
        if !g.payload.is_empty() {
            encoded.push(';');
            encoded.push_str(&BASE64_STANDARD.encode(&g.payload));
        }
        encoded
    }

    #[test]
    fn empty_command() {
        assert_eq!(ok(""), GraphicsCommand::default());
    }

    #[test]
    fn every_key_is_parsed() {
        let g = ok("a=t,d=a,t=f,o=z,f=32,m=1,i=5,p=9,q=2,w=10,h=11,x=1,y=2,v=3,s=4,S=100,O=50,\
                    c=6,r=7,X=8,Y=9,z=-5,C=1,U=1,P=2,Q=3,H=-4,V=6");
        assert_eq!(g.action, b't');
        assert_eq!(g.delete_action, b'a');
        assert_eq!(g.transmission_type, b'f');
        assert_eq!(g.compressed, b'z');
        assert_eq!(g.format, 32);
        assert_eq!(g.more, 1);
        assert_eq!(g.id, 5);
        assert_eq!(g.placement_id, 9);
        assert_eq!(g.quiet, 2);
        assert_eq!(g.width, 10);
        assert_eq!(g.height, 11);
        assert_eq!(g.x_offset, 1);
        assert_eq!(g.y_offset, 2);
        assert_eq!(g.data_height, 3);
        assert_eq!(g.data_width, 4);
        assert_eq!(g.data_sz, 100);
        assert_eq!(g.data_offset, 50);
        assert_eq!(g.num_cells, 6);
        assert_eq!(g.num_lines, 7);
        assert_eq!(g.cell_x_offset, 8);
        assert_eq!(g.cell_y_offset, 9);
        assert_eq!(g.z_index, -5);
        assert_eq!(g.cursor_movement, 1);
        assert!(g.unicode_placement);
        assert_eq!(g.parent_id, 2);
        assert_eq!(g.parent_placement_id, 3);
        assert_eq!(g.offset_from_parent_x, -4);
        assert_eq!(g.offset_from_parent_y, 6);
        // `I=` separately since it conflicts with `i=`.
        assert_eq!(ok("I=7").image_number, 7);
    }

    #[test]
    fn overload_accessors() {
        let g = ok("a=a,s=2,v=7,r=3,c=4,z=40,C=1,Y=255");
        assert_eq!(g.animation_state(), 2);
        assert_eq!(g.loop_count(), 7);
        assert_eq!(g.frame_number(), 3);
        assert_eq!(g.other_frame_number(), 4);
        assert_eq!(g.gap(), 40);
        assert_eq!(g.compose_mode(), 1);
        assert_eq!(g.bgcolor(), 255);
    }

    #[test]
    fn every_action_char() {
        for action in b"tTpqdfac" {
            let g = ok(&format!("a={}", *action as char));
            assert_eq!(g.action, *action);
        }
        for invalid in ["a=x", "a=A", "a=D", "a=0", "a= "] {
            let e = err(invalid);
            assert_eq!(e.code, ErrorCode::EINVAL);
            assert!(e.message.contains("unknown flag value for action"), "{e}");
            assert!(!e.sends_response);
        }
    }

    #[test]
    fn every_delete_action_char() {
        for delete in b"ACFINPQRXYZacfinpqrxyz" {
            let g = ok(&format!("d={}", *delete as char));
            assert_eq!(g.delete_action, *delete);
        }
        for invalid in ["d=b", "d=B", "d=0"] {
            let e = err(invalid);
            assert!(e.message.contains("unknown flag value for delete_action"), "{e}");
        }
    }

    #[test]
    fn every_transmission_type_char() {
        for medium in b"dfst" {
            let g = ok(&format!("t={}", *medium as char));
            assert_eq!(g.transmission_type, *medium);
        }
        for invalid in ["t=x", "t=T", "t=D"] {
            let e = err(invalid);
            assert!(e.message.contains("unknown flag value for transmission_type"), "{e}");
        }
    }

    #[test]
    fn compression_flag() {
        assert_eq!(ok("o=z").compressed, b'z');
        let e = err("o=x");
        assert!(e.message.contains("unknown flag value for compressed"), "{e}");
    }

    #[test]
    fn flag_value_missing_at_end() {
        let e = err("a=");
        assert!(e.message.contains("expecting a flag value"), "{e}");
    }

    #[test]
    fn unknown_key_rejected() {
        for invalid in ["b=1", "e=1", "u=1", "Z=1", "A=t", ",=1"] {
            let e = err(invalid);
            assert_eq!(e.code, ErrorCode::EINVAL);
            assert!(e.message.contains("invalid key character"), "{e}");
        }
    }

    #[test]
    fn missing_equals() {
        let e = err("ax");
        assert!(e.message.contains("no = after key, found: 0x78"), "{e}");
        let e = err("a");
        assert!(e.message.contains("no = after key"), "{e}");
    }

    #[test]
    fn integer_values() {
        assert_eq!(ok("i=0").id, 0);
        assert_eq!(ok("i=42").id, 42);
        // 10 digits, exactly u32::MAX.
        assert_eq!(ok("i=4294967295").id, u32::MAX);
        // Leading zeros count toward the 10 digit window.
        assert_eq!(ok("i=0000000001").id, 1);
    }

    #[test]
    fn integer_no_digits() {
        for invalid in ["i=", "i=,a=t", "i=abc", "i=;QQ==", "q=-1"] {
            let e = err(invalid);
            assert!(e.message.contains("expecting an integer value"), "{e}");
        }
    }

    #[test]
    fn integer_too_large() {
        // 10 digits above u32::MAX.
        let e = err("i=4294967296");
        assert!(e.message.contains("number is too large"), "{e}");
        let e = err("i=9999999999");
        assert!(e.message.contains("number is too large"), "{e}");
    }

    #[test]
    fn integer_more_than_ten_digits() {
        // The 11th digit falls outside the digit window and is rejected as a
        // stray character after the value, exactly like kitty.
        for invalid in ["i=12345678901", "i=00000000001"] {
            let e = err(invalid);
            assert!(e.message.contains("expecting a , or semi-colon"), "{e}");
        }
    }

    #[test]
    fn signed_values() {
        assert_eq!(ok("z=-1").z_index, -1);
        assert_eq!(ok("z=2147483647").z_index, i32::MAX);
        assert_eq!(ok("z=-2147483648").z_index, i32::MIN);
        assert_eq!(ok("H=-7").offset_from_parent_x, -7);
        assert_eq!(ok("V=-9").offset_from_parent_y, -9);
        // Kitty casts the u32 magnitude to int32_t with wrapping.
        assert_eq!(ok("z=4294967295").z_index, -1);
        assert_eq!(ok("z=-4294967295").z_index, 1);
        assert_eq!(ok("z=-2147483649").z_index, 2147483647);
    }

    #[test]
    fn signed_value_dangling_minus() {
        for invalid in ["z=-", "z=-,a=t", "z=-x"] {
            let e = err(invalid);
            assert!(e.message.contains("expecting an integer value"), "{e}");
        }
    }

    #[test]
    fn value_missing_at_end() {
        for invalid in ["i=", "z=", "z=-"] {
            let e = err(invalid);
            assert!(e.message.contains("expecting an integer value"), "{e}");
        }
    }

    #[test]
    fn junk_after_value() {
        for invalid in ["i=1x", "a=tt", "i=1 ,a=t"] {
            let e = err(invalid);
            assert!(e.message.contains("expecting a , or semi-colon"), "{e}");
        }
    }

    #[test]
    fn trailing_comma_accepted() {
        // Kitty quirk: the loop may end in the KEY state.
        assert_eq!(ok("a=t,").action, b't');
    }

    #[test]
    fn duplicate_key_last_wins() {
        assert_eq!(ok("i=1,i=2").id, 2);
    }

    #[test]
    fn payload_decoding() {
        assert_eq!(ok("a=t;QUJD").payload, b"ABC");
        assert_eq!(ok("a=t;QQ==").payload, b"A");
        assert_eq!(ok("a=t;QQ").payload, b"A");
        assert_eq!(ok("a=t;QUI=").payload, b"AB");
        assert_eq!(ok("a=t;").payload, b"");
        // Payload with no keys at all.
        assert_eq!(ok(";QUJD").payload, b"ABC");
    }

    #[test]
    fn payload_base64_errors() {
        for invalid in ["a=t;Q!JD", "a=t;Q", "a=t;QQ==QQ", "a=t;Q=QQ", "a=t;QUJ D", "a=t;QQ==="] {
            let e = err(invalid);
            assert_eq!(e.code, ErrorCode::EINVAL);
            assert!(e.message.contains("invalid base64"), "{e}");
        }
    }

    #[test]
    fn id_and_image_number_conflict() {
        let e = err("i=1,I=2");
        assert_eq!(e.code, ErrorCode::EINVAL);
        assert_eq!(e.message, "Must not specify both image id and image number");
        assert!(e.sends_response);
        assert_eq!(e.to_string(), "EINVAL:Must not specify both image id and image number");
        // Either alone, or both zero, is fine.
        assert_eq!(ok("i=1").id, 1);
        assert_eq!(ok("I=2").image_number, 2);
        assert_eq!(ok("i=0,I=2").image_number, 2);
        assert_eq!(ok("i=1,I=0").id, 1);
    }

    #[test]
    fn unicode_placement_is_boolish() {
        assert!(!ok("U=0").unicode_placement);
        assert!(ok("U=1").unicode_placement);
        assert!(ok("U=7").unicode_placement);
    }

    #[test]
    fn error_codes_render_for_responses() {
        for (code, name) in [
            (ErrorCode::EINVAL, "EINVAL"),
            (ErrorCode::ENOENT, "ENOENT"),
            (ErrorCode::EBADF, "EBADF"),
            (ErrorCode::ENOMEM, "ENOMEM"),
            (ErrorCode::ENOSPC, "ENOSPC"),
            (ErrorCode::ENODATA, "ENODATA"),
            (ErrorCode::EFBIG, "EFBIG"),
            (ErrorCode::EPERM, "EPERM"),
            (ErrorCode::EILSEQ, "EILSEQ"),
            (ErrorCode::ECYCLE, "ECYCLE"),
            (ErrorCode::ETOODEEP, "ETOODEEP"),
            (ErrorCode::ENOPARENT, "ENOPARENT"),
        ] {
            assert_eq!(code.to_string(), name);
        }
    }

    #[test]
    fn streaming_decoder_chunks() {
        let data: Vec<u8> = (0u8..=255).collect();
        let encoded = BASE64_STANDARD.encode(&data);
        // Split at a few representative points: quantum boundaries and off-by-one
        // positions that exercise the cross-chunk carry.
        let len = encoded.len();
        for split in [0, 1, 3, 4, len / 2, len - 1, len] {
            let mut decoder = Base64Decoder::new();
            let mut out = Vec::new();
            decoder.push(&encoded.as_bytes()[..split], &mut out).unwrap();
            decoder.push(&encoded.as_bytes()[split..], &mut out).unwrap();
            decoder.finish(&mut out).unwrap();
            assert_eq!(out, data, "split at {split}");
        }
    }

    #[test]
    fn streaming_decoder_rejects_data_after_padding() {
        let mut decoder = Base64Decoder::new();
        let mut out = Vec::new();
        decoder.push(b"QQ==", &mut out).unwrap();
        assert!(decoder.push(b"QQ==", &mut out).is_err());
    }

    #[test]
    fn streaming_decoder_dangling_sextet() {
        let mut decoder = Base64Decoder::new();
        let mut out = Vec::new();
        decoder.push(b"Q", &mut out).unwrap();
        assert!(decoder.finish(&mut out).is_err());
    }

    fn arb_command() -> impl Strategy<Value = GraphicsCommand> {
        let flags = (
            prop_oneof![Just(0u8), prop::sample::select(b"tTpqdfac".to_vec())],
            prop_oneof![Just(0u8), prop::sample::select(b"ACFINPQRXYZacfinpqrxyz".to_vec())],
            prop_oneof![Just(0u8), prop::sample::select(b"dfst".to_vec())],
            prop_oneof![Just(0u8), Just(b'z')],
        );
        // `i=` and `I=` must not both be non-zero.
        let ids = prop_oneof![(any::<u32>(), Just(0u32)), (Just(0u32), any::<u32>())];
        let uints_a = (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
        );
        let uints_b = (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
        );
        let rest = (
            any::<u32>(),
            any::<u32>(),
            any::<u32>(),
            any::<i32>(),
            any::<i32>(),
            any::<i32>(),
            any::<bool>(),
            prop::collection::vec(any::<u8>(), 0..256),
        );
        (flags, ids, uints_a, uints_b, rest).prop_map(
            |(
                (action, delete_action, transmission_type, compressed),
                (id, image_number),
                (format, more, placement_id, quiet, width, height, x_offset, y_offset),
                (
                    data_height,
                    data_width,
                    data_sz,
                    data_offset,
                    num_cells,
                    num_lines,
                    cell_x_offset,
                    cell_y_offset,
                ),
                (
                    cursor_movement,
                    parent_id,
                    parent_placement_id,
                    z_index,
                    offset_from_parent_x,
                    offset_from_parent_y,
                    unicode_placement,
                    payload,
                ),
            )| GraphicsCommand {
                action,
                delete_action,
                transmission_type,
                compressed,
                format,
                more,
                id,
                image_number,
                placement_id,
                quiet,
                width,
                height,
                x_offset,
                y_offset,
                data_height,
                data_width,
                data_sz,
                data_offset,
                num_cells,
                num_lines,
                cell_x_offset,
                cell_y_offset,
                cursor_movement,
                parent_id,
                parent_placement_id,
                z_index,
                offset_from_parent_x,
                offset_from_parent_y,
                unicode_placement,
                payload,
            },
        )
    }

    proptest! {
        #[test]
        fn roundtrip(command in arb_command()) {
            let encoded = encode(&command);
            let parsed = GraphicsCommand::parse(encoded.as_bytes())
                .unwrap_or_else(|err| panic!("failed to parse {encoded:?}: {err}"));
            prop_assert_eq!(parsed, command);
        }
    }
}
