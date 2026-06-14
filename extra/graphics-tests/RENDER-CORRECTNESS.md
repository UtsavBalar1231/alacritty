# Render-Correctness Harness

Position/color-aware rendering regression tests for the kitty graphics renderer.
Runs entirely headless under Xvfb with no golden PNG files — every assertion is
an explicit coordinate+color check against an asymmetric fixture.

## How to run

```sh
bash extra/graphics-tests/render-correctness.sh
```

Exits 0 if all cases pass, 1 if any fail.

## Why not golden-AE tests?

The prior golden harness (`run.sh`) recorded goldens with `RECORD_GOLDENS=1` from the
build under test. When the renderer had a double-Y-flip bug, goldens were recorded
upside-down and 42/42 tests approved the broken output — `compare -metric AE` found
no difference because the golden matched the capture exactly. A second bug (image offset
by 2×padding) was also invisible because the probe forced `padding=0`.

Coordinate+color assertions catch these bugs because:
- They never record from the build under test.
- An asymmetric fixture changes the color at every sample point when flipped or mirrored.
- The padding case measures a pixel coordinate, not image content.

## Test cases

### Case 1: orientation

Fixture: 240x240 image with TL=red, TR=lime, BL=blue, BR=yellow (four distinct quadrants).

Samples the interior of each quadrant and asserts the expected color.

What it catches:
- **Y-flip**: TL↔BL and TR↔BR swap colors.
- **H-mirror**: TL↔TR and BL↔BR swap colors.
- Any combination of both.

### Case 2: padding-alignment

Fixture: solid magenta image rendered at cell (0,0) with `window.padding.x/y=24`.

Isolates the magenta region in the capture via color mask and reads its bounding-box
offset using `magick -trim` (without `+repage`, which would erase the offset).
Asserts the top-left corner is within ±10 px of 24, not ~48.

What it catches:
- The **double-offset regression**: `quad_corners` adding padding on top of a GL
  viewport that was already translated by padding, placing images at 2×pad instead of pad.

### Case 3: horizontal-mirror

Fixture: 240x240 image with left half=red, right half=blue.

Samples left-center and right-center, asserts left=red and right=blue.

What it catches:
- **H-mirror** independently of any Y behavior (Case 1 catches both together;
  this isolates the horizontal axis).

### Case 4: multi-cell tiling

Fixture: same 4-quadrant layout as Case 1, but rendered at `--size=48x24` (2 cells
wide × 2 cells tall) to exercise the tiling/row path.

Samples the interior of each quadrant.

What it catches:
- **Row-order bugs** in tiling: if rows are emitted in reverse order, TL↔BL and
  TR↔BR colors swap, identical to a Y-flip but triggered by the tile sequencing.

### Case 5: source-crop

Fixture: 200×100 image whose left 100 px are solid red and right 100 px are solid
blue (f=24 RGB, constructed directly in python3, no PNG dependency).

APC sequence used:
```
ESC_Ga=t,f=24,s=200,v=100,i=10,q=2;<base64-RGB>ESC\
ESC_Ga=p,i=10,x=100,y=0,w=100,h=100,c=12,r=6,q=2;ESC\
```

The transmit command (`a=t`) stores the full 200×100 image under id 10. The put
command (`a=p`) places it with a source crop: `x=100,y=0,w=100,h=100` selects only
the right (blue) half. The test samples the center of the rendered region and asserts
it is blue.

What it catches:
- **Source-crop offset correctness**: `x=`, `y=`, `w=`, `h=` select the right
  sub-region (not the whole image, not the wrong half). The prior `kitty-crop-blue`
  golden in `run.sh` was created from a capture with zero image pixels and proved
  nothing.

Parser support: `x=` (`x_offset`), `y=` (`y_offset`), `w=` (`width`), `h=` (`height`)
are all parsed by `kitty_command.rs` and mapped to `PlacementSpec.src_x/src_y/src_width/src_height`
in `term/mod.rs:2022-2025`. UV coordinates computed at `graphics/mod.rs:2583-2587`.

### Case 6: z-order

Fixture: two solid-color images (red and blue), each 120×60 px f=24, placed at the
same cell position using `C=1` (cursor-no-move) so they genuinely overlap.

Subcase A — blue wins (higher z):
```
ESC_Ga=t,f=24,s=120,v=60,i=20,q=2;<red-base64>ESC\
ESC_Ga=t,f=24,s=120,v=60,i=21,q=2;<blue-base64>ESC\
ESC_Ga=p,i=20,c=12,r=6,z=0,C=1,q=2;ESC\
ESC_Ga=p,i=21,c=12,r=6,z=1,C=1,q=2;ESC\
```
Red placed at z=0, blue at z=1. Items are sorted `(z_index, image_id, placement_id)`
ascending and drawn in order; last draw wins. Blue (z=1) is drawn last → blue wins.

Subcase B — red wins (inverse, red has higher z):
```
ESC_Ga=p,i=30,c=12,r=6,z=1,C=1,q=2;ESC\
ESC_Ga=p,i=31,c=12,r=6,z=0,C=1,q=2;ESC\
```
Red placed at z=1, blue at z=0. Red (z=1) is drawn last → red wins.

What it catches:
- **Z-order stacking**: higher z-index placements occlude lower ones. If the sort
  or draw order is wrong, the lower-z image wins the overlap.

Note: `C=1` is required on every put command so that both images are anchored at the
same cursor position. Without `C=1` the cursor advances after the first put and the
second image lands at a different cell — the images never overlap and the test cannot
distinguish z-order from image position.

Parser support: `z=` (`z_index`, signed i32) is parsed by `kitty_command.rs` and
mapped to `PlacementSpec.z_index` in `term/mod.rs:2031`. `C=` (`cursor_movement`)
is also parsed and suppresses cursor advance when non-zero.

## Known gaps

The following scenario was considered but excluded:

- **Cross-bucket z-order** (e.g. z=-1 `BetweenBgAndText` vs z=2 `AboveText`): the
  render pipeline draws `BetweenBgAndText` images before glyphs and `AboveText`
  images after glyphs. Cross-bucket ordering depends on the GL compositing of those
  two separate draw passes and is not straightforwardly observable as a single pixel
  color. Cases 6A and 6B cover within-bucket ordering (both images `AboveText`),
  which exercises the sort and draw-order path directly.
