# Sinden Lightgun on macOS 27

Goal: make the Sinden Lightgun work on macOS 27 (Apple Silicon) with PCSX2, RPCS3 and Dolphin.

## Key facts (from research, Sept 2026)

- No official Sinden macOS software exists. Vendor ships Windows, Linux x86, Raspberry Pi only.
- The gun is two USB devices behind its own hub: the camera, and an ATmega32U4 (Arduino
  Leonardo bootloader) with a CDC serial port and its own HID mouse, keyboard and joystick.
- Host software's job: read camera frames -> detect the white coded border -> compute aim ->
  write the position to the gun over serial. The gun then reports the aim through its own
  absolute USB mouse. **No kernel driver / DriverKit extension should be needed.**
- Base to build on: https://github.com/schlarpc/sindenrs (MIT, Rust, clean-room).
  - Done: protocol (handshake, auth hashes, events), tracking at 60 fps, coded border,
    calibration, recoil/button TOML config, firmware backup/flash, hotplug, two guns.
  - Platform-neutral: `src/protocol/`, `src/gun.rs`, vision code.
  - Linux-only at the fork point: sysfs discovery, `src/camera/v4l2/`, X11/Wayland overlay.
  - Camera is mounted upside down (frames rotated 180°).
  - Reference settings: threshold 48, exposure 78 (0.1 ms units), border 3% of short side.

## Status (2026-09-24)

| Phase | State |
|-------|-------|
| 0 — Hardware check | **done** |
| 1 — macOS backends | **done** |
| 2 — Border overlay | in progress: overlay, calibrate, packaging done; emulator check left |
| 3 — Emulator setup | not started |
| 4 — Two players | optional, not started |

Measurements and decisions are in `docs/macos-notes.md`.

## Plan

### Phase 0 — Hardware check (do first) — done
- Confirm the gun enumerates on macOS: `system_profiler SPUSBDataType`, `ls /dev/cu.*`.
- Confirm the camera is visible to AVFoundation (list capture devices; grab a frame).
- Confirm the gun's HID mouse moves the cursor once the gun is told a position.
- Exit criteria: camera frames + serial handshake working from a tiny test binary.
- Result: met. The gun and camera enumerate (as two devices behind the gun's hub); the camera
  gives 640x480 at 60 fps through AVFoundation; the serial session authenticates unchanged; the
  gun's HID mouse moves the cursor; all ten controls report over serial and arrive as clicks
  and keys (`examples/serial_probe.rs`, `tools/macos/`). Camera access needs the binary to run
  as its own app bundle (`tools/macos/run-bundled.sh`).

### Phase 1 — macOS backends for sindenrs (fork) — done
- `src/discovery.rs` (macOS, behind `cfg`): IOKit enumeration (USB VID/PID, serial path, camera unique ID,
  matching a gun's serial port to its camera).
- `src/camera/avfoundation`: capture via `objc2` bindings (or a small Swift shim).
- Camera controls: AVFoundation doesn't expose manual exposure on external UVC cams.
  Plan to send UVC control requests (exposure, brightness, contrast) via IOKit/libusb
  on the control interface. **Highest-risk item** — spike it early.
- Exit criteria: `sindenrs run` tracks one gun on macOS with a fullscreen border page.
- Result: met. `src/discovery/macos.rs` (IOKit; pairing by USB location), `src/camera/avfoundation.rs`
  (420v luma at 60 fps), `src/camera/uvc.rs` + `uvc/iokit.rs` (UVC controls over the default
  control pipe, working alongside Apple's driver — the high-risk item), and a per-platform
  frame source (`src/camera/source.rs`) that lets the tracking loop run on macOS.
  `sindenrs run --no-overlay` against a full-screen border page tracked at 60 fps (98% of
  frames found) and moved the system cursor with the aim (logged independently); it shuts
  down cleanly on Ctrl-C. `debug track --send --preview` adds a full-screen test page with the
  camera feed, the detector's view, targets, click scoring and an aim marker.

### Phase 2 — Border overlay — in progress
- AppKit borderless, click-through, transparent window above fullscreen apps
  (`ignoresMouseEvents`, high window level, `.canJoinAllSpaces` + `.fullScreenAuxiliary`).
  **Done:** `src/overlay/macos.rs` is an overlay backend on the main thread; `run` draws its own
  border and tracks against it (100% of frames found); clicks pass through; it stays on top of
  apps in native full screen. Still to check: the emulators' own full-screen modes.
- `calibrate` on macOS: it draws its targets through the overlay, so it follows the overlay.
  **Done:** works as on Linux; measured mean error 1.14% of the screen over a 3x3 grid, and
  the bore it measures matches the gun's stored factory calibration, so nothing was saved.
  Run it with `--no-save`: nothing is written to the gun unless the user asks.
- Package `sindenrs` as an app with a stable signing identity, so the camera grant survives
  rebuilds. **Done:** `tools/macos/bundle.sh` with a self-signed certificate; the grant held
  across different builds. Starting `run` at login is available (`login-item.sh`) but opt-in
  and off by default.
- Fallback: emulator post-processing shaders that draw the coded border.

### Carried over (not blocking)
- Top-corner tracking losses from bright objects next to the user's screen: a detector fix
  (reject a line fitted just outside an edge that already has decoded tabs) is deferred; test
  frames are in `corpus/macos-corners-2026-09-24/` (local, gitignored).
- Frame age is ~31 ms on macOS against ~16 ms on Linux; measure end to end.
- `sindenrs list` gives Linux-only advice when a port cannot be opened.

### Phase 3 — Single-player emulator setup
- PCSX2: GunCon2 bound to pointer.
- Dolphin: Wii Remote IR bound to cursor.
- RPCS3: GunCon 3 bound to mouse.
- Document working configs; add calibration steps.

### Phase 4 (optional) — Two players
- macOS merges all mice into one cursor. Each emulator must read each gun's HID device
  separately (IOHIDManager or ManyMouse; reference: DirtBagXon/model3emu-code-sinden).
- Requires patches to PCSX2, RPCS3 and Dolphin input layers. Needs Input Monitoring permission.

## Risks
1. ~~UVC exposure control on macOS (Phase 1).~~ Retired: works mid-stream (notes, Phase 1 spike).
2. Overlay visibility over fullscreen Metal games (Phase 2). Native full-screen apps: fine.
   Emulator full-screen modes: not yet tested.
3. Two-player patches: three codebases, upstream buy-in.
4. macOS 27 permission prompts (camera, Input Monitoring). Camera: handled by running as an
   app bundle; ad-hoc signatures can re-prompt after rebuilds (see Phase 2 packaging).

## Working notes
- Hardware is physical: ask the user to plug in / aim / pull the trigger when a step needs it.
- Keep a `docs/macos-notes.md` log of measurements and decisions, like upstream's `docs/notes.md`.
