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

### Serial / HID (`examples/serial_probe.rs`)

- Open, authenticate (0.65 s), firmware 2.1, camera name `SindenCameraJ`, unique id
  `0942670342`: all work unchanged through the `serialport` crate.
- Streaming positions at 60 Hz moves the macOS cursor through the gun's own HID mouse. No
  driver, no extra permission. Button events over serial were not exercised yet.

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
