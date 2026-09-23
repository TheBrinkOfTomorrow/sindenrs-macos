# Sinden Lightgun on macOS 27

Goal: make the Sinden Lightgun work on macOS 27 (Apple Silicon) with PCSX2, RPCS3 and Dolphin.

## Key facts (from research, Sept 2026)

- No official Sinden macOS software exists. Vendor ships Windows, Linux x86, Raspberry Pi only.
- The gun is a USB composite device: a camera, a CDC serial port, and its own HID mouse
  (ATmega32U4, Arduino Leonardo bootloader).
- Host software's job: read camera frames -> detect the white coded border -> compute aim ->
  write the position to the gun over serial. The gun then reports the aim through its own
  absolute USB mouse. **No kernel driver / DriverKit extension should be needed.**
- Base to build on: https://github.com/schlarpc/sindenrs (MIT, Rust, clean-room).
  - Done: protocol (handshake, auth hashes, events), tracking at 60 fps, coded border,
    calibration, recoil/button TOML config, firmware backup/flash, hotplug, two guns.
  - Platform-neutral: `src/protocol/`, `src/gun.rs`, vision code.
  - Linux-only: `src/discovery/` (sysfs), `src/camera/v4l2/`, X11/Wayland overlay.
  - Camera is mounted upside down (frames rotated 180°).
  - Reference settings: threshold 48, exposure 78 (0.1 ms units), border 3% of short side.

## Plan

### Phase 0 — Hardware check (do first)
- Confirm the gun enumerates on macOS: `system_profiler SPUSBDataType`, `ls /dev/cu.*`.
- Confirm the camera is visible to AVFoundation (list capture devices; grab a frame).
- Confirm the gun's HID mouse moves the cursor once the gun is told a position.
- Exit criteria: camera frames + serial handshake working from a tiny test binary.

### Phase 1 — macOS backends for sindenrs (fork)
- `src/discovery.rs` (macOS, behind `cfg`): IOKit enumeration (USB VID/PID, serial path, camera unique ID,
  matching a gun's serial port to its camera).
- `src/camera/avfoundation`: capture via `objc2` bindings (or a small Swift shim).
- Camera controls: AVFoundation doesn't expose manual exposure on external UVC cams.
  Plan to send UVC control requests (exposure, brightness, contrast) via IOKit/libusb
  on the control interface. **Highest-risk item** — spike it early.
- Exit criteria: `sindenrs run` tracks one gun on macOS with a fullscreen border page.

### Phase 2 — Border overlay
- AppKit borderless, click-through, transparent window above fullscreen apps
  (`ignoresMouseEvents`, high window level, `.canJoinAllSpaces` + `.fullScreenAuxiliary`).
- Fallback: emulator post-processing shaders that draw the coded border.

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
1. UVC exposure control on macOS (Phase 1).
2. Overlay visibility over fullscreen Metal games (Phase 2).
3. Two-player patches: three codebases, upstream buy-in.
4. macOS 27 permission prompts (camera, Input Monitoring).

## Working notes
- Hardware is physical: ask the user to plug in / aim / pull the trigger when a step needs it.
- Keep a `docs/macos-notes.md` log of measurements and decisions, like upstream's `docs/notes.md`.
