# alacritty graphics-tests

Headless golden-image test harness. Runs alacritty under Xvfb, feeds escape-stream
fixtures into the terminal, captures with ImageMagick `import -window root`, and
compares against golden PNGs.

## Running

```sh
./extra/graphics-tests/run.sh
```

Re-record goldens (run after any intentional rendering change):

```sh
RECORD_GOLDENS=1 ./extra/graphics-tests/run.sh
```

## Dependencies

**Required (harness):** `Xvfb`, `import`, `compare` (ImageMagick), built alacritty binary.

```sh
pacman -S xorg-server-xvfb imagemagick
cargo build -p alacritty
```

**Optional (later phases):** `kitten timg chafa viu yazi notcurses-demo mpv img2sixel lsix wezterm`.
Missing client tools are reported in the preflight and their tests skipped — the harness never hangs.

## Capture method (FIXED)

Every golden is recorded with `import -display :99 -window root <output.png>`. This is
the sole capture backend. Do not mix in scrot, xwd, or import-by-window-name.

## Renderer paths

Alacritty selects GLSL3 vs GLES2 via the config key `[debug] renderer`, passed on the
CLI as `-o 'debug.renderer="Gles2"'`. There is no `ALACRITTY_RENDERER` environment
variable. Source: `alacritty/src/renderer/mod.rs` (`RendererPreference` enum).

Each test runs on both renderer paths. Under software GL (llvmpipe / Xvfb) both paths
produce pixel-identical output, so one shared golden covers both. If a test ever
diverges per-renderer, name the goldens `<test>-glsl3.png` and `<test>-gles2.png` and
update the lookup in `run_test()`.

## Compare metric and threshold

`compare -metric AE -fuzz 5%`, threshold **2000 AE pixels**. SGR solid-block fills are
deterministic; 2000 px absorbs sub-pixel AA fringe on window edges.

## Directory layout

```
extra/graphics-tests/
  run.sh          — harness entry point
  fixtures/       — raw escape-stream binary files fed into alacritty
  goldens/        — reference PNG images (one per test, committed)
  README.md       — this file
```

## Adding a new test (Task 17+)

1. Write the fixture bytes to `fixtures/<name>.bin`.
2. Record the golden: `RECORD_GOLDENS=1 ./extra/graphics-tests/run.sh` (after adding the
   `run_test` call in `main()`).
3. Verify the golden visually, then commit `goldens/<name>.png`.

Graphics-protocol image goldens (kitty icat, sixel, iTerm2) are added starting in
Task 17 and reuse this same harness unchanged.
