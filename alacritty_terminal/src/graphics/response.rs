//! Kitty graphics protocol response building.
//!
//! Port of kitty's `finish_command_response` (`kitty/graphics.c`): every
//! response echoes the identity keys of the command it answers (`i=`, `I=`,
//! `p=`), followed by `;OK` or `;CODE:message`, wrapped in an APC sequence:
//!
//! ```text
//! ESC _ G i=<id>,I=<number>,p=<placement> ; OK ESC \
//! ```
//!
//! Suppression rules (kitty `finish_command_response`, graphics.c:788-813):
//!
//! * `q=1` suppresses the `OK` response, `q=2` suppresses *all* responses.
//! * A command without an image id (`i=`) and image number (`I=`) is never answered, success or
//!   failure.
//! * A successful command whose data transfer is still incomplete (`m=1` chunk accepted, more to
//!   come) gets no `OK`; errors mid-stream are still reported.

use crate::graphics::kitty_command::CommandError;

/// Identity keys echoed back in a kitty graphics response.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResponseEcho {
    /// `i=`: client image id.
    pub id: u32,
    /// `I=`: client image number.
    pub image_number: u32,
    /// `p=`: client placement id.
    pub placement_id: u32,
    /// `q=`: response suppression level.
    pub quiet: u32,
    /// `r=`: frame/num_lines, echoed for `a=f`/`a=a` when non-zero
    /// (kitty `finish_command_response`, graphics.c:805).
    pub num_lines: u32,
}

/// Build the full APC response for a graphics command, or `None` when
/// kitty's suppression rules call for silence.
///
/// `error` is the command failure, if any; `data_loaded` reports whether the
/// image data transfer is complete (always pass `true` for non-transmission
/// commands, kitty does the same).
pub fn build_response(
    echo: &ResponseEcho,
    error: Option<&CommandError>,
    data_loaded: bool,
) -> Option<String> {
    let is_ok = error.is_none();

    // q=1 suppresses OK, q=2 suppresses everything.
    if echo.quiet != 0 && (is_ok || echo.quiet > 1) {
        return None;
    }

    // Commands without any image identity are never answered.
    if echo.id == 0 && echo.image_number == 0 {
        return None;
    }

    // No `OK` for an accepted-but-incomplete chunk (m=1 mid-stream).
    if is_ok && !data_loaded {
        return None;
    }

    let mut text = String::from("\x1b_G");
    if echo.id != 0 {
        text.push_str(&format!("i={}", echo.id));
    }
    // Kitty prints `,I=`/`,p=` with an unconditional comma prefix; mirrored
    // exactly, including the `G,I=...` form when only `I=` is present.
    if echo.image_number != 0 {
        text.push_str(&format!(",I={}", echo.image_number));
    }
    if echo.placement_id != 0 {
        text.push_str(&format!(",p={}", echo.placement_id));
    }
    // Kitty echoes r= for a=f/a=a when num_lines is non-zero (graphics.c:805).
    if echo.num_lines != 0 {
        text.push_str(&format!(",r={}", echo.num_lines));
    }
    text.push(';');
    match error {
        // `CommandError` displays as `CODE:message`.
        Some(error) => text.push_str(&error.to_string()),
        None => text.push_str("OK"),
    }
    text.push_str("\x1b\\");

    Some(text)
}

/// Best-effort scan of a raw control block for the response echo keys.
///
/// Used when a response must be built without a successfully parsed
/// [`GraphicsCommand`](crate::graphics::kitty_command::GraphicsCommand)
/// (oversized APC payloads, or the `i=`+`I=` `EINVAL` which kitty raises
/// *after* parsing). Malformed entries are ignored; the corresponding key
/// stays `0`.
pub fn scan_echo_keys(body: &[u8]) -> ResponseEcho {
    let control = body.split(|&b| b == b';').next().unwrap_or(&[]);
    let mut echo = ResponseEcho::default();

    for entry in control.split(|&b| b == b',') {
        let (key, value) = match entry.split_first() {
            Some((key, [b'=', value @ ..])) => (*key, value),
            _ => continue,
        };
        let field = match key {
            b'i' => &mut echo.id,
            b'I' => &mut echo.image_number,
            b'p' => &mut echo.placement_id,
            b'q' => &mut echo.quiet,
            _ => continue,
        };
        if let Ok(value) = std::str::from_utf8(value).unwrap_or("").parse() {
            *field = value;
        }
    }

    echo
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graphics::kitty_command::ErrorCode;

    fn error(code: ErrorCode, message: &str) -> CommandError {
        CommandError { code, message: message.into(), sends_response: true }
    }

    #[test]
    fn ok_response_echoes_identity_keys() {
        let echo = ResponseEcho { id: 31, ..Default::default() };
        assert_eq!(build_response(&echo, None, true).unwrap(), "\x1b_Gi=31;OK\x1b\\");

        let echo = ResponseEcho { id: 1, image_number: 5, placement_id: 7, quiet: 0, num_lines: 0 };
        assert_eq!(build_response(&echo, None, true).unwrap(), "\x1b_Gi=1,I=5,p=7;OK\x1b\\");

        // Kitty's unconditional comma before `I=`.
        let echo = ResponseEcho { image_number: 5, ..Default::default() };
        assert_eq!(build_response(&echo, None, true).unwrap(), "\x1b_G,I=5;OK\x1b\\");
    }

    #[test]
    fn error_response_format() {
        let echo = ResponseEcho { id: 3, ..Default::default() };
        let err = error(ErrorCode::ENOENT, "no such image");
        assert_eq!(
            build_response(&echo, Some(&err), false).unwrap(),
            "\x1b_Gi=3;ENOENT:no such image\x1b\\"
        );
    }

    #[test]
    fn suppression_rules() {
        let err = error(ErrorCode::EINVAL, "bad");

        // q=1: OK suppressed, errors still sent.
        let echo = ResponseEcho { id: 1, quiet: 1, ..Default::default() };
        assert_eq!(build_response(&echo, None, true), None);
        assert!(build_response(&echo, Some(&err), false).is_some());

        // q=2: everything suppressed.
        let echo = ResponseEcho { id: 1, quiet: 2, ..Default::default() };
        assert_eq!(build_response(&echo, None, true), None);
        assert_eq!(build_response(&echo, Some(&err), false), None);

        // No identity keys: silent, even on error.
        let echo = ResponseEcho::default();
        assert_eq!(build_response(&echo, None, true), None);
        assert_eq!(build_response(&echo, Some(&err), false), None);

        // Incomplete chunk: no OK, but errors are reported.
        let echo = ResponseEcho { id: 1, ..Default::default() };
        assert_eq!(build_response(&echo, None, false), None);
        assert!(build_response(&echo, Some(&err), false).is_some());
    }

    #[test]
    fn scan_extracts_echo_keys() {
        let echo = scan_echo_keys(b"a=t,i=1,I=2,p=3,q=2,f=32;AAAA");
        assert_eq!(echo, ResponseEcho {
            id: 1,
            image_number: 2,
            placement_id: 3,
            quiet: 2,
            num_lines: 0
        });

        // Malformed entries are skipped, keys after the payload separator are
        // not scanned.
        let echo = scan_echo_keys(b"i=x,I=4,zz,q;i=9");
        assert_eq!(echo, ResponseEcho { image_number: 4, ..Default::default() });

        assert_eq!(scan_echo_keys(b""), ResponseEcho::default());
    }
}
