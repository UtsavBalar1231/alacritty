<p align="center">
    <img width="200" alt="Alacritty Logo" src="https://raw.githubusercontent.com/alacritty/alacritty/master/extra/logo/compat/alacritty-term%2Bscanlines.png">
</p>

<h1 align="center">Alacritty — graphics fork</h1>

<p align="center">
  A downstream fork of <a href="https://github.com/alacritty/alacritty">Alacritty</a>, the fast,
  cross-platform, OpenGL terminal emulator — extended with inline terminal graphics,
  a built-in tab bar, and improved built-in glyph rendering.
</p>

## About this fork

This is a personal fork of [Alacritty] maintained by Utsav Balar. It tracks
upstream Alacritty and layers a set of additional features on top of it:

- **Inline terminal graphics** via the kitty graphics protocol, Sixel, and the
  iTerm2 inline-image protocol — display images directly in the grid.
- **A built-in tab bar** with configurable position, separators, margins,
  templates, and activity/bell indicators (upstream Alacritty has no tabs).
- **Improved built-in glyph and symbol rendering** for box-drawing, block, and
  related characters.

Everything else is upstream Alacritty: the same renderer, the same configuration
format, the same performance characteristics. The fork is additive — existing
configs keep working, and every new feature is opt-out via configuration.

> This fork is **not** affiliated with or endorsed by the upstream Alacritty
> project. Please do not report issues with these patches to upstream. The
> changes here are not intended for upstream contribution.

[Alacritty]: https://github.com/alacritty/alacritty

## Fork features

### Terminal graphics

Three inline-image protocols are supported, all enabled by default and
independently toggleable in the `[graphics]` config section:

| Protocol | Trigger | Notes |
|----------|---------|-------|
| **kitty graphics protocol** | APC `_G` | Direct, base64, and shared-memory upload; placement with optional Unicode placeholder (v1); delete commands; multi-frame animations. Works on both the GLSL 3 and GLES 2 renderer paths. |
| **Sixel** | DCS `q` | Sixel sequences decoded and displayed inline, with a per-image color-register model. |
| **iTerm2 inline images** | OSC `1337;File=` | Common image formats (PNG, JPEG, GIF, WebP, …) displayed inline. |

Images are composited in the OpenGL renderer, scroll with the grid, and are
removed when their anchor scrolls out of the scrollback buffer. Tools that emit
these protocols — e.g. `yazi`, `timg`, `chafa`, `img2sixel`, `viu`, and kitty's
`icat` — display images inline.

A full description of capabilities, the storage model, and the deliberate
divergences from kitty's reference implementation is in
[`docs/features.md`](./docs/features.md#terminal-graphics).

### Built-in tab bar

A lightweight tab bar rendered inside the Alacritty window, configured under
`[window.tab_bar]`. Tabs are created and switched via keybinding actions
(`CreateNewTab`, `CloseTab`, `SelectNextTab`, `SelectTab1`…`SelectTab9`,
`MoveTabLeft`, …) or `alacritty msg`. It supports:

- Auto/Always/Never visibility, `Left`/`Center`/`Right` alignment.
- `Plain` or `Separator` styles with a configurable separator string.
- Horizontal and vertical (outer/inner) margins.
- Tab title templates with `{index}`, `{title}`, `{activity}`, and `{bell}`
  placeholders, plus separate active/inactive templates.
- Per-tab activity and bell indicators.
- Tab-bar colors via `[colors.tab_bar.active]` / `[colors.tab_bar.inactive]`.

### Improved built-in glyph rendering

When `font.builtin_box_drawing` is enabled (the default), box-drawing, block,
and related symbol glyphs are rendered by Alacritty itself rather than the font,
for crisp, gap-free lines and blocks regardless of the configured font.

## Configuration

The configuration format is identical to upstream Alacritty (see `man 5
alacritty` or [the website]). The fork adds the following:

```toml
# Inline terminal graphics (kitty / Sixel / iTerm2).
[graphics]
enabled         = true   # master switch; disabling removes all images immediately
kitty_protocol  = true
sixel           = true
iterm2          = true
max_storage_mib = 320     # decoded-image storage quota in MiB (frame data capped at 5×)

# Built-in tab bar.
[window.tab_bar]
visibility              = "Auto"        # Auto | Always | Never
position                = "Bottom"      # Bottom (only supported position)
alignment               = "Left"        # Left | Center | Right
style                   = "Separator"   # Plain | Separator
separator               = " ┇ "
margin_width            = 0.0
margin_height           = { outer = 0.0, inner = 0.0 }
max_width               = 24
min_width               = 6
show_index              = true
close_button            = "Hover"       # Never | Hover | Always
# title_template        = "{index} {title}"
# active_title_template = "{index} {title}"
# inactive_title_template = "{bell}{activity}{index} {title}"
activity_indicator      = "•"
bell_indicator          = "!"
show_activity_indicator = true

# Crisp built-in box-drawing/symbol glyphs.
[font]
builtin_box_drawing = true
```

All of these honor `live_config_reload`. A fully worked example config —
including tab keybindings, tab-bar colors, and a `[graphics]` section — lives in
this repository's history and at `~/.config/alacritty/alacritty.toml` on the
maintainer's system.

Alacritty looks for its config file in (first match wins):

1. `$XDG_CONFIG_HOME/alacritty/alacritty.toml`
2. `$XDG_CONFIG_HOME/alacritty.toml`
3. `$HOME/.config/alacritty/alacritty.toml`
4. `$HOME/.alacritty.toml`
5. `/etc/alacritty/alacritty.toml`

[the website]: https://alacritty.org/config-alacritty.html

## Building & installing

### Requirements

- A Rust toolchain (stable) and `cargo`.
- At least OpenGL ES 2.0.
- Build dependencies: `cmake`, `fontconfig`, `freetype2`, `libxcb`, plus the
  usual X11/Wayland client libraries. `scdoc` is needed for the man pages.

### Arch Linux (recommended)

This repository ships a `PKGBUILD` that builds **strictly from the local
working tree** (never a remote clone) and produces an `alacritty-dev` package
that `conflicts`/`replaces` the stock `alacritty`:

```sh
cd /path/to/this/repo
makepkg -si
```

`makepkg` derives the package version from `git` (`<version>.r<rev>.g<commit>`),
runs the test suite as part of `check()`, and installs via `pacman`.

> **Note:** if you export `CARGO_TARGET_DIR` globally (e.g. in `~/.zshenv`),
> unset it for the build — the `PKGBUILD`'s `package()` step expects the binary
> at the in-tree `target/release/alacritty`:
> `env -u CARGO_TARGET_DIR makepkg -si`.

### Other platforms / generic build

```sh
cargo build --release
# binary: ./target/release/alacritty
```

See [`INSTALL.md`](INSTALL.md) for terminfo, desktop entry, and per-platform
details from upstream.

## Relationship to upstream

This fork stays close to upstream Alacritty and merges upstream changes
periodically. The graphics implementation intentionally diverges from kitty's
reference in a few documented ways (no on-disk frame cache, placements dropped
on reflow, scrolled-past placements hard-deleted, sRGB color-space conversion
skipped, Unicode placeholder v1 only) — see
[`docs/features.md`](./docs/features.md#known-divergences-from-kitty).

For upstream Alacritty itself, its features, and its community:

- Upstream repository: <https://github.com/alacritty/alacritty>
- Upstream config docs: <https://alacritty.org/config-alacritty.html>

## License

Alacritty is released under the [Apache License, Version 2.0], and this fork
inherits that license. See [`LICENSE-APACHE`](LICENSE-APACHE) and
[`LICENSE-MIT`](LICENSE-MIT).

[Apache License, Version 2.0]: https://github.com/alacritty/alacritty/blob/master/LICENSE-APACHE
