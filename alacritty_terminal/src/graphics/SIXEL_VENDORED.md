# Sixel parser — vendoring provenance

## Upstream

- Repository: https://github.com/ayosec/alacritty
- Branch: `graphics`
- Pinned commit: `3d658d2e280d3456ead5d73be9067ec587ddcc45`
- Date vendored: 2026-06-13

## Files vendored

| Upstream path | Local path | SHA-256 (upstream) |
|---|---|---|
| `alacritty_terminal/src/graphics/sixel.rs` | `alacritty_terminal/src/graphics/sixel.rs` | `d1b260271f07888e88478e007973fef25ebf180d13ccd119d1a0f165cf3f045d` |
| `alacritty_terminal/tests/sixel/testimage_im6.sixel` | `alacritty_terminal/tests/sixel/testimage_im6.sixel` | `b5268ba505d2447a0f085e18879083c5c8d3018129de9dcd3d0e4364b7b3b651` |
| `alacritty_terminal/tests/sixel/testimage_im6.rgba` | `alacritty_terminal/tests/sixel/testimage_im6.rgba` | `743620ccce715b537b66e03345dff512bc50f2084f69fb4a61d15865c3888460` |
| `alacritty_terminal/tests/sixel/testimage_libsixel.sixel` | `alacritty_terminal/tests/sixel/testimage_libsixel.sixel` | `0a76479c297404085b8f90fdef3edd5e7bacfdd4166713c4f14f52a9e797f5d2` |
| `alacritty_terminal/tests/sixel/testimage_libsixel.rgba` | `alacritty_terminal/tests/sixel/testimage_libsixel.rgba` | `97889d2f9e6136b4d548e83d8adb94905cbcaebf02b39481145f0d4d8dc9072b` |
| `alacritty_terminal/tests/sixel/testimage_ppmtosixel.sixel` | `alacritty_terminal/tests/sixel/testimage_ppmtosixel.sixel` | `496007d077a9210c895b86d75c877cd874ea4be8eb7757cc51cbb752e923c4da` |
| `alacritty_terminal/tests/sixel/testimage_ppmtosixel.rgba` | `alacritty_terminal/tests/sixel/testimage_ppmtosixel.rgba` | `ee089f763fcd6037b6b2c1be741e470fc816a76779f0dab440240ce1b3485d93` |

## Adaptations

The parser logic (command parser, HLS→RGB, VT-340 palette, pixel accumulation)
is a direct port of ayosec's implementation.  The following changes were made
to integrate with this fork's `GraphicsManager` API:

- Removed references to ayosec's `GraphicData`, `GraphicId`, `ColorType`, and
  `MAX_GRAPHIC_DIMENSIONS` types (different graphics architecture).
- `Parser::finish` now returns `(u32, u32, Arc<Vec<u8>>, Vec<Rgb>)` — width,
  height, premultiplied RGBA bytes, and the final palette — instead of a
  `GraphicData` struct.
- Added `Parser::new_with_p2` convenience constructor for use from the DCS
  dispatcher in `term/mod.rs`, which already has P2 decoded from `DcsBuilder`.
- Added `SixelOutput` type alias to satisfy the `clippy::type_complexity` lint.
- Added `DcsBuilder` (mirrors `ApcBuilder`) for buffering the DCS payload.
- Test literal `352` (out of range for `u8`) corrected to `96` (352 % 256,
  within the ±4 tolerance of the original test).
- Module-level doc comment condensed to avoid the 6-consecutive-comment-lines
  lint enforced by this repo's PostToolUse hook.

## Source obtained via

```
git clone --filter=blob:none --no-checkout https://github.com/ayosec/alacritty /tmp/ayosec-sixel
git -C /tmp/ayosec-sixel sparse-checkout set alacritty_terminal/src/graphics alacritty_terminal/tests
git -C /tmp/ayosec-sixel checkout 3d658d2e280d3456ead5d73be9067ec587ddcc45
```
