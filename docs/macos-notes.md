# macOS dev log

Measurements and decisions from porting sindenrs to macOS. The plan is in `macos-plan.md`.

## 2026-09-23 — Phase 0: hardware check

Machine: Apple Silicon, macOS 27, rustup toolchain 1.96.1 (no Nix).

### Build

- The library built and its tests passed as-is; the binary did not, because the V4L2 `camera`
  debug command was not gated on Linux. Fixed in `b700c5c`. Windows `cargo check` passes too.
- `flake.nix` still names `pkgs.darwin.apple_sdk.frameworks.Security`, which current nixpkgs
  removed. Irrelevant while building with rustup; fix if Nix on macOS is ever wanted.

### Enumeration

`system_profiler SPUSBDataType` is empty on macOS 27; use `SPUSBHostDataType`.

The gun is not one composite device but two behind its own hub:

```
SMSC hub 0424:2512                    location 0x08340000
├── SindenLightgun 16c0:0f01 (Blue)   location 0x08342000, serial "HIDDO", 12 Mb/s
└── SindenCameraJ  32e4:9210          location 0x08341000, 480 Mb/s
```

- Serial port: `/dev/cu.usbmodemHIDDO1` (use `cu.*`, not `tty.*`). The name is the USB serial
  number plus the interface, so it is stable for a given gun.
- AVFoundation camera `uniqueID` is `0x834100032e49210` = location ID `0x08341000` followed by
  VID `32e4` and PID `9210`. Pairing a gun with its camera is therefore: same parent hub, i.e.
  both location IDs extend the hub's `0x0834....` (each hub level adds one port nibble).

### Camera (AVFoundation)

- Offered formats top out at 640x480. `420v` (NV12, video range) at 640x480 runs at **60 fps**;
  `yuvs` (YUYV) at 640x480 is 30 fps. macOS decodes the camera's MJPEG for us, and the luma
  plane is exactly what the vision code consumes.
- Measured: 180 frames in 2.82 s (63.5 fps over the first frames), plane 0 is 640x480 with
  `bytesPerRow` 640 (no padding), luma mean 81.
- `isExposureModeSupported(.custom)` is false, as expected for an external UVC camera. With the
  camera's auto exposure the screen is blown out to near white, so manual exposure over UVC
  (Phase 1 risk #1) is required, not optional.
- Aimed at the screen, the frame shows it with upside-down text: the camera is mounted rotated
  180°, as on Linux. A fully black frame (all zeros) just means the gun faces something dark;
  black regions of normal frames are 0 too.

### Serial / HID (`examples/serial_probe.rs`)

- Open, authenticate (0.65 s), firmware 2.1, camera name `SindenCameraJ`, unique id
  `0942670342`: all work unchanged through the `serialport` crate.
- Streaming positions at 60 Hz moves the macOS cursor through the gun's own HID mouse. No
  driver, no extra permission. Buttons: see the section below.

### Camera permission (TCC)

Camera access is granted per app, and macOS refuses it *without a prompt* to an app whose
Info.plist has no `NSCameraUsageDescription`. A bare binary is judged as the app that launched
it: from Terminal.app it prompts normally, but from the Claude desktop app (no usage
description; it never shows up under Privacy & Security → Camera, and that pane has no + button
on macOS 27) it is silently denied.

Fix: wrap the binary in an app bundle that declares `NSCameraUsageDescription`, ad-hoc sign it,
and start it with `open` so LaunchServices makes it its own responsible process. It then gets its
own entry and prompt. `tools/macos/probe-app.sh` does this for the capture probe; `sindenrs` will
need the same treatment (a `.app` wrapper, or run from Terminal).

## 2026-09-23 — Phase 1 spike: UVC exposure control (`tools/macos/uvcctl.c`)

Plain UVC class requests (`DeviceRequest` on the default pipe via `IOUSBDeviceInterface`,
without `USBDeviceOpen`, so nothing is seized) work alongside Apple's UVC driver while
AVFoundation streams. **Risk #1 is retired.**

- Descriptors: VideoControl interface 0, camera terminal id 1 (bmControls `0a 22 00`: AE mode,
  exposure absolute, ...), processing unit id 3 (bmControls `7f 15`: brightness, contrast, hue,
  saturation, sharpness, gamma, white balance; **no gain**). VideoStreaming interface 1, alts 0-8.
- Ranges: AE mode default 8 (aperture priority), 1 = manual. Exposure absolute 19..5000
  (100 µs units), default 39. Brightness 0..255 default 110. Contrast 0..127 default 32 (the
  Linux default of 50 is in V4L2 units from the vendor's scale; check what it maps to).
- Setting manual exposure before streaming survives AVFoundation starting the session.
- Mid-stream, at 640x480 60 fps with the gun on a desk facing the screen, mean luma (1-in-16
  sampled) settled within one 0.5 s report: 78 → 6.0, 1000 → 11.8, 19 → 0.2, auto → 15.6.
  Frame rate stayed 58-60 fps. Exposure above one frame period (~167 units at 60 fps) is
  clipped, hence 1000 only doubling.
- At 78 the frame is black except the lit screen, a sharp-edged bright quad — the image the
  border detector expects.

For the Rust port: the same requests through IOKit from Rust (`io-kit-sys` or hand-written FFI),
matched to the camera by its location ID.

## 2026-09-23 — Buttons, trigger, d-pad (`serial_probe --buttons`, `tools/macos/input_window.swift`)

The probe sends the default button map (recoil off), holds the aim at screen centre, and prints
button-bit changes with wall-clock times; the input window covers the main screen, draws a
crosshair at the centre and logs every mouse, key and scroll event it gets plus
`NSEvent.pressedMouseButtons`. Neither needs Input Monitoring: a window sees its own events.

| Control     | Serial bit | macOS event (default map) |
|-------------|------------|---------------------------|
| trigger     | s1.0       | left mouse                |
| pump        | s1.1       | right mouse               |
| front left  | s1.2       | right mouse               |
| front right | s1.3       | middle mouse (button 2)   |
| rear left   | s1.4       | key `1`                   |
| rear right  | s1.5       | key `5`                   |
| d-pad up / down / left / right | s2.0 / s2.1 / s2.2 / s2.3 | arrow keys |

- All ten controls report press and release over serial (`FE s1 s2 96`); a held trigger is one
  down and one up, no repeats. The serial event arrives 10-25 ms *after* the HID event.
- HID clicks land at the gun's aim, not at the visible cursor. The gun only sends a mouse report
  when its position changes or a button does, so a constant aim leaves the cursor wherever the
  trackpad put it, while clicks still go to the aim point (screen centre, `(1680, 945)` on this
  3360x1890-point display). Irrelevant while tracking (the aim changes every frame), but an
  earlier test window that did not cover the centre saw `pressedMouseButtons` change with no
  click delivered, which looked like dropped clicks.
- The HID report descriptor (from `ioreg`): report 2 keyboard, report 1 absolute mouse (5
  buttons, X/Y 0..32767), report 3 joystick (32 buttons, throttle/rudder, two hats).

## 2026-09-23 — Phase 1: discovery (`src/discovery/macos.rs`)

IOKit registry walk over `IOUSBHostDevice` (VID, PID, `locationID`, product and serial strings);
a gun's port is the `IOCalloutDevice` found by a recursive search below its device. `locationID`
becomes a Linux-style path (`0x08342000` → `8-3.4.2`), so the existing `sibling_camera` pairing
is reused as is; a camera's `node` is its AVFoundation `uniqueID`, computed from location, VID
and PID (`0x834100032e49210`), which capture will open. No permissions needed.

`sindenrs list` finds gun, port, camera and pairing, and connects; `debug gun-info` and every
other command that selects a gun through discovery now work on macOS.
