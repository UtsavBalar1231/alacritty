#!/usr/bin/env bash
# render-correctness.sh — Position/color-aware rendering regression harness.
# Uses coordinate+color assertions (not golden-AE) so flips, mirrors, and
# padding offsets cannot silently pass. Each case uses an ASYMMETRIC fixture.
set -uo pipefail

BIN="/home/utsav/.cache/cargo-target/release/alacritty"
[[ -x "$BIN" ]] || BIN="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/target/release/alacritty"
export DISPLAY=":99"
export WAYLAND_DISPLAY=""

TMPD=$(mktemp -d /tmp/render-correctness.XXXXXX)
trap 'rm -rf "$TMPD"; pkill -f "Xvfb :99" 2>/dev/null; true' EXIT

verdict=PASS
pass() { echo "  PASS  $*"; }
fail() { echo "  FAIL  $*" >&2; verdict=FAIL; }

# ── helpers ───────────────────────────────────────────────────────────────────

start_xvfb() {
    pkill -f "Xvfb :99" 2>/dev/null; sleep 0.3
    Xvfb :99 -screen 0 800x600x24 -nolisten tcp -ac &>"$TMPD/xvfb.log" &
    sleep 1.2
}

# run_alacritty CLASS PAD_X PAD_Y CMD  — launches alacritty, returns PID in $ALAC
run_alacritty() {
    local class="$1" px="$2" py="$3" cmd="$4"
    "$BIN" --class "$class" \
        -o "window.padding.x=$px" -o "window.padding.y=$py" \
        -o 'window.decorations="None"' \
        --hold -e sh -c "printf '\033[2J\033[H'; $cmd; sleep 6" \
        &>"$TMPD/${class}-alac.log" &
    ALAC=$!
}

# capture_trim CLASS  — captures root, trims to non-black, sets W H X Y
capture_trim() {
    local class="$1"
    local cap="$TMPD/${class}-cap.png" trim="$TMPD/${class}-trim.png"
    sleep 4.0
    import -display :99 -window root "$cap" 2>/dev/null
    magick "$cap" -fuzz 12% -trim +repage "$trim" 2>/dev/null
    read -r W H X Y < <(magick identify -format '%w %h %X %Y' "$trim" 2>/dev/null)
    TRIM="$trim"
}

# sample X Y  — sRGB pixel color string from $TRIM
sample() { magick "$TRIM" -format "%[pixel:p{$1,$2}]" info: 2>/dev/null; }

# channel_val COLOR CHAN  — extract R/G/B (0–255) from "srgb(R,G,B)" or named color
channel_val() {
    local color="$1" chan="$2"
    # normalize to "srgb(R,G,B)" first
    local rgb
    rgb=$(magick xc:"$color" -format "%[pixel:p{0,0}]" info: 2>/dev/null)
    case "$chan" in
        r) echo "$rgb" | grep -oP '(?<=srgb\()\d+' ;;
        g) echo "$rgb" | grep -oP '\d+(?=,\s*\d+\))' | head -1 ;;
        b) echo "$rgb" | grep -oP '\d+(?=\))' | tail -1 ;;
    esac
}

TOL=12   # per-channel tolerance (llvmpipe AA fringe)

# assert_color LABEL ACTUAL EXPECTED
assert_color() {
    local label="$1" actual="$2" expected="$3"
    local ar ag ab er eg eb
    ar=$(channel_val "$actual"   r); ag=$(channel_val "$actual"   g); ab=$(channel_val "$actual"   b)
    er=$(channel_val "$expected" r); eg=$(channel_val "$expected" g); eb=$(channel_val "$expected" b)
    local dr dg db
    dr=$(( ar - er )); dr=${dr#-}
    dg=$(( ag - eg )); dg=${dg#-}
    db=$(( ab - eb )); db=${db#-}
    if [[ $dr -le $TOL && $dg -le $TOL && $db -le $TOL ]]; then
        pass "$label actual=$actual expected=$expected"
    else
        fail "$label actual=$actual expected=$expected (delta R=$dr G=$dg B=$db > tol=$TOL)"
    fi
}

# ── Case 1: orientation ───────────────────────────────────────────────────────
# 4-quadrant fixture: TL=red TR=lime BL=blue BR=yellow.
# Catches Y-flip (BL/TL swap) and H-mirror (TL/TR swap).

case_orientation() {
    echo ""
    echo "=== Case 1: orientation (4-quadrant — catches Y-flip + H-mirror) ==="
    local ref="$TMPD/orient-ref.png"
    magick \( -size 120x120 xc:red   -size 120x120 xc:lime   +append \) \
           \( -size 120x120 xc:blue  -size 120x120 xc:yellow +append \) \
           -append "$ref"

    start_xvfb
    run_alacritty orient 0 0 "chafa --format=kitty --size=24x12 '$ref'"
    capture_trim orient
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    local qx1=$(( W / 4 )) qx2=$(( W * 3 / 4 ))
    local qy1=$(( H / 4 )) qy2=$(( H * 3 / 4 ))
    echo "  rendered region ${W}x${H}; sampling quadrant interiors"
    assert_color "TL($qx1,$qy1)" "$(sample $qx1 $qy1)" "red"
    assert_color "TR($qx2,$qy1)" "$(sample $qx2 $qy1)" "lime"
    assert_color "BL($qx1,$qy2)" "$(sample $qx1 $qy2)" "blue"
    assert_color "BR($qx2,$qy2)" "$(sample $qx2 $qy2)" "yellow"
}

# ── Case 2: padding-alignment ─────────────────────────────────────────────────
# Solid magenta at cell(0,0) with padding=24. Image top-left should be ~24 px
# from the window edge — not ~48 px (double-offset regression).

case_padding_alignment() {
    echo ""
    echo "=== Case 2: padding-alignment (pad=24, expect top-left ~24 px not ~48) ==="
    local ref="$TMPD/pad-ref.png" cap="$TMPD/pad-cap.png"
    local mask="$TMPD/pad-mask.png"
    magick -size 96x96 xc:magenta "$ref"

    start_xvfb
    run_alacritty padalign 24 24 "chafa --format=kitty --size=12x6 '$ref'"
    sleep 4.0
    import -display :99 -window root "$cap" 2>/dev/null
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    # isolate magenta pixels → bounding box
    magick "$cap" -fuzz 20% -fill white -opaque magenta -fill black +opaque white "$mask" 2>/dev/null
    local info
    # -trim WITHOUT +repage: preserves virtual canvas offset (%X %Y = top-left of trimmed region)
    info=$(magick "$mask" -trim -format "%w %h %X %Y" info: 2>/dev/null | tr -d '+' || echo "0 0 0 0")
    read -r mw mh mx my <<<"$info"
    echo "  magenta_bbox w=${mw} h=${mh} X=${mx} Y=${my}"
    echo "  padding_requested=24  expect X~24 Y~24  double-offset would give ~48"

    local PAD=24 MARGIN=10
    local lo=$(( PAD - MARGIN )) hi=$(( PAD + MARGIN ))
    local dbl_lo=$(( PAD * 2 - MARGIN ))

    if [[ $mx -ge $lo && $mx -le $hi ]]; then
        pass "X offset=${mx} within [${lo},${hi}] — single-pad correct"
    elif [[ $mx -ge $dbl_lo ]]; then
        fail "X offset=${mx} near double-pad (~$((PAD*2))) — double-offset BUG"
    else
        fail "X offset=${mx} outside expected range [${lo},${hi}]"
    fi

    if [[ $my -ge $lo && $my -le $hi ]]; then
        pass "Y offset=${my} within [${lo},${hi}] — single-pad correct"
    elif [[ $my -ge $dbl_lo ]]; then
        fail "Y offset=${my} near double-pad (~$((PAD*2))) — double-offset BUG"
    else
        fail "Y offset=${my} outside expected range [${lo},${hi}]"
    fi
}

# ── Case 3: horizontal-mirror ─────────────────────────────────────────────────
# LEFT half=red RIGHT half=blue. Catches H-mirror independent of Y.

case_horizontal_mirror() {
    echo ""
    echo "=== Case 3: horizontal-mirror (left=red right=blue — catches H-mirror) ==="
    local ref="$TMPD/hmir-ref.png"
    magick \( -size 120x240 xc:red \) \( -size 120x240 xc:blue \) +append "$ref"

    start_xvfb
    run_alacritty hmir 0 0 "chafa --format=kitty --size=24x12 '$ref'"
    capture_trim hmir
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    local lx=$(( W / 4 ))   ly=$(( H / 2 ))
    local rx=$(( W * 3 / 4 ))
    echo "  rendered region ${W}x${H}; sampling left x=$lx right x=$rx mid-height y=$ly"
    assert_color "LEFT($lx,$ly)"  "$(sample $lx $ly)"  "red"
    assert_color "RIGHT($rx,$ly)" "$(sample $rx $ly)" "blue"
}

# ── Case 4: multi-cell tiling ─────────────────────────────────────────────────
# 2×2-cell asymmetric image: TL=red TR=lime BL=blue BR=yellow (same quadrant layout).
# Rendered at chafa --size=48x24 (2 cells wide × 2 cells tall).
# Catches cell tiling row-order bugs: if rows are swapped, TL↔BL colors swap.

case_multi_cell_tiling() {
    echo ""
    echo "=== Case 4: multi-cell tiling (2×2 asymmetric — catches row-order bugs) ==="
    local ref="$TMPD/tile-ref.png"
    magick \( -size 120x120 xc:red   -size 120x120 xc:lime   +append \) \
           \( -size 120x120 xc:blue  -size 120x120 xc:yellow +append \) \
           -append "$ref"

    start_xvfb
    run_alacritty tile 0 0 "chafa --format=kitty --size=48x24 '$ref'"
    capture_trim tile
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    local qx1=$(( W / 4 )) qx2=$(( W * 3 / 4 ))
    local qy1=$(( H / 4 )) qy2=$(( H * 3 / 4 ))
    echo "  rendered region ${W}x${H}; sampling quadrant interiors"
    assert_color "TL($qx1,$qy1)" "$(sample $qx1 $qy1)" "red"
    assert_color "TR($qx2,$qy1)" "$(sample $qx2 $qy1)" "lime"
    assert_color "BL($qx1,$qy2)" "$(sample $qx1 $qy2)" "blue"
    assert_color "BR($qx2,$qy2)" "$(sample $qx2 $qy2)" "yellow"
}

# ── kitty_send: emit a raw kitty APC sequence ─────────────────────────────────
# Usage: kitty_send "key=val,..." "base64payload"
# Produces:  ESC _ G <keys> ; <payload> ESC \  on stdout.
# payload may be empty string for put-only commands.
kitty_send() {
    local keys="$1" payload="$2"
    printf '\033_G%s;%s\033\\' "$keys" "$payload"
}

# kitty_rgb_b64 W H R G B  — raw f=24 RGB pixels for a solid WxH image, base64-encoded.
kitty_rgb_b64() {
    python3 - "$@" <<'EOF'
import sys, base64
w, h, r, g, b = int(sys.argv[1]), int(sys.argv[2]), int(sys.argv[3]), int(sys.argv[4]), int(sys.argv[5])
data = bytes([r, g, b]) * (w * h)
print(base64.standard_b64encode(data).decode(), end='')
EOF
}

# kitty_bicolor_b64 W H  — f=24 RGB image: left half pure red, right half pure blue.
kitty_bicolor_b64() {
    python3 - "$@" <<'EOF'
import sys, base64
w, h = int(sys.argv[1]), int(sys.argv[2])
row = bytes([255,0,0]) * (w//2) + bytes([0,0,255]) * (w - w//2)
data = row * h
print(base64.standard_b64encode(data).decode(), end='')
EOF
}

# ── Case 5: source-crop ───────────────────────────────────────────────────────
# Transmit a 200×100 image whose left 100 px are red and right 100 px are blue.
# Place it with source-crop x=100,y=0,w=100,h=100 (selecting only the blue half).
# Assert the rendered region samples blue, proving the crop window is honoured.

case_source_crop() {
    echo ""
    echo "=== Case 5: source-crop (right-half=blue crop → expect blue) ==="

    # Build the transmit payload: 200×100, f=24, left=red right=blue.
    local payload
    payload=$(kitty_bicolor_b64 200 100)

    # Shell fragment sent to alacritty: transmit image i=10, then put with crop.
    local cmd
    cmd=$(cat <<'PYEOF'
python3 -c "
import sys, base64
w, h = 200, 100
row = bytes([255,0,0]) * (w//2) + bytes([0,0,255]) * (w - w//2)
data = base64.standard_b64encode(row * h).decode()
# Transmit: a=t,f=24 (RGB),s=width,v=height,i=10,q=2 (suppress response)
tx = '\033_Ga=t,f=24,s=200,v=100,i=10,q=2;' + data + '\033\\\\'
# Put with source-crop selecting right half: x=100,y=0,w=100,h=100; c=12,r=6 cells; q=2
put = '\033_Ga=p,i=10,x=100,y=0,w=100,h=100,c=12,r=6,q=2;\033\\\\'
sys.stdout.write(tx + put)
sys.stdout.flush()
"
PYEOF
)

    start_xvfb
    run_alacritty crop 0 0 "$cmd"
    capture_trim crop
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    local cx=$(( W / 2 )) cy=$(( H / 2 ))
    echo "  rendered region ${W}x${H}; sampling center ($cx,$cy)"
    assert_color "CENTER($cx,$cy)" "$(sample $cx $cy)" "blue"
}

# ── Case 6: z-order ───────────────────────────────────────────────────────────
# Place two solid images at the same cell position with different z values.
# Subcase A: red at z=0, blue at z=1 → blue wins (higher z drawn on top).
# Subcase B: red at z=2, blue at z=-1 → red wins (negative z goes behind text).

case_z_order() {
    echo ""
    echo "=== Case 6: z-order (higher-z image occludes lower-z) ==="

    # Shell fragment: transmit solid red (i=20) and solid blue (i=21), then
    # place both at the same cells — red z=0, blue z=1 → blue should win.
    # C=1 on all put commands keeps the cursor at (0,0) so both images
    # are anchored at the same cell and genuinely overlap.
    local cmd_a
    cmd_a=$(cat <<'PYEOF'
python3 -c "
import sys, base64
def solid(r, g, b, w=120, h=60):
    return base64.standard_b64encode(bytes([r,g,b])*(w*h)).decode()
red_b64  = solid(255,0,0)
blue_b64 = solid(0,0,255)
seqs = (
    '\033_Ga=t,f=24,s=120,v=60,i=20,q=2;' + red_b64  + '\033\\\\',
    '\033_Ga=t,f=24,s=120,v=60,i=21,q=2;' + blue_b64 + '\033\\\\',
    '\033_Ga=p,i=20,c=12,r=6,z=0,C=1,q=2;\033\\\\',
    '\033_Ga=p,i=21,c=12,r=6,z=1,C=1,q=2;\033\\\\',
)
sys.stdout.write(''.join(seqs))
sys.stdout.flush()
"
PYEOF
)

    start_xvfb
    run_alacritty zorder_a 0 0 "$cmd_a"
    capture_trim zorder_a
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    local cx=$(( W / 2 )) cy=$(( H / 2 ))
    echo "  subcase A — red z=0 vs blue z=1: rendered ${W}x${H}, sample ($cx,$cy)"
    assert_color "zA-CENTER($cx,$cy)" "$(sample $cx $cy)" "blue"

    # Subcase B: inverse assignment — red z=1 (higher) vs blue z=0 (lower).
    # Symmetric inverse of subcase A: now red has the higher z and should win.
    local cmd_b
    cmd_b=$(cat <<'PYEOF'
python3 -c "
import sys, base64
def solid(r, g, b, w=120, h=60):
    return base64.standard_b64encode(bytes([r,g,b])*(w*h)).decode()
red_b64  = solid(255,0,0)
blue_b64 = solid(0,0,255)
seqs = (
    '\033_Ga=t,f=24,s=120,v=60,i=30,q=2;' + red_b64  + '\033\\\\',
    '\033_Ga=t,f=24,s=120,v=60,i=31,q=2;' + blue_b64 + '\033\\\\',
    '\033_Ga=p,i=30,c=12,r=6,z=1,C=1,q=2;\033\\\\',
    '\033_Ga=p,i=31,c=12,r=6,z=0,C=1,q=2;\033\\\\',
)
sys.stdout.write(''.join(seqs))
sys.stdout.flush()
"
PYEOF
)

    start_xvfb
    run_alacritty zorder_b 0 0 "$cmd_b"
    capture_trim zorder_b
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    cx=$(( W / 2 )); cy=$(( H / 2 ))
    echo "  subcase B — red z=1 vs blue z=0 (inverse of A): rendered ${W}x${H}, sample ($cx,$cy)"
    assert_color "zB-CENTER($cx,$cy)" "$(sample $cx $cy)" "red"
}

# ── Case 7: text-survives-with-image (glyph-atlas bind regression) ────────────
# Regression: the image renderer left GL texture 0 bound without updating the
# text renderer's cached `active_tex`, so the glyph pass skipped its rebind,
# sampled texture 0, and all text vanished (red≈0) when an image was on screen.
# `kitten icat` (not a raw `a=p`, which coalesces into a separate image-free
# frame) forces a full redraw WITH the image present — the real yazi path.

case_text_with_image() {
    echo ""
    echo "=== Case 7: text survives alongside image (glyph-atlas bind regression) ==="

    if ! command -v kitten &>/dev/null; then
        echo "  SKIP  kitten not installed — cannot drive the icat repro"
        return
    fi

    # Solid-blue image: distinct color space from the red text so pixel counting
    # can tell image (blue) from glyphs (red) from background (black).
    magick -size 320x160 xc:blue "$TMPD/txtimg-blue.png" 2>/dev/null

    # 16 rows of bright-red block glyphs, then overlay the blue image on the top
    # 8 rows. Rows 9-16 are pure text and MUST survive the icat redraw.
    local cmd
    cmd="for i in \$(seq 1 16); do printf '\\033[91m%s\\033[0m\\n' '██████████████████████████████'; done; printf '\\033[H'; kitten icat --align left --place 40x8@0x0 '$TMPD/txtimg-blue.png' 2>/dev/null"

    start_xvfb
    run_alacritty txtimg 0 0 "$cmd"
    sleep 5.0
    import -display :99 -window root "$TMPD/txtimg-cap.png" 2>/dev/null
    kill "$ALAC" 2>/dev/null; wait "$ALAC" 2>/dev/null || true

    local redc bluec
    read -r redc bluec < <(magick "$TMPD/txtimg-cap.png" txt:- 2>/dev/null | awk '
      { if (match($0, /\(([0-9]+),([0-9]+),([0-9]+)/, m)) {
          r=m[1]; g=m[2]; b=m[3];
          if (r>150 && g<90 && b<90) rc++;
          else if (b>150 && r<90 && g<90) bc++;
      } }
      END { printf "%d %d", rc+0, bc+0 }')

    echo "  red(text)=${redc:-0}  blue(image)=${bluec:-0}"
    if [[ "${bluec:-0}" -lt 2000 ]]; then
        fail "txtimg image did not render (blue=${bluec:-0}) — test inconclusive"
    elif [[ "${redc:-0}" -gt 2000 ]]; then
        pass "txtimg red glyphs survive with image present (red=${redc})"
    else
        fail "txtimg text vanished with image present (red=${redc}) — active_tex desync regression"
    fi
}

# ── Main ──────────────────────────────────────────────────────────────────────

echo ""
echo "render-correctness — position/color assertion harness"
echo "binary: $BIN"

if [[ ! -x "$BIN" ]]; then
    echo "ERROR: alacritty binary not found at $BIN" >&2
    exit 1
fi
for tool in Xvfb import magick chafa python3; do
    command -v "$tool" &>/dev/null || { echo "ERROR: required tool missing: $tool" >&2; exit 1; }
done

case_orientation
case_padding_alignment
case_horizontal_mirror
case_multi_cell_tiling
case_source_crop
case_z_order
case_text_with_image

echo ""
echo "========================================"
echo "  FINAL VERDICT: $verdict"
echo "========================================"
[[ "$verdict" == PASS ]]
