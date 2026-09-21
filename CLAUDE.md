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

- **src/main.rs** — CLI (`probe`, `camera ...`, `gun ...`)
- **src/protocol/** — wire protocol, auth, events (pure, unit tested)
- **src/config.rs** — TOML config (global / display+profiles / per-gun buttons+recoil); `~/.config/sindenrs/config.toml`
- **src/gun.rs** — serial session with one gun; `apply_config` sends the vendor startup burst
- **src/camera/v4l2/** — hand-written V4L2 ABI + capture; `sys.rs` tests pin struct sizes
- **src/discovery.rs**, **src/usb.rs** — sysfs discovery, hub power-cycle
- **src/vision/** — homography (verified), luma helpers, `acquire.rs` border finder
- **tools/border.html** — fullscreen white-border page for testing; `corpus/` holds recorded frames (gitignored)
- **flake.nix** also exports `nixosModules.default` (udev rules, groups, optional service)

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
