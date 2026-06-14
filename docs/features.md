# Features

This document gives an overview over Alacritty's features beyond its terminal
emulation capabilities. To get a list with supported control sequences take a
look at [Alacritty's escape sequence support](./escape_support.md).

## Vi Mode

The vi mode allows moving around Alacritty's viewport and scrollback using the
keyboard. It also serves as a jump-off point for other features like search and
opening URLs with the keyboard. By default you can launch it using
<kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>Space</kbd>.

### Motion

The cursor motions are setup by default to mimic vi, however they are fully
configurable. If you don't like vi's bindings, take a look at the configuration
file to change the various movements.

### Selection

One useful feature of vi mode is the ability to make selections and copy text to
the clipboard. By default you can start a selection using <kbd>v</kbd> and copy
it using <kbd>y</kbd>. All selection modes that are available with the mouse can
be accessed from vi mode, including the semantic (<kbd>Alt</kbd> <kbd>v</kbd>),
line (<kbd>Shift</kbd> <kbd>v</kbd>) and block selection (<kbd>Ctrl</kbd>
<kbd>v</kbd>). You can also toggle between them while the selection is still
active.

## Search

Search allows you to find anything in Alacritty's scrollback buffer. You can
search forward using <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>f</kbd> (<kbd>Command</kbd> <kbd>f</kbd> on macOS) and
backward using <kbd>Ctrl</kbd> <kbd>Shift</kbd> <kbd>b</kbd> (<kbd>Command</kbd> <kbd>b</kbd> on macOS).

### Vi Search

In vi mode the search is bound to <kbd>/</kbd> for forward and <kbd>?</kbd> for
backward search. This allows you to move around quickly and help with selecting
content. The `SearchStart` and `SearchEnd` keybinding actions can be bound if
you're looking for a way to jump to the start or the end of a match.

### Normal Search

During normal search you don't have the opportunity to move around freely, but
you can still jump between matches using <kbd>Enter</kbd> and <kbd>Shift</kbd>
<kbd>Enter</kbd>. After leaving search with <kbd>Escape</kbd> your active match
stays selected, allowing you to easily copy it.

## Hints

Terminal hints allow easily interacting with visible text without having to
start vi mode. They consist of a regex that detects these text elements and then
either feeds them to an external application or triggers one of Alacritty's
built-in actions.

Hints can also be triggered using the mouse or vi mode cursor. If a hint is
enabled for mouse interaction and recognized as such, it will be underlined when
the mouse or vi mode cursor is on top of it. Using the left mouse button or
<kbd>Enter</kbd> key in vi mode will then trigger the hint.

Hints can be configured in the `hints` and `colors.hints` sections in the
Alacritty configuration file.

## Selection expansion

After making a selection, you can use the right mouse button to expand it.
Double-clicking will expand the selection semantically, while triple-clicking
will perform line selection. If you hold <kbd>Ctrl</kbd> while expanding the
selection, it will switch to the block selection mode.

## Opening URLs with the mouse

You can open URLs with your mouse by clicking on them. The modifiers required to
be held and program which should open the URL can be setup in the configuration
file. If an application captures your mouse clicks, which is indicated by a
change in mouse cursor shape, you're required to hold <kbd>Shift</kbd> to bypass
that.

## Multi-Window

Alacritty supports running multiple terminal emulators from the same Alacritty
instance. New windows can be created either by using the `CreateNewWindow`
keybinding action, or by executing the `alacritty msg create-window` subcommand.

## Terminal Graphics

Alacritty supports three terminal graphics protocols for displaying inline
images. All three are enabled by default and can be toggled independently in the
`[graphics]` config section.

### Supported Protocols

**Kitty graphics protocol** (`kitty_protocol = true`)  
Full support for the kitty graphics protocol (APC `G` commands), including
direct, base64, and shared-memory image upload, placement with optional Unicode
placeholder (v1), delete commands, and multi-frame animations. Both the GLSL 3
and GLES 2 renderer paths are supported.

**Sixel** (`sixel = true`)  
Sixel image sequences are decoded and displayed inline as images. Color
registers use a per-image model.

**iTerm2 inline images** (`iterm2 = true`)  
The iTerm2 `ESC]1337;File=` inline image protocol is supported for common
image formats (PNG, JPEG, GIF, WebP, and others).

### Configuration

```toml
[graphics]
enabled          = true   # master switch; disabling removes all images immediately
kitty_protocol   = true
sixel            = true
iterm2           = true
max_storage_mib  = 320    # decoded image storage quota in MiB (frame data capped at 5×)
```

Changes to `[graphics]` take effect on live config reload. Disabling `enabled`
removes all displayed images immediately.

### Known Divergences from kitty

This implementation differs from kitty's reference in the following deliberate
ways:

- **No disk cache**: kitty spills animation frame data to disk to bound RAM
  use. This fork keeps all frame data in memory, capped at five times
  `max_storage_mib`. This avoids filesystem I/O at the cost of higher peak RAM
  when large animated images are displayed.

- **Reflow drops placements**: When the terminal grid is reflowed on window
  resize, image placements are removed rather than re-anchored. kitty
  re-anchors placements after reflow.

- **Hard-delete scrolled-past placements**: Placements whose anchor row scrolls
  off the top of the scrollback buffer are deleted immediately rather than
  retained in scrollback.

- **sRGB / color-space skip**: Images are uploaded to the GPU without
  color-space conversion. kitty applies sRGB linearization during compositing.

- **Unicode placeholder v1 only**: Only Unicode placeholder version 1
  (single-codepoint per cell) is implemented. kitty's v2 diacritic column
  indices are not supported.
