# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Development Commands

This is a Rust project using Nix flakes with a pinned toolchain. First load the environment:

- `direnv allow` — load the development environment (or `nix develop`)

### Build & run

- `cargo run` — build (debug) and run
- `cargo build --release` — optimized build
- `nix run` — build and run via Nix
- `./result/bin/sindenrs` — the nix-built binary

### Test, lint, format

- `cargo nextest run` — fast parallel test runner
- `cargo llvm-cov nextest` — tests with coverage
- `cargo clippy --all-targets` — lint
- `cargo fmt` — format

### macOS

- `tools/macos/bundle.sh [bin]` — package a build as `target/macos/Sindenrs.app`, signed with
  `$SINDENRS_SIGN_IDENTITY` (default "sindenrs dev", a self-signed Code Signing certificate;
  ad hoc without it). Camera access is granted to that app.
- `tools/macos/run-bundled.sh [--bin path] -- <args>` — run sindenrs from the app (needed for
  the camera from an agent or IDE session); pass absolute paths.
- `tools/macos/login-item.sh install|uninstall|status` — `sindenrs run` at login via launchd.

### Nix

- `nix build` — build the package
- `nix build .#windows` — cross-compile for Windows (x86_64-pc-windows-msvc)
- `nix flake check` — run all checks (build, clippy, fmt, test, coverage)
- `nix flake update` — update flake inputs

### Windows cross-compilation

- `cargo xwin build --release --target x86_64-pc-windows-msvc` — cross-compile from the dev shell
- The pure Nix build gets the MSVC CRT/SDK from a pinned `xwin` fixed-output derivation in
  `flake.nix`; to bump the pinned versions, update them there, set `outputHash = pkgs.lib.fakeHash`,
  build, and copy the real hash from the mismatch error.

## Architecture

Built from the [rust-flake](https://github.com/schlarpc/rust-flake) template. A clean-room
Sinden Lightgun driver; the reverse-engineering notes it is built from live in
`~/re-shell/artifacts/sinden-lightgun/` (read `rust-redesign.md` first). README.md has the
hardware findings and the roadmap.

- **src/main.rs** — CLI (`list`, `run`, `calibrate`, `border`, `config`, `gun ...`, and `debug ...` for
  `track`, `replay`, `camera`, raw frames)
- **src/protocol/** — wire protocol, auth, events (pure, unit tested)
- **src/config.rs** — TOML config (global / display+profiles / `[gun]` baseline + `[guns."<id>"]`
  overrides); `~/.config/sindenrs/config.toml`; `to_toml` writes only non-default keys
- **src/gun.rs** — serial session with one gun; `apply_config` sends the vendor startup burst
- **src/runtime.rs** — the per-gun tracking loop shared by `debug track`, `run` and `calibrate`
  (Linux and macOS; `run`/`calibrate` still Linux-only for the overlay);
  `JumpGuard` holds a frame that leaps on a weaker solve
- **src/overlay/** — the on-screen border and calibration UI: `draw.rs` CPU renderer, `scene.rs`
  shared state, `x11.rs` (x11rb: override-redirect, shaped, click-through, re-raised) and
  `wayland.rs` (layer shell) behind the `Backend` trait, `artwork.rs` MAME artwork export.
  No main-thread requirement; `run` draws it from a worker.
- **src/camera/v4l2/** — hand-written V4L2 ABI + capture; `sys.rs` tests pin struct sizes
- **src/camera/avfoundation.rs** — macOS capture (objc2): `420v` luma by `uniqueID`, pooled
  frames over a channel. Camera access needs its own app bundle: `tools/macos/run-bundled.sh`
- **src/camera/source.rs** — the tracker's camera: per-platform open + settings + luma frames
  (V4L2 MJPEG decode with truncation check on Linux, AVFoundation + UVC controls on macOS)
- **src/camera/uvc.rs** — UVC controls as raw class requests (topology parse, V4L2 CID → UVC
  selector map; pure, tested); `uvc/iokit.rs` sends them on macOS via the IOKit USB user client
- **src/discovery.rs**, **src/usb.rs** — sysfs discovery, hub power-cycle; `src/discovery/macos.rs`
  reads the IOKit registry (`locationID` rendered as `bus-port.port`, camera node = AVFoundation
  `uniqueID`)
- **src/vision/** — `homography.rs` (quad map plus a DLT over line and point
  correspondences), luma helpers, `lens.rs` division-model undistortion, `lines.rs` RANSAC
  line extraction, `code.rs` the coded-border tab layout (shared by overlay and detector),
  `acquire.rs` border finder (edge lines and decoded tabs → corners; hull fallback is always
  flagged unreliable), `lensfit.rs` fits `k1` from recorded frames
  (`sindenrs debug replay --fit-lens corpus/<dir>`; `--per-frame --lines` dumps sides and tabs)
- **udev/** — the udev rules files; **docs/notes.md** — the hardware-session dev log; `corpus/` holds
  recorded frames (gitignored)
- **flake.nix** also exports `nixosModules.default` (udev rules, groups, settings, user service)

- **Cargo.toml** — package manifest; lints configured under `[lints.rust]` and `[lints.clippy]`
- **flake.nix** — Nix build (Crane), dev shell, and CI checks
- **rust-toolchain.toml** — single source of truth for the Rust version; Nix reads it via
  `rust-bin.fromRustupToolchainFile`, so builds stay reproducible. Bump `channel` to upgrade.

Hardware notes: the gun firmware wedges if an auth command (109/110) is sent without its
32-byte payload; recover with `sindenrs gun reset` (1200-baud bootloader touch). Hub port
power-cycling only re-enumerates it. Never send a frame with command byte 0xAA. Vendor reference binary (Mono) can be run from the RE tree for A/B checks.

## Commits

Commit work as you finish it, without being asked. One commit per work item — a self-contained
change with its own reason to exist (a bug fix, a new module, a refactor, a doc update). Do not
batch unrelated work into one commit, and do not leave finished work uncommitted at the end of a
turn.

- Split a mixed working tree into discrete commits rather than a single `git add -A`.
- Each commit should build and pass `cargo clippy --all-targets` and `cargo nextest run`. If a
  change cannot stand alone, fold it into the commit it belongs with.
- Subject line in the imperative mood, under ~72 chars; add a body when the *why* is not obvious
  from the diff. No attribution or tool-credit lines.
- Vendor firmware (`firmware/`) and recorded frames (`corpus/`) are gitignored — never commit them.
- Pushing is not automatic: commit freely, but only push when asked.

## Keeping in sync with the base template

Pull upstream template updates with [cruft](https://cruft.github.io/cruft/):

- `cruft update --checkout template`
