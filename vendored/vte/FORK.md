# Vendored vte fork

- **Upstream repository:** https://github.com/alacritty/vte
- **Pinned base:** tag `v0.15.0`, commit `3b3da71c34cc1256c7e20981cf03f8eb95e08ffc`
- **Date vendored:** 2026-06-13
- **Reason:** Local fork to add APC passthrough and DCS plumbing required for
  terminal graphics protocols (Kitty graphics / Sixel). The design of upstream
  PR alacritty/vte#115 will be applied on top of this base in a follow-up task.
  This initial vendoring is byte-identical to upstream `v0.15.0` (the `.git`
  directory is excluded) so future diffs against upstream stay clean.

## Provenance verification

- Source obtained via `git clone https://github.com/alacritty/vte` followed by
  `git checkout v0.15.0`.
- Cross-checked against the crates.io registry copy at
  `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/vte-0.15.0/`:
  `/usr/bin/diff -r <git-checkout>/src <registry>/src` reported the `src/`
  trees identical (registry packages strip repo-only files such as `.git`,
  `.builds`, `tests/`, and `doc/`, which are intentionally kept here).
- vte 0.15.0 has no path dependencies (the `vte_generate_state_changes`
  proc-macro crate was dropped before this release); all of its dependencies
  resolve from crates.io.

## Integration

- Added as a workspace member (`vendored/vte`) in the root `Cargo.toml`.
- Wired via `[patch.crates-io] vte = { path = "vendored/vte" }` so
  `alacritty_terminal`'s `vte = { version = "0.15.0", default-features = false,
  features = ["std", "ansi"] }` dependency resolves to this copy. The crate
  name (`vte`) and version (`0.15.0`) are kept unchanged for the patch to
  apply cleanly.

## Local changes

### SOS/PM/APC opaque-sequence dispatch (`src/lib.rs`)

Applies the streaming design of upstream PR alacritty/vte#115, adapted to the
0.15.0 codebase and a kind-parameterized API:

- `State::SosPmApcString` (which silently discarded payloads via `anywhere()`)
  is split into `State::OpaqueString` and `State::OpaqueEscape`. `ESC X`
  (SOS), `ESC ^` (PM), and `ESC _` (APC) call `Perform::opaque_hook(kind)`
  with the matching `OpaqueSequenceKind` and enter `OpaqueString`.
- New `Perform` methods, all defaulted so existing implementors compile
  unchanged: `opaque_hook(kind)`, `opaque_put(byte)`, `opaque_put_bytes(&[u8])`
  (default forwards per byte to `opaque_put`; the parser delivers payload
  exclusively through this method), and `opaque_unhook()`.
- Payload fast path: `OpaqueString` is handled in bulk by the `advance` loops
  (like the `Ground` memchr path). The input is scanned with a predicate
  (`b < 0x20 || b == 0x9C`) for the next special byte and the preceding run is
  delivered with a single `opaque_put_bytes` call. `memchr3` cannot express
  this byte set without admitting C0 controls into the payload, hence the
  predicate scan (which auto-vectorizes well).
- Terminators: `ESC \` (via `OpaqueEscape`, also emitting the usual
  `esc_dispatch` for the trailing `\`), C1 ST `0x9C` (matching the existing
  `DcsPassthrough` handling), and BEL `0x07` (PR #115 parity). CAN/SUB abort
  with `opaque_unhook` + `execute`, matching DCS semantics.
- An ESC inside the payload that is *not* followed by `\` does not abort the
  string: both the ESC and the following byte are delivered as payload
  (`OpaqueEscape` state; required for robust handling of binary-ish payloads,
  deviating from PR #115 which aborts on any ESC).
- PR #115's packable/non-packable action-table split is not applicable: the
  packed action table was removed upstream before 0.15.0; both the PR and
  this codebase use per-state `match` dispatch.
- 14 new unit tests cover APC/SOS/PM with `ESC \`/C1-ST/BEL terminators,
  embedded-ESC continuation (including an exhaustive follow-byte sweep),
  CAN/SUB aborts, chunked feeding split at every input offset, and bulk
  `opaque_put_bytes` delivery.

### DCS + APC forwarding through `Handler` (`src/ansi.rs`)

Forwards the low-level `Perform` plumbing through to the high-level `Handler`
trait so terminal implementations (e.g. alacritty_terminal's `Term`) can
receive DCS and APC payloads:

- New `Handler` methods, all defaulted to no-ops so existing implementors
  compile unchanged: `dcs_hook(params, intermediates, ignore, action)`,
  `dcs_put(byte)`, `dcs_unhook()`, `apc_start()`, `apc_put(&[u8])`, and
  `apc_end()`. `apc_put` takes byte slices (not single bytes) to preserve the
  parser's bulk `opaque_put_bytes` fast path end-to-end.
- `Performer`'s previously dead `hook`/`put`/`unhook` stubs (which only logged
  "[unhandled ...]" debug messages — verified that no DCS sequence was routed
  through them; synchronized updates use CSI `?2026` via the pre-parse sync
  buffer, not DCS) now forward to `Handler::dcs_hook`/`dcs_put`/`dcs_unhook`.
- `Performer` implements `opaque_hook`/`opaque_put_bytes`/`opaque_unhook`:
  APC sequences forward to `apc_start`/`apc_put`/`apc_end`; SOS/PM payloads
  remain consumed silently as before. The active sequence kind is tracked in
  `ProcessorState` (not `Performer`) because a single sequence may span
  multiple `Processor::advance` calls, each of which creates a fresh
  `Performer`.
- 4 new unit tests in `ansi::tests` (run with `cargo test -p vte --features
  ansi`; the `ansi` module is feature-gated and excluded from default-feature
  test runs): DCS params/intermediates/action/payload reach the handler, APC
  payload reaches the handler, APC kind survives a split across `advance`
  calls, and SOS/PM stay silent. The unchanged `MockHandler` proves the new
  methods default correctly.

## Performance gate (Task 4)

- **vtebench:** https://github.com/alacritty/vtebench, pinned commit
  `ead80032e57dee2e75f0b51f2ea67528647d9944` (clone at
  `/home/utsav/dev/softs/vtebench`). **This same revision must be reused for
  the final perf validation in Task 37.**
- **Machine/date:** AMD Ryzen 9 9950X3D, Linux 7.0.12-arch1-1 (Wayland),
  2026-06-13.
- **Builds compared** (both `cargo build --release`, separate explicit
  `CARGO_TARGET_DIR`s — the machine sets a global shared
  `CARGO_TARGET_DIR`, which would otherwise make the two builds overwrite
  each other):
  - *baseline:* git worktree of commit `bdca2c9f` (pre-fork HEAD; verified no
    `vendored/`, `Cargo.lock` resolves `vte 0.15.0` from crates.io).
  - *fork:* working tree with this vendored vte via `[patch.crates-io]`.
- **Method:** default `./benchmarks` suite run inside each alacritty build
  (`alacritty -o window.dimensions.columns=120 -o window.dimensions.lines=40
  -e vtebench --dat ... --silent`), 3 full-suite runs per build, interleaved
  baseline/fork to control thermal drift, samples pooled per benchmark,
  compared on median. vtebench emits integer-millisecond samples.

| benchmark | baseline median (ms) | fork median (ms) | delta | samples (b/f) |
|---|---|---|---|---|
| cursor_motion | 4.0 | 4.0 | +0.00% | 6724/6320 |
| dense_cells | 12.0 | 12.0 | +0.00% | 2439/2346 |
| light_cells | 3.0 | 3.0 | +0.00% | 7770/7777 |
| medium_cells | 4.0 | 4.0 | +0.00% | 6232/5942 |
| scrolling | 84.0 | 83.0 | -1.19% | 294/297 |
| scrolling_bottom_region | 85.0 | 84.0 | -1.18% | 348/354 |
| scrolling_bottom_small_region | 91.0 | 90.0 | -1.10% | 332/330 |
| scrolling_fullscreen | 5.0 | 5.0 | +0.00% | 1342/1344 |
| scrolling_top_region | 90.0 | 90.0 | +0.00% | 333/332 |
| scrolling_top_small_region | 90.0 | 89.0 | -1.11% | 333/336 |
| sync_medium_cells | 5.0 | 5.0 | +0.00% | 5622/5587 |
| unicode | 3.0 | 3.0 | +0.00% | 7771/7763 |

- **Verdict: PASS** — worst median regression +0.00% (gate: fail if any
  benchmark regresses >2% on median). Four scrolling benchmarks are ~1%
  faster on the fork (within noise).
- **Watch item for Task 37:** `dense_cells` shows a reproducible *mean*-level
  shift of about +4% (median and p90 unchanged at 12/13 ms). A dedicated
  re-run with ~4000 samples per build confirmed it is a distribution shift,
  not outlier noise: baseline samples land 30% at 11 ms / 55% at 12 ms /
  8% at 13 ms, while fork samples land 2% / 67% / 21%. This passes the
  median gate but suggests a small hot-path cost (or codegen/layout change)
  on the densest parse workload; re-check at Task 37 and profile
  `Parser::advance` if it grows.
