#!/usr/bin/env bash
# extra/graphics-tests/run.sh — Headless golden-image test harness for alacritty.

# Capture (FIXED): ImageMagick `import -window root` on the Xvfb display.
# Renderer selector: -o 'debug.renderer="Gles2"' (no ALACRITTY_RENDERER env var;
#   see alacritty/src/renderer/mod.rs RendererPreference + config/debug.rs).

# Compare: `compare -metric AE -fuzz 5%`, threshold 2000 px (absorbs AA fringe).
# Re-record goldens: RECORD_GOLDENS=1 ./extra/graphics-tests/run.sh
# Task-17+ reuse: add fixture + golden + run_test call in the main() section.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# Binary: respect CARGO_TARGET_DIR env (this machine uses a global cache dir).
if [[ -n "${CARGO_TARGET_DIR:-}" ]]; then
    ALACRITTY_BIN="$CARGO_TARGET_DIR/debug/alacritty"
else
    ALACRITTY_BIN="$REPO_ROOT/target/debug/alacritty"
fi

FIXTURES_DIR="$SCRIPT_DIR/fixtures"
GOLDENS_DIR="$SCRIPT_DIR/goldens"
TMP_DIR=""

DISPLAY_NUM=99
export DISPLAY=":$DISPLAY_NUM"
# Force X11 backend: on Wayland desktops alacritty defaults to the Wayland
# compositor and ignores $DISPLAY, leaving the Xvfb screen blank.
export WAYLAND_DISPLAY=""
DISPLAY_W=800
DISPLAY_H=600
DISPLAY_DEPTH=24

COMPARE_METRIC="AE"
COMPARE_FUZZ="5%"
AE_THRESHOLD=2000

RECORD_GOLDENS="${RECORD_GOLDENS:-0}"

XVFB_PID=""
ALACRITTY_PID=""
TESTS_PASSED=0
TESTS_FAILED=0
TESTS_SKIPPED=0

# ── Cleanup ───────────────────────────────────────────────────────────────────

cleanup() {
    local exit_code=$?
    if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
        kill "$ALACRITTY_PID" 2>/dev/null || true
        wait "$ALACRITTY_PID" 2>/dev/null || true
    fi
    if [[ -n "$XVFB_PID" ]] && kill -0 "$XVFB_PID" 2>/dev/null; then
        kill "$XVFB_PID" 2>/dev/null || true
        wait "$XVFB_PID" 2>/dev/null || true
    fi
    if [[ -n "$TMP_DIR" && -d "$TMP_DIR" ]]; then
        rm -rf "$TMP_DIR"
    fi
    exit $exit_code
}
trap cleanup EXIT INT TERM

info()  { echo "[INFO]  $*"; }
warn()  { echo "[WARN]  $*" >&2; }
fail()  { echo "[FAIL]  $*" >&2; }
pass()  { echo "[PASS]  $*"; }

# ── Preflight ─────────────────────────────────────────────────────────────────

preflight() {
    echo ""
    echo "  PREFLIGHT"

    local harness_ok=1
    echo "  Harness deps (required):"
    for tool in Xvfb import compare xdotool; do
        if command -v "$tool" > /dev/null 2>&1; then
            echo "    $tool: present"
        else
            echo "    $tool: MISSING"
            harness_ok=0
        fi
    done

    if [[ -x "$ALACRITTY_BIN" ]]; then
        echo "    alacritty: present ($ALACRITTY_BIN)"
    else
        echo "    alacritty: MISSING ($ALACRITTY_BIN)"
        harness_ok=0
    fi

    if [[ $harness_ok -eq 0 ]]; then
        fail "Harness dependencies missing — cannot run tests."
        echo "  Install: pacman -S xorg-server-xvfb imagemagick"
        echo "  Build:   cargo build -p alacritty"
        exit 1
    fi

    echo "  Client tools (missing = that test skipped):"
    declare -gA CLIENT_TOOL_PRESENT
    for tool in kitten timg chafa viu yazi notcurses-demo ncplayer mpv img2sixel lsix wezterm; do
        if command -v "$tool" > /dev/null 2>&1; then
            echo "    $tool: present"
            CLIENT_TOOL_PRESENT[$tool]=1
        else
            echo "    $tool: MISSING"
            CLIENT_TOOL_PRESENT[$tool]=0
        fi
    done
    echo ""
}

# ── Xvfb ──────────────────────────────────────────────────────────────────────

start_xvfb() {
    info "Starting Xvfb on $DISPLAY (${DISPLAY_W}x${DISPLAY_H}x${DISPLAY_DEPTH})"
    Xvfb "$DISPLAY" \
        -screen 0 "${DISPLAY_W}x${DISPLAY_H}x${DISPLAY_DEPTH}" \
        -nolisten tcp -ac \
        &> /tmp/alacritty-test-xvfb.log &
    XVFB_PID=$!

    local waited=0
    while ! xdpyinfo -display "$DISPLAY" > /dev/null 2>&1; do
        sleep 0.2
        waited=$((waited + 1))
        if [[ $waited -ge 25 ]]; then
            fail "Xvfb did not become ready within 5 seconds"
            exit 1
        fi
    done
    info "Xvfb ready (pid=$XVFB_PID)"
}

# ── Alacritty launcher ────────────────────────────────────────────────────────

start_alacritty() {
    local renderer_opt="$1"  # e.g. 'debug.renderer="Gles2"' or empty string
    local fixture_bin="$2"

    if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
        kill "$ALACRITTY_PID" 2>/dev/null || true
        wait "$ALACRITTY_PID" 2>/dev/null || true
        ALACRITTY_PID=""
    fi

    # Feed mechanism: alacritty -e sh -c "cat <fixture>; sleep N"
    # The shell cats raw escape bytes into the PTY; the terminal emulator renders
    # them immediately. The trailing sleep keeps the window open for capture.
    # No stdin blocking, no interactive prompts.
    local cmd_args=(
        "$ALACRITTY_BIN"
        --hold
        -o 'window.dimensions.columns=80'
        -o 'window.dimensions.lines=24'
        -o 'window.decorations="None"'
        -o 'window.startup_mode="Windowed"'
        -o 'debug.print_events=false'
    )

    if [[ -n "$renderer_opt" ]]; then
        cmd_args+=(-o "$renderer_opt")
    fi

    cmd_args+=(-e sh -c "cat '${fixture_bin}'; sleep 3")

    info "Launching alacritty (renderer='${renderer_opt:-default}')"
    "${cmd_args[@]}" > /tmp/alacritty-test-stdout.log 2> /tmp/alacritty-test-stderr.log &
    ALACRITTY_PID=$!

    sleep 2
    if ! kill -0 "$ALACRITTY_PID" 2>/dev/null; then
        fail "Alacritty exited unexpectedly:"
        cat /tmp/alacritty-test-stderr.log >&2
        exit 1
    fi
    info "Alacritty running (pid=$ALACRITTY_PID)"
}

# ── Capture ───────────────────────────────────────────────────────────────────

capture_screen() {
    local output="$1"
    # FIXED method: import -window root captures the full Xvfb root window.
    import -display "$DISPLAY" -window root "$output"
    info "Captured → $output"
}

# ── Compare ───────────────────────────────────────────────────────────────────

compare_to_golden() {
    local captured="$1"
    local golden="$2"
    local diff_out="$3"
    local label="$4"

    local ae_value
    ae_value=$(compare \
        -metric "$COMPARE_METRIC" \
        -fuzz "$COMPARE_FUZZ" \
        "$captured" "$golden" "$diff_out" 2>&1 || true)

    ae_value="${ae_value%% *}"   # strip " (normalized)" suffix; format is "N (M)"

    if ! [[ "$ae_value" =~ ^[0-9]+$ ]]; then
        fail "[$label] compare returned non-numeric AE: '$ae_value'"
        return 1
    fi

    if [[ "$ae_value" -le "$AE_THRESHOLD" ]]; then
        pass "[$label] AE=$ae_value (<= $AE_THRESHOLD) PASS"
        return 0
    else
        fail "[$label] AE=$ae_value (> $AE_THRESHOLD) FAIL"
        fail "  captured : $captured"
        fail "  golden   : $golden"
        fail "  diff     : $diff_out"
        return 1
    fi
}

# ── Test runner ───────────────────────────────────────────────────────────────

run_test() {
    local test_name="$1"
    local fixture_file="$2"

    echo ""
    echo "  TEST: $test_name"

    if [[ ! -f "$fixture_file" ]]; then
        warn "[$test_name] Fixture not found: $fixture_file — skipping"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local golden_path="$GOLDENS_DIR/${test_name}.png"

    # Run on both renderer paths. Under llvmpipe/software GL both produce
    # pixel-identical output, so a single shared golden suffices.
    # If per-renderer goldens are ever needed, name them ${test_name}-glsl3.png
    # and ${test_name}-gles2.png and adjust the lookup below.
    local renderer_opts=("" 'debug.renderer="Gles2"')
    local renderer_names=("glsl3" "gles2")

    for i in 0 1; do
        local ropt="${renderer_opts[$i]}"
        local rname="${renderer_names[$i]}"
        local captured="$TMP_DIR/${test_name}-${rname}-captured.png"
        local diff_png="$TMP_DIR/${test_name}-${rname}-diff.png"

        start_alacritty "$ropt" "$fixture_file"
        sleep 1
        capture_screen "$captured"

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        if [[ "$RECORD_GOLDENS" == "1" ]]; then
            cp "$captured" "$golden_path"
            info "[$test_name/$rname] Recorded golden → $golden_path"
        else
            if [[ ! -f "$golden_path" ]]; then
                fail "[$test_name/$rname] Golden missing: $golden_path"
                fail "  Run with RECORD_GOLDENS=1 to record it first."
                TESTS_FAILED=$((TESTS_FAILED + 1))
                continue
            fi
            if compare_to_golden "$captured" "$golden_path" "$diff_png" "$test_name/$rname"; then
                TESTS_PASSED=$((TESTS_PASSED + 1))
            else
                TESTS_FAILED=$((TESTS_FAILED + 1))
            fi
        fi
    done
}

# ── Client-inside test runner ─────────────────────────────────────────────────
# Runs a client command INSIDE alacritty (real PTY) rather than via pre-captured fixture.
# Needed for clients that probe the terminal with DA1/a=q before rendering.

run_client_test() {
    local test_name="$1"
    local client_cmd="$2"   # command to run inside alacritty, e.g. "viu --width=8 /path.png"

    echo ""
    echo "  TEST: $test_name (client-inside)"

    local golden_path="$GOLDENS_DIR/${test_name}.png"

    local renderer_opts=("" 'debug.renderer="Gles2"')
    local renderer_names=("glsl3" "gles2")

    for i in 0 1; do
        local ropt="${renderer_opts[$i]}"
        local rname="${renderer_names[$i]}"
        local captured="$TMP_DIR/${test_name}-${rname}-captured.png"
        local diff_png="$TMP_DIR/${test_name}-${rname}-diff.png"

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        local cmd_args=(
            "$ALACRITTY_BIN"
            --hold
            -o 'window.dimensions.columns=80'
            -o 'window.dimensions.lines=24'
            -o 'window.decorations="None"'
            -o 'window.startup_mode="Windowed"'
            -o 'debug.print_events=false'
        )
        if [[ -n "$ropt" ]]; then
            cmd_args+=(-o "$ropt")
        fi
        cmd_args+=(-e sh -c "printf '\\033[2J\\033[H'; ${client_cmd}; sleep 3")

        info "Launching alacritty+client (renderer='${ropt:-default}'): ${client_cmd}"
        "${cmd_args[@]}" > /tmp/alacritty-test-stdout.log 2> /tmp/alacritty-test-stderr.log &
        ALACRITTY_PID=$!

        sleep 3
        if ! kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            fail "[$test_name/$rname] Alacritty exited unexpectedly"
            cat /tmp/alacritty-test-stderr.log >&2
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        capture_screen "$captured"

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        if [[ "$RECORD_GOLDENS" == "1" ]]; then
            cp "$captured" "$golden_path"
            info "[$test_name/$rname] Recorded golden → $golden_path"
        else
            if [[ ! -f "$golden_path" ]]; then
                fail "[$test_name/$rname] Golden missing: $golden_path"
                fail "  Run with RECORD_GOLDENS=1 to record it first."
                TESTS_FAILED=$((TESTS_FAILED + 1))
                continue
            fi
            if compare_to_golden "$captured" "$golden_path" "$diff_png" "$test_name/$rname"; then
                TESTS_PASSED=$((TESTS_PASSED + 1))
            else
                TESTS_FAILED=$((TESTS_FAILED + 1))
            fi
        fi
    done
}

# ── Detect-support probe ───────────────────────────────────────────────────────
# Runs kitten icat --detect-support inside a real PTY; succeeds if exit code = 0.

probe_detect_support() {
    echo ""
    echo "  TEST: kitten-detect-support (a=q probe)"

    local marker_file="$TMP_DIR/detect-support-exitcode"

    local renderer_opts=("" 'debug.renderer="Gles2"')
    local renderer_names=("glsl3" "gles2")

    for i in 0 1; do
        local ropt="${renderer_opts[$i]}"
        local rname="${renderer_names[$i]}"

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        rm -f "${marker_file}-${rname}"

        local cmd_args=(
            "$ALACRITTY_BIN"
            --hold
            -o 'window.dimensions.columns=80'
            -o 'window.dimensions.lines=24'
            -o 'window.decorations="None"'
            -o 'window.startup_mode="Windowed"'
            -o 'debug.print_events=false'
        )
        if [[ -n "$ropt" ]]; then
            cmd_args+=(-o "$ropt")
        fi
        local detect_script="kitten icat --detect-support --detection-timeout=5 2>/tmp/kitten-detect-stderr.txt; echo \$? > '${marker_file}-${rname}'; sleep 2"
        cmd_args+=(-e sh -c "$detect_script")

        info "Running kitten --detect-support (renderer='${ropt:-default}')"
        "${cmd_args[@]}" > /tmp/alacritty-test-stdout.log 2> /tmp/alacritty-test-stderr.log &
        ALACRITTY_PID=$!

        local waited=0
        while [[ ! -f "${marker_file}-${rname}" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; do
            sleep 0.5
            waited=$((waited + 1))
            if [[ $waited -ge 16 ]]; then
                break
            fi
        done

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        if [[ ! -f "${marker_file}-${rname}" ]]; then
            fail "[kitten-detect-support/$rname] Marker file not written — kitten may have hung or alacritty exited"
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        local exit_code
        exit_code=$(cat "${marker_file}-${rname}")
        local stderr_out=""
        [[ -f /tmp/kitten-detect-stderr.txt ]] && stderr_out=$(cat /tmp/kitten-detect-stderr.txt)

        if [[ "$exit_code" == "0" ]]; then
            pass "[kitten-detect-support/$rname] exit=$exit_code graphics detected OK (mode: ${stderr_out:-unknown})"
            TESTS_PASSED=$((TESTS_PASSED + 1))
        else
            fail "[kitten-detect-support/$rname] exit=$exit_code — kitten says graphics NOT supported"
            TESTS_FAILED=$((TESTS_FAILED + 1))
        fi
    done
}

# ── Skip helper for missing client tools ──────────────────────────────────────

skip_missing_tool() {
    local test_name="$1"
    local tool_name="$2"
    echo ""
    echo "  TEST: $test_name"
    warn "[$test_name] Client tool '$tool_name' not installed — SKIP"
    warn "  Install with: pacman -S $tool_name  (or equivalent)"
    TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
}

# ── Yazi navigation test ─────────────────────────────────────────────────────
# rio#709 regression guard: navigates blue→green→readme.txt via `ya emit-to`
# (yazi IPC, no X11 focus needed). Solid-color fixtures make stale placements
# obvious. Step-3 on readme.txt asserts full placement teardown.

run_yazi_nav_test() {
    local yazi_fixture_dir="$FIXTURES_DIR/yazi-images"
    local yazi_config_dir="$FIXTURES_DIR/yazi-config"
    local yazi_client_id="31337"

    echo ""
    echo "  TEST: yazi-nav (rio#709 regression guard)"

    if [[ "${CLIENT_TOOL_PRESENT[yazi]:-0}" -eq 0 ]]; then
        warn "[yazi-nav] yazi not installed — SKIP"
        warn "  Install with: pacman -S yazi"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    if [[ ! -d "$yazi_fixture_dir" ]]; then
        warn "[yazi-nav] Fixture image dir not found: $yazi_fixture_dir — SKIP"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local renderer_opts=("" 'debug.renderer="Gles2"')
    local renderer_names=("glsl3" "gles2")

    for i in 0 1; do
        local ropt="${renderer_opts[$i]}"
        local rname="${renderer_names[$i]}"

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        local cmd_args=(
            "$ALACRITTY_BIN"
            --hold
            -o 'window.dimensions.columns=120'
            -o 'window.dimensions.lines=30'
            -o 'window.decorations="None"'
            -o 'window.startup_mode="Windowed"'
            -o 'debug.print_events=false'
        )
        if [[ -n "$ropt" ]]; then
            cmd_args+=(-o "$ropt")
        fi

        # alphabetical sort: blue.png, green.png, readme.txt, red.png
        # --client-id lets ya emit-to target this instance without X11 focus.
        local yazi_cmd="YAZI_CONFIG_HOME='${yazi_config_dir}' yazi --client-id '${yazi_client_id}' '${yazi_fixture_dir}'"
        cmd_args+=(-e sh -c "${yazi_cmd}")

        info "[yazi-nav/$rname] Launching alacritty+yazi (client-id=${yazi_client_id})"
        "${cmd_args[@]}" > /tmp/alacritty-test-yazi-stdout.log 2> /tmp/alacritty-test-yazi-stderr.log &
        ALACRITTY_PID=$!

        sleep 1
        if ! kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            fail "[yazi-nav/$rname] Alacritty exited unexpectedly at startup"
            cat /tmp/alacritty-test-yazi-stderr.log >&2
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        # step 1: 5 s settle for a=q handshake + first image decode + render
        # alphabetical first file is blue.png
        sleep 5
        if ! kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            fail "[yazi-nav/$rname] Alacritty died before step-1 capture"
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        local step1_captured="$TMP_DIR/yazi-nav-step1-${rname}-captured.png"
        local step1_golden="$GOLDENS_DIR/yazi-nav-step1.png"
        local step1_diff="$TMP_DIR/yazi-nav-step1-${rname}-diff.png"
        capture_screen "$step1_captured"
        info "[yazi-nav/$rname] Step-1 captured (blue.png hovered)"

        # step 2: arrow down → green.png; rio#709 guard: blue placement deleted
        ya emit-to "$yazi_client_id" arrow down 2>/dev/null || true
        sleep 2
        if ! kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            fail "[yazi-nav/$rname] Alacritty died after step-2 navigation"
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        local step2_captured="$TMP_DIR/yazi-nav-step2-${rname}-captured.png"
        local step2_golden="$GOLDENS_DIR/yazi-nav-step2.png"
        local step2_diff="$TMP_DIR/yazi-nav-step2-${rname}-diff.png"
        capture_screen "$step2_captured"
        info "[yazi-nav/$rname] Step-2 captured (green.png hovered, blue deleted)"

        # step 3: arrow down → readme.txt; teardown: green placement deleted
        ya emit-to "$yazi_client_id" arrow down 2>/dev/null || true
        sleep 2
        if ! kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            fail "[yazi-nav/$rname] Alacritty died after step-3 navigation"
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        local step3_captured="$TMP_DIR/yazi-nav-step3-${rname}-captured.png"
        local step3_golden="$GOLDENS_DIR/yazi-nav-step3.png"
        local step3_diff="$TMP_DIR/yazi-nav-step3-${rname}-diff.png"
        capture_screen "$step3_captured"
        info "[yazi-nav/$rname] Step-3 captured (readme.txt hovered, images deleted)"

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        if [[ "$RECORD_GOLDENS" == "1" ]]; then
            cp "$step1_captured" "$step1_golden"
            cp "$step2_captured" "$step2_golden"
            cp "$step3_captured" "$step3_golden"
            info "[yazi-nav/$rname] Recorded goldens: step1, step2, step3"
        else
            for step_n in 1 2 3; do
                local sc_var="step${step_n}_captured"
                local gld_var="step${step_n}_golden"
                local diff_var="step${step_n}_diff"
                local sc="${!sc_var}"
                local gld="${!gld_var}"
                local diff="${!diff_var}"
                if [[ ! -f "$gld" ]]; then
                    fail "[yazi-nav/$rname/step${step_n}] Golden missing: $gld"
                    fail "  Run with RECORD_GOLDENS=1 to record it first."
                    TESTS_FAILED=$((TESTS_FAILED + 1))
                    continue
                fi
                if compare_to_golden "$sc" "$gld" "$diff" "yazi-nav/$rname/step${step_n}"; then
                    TESTS_PASSED=$((TESTS_PASSED + 1))
                else
                    TESTS_FAILED=$((TESTS_FAILED + 1))
                fi
            done
        fi
    done
}

# ── Task-23: tmux kitten icat ────────────────────────────────────────────────
# kitten-tmux-icat.bin: the kitty-unicode-placeholder APC wrapped in tmux DCS
# passthrough escapes (ESC P tmux ; ESC <doubled-ESCs> ESC \) plus the same
# U+10EEEE placeholder cells — identical to what kitten icat --passthrough=tmux emits.
# Regression: passthrough stripping broken → blank screen → AE diverges.

run_tmux_icat_test() {
    echo ""
    echo "  TEST: kitten-tmux-icat (tmux passthrough + unicode-placeholder)"

    if [[ "${CLIENT_TOOL_PRESENT[kitten]:-0}" -eq 0 ]]; then
        warn "[kitten-tmux-icat] kitten not installed — SKIP"
        warn "  Install with: pacman -S kitty"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    run_test "kitten-tmux-icat" "$FIXTURES_DIR/kitten-tmux-icat.bin"
}

# ── Task-23: tmux yazi nav ────────────────────────────────────────────────────
# Three-step navigation: cyan (step1) → green (step2) → text-only (step3).
# Each fixture is a pre-recorded tmux-passthrough-wrapped APC + placeholder stream,
# matching what yazi emits inside tmux with the kitty unicode-placeholder path.
# step3 deletes all images; stale placement leaves residual pixels that AE catches.

run_tmux_yazi_nav_test() {
    echo ""
    echo "  TEST: tmux-yazi-nav (tmux passthrough unicode-placeholder nav sequence)"

    if [[ "${CLIENT_TOOL_PRESENT[yazi]:-0}" -eq 0 ]]; then
        warn "[tmux-yazi-nav] yazi not installed — SKIP"
        warn "  Install with: pacman -S yazi"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local golden_step1="$GOLDENS_DIR/tmux-yazi-nav-step1.png"
    local golden_step2="$GOLDENS_DIR/tmux-yazi-nav-step2.png"
    local golden_step3="$GOLDENS_DIR/tmux-yazi-nav-step3.png"

    local renderer_opts=("" 'debug.renderer="Gles2"')
    local renderer_names=("glsl3" "gles2")

    for i in 0 1; do
        local ropt="${renderer_opts[$i]}"
        local rname="${renderer_names[$i]}"

        for step_n in 1 2 3; do
            local fixture="$FIXTURES_DIR/tmux-yazi-nav-step${step_n}.bin"
            local golden_var="golden_step${step_n}"
            local golden="${!golden_var}"
            local captured="$TMP_DIR/tmux-yazi-nav-step${step_n}-${rname}-captured.png"
            local diff_png="$TMP_DIR/tmux-yazi-nav-step${step_n}-${rname}-diff.png"

            if [[ ! -f "$fixture" ]]; then
                warn "[tmux-yazi-nav/step${step_n}/$rname] Fixture not found: $fixture — SKIP"
                TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
                continue
            fi

            if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
                kill "$ALACRITTY_PID" 2>/dev/null || true
                wait "$ALACRITTY_PID" 2>/dev/null || true
                ALACRITTY_PID=""
            fi

            start_alacritty "$ropt" "$fixture"
            sleep 1
            capture_screen "$captured"

            if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
                kill "$ALACRITTY_PID" 2>/dev/null || true
                wait "$ALACRITTY_PID" 2>/dev/null || true
                ALACRITTY_PID=""
            fi

            if [[ "$RECORD_GOLDENS" == "1" ]]; then
                cp "$captured" "$golden"
                info "[tmux-yazi-nav/step${step_n}/$rname] Recorded golden → $golden"
            else
                if [[ ! -f "$golden" ]]; then
                    fail "[tmux-yazi-nav/step${step_n}/$rname] Golden missing: $golden"
                    fail "  Run with RECORD_GOLDENS=1 to record it first."
                    TESTS_FAILED=$((TESTS_FAILED + 1))
                    continue
                fi
                if compare_to_golden "$captured" "$golden" "$diff_png" "tmux-yazi-nav/step${step_n}/$rname"; then
                    TESTS_PASSED=$((TESTS_PASSED + 1))
                else
                    TESTS_FAILED=$((TESTS_FAILED + 1))
                fi
            fi
        done
    done
}

# ── Task-28: sixel DA1 gate + img2sixel / mpv --vo=sixel ─────────────────────

run_img2sixel_test() {
    echo ""
    echo "  TEST: img2sixel-solid-red (sixel DCS render)"

    if [[ "${CLIENT_TOOL_PRESENT[img2sixel]:-0}" -eq 0 ]]; then
        warn "[img2sixel-solid-red] img2sixel not installed — SKIP"
        warn "  Install with: pacman -S libsixel"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local fixture_jpg="$FIXTURES_DIR/sixel-solid-red.jpg"
    if [[ ! -f "$fixture_jpg" ]]; then
        warn "[img2sixel-solid-red] Fixture JPEG not found: $fixture_jpg — SKIP"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    # Smoke-test: img2sixel must produce actual DCS output; skip if broken build.
    local probe_out
    probe_out=$(img2sixel -o /tmp/img2sixel-probe.six "$fixture_jpg" 2>/dev/null; wc -c < /tmp/img2sixel-probe.six)
    if [[ "${probe_out:-0}" -eq 0 ]]; then
        warn "[img2sixel-solid-red] img2sixel produced 0 bytes — binary misconfigured, SKIP"
        warn "  (libpng/libjpeg support may be missing from this build)"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    run_client_test "img2sixel-solid-red" "img2sixel '${fixture_jpg}'"
}

run_chafa_sixel_test() {
    echo ""
    echo "  TEST: chafa-sixel-solid-red (chafa --format=sixels DCS render)"

    if [[ "${CLIENT_TOOL_PRESENT[chafa]:-0}" -eq 0 ]]; then
        warn "[chafa-sixel-solid-red] chafa not installed — SKIP"
        warn "  Install with: pacman -S chafa"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local fixture_png="$FIXTURES_DIR/sixel-solid-red.png"
    if [[ ! -f "$fixture_png" ]]; then
        warn "[chafa-sixel-solid-red] Fixture PNG not found: $fixture_png — SKIP"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    run_client_test "chafa-sixel-solid-red" "chafa --format=sixels --size=40x20 '${fixture_png}'"
}

run_mpv_sixel_test() {
    echo ""
    echo "  TEST: mpv-sixel-clip (mpv --vo=sixel render)"

    if [[ "${CLIENT_TOOL_PRESENT[mpv]:-0}" -eq 0 ]]; then
        warn "[mpv-sixel-clip] mpv not installed — SKIP"
        warn "  Install with: pacman -S mpv"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local fixture_clip="$FIXTURES_DIR/sixel-test-clip.mp4"
    if [[ ! -f "$fixture_clip" ]]; then
        warn "[mpv-sixel-clip] Fixture clip not found: $fixture_clip — SKIP"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    run_client_test "mpv-sixel-clip" "mpv --vo=sixel --no-audio --frames=1 '${fixture_clip}' 2>/dev/null"
}

# ── Task-25: notcurses ncplayer (liveness / no-hang check) ────────────────────
# NOT kitty-graphics conformance: notcurses gates graphics on terminal identity
# and falls back to cell blitters here. Kitty-probe conformance is proven by Rust
# test `graphics::notcurses_self_rgba_probe_answers_ok`; this only checks ncplayer
# runs without hanging and the cell fallback renders stably.
run_notcurses_test() {
    echo ""
    echo "  TEST: notcurses-ncplayer (liveness / cell-fallback — NOT kitty graphics)"

    if [[ "${CLIENT_TOOL_PRESENT[ncplayer]:-0}" -eq 0 ]]; then
        warn "[notcurses-ncplayer] ncplayer not installed — SKIP"
        warn "  Install with: pacman -S notcurses"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local source_png="$FIXTURES_DIR/ncplayer-source.png"
    if [[ ! -f "$source_png" ]]; then
        warn "[notcurses-ncplayer] Fixture PNG not found: $source_png — SKIP"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local golden_path="$GOLDENS_DIR/notcurses-ncplayer.png"

    local renderer_opts=("" 'debug.renderer="Gles2"')
    local renderer_names=("glsl3" "gles2")

    for i in 0 1; do
        local ropt="${renderer_opts[$i]}"
        local rname="${renderer_names[$i]}"
        local captured="$TMP_DIR/notcurses-ncplayer-${rname}-captured.png"
        local diff_png="$TMP_DIR/notcurses-ncplayer-${rname}-diff.png"
        local hang_marker="$TMP_DIR/notcurses-ncplayer-done-${rname}"

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        rm -f "$hang_marker"

        # ncplayer uses the alternate screen; capture must happen WHILE ncplayer is
        # alive.  -t 4 holds the rendered image for 4 seconds before exit/restore.
        # We wait 2 s for alacritty startup + render, then capture, then kill.
        # The hang_marker is written after ncplayer exits (altscreen already restored)
        # so it is only used to confirm ncplayer didn't hang beyond the 4-second window.
        local cmd_args=(
            "$ALACRITTY_BIN"
            --hold
            -o 'window.dimensions.columns=80'
            -o 'window.dimensions.lines=24'
            -o 'window.decorations="None"'
            -o 'window.startup_mode="Windowed"'
            -o 'debug.print_events=false'
        )
        if [[ -n "$ropt" ]]; then
            cmd_args+=(-o "$ropt")
        fi
        cmd_args+=(-e sh -c "/usr/sbin/ncplayer -q -t 4 -b pixel '${source_png}' 2>/dev/null; echo done > '${hang_marker}'; sleep 2")

        info "[notcurses-ncplayer/$rname] Launching alacritty+ncplayer (renderer='${ropt:-default}')"
        "${cmd_args[@]}" > /tmp/alacritty-test-ncplayer-stdout.log 2> /tmp/alacritty-test-ncplayer-stderr.log &
        ALACRITTY_PID=$!

        # 2 s: alacritty startup (~1 s) + ncplayer kitty APC render (~0.5 s) + margin
        sleep 2
        if ! kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            fail "[notcurses-ncplayer/$rname] Alacritty exited unexpectedly before capture"
            cat /tmp/alacritty-test-ncplayer-stderr.log >&2
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        capture_screen "$captured"

        # Hang guard: ncplayer -t 4 must write the marker within ~10 s total
        local waited=0
        while [[ ! -f "$hang_marker" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; do
            sleep 0.5
            waited=$((waited + 1))
            if [[ $waited -ge 20 ]]; then
                fail "[notcurses-ncplayer/$rname] TIMEOUT: ncplayer did not complete within 10 s — FAIL"
                break
            fi
        done

        if [[ -n "$ALACRITTY_PID" ]] && kill -0 "$ALACRITTY_PID" 2>/dev/null; then
            kill "$ALACRITTY_PID" 2>/dev/null || true
            wait "$ALACRITTY_PID" 2>/dev/null || true
            ALACRITTY_PID=""
        fi

        if [[ $waited -ge 20 ]]; then
            TESTS_FAILED=$((TESTS_FAILED + 1))
            continue
        fi

        info "[notcurses-ncplayer/$rname] ncplayer completed OK"

        if [[ "$RECORD_GOLDENS" == "1" ]]; then
            cp "$captured" "$golden_path"
            info "[notcurses-ncplayer/$rname] Recorded golden → $golden_path"
        else
            if [[ ! -f "$golden_path" ]]; then
                fail "[notcurses-ncplayer/$rname] Golden missing: $golden_path"
                fail "  Run with RECORD_GOLDENS=1 to record it first."
                TESTS_FAILED=$((TESTS_FAILED + 1))
                continue
            fi
            if compare_to_golden "$captured" "$golden_path" "$diff_png" "notcurses-ncplayer/$rname"; then
                TESTS_PASSED=$((TESTS_PASSED + 1))
            else
                TESTS_FAILED=$((TESTS_FAILED + 1))
            fi
        fi
    done
}

# ── Task-30: iTerm2 (OSC 1337) client gate ────────────────────────────────────
# chafa -f iterm emits genuine OSC 1337;File=inline=1;... unconditionally.
# wezterm imgcat / timg --protocol=iterm2 skip when the tool is absent.

run_chafa_iterm_test() {
    echo ""
    echo "  TEST: chafa-iterm-aspect (chafa -f iterm OSC 1337 render)"

    if [[ "${CLIENT_TOOL_PRESENT[chafa]:-0}" -eq 0 ]]; then
        warn "[chafa-iterm-aspect] chafa not installed — SKIP"
        warn "  Install with: pacman -S chafa"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    local fixture_png="$FIXTURES_DIR/aspect-test-2x1.png"
    if [[ ! -f "$fixture_png" ]]; then
        warn "[chafa-iterm-aspect] Fixture PNG not found: $fixture_png — SKIP"
        TESTS_SKIPPED=$((TESTS_SKIPPED + 1))
        return 0
    fi

    run_client_test "chafa-iterm-aspect" "chafa -f iterm --size=8x4 '${fixture_png}'"
}

run_wezterm_imgcat_test() {
    echo ""
    echo "  TEST: wezterm-imgcat-iterm2 (wezterm imgcat OSC 1337 render)"

    if [[ "${CLIENT_TOOL_PRESENT[wezterm]:-0}" -eq 0 ]]; then
        skip_missing_tool "wezterm-imgcat-iterm2" "wezterm"
        return 0
    fi

    local fixture_png="$FIXTURES_DIR/aspect-test-2x1.png"
    run_client_test "wezterm-imgcat-iterm2" "wezterm imgcat '${fixture_png}'"
}

run_timg_iterm2_test() {
    echo ""
    echo "  TEST: timg-iterm2-aspect (timg --protocol=iterm2 OSC 1337 render)"

    if [[ "${CLIENT_TOOL_PRESENT[timg]:-0}" -eq 0 ]]; then
        skip_missing_tool "timg-iterm2-aspect" "timg"
        return 0
    fi

    local fixture_png="$FIXTURES_DIR/aspect-test-2x1.png"
    run_client_test "timg-iterm2-aspect" "timg --protocol=iterm2 '${fixture_png}'"
}

# ── Main ──────────────────────────────────────────────────────────────────────

main() {
    echo ""
    echo "  alacritty headless golden-image harness"
    [[ "$RECORD_GOLDENS" == "1" ]] && echo "  MODE: RECORD GOLDENS" || echo "  MODE: COMPARE"
    echo ""

    TMP_DIR="$(mktemp -d /tmp/alacritty-graphics-test.XXXXXX)"

    preflight
    start_xvfb

    # TRIVIAL GOLDEN: solid-block-sgr
    # Fixture: ESC[2J (clear) + ESC[1;1H (home) + ESC[48;2;200;50;50m (red bg) +
    #   8 space chars × 4 rows + ESC[0m. Pure SGR — no graphics protocol needed.
    # Exercises the full pipeline: Xvfb → feed → import capture → compare → threshold.
    # Graphics-protocol image goldens (icat, sixel, iTerm2) are added in Task 17
    # and reuse this same run_test / harness infrastructure.
    run_test "solid-block-sgr" "$FIXTURES_DIR/solid-block-sgr.bin"

    # TEXT-COLOR-SPLIT: colored fg+bg text cells.
    # Validates that the bg-pass/glyph-pass split (Task 14) does not regress text rendering.
    # Fixture: 4 rows with distinct fg+bg colour combinations (SGR 3x/4x) and a
    # transparent-bg glyph-only row. If the split breaks either pass the golden diverges.
    run_test "text-color-split" "$FIXTURES_DIR/text-color-split.bin"

    # KITTY-SOLID-RED: first real kitty-protocol image render.
    # Fixture: ESC[2J + ESC[H + kitty APC a=T (transmit+place in one command) with a
    # 16x16 solid-red PNG (f=100, direct base64, i=1, 4 cols × 4 rows, q=2 silent).
    # This is the headline Task-15 proof: the snapshot feed wires Task-12 → Task-13 → Task-14
    # so the image actually appears on screen. The golden captures the rendered red block.
    run_test "kitty-solid-red" "$FIXTURES_DIR/kitty-solid-red.bin"

    # ── Task-17: client smoke gate ─────────────────────────────────────────────

    # Pre-captured kitten icat stream output for a 64x32 (2:1) red/blue PNG.
    # Aspect-ratio regression: golden will diverge if width/height math breaks.
    if [[ "${CLIENT_TOOL_PRESENT[kitten]:-0}" -eq 1 ]]; then
        run_test "kitten-icat-aspect" "$FIXTURES_DIR/kitten-icat-aspect.bin"
    else
        skip_missing_tool "kitten-icat-aspect" "kitten"
    fi

    # a=q handshake end-to-end via real kitten client inside a live PTY. No visual golden.
    if [[ "${CLIENT_TOOL_PRESENT[kitten]:-0}" -eq 1 ]]; then
        probe_detect_support
    else
        skip_missing_tool "kitten-detect-support" "kitten"
    fi

    # Pre-captured chafa --format=kitty output (outputs APC unconditionally, no detection).
    if [[ "${CLIENT_TOOL_PRESENT[chafa]:-0}" -eq 1 ]]; then
        run_test "chafa-kitty-aspect" "$FIXTURES_DIR/chafa-kitty-aspect.bin"
    else
        skip_missing_tool "chafa-kitty-aspect" "chafa"
    fi

    # viu sends DA1+a=q first, so must run inside a live alacritty PTY (not pre-captured).
    if [[ "${CLIENT_TOOL_PRESENT[viu]:-0}" -eq 1 ]]; then
        run_client_test "viu-kitty-aspect" "viu --width=8 '$FIXTURES_DIR/aspect-test-2x1.png'"
    else
        skip_missing_tool "viu-kitty-aspect" "viu"
    fi

    # timg -p kitty: not in pacman extra on this machine; test defined, skips cleanly.
    if [[ "${CLIENT_TOOL_PRESENT[timg]:-0}" -eq 1 ]]; then
        run_client_test "timg-kitty-aspect" "timg -p kitty '$FIXTURES_DIR/aspect-test-2x1.png'"
    else
        skip_missing_tool "timg-kitty-aspect" "timg"
    fi

    # ── Task-19: src-crop golden ──────────────────────────────────────────────
    # 4×4 PNG: top half red, bottom half blue. Placed with x=0,y=2,w=4,h=2 crop
    # (blue half only). Wrong crop → red visible; correct crop → solid blue.
    run_test "kitty-crop-blue" "$FIXTURES_DIR/kitty-crop-blue.bin"

    # ── Task-20: yazi navigation — rio#709 regression guard ──────────────────
    run_yazi_nav_test

    # ── Task-22: Unicode placeholder golden ───────────────────────────────────
    # 20×20 solid-cyan image placed via U=1 virtual placement, tiled 2 columns
    # wide by 1 row using U+10EEEE placeholder cells with row/col diacritics.
    run_test "kitty-unicode-placeholder" "$FIXTURES_DIR/kitty-unicode-placeholder.bin"

    # ── Task-23: tmux conformance (passthrough DCS stripping + placeholder scanner) ──
    run_tmux_icat_test
    run_tmux_yazi_nav_test

    # ── Task-25: notcurses V1-gate ────────────────────────────────────────────
    run_notcurses_test

    # ── Task-28: sixel DA1 gate ───────────────────────────────────────────────
    run_img2sixel_test
    run_chafa_sixel_test
    run_mpv_sixel_test
    if [[ "${CLIENT_TOOL_PRESENT[lsix]:-0}" -eq 1 ]]; then
        run_client_test "lsix-sixel" "lsix '${FIXTURES_DIR}/sixel-solid-red.png'"
    else
        skip_missing_tool "lsix-sixel" "lsix"
    fi

    # ── Task-30: iTerm2 (OSC 1337) client gate ────────────────────────────────
    run_chafa_iterm_test
    run_wezterm_imgcat_test
    run_timg_iterm2_test

    # ── Task-35: parent-relative placement golden ──────────────────────────────
    # Red block at col 0 (i=1,p=1), blue block at col 2 via P=1,Q=1,H=2,V=0.
    run_test "kitty-relative-placement" "$FIXTURES_DIR/kitty-relative-placement.bin"

    echo ""
    echo "  SUMMARY: passed=$TESTS_PASSED failed=$TESTS_FAILED skipped=$TESTS_SKIPPED"
    echo ""
    if [[ $TESTS_FAILED -gt 0 ]]; then
        fail "$TESTS_FAILED test(s) failed."
        exit 1
    fi
    pass "All tests passed."
}

main "$@"
