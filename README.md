# sindenrs

A clean-room driver for the [Sinden Lightgun](https://sindenlightgun.com), written in Rust
from the reverse-engineering notes in `~/re-shell/artifacts/sinden-lightgun/`. It replaces the
vendor's abandoned Mono binary on Linux and is structured so Windows can follow.

Status (2026-09-21): **transport and measurement stages are done and validated on hardware**;
the border tracker is next. See [Roadmap](#roadmap).

## What works today

| Piece | State |
|---|---|
| Device discovery (sysfs, no `lsusb`/`udevadm` shell-outs) | done |
| Serial protocol: framing, mutual SHA-256 handshake, queries, position writes, events | done, validated against firmware 1.5 |
| V4L2 capture with newest-frame drain and kernel timestamps | done, 60 fps MJPEG / 30 fps YUYV measured |
| Camera control enumeration and exposure sweeps | done |
| Resetting a wedged gun (bootloader touch; hub port power-cycle as fallback) | done, validated |
| Homography (border quad to screen) | done, unit tested |
| Border acquisition (threshold, blobs, undistort, edge-line fit, corner intersection) + live `track` | done, first light on the OLED |
| Lens distortion model and corpus fit (`replay --fit-lens`) | done, k1 = -0.178 on this gun |
| Multi-gun runtime (`run`: one thread per gun, camera paired by hub, Ctrl-C) | done, two guns |
| Fullscreen overlay: draws the border, calibration UI (Wayland + X11) | done |
| Aim accuracy harness (`aim-test`) | done, not yet measured |
| TOML config (display profiles, per-gun buttons/recoil by unique id) | done |
| Firmware backup/flash, bootloader reset, joystick device switch | done, both guns on 2.1 |
| Sub-pixel measurement: tab widths from the luma profile (on), edge refit (off, A/B-tested worse) | done |
| Self-locating border: coded tabs, line-plus-point solve for two or three visible sides | done, synthetic tests; not yet shot on hardware |
| Calibration overlay (Wayland / X11 / Windows) | not started |
| Windows capture and discovery backends | stubs |

## Hardware access on Linux

The gun is a composite USB device: CDC-ACM serial (`/dev/ttyACM*`), HID mouse and keyboard,
and a separate UVC camera behind the gun's internal hub. Three things must be true:

1. Your user can open the gun's tty. It is `root:dialout` by default.
2. Your user can open the camera's `/dev/video*` node (usually `root:video`).
3. ModemManager must not probe the tty.
4. (Optional) your user is in `dialout`, so `sindenrs gun power-cycle` can write the gun's
   hub port `disable` attribute in sysfs. The normal recovery path (`gun reset`) needs only
   the tty.

### NixOS

The flake exports a module that installs the udev rules for all of the above:

```nix
{
  inputs.sindenrs.url = "github:schlarpc/sindenrs";
  # ...
  imports = [ inputs.sindenrs.nixosModules.default ];
  services.sindenrs.enable = true;
  # Headless / non-seat users who need the devices:
  services.sindenrs.users = [ "arcade" ];
}
```

The rules use `TAG+="uaccess"`, so whoever is logged in on the seat gets an ACL on the gun,
the camera and the hub automatically, and `ENV{ID_MM_DEVICE_IGNORE}="1"` keeps ModemManager
away. The rules file is numbered `70-` so it runs before `73-seat-late.rules`; putting the
same rules in `services.udev.extraRules` (which becomes `99-local.rules`) would be too late
for `uaccess` to take effect.

### Other distributions

Install `70-sinden-lightgun.rules` from `flake.nix` into `/etc/udev/rules.d/` and run
`udevadm control --reload && udevadm trigger`, or add yourself to `dialout` and `video` and
re-login. For a quick one-off: `sudo setfacl -m u:$USER:rw /dev/ttyACM0`.

## Firmware

The gun is an ATmega32U4 with the Arduino Leonardo (Caterina) bootloader, reached by the same
1200-baud touch used for resets; it then enumerates as `2341:0036` and speaks AVR109. The
vendor's Windows app flashes it with the .NET ArduinoSketchUploader; `sindenrs` does the same
natively, on any platform with a serial port:

```
sindenrs gun firmware info firmware/LightgunFirmwareBlue.hex   # range, embedded USB id, sections
sindenrs gun firmware backup                                   # dump all 32 KiB to corpus/firmware/
sindenrs gun firmware flash firmware/LightgunFirmwareBlue.hex  # dry run: backup + compare only
sindenrs gun firmware flash firmware/LightgunFirmwareBlue.hex --yes
```

Only the application section (below 0x7000) is ever written; the bootloader section in the
image is compared to the gun's and left alone. Every flash first saves a full backup. The
images in `firmware/` are the vendor's, from the V2.08b Windows bundle; despite the release
notes calling them "1.9" the gun reports **v2.1** after flashing. The variants differ in a
single byte (the USB product id), so the tool refuses an image whose id does not match the
gun unless told otherwise. This gun was flashed from 1.5 to 2.1 on 2026-09-21; the 1.5 image
is kept at `corpus/firmware/backup-fw1.5-blue.hex`.

**Joystick mode (firmware 1.9+).** The newer firmware can expose a standard HID joystick
(32 buttons, two hats, six 16-bit axes; the Arduino Joystick library layout). It is off by
default: `sindenrs gun joystick-device enable` sets the persistent flag (command 184) and
resets the gun so it re-enumerates with the extra HID collection, after which the gun shows
up as a third input device (`js1`). Command 182, `--joystick` on `sweep` and `track`, then
switches position output from the mouse to the joystick axes. It is not XInput and has no force-feedback
output reports; rumble for games will come from a virtual gamepad the driver creates itself
(uinput on Linux), which also lets it drive the recoil solenoid from game rumble.

## Configuration

`sindenrs config init` writes a commented TOML file to `~/.config/sindenrs/config.toml`
(`%APPDATA%\sindenrs\config.toml` on Windows; `$SINDENRS_CONFIG` or `--config` override it;
`config show` prints the effective result, `config path` says where it is looking). It
talks to every attached gun and writes one `[[gun]]` entry per gun, keyed by the gun's
**unique id** (a per-unit number the firmware reports; `probe` prints it), so entries
survive port renumbering and two guns of the same colour. Every key has a default, so the
file is optional. Firmware backups go to `~/.local/share/sindenrs/` (`%LOCALAPPDATA%` on
Windows, `$SINDENRS_DATA` overrides).

```toml
[global]                 # log filter, auto-recover on a wedged handshake
[display]                # threshold, min_size, exposure, contrast, flip, gunsight_y, offset/ratio trims,
                         # orientation, fps — the tuning that varies per display
[profiles.crt]           # any subset of [display] keys; select with --profile crt
exposure = 120
[[gun]]                  # one per gun; matched by [gun.match] id (preferred), variant, usb_path or port
name = "player1"
[gun.match]
id = "2146665221"
joystick = false         # positions to the joystick HID device (needs `gun joystick-device enable`)
[gun.buttons.trigger]    # ten inputs, each with onscreen/offscreen actions and modifiers
onscreen = "mouse_left"
[gun.recoil]             # the vendor's whole recoil model: enable, strength, single/repeat,
enabled = false          # which events fire it, automatic-mode timing
```

Button actions are strings: `mouse_left|middle|right`, `key:<char>`, `key:f1`..`key:f12`,
`key:up|down|left|right|return|escape|tab|space`, `joy:<1-20>`, `turbo`, `turbo_reload`,
`pause`, `border_toggle`, `none`. Bad values are rejected when the file is parsed.

**Recoil strength, measured.** The kick is controlled by command **172**, not 167, and both
take a **0-250** scale. The Windows app sends `slider * 10` to both (its slider is 0-25); the
Linux driver sends a raw 0-100 to 167 and only `* 2.5` to 172, which is why 167 looks inert
there. `strength` in the config is a percentage and is sent as 0-250 to both, matching the
Windows app. Measured acoustically on firmware 2.1 by recording the solenoid:

| wire level (167 and 172) | kick |
|---|---|
| 0 | none |
| 50 | weak, ~10 ms |
| 100 | full, ~20 ms |
| 150, 200 | same as 100 (saturated) |

So the useful range is roughly the bottom 40% of the config's `strength`; above that it
saturates. 172 also **latches**: zero it and no other frame revives recoil until it is set
again. There is **no duty-cycle cutoff**: 40 consecutive full-strength pulses over 20 seconds
all fired, with only a mild shortening of the ring-down (about 20 ms early, 10 ms late).

`gun setup` sends the whole startup configuration to the gun (modes, 41 button-map frames,
the nine-frame recoil burst); `track --send` does the same before streaming. The vendor
driver pauses 100 ms between recoil frames. Measured: only three of the nine frames answer
(167 strength, 171 timing, 172 extended strength, each within 2 ms, 11 bytes in total), and
those bytes were what the sleeps kept out of the way of the next query. The driver drains
after each frame and pauses 5 ms (`global.recoil_gap_ms`, `--recoil-gap-ms`), so the whole
startup takes about 60 ms instead of 900. A query after the burst is used as the alignment
check: with no gap at all the version read back as "v10.1". Recoil on the
gun is trigger-driven by the firmware once configured; `gun recoil test` fires pulses on
demand through command 168, which is the hook for game-driven rumble later.

## Command-line tool

```
sindenrs run                            # every attached gun, one thread each, until Ctrl-C (the service entry point)
sindenrs border                         # draw the tracking border fullscreen (Escape quits)
sindenrs aim-test --grid 3 [--out f.csv]  # measure aim error, with an on-screen calibration overlay
sindenrs probe                          # list guns and cameras, and whether you can open them
sindenrs config init|show|path          # TOML config (see Configuration)
sindenrs gun setup                      # apply the config's modes, button map and recoil to the gun
sindenrs gun recoil test|auto|off       # on-demand recoil pulses (168), automatic recoil (169), or off
sindenrs track [--send] [--threshold N] [--contrast N] [--record DIR]   # camera -> border -> aim -> gun
sindenrs replay DIR... [--fit-lens] [--per-frame [--lines]]             # detection over recorded frames
sindenrs track --send --threshold 48 --contrast 50                     # what worked on the OLED
sindenrs camera info                    # formats, frame rates, every control with its range
sindenrs camera capture [--format mjpeg|yuyv] [--exposure N|auto] [--frames N] [--out DIR]
sindenrs camera sweep-exposure          # frame rate and frame age across exposure values
sindenrs gun info                       # handshake, firmware, stored camera name, calibration, identity
sindenrs gun monitor --seconds 10       # stream, hold a position, print every event byte
sindenrs gun sweep --pattern circle     # move the pointer through a pattern (validates the position path)
sindenrs gun sweep --joystick           # same, as joystick axes (firmware 1.9+)
sindenrs gun joystick-device enable|disable|status   # persistent joystick HID device flag (resets the gun)
sindenrs gun reset                      # reset a wedged gun via the bootloader (1200-baud touch)
sindenrs gun power-cycle                # switch the gun's hub port off/on (fallback; re-enumerates only)
sindenrs gun write-calibration --x -4.4 --y 4.2   # persist bore offsets to the gun's EEPROM
```

`--log debug` (or `RUST_LOG=sindenrs=trace`) shows every byte on the serial port.

## Measurements from the first hardware session

Blue gun, firmware **1.5**, `SindenCameraL` (`32e4:9210`), Ryzen 7 5800X, Linux 7.2.

**Camera.** MJPEG runs 640x480 at **60 fps**; YUYV only reaches **30 fps**. The redesign's
"prefer YUYV to skip the decode" trade-off therefore costs a whole frame period, so MJPEG is
the right default and the 0.4 ms grayscale decode is cheap by comparison. Kernel timestamps
are monotonic; frame age at dequeue is one frame period (16 ms at 60 fps, 32 ms at 30 fps),
which means uvcvideo stamps the start of the frame and the data lands one period later.
Exposure is advertised as 19..5000 in 100 µs units (nothing clamps a 60 Hz CRT value of
167), but the frame rate did not fall at any exposure up to 100 ms and the scene was fully
dark during this session, so whether the sensor honours long exposures is still unconfirmed.
Gain, gamma, sharpness, zoom and power-line-frequency controls exist and are unused by the
vendor driver.

**Gun.** Full handshake takes about 650 ms, almost all of it the gun computing SHA-256
(337 ms for leg 1, 298 ms for the leg 2 challenge). Every other query answers in about
1 ms, so the vendor driver's fixed 50-200 ms sleeps are pure waste. Commands 111/113/115
(unique id, factory colour, manufacture date) work on firmware 1.5 even though only the
Windows driver uses them; the joystick probe (184) does not answer on 1.5. Position reports
at 60 Hz produce two absolute-axis events each on the gun's own HID mouse, confirmed with
evdev.

**Firmware hazard and recovery.** If an auth command (109 or 110) reaches the gun without
its 32-byte payload in the same USB packet, the firmware sits waiting for those 32 bytes and
services nothing else; once its two 64-byte receive banks fill it stops accepting packets at
all (writes time out on the host). This driver therefore writes command and payload in one
write. Recovery is layered and automatic in every `gun` command:

1. Feed the pending read 32 bytes in one packet; the firmware answers and carries on. No
   re-enumeration, a few hundred milliseconds. Only works while the gun still accepts packets.
2. **Bootloader touch:** open the port at 1200 baud with DTR low (the Leonardo reset). The
   Arduino USB core handles it in the USB interrupt, so it works with the main loop stuck.
   The gun spends about four seconds as the Caterina bootloader (`2341:0036`) and comes back
   as itself; `sindenrs gun reset` does just this, and it is plain serial so it works on
   Windows too.
3. Switch the gun's internal hub port off and on (`sindenrs gun power-cycle`, through the
   kernel's sysfs `disable` attribute so the hub driver does not immediately re-power it).
   Measured: this re-enumerates the gun but does **not** reset the microcontroller, so it is
   only a fallback for when the serial port has vanished.

A plain USB bus reset does nothing useful either.

**First light (OLED, `tools/border.html` fullscreen, 3vh white border).** At the vendor's
exposure of 7.8 ms the border peaks around luma 100 on this display, so the stock-style 128
threshold misses it; threshold 48 with contrast 50 detects it in 100% of frames with all four
corners visible, and 86% while swinging the gun around with the border partly out of frame.
Processing is 11 ms per frame in a debug build. The border edges bow visibly in the camera
image on this flat panel, about 10 px over the top edge: that is the lens's barrel distortion.
A one-parameter division model fitted on 200 recorded frames (`replay --fit-lens`) gives
k1 = -0.178 with a clear optimum, and takes the line-fit residual from 1.36 px to 0.81 px;
it is the default in `[global] lens_k1`. The camera is mounted **upside down** (the cursor moved opposite to the gun on both axes
until the frame was rotated 180°; `track --flip both` is the default), which is what the
vendor driver's "camera is upside down" sign encodes. Recorded frames from the session are kept under `corpus/` (not in
git) for regression replay.

**Edge lines instead of corners.** The finder undistorts the boundary of every sizeable blob,
pulls straight lines out of it with sequential RANSAC, and intersects the outermost line on
each side. A line is pinned by any visible stretch of it, so the corners can all be off frame
as long as the four edges cross it; the ring then breaks into four separate strips, which is
why the boundaries are pooled across blobs before fitting. On the recorded full-view corpus
every frame with the whole border showing solves this way. The convex-hull quad is kept only
as a fallback and is always flagged unreliable: on 133 frames of a border page that was not
yet fullscreen it returned a confident quad with one side invented. The recorded close-range
corpus is the other story: 600 frames of which almost all show only two or three sides, and a
plain border carries no information about which stretch of an edge is in view, so those need
a border that encodes position (thickness is not a usable cue on a CRT).

**The coded border.** Each side carries a row of tabs on the inner edge of the border, one
border-thickness deep, at a constant pitch; a tab's width (one to four units) is a symbol,
and each side has its own sequence, chosen so that every window of three consecutive
symbols, read in either direction, occurs exactly once across all four sides
and every window of two occurs once within its side
(`src/vision/code.rs`, shared by the overlay that draws it and the detector that reads it).
Three adjacent tabs therefore name the side, the position and the reading direction, and the
solve does not depend on how the gun is rolled: an edge's normal only guesses which side it
is, the tabs settle it. (The second recording, `corpus/wow2`, was shot with the gun rolled
about 45 degrees, where a normal-based guess flips between two sides frame by frame; the
tabs shared one sequence then, so the wrong guess still decoded and produced a mirrored quad
that was refused, giving a 30 Hz flicker between solve and no solve with the gun held still.) The detector finds the tabs as boundary points in the band just inside a side's
inner edge that belong to no fitted line, clusters them along the outer line, takes the unit
from the centre-to-centre spacing (thresholding fattens bright regions, which biases widths
and gaps but not centres) and matches runs of symbols against the side's code. Each decoded
tab is a known point on that side's outer edge. The solve is then a direct linear transform
over line correspondences (two constraints each) and tab points (one each beyond their line):
two visible sides need four decoded tabs, three sides need two, four sides need none. Every
result carries the decoded tab count, which also tells a Sinden border from any other bright
rectangle; the calibration overlay shows the sides used, the tab count and the camera's field
of view projected onto the screen.

**First recording with the coded border (3694 frames, `aim-test --record`).** Tabs decode on
92% of frames, at every distance the test covered. The first replay showed aim jumps of up to
85% of the screen at regime changes, all from side classification: picking the outermost line
from the boundary centroid fails whenever the ring does not enclose the centroid, so with
three sides in view an inner edge became a confident, wrong fourth side. Edges are now
oriented by which side of the line is bright, outer and inner edges are paired, the tabs (which
sit only on the inner edge) say which is which, and an inner candidate must have a solid bright
strip along its span so the line the tab tips form cannot pass for it. Decoding got stricter
too: a width near a symbol boundary is uncertain and ends a run, a run needs the three symbols
the code makes unique, two visible sides need two tabs each, and a solve that does not land
its own tabs within 2% is refused. The worst remaining frame-to-frame spike is 2.9%, and the
tracker holds back a frame that leaps on a weaker solve until the next frame confirms it. What
is left is physical: the OLED caught mid-refresh draws one side a third as thick, and at the
far end of the range a six-pixel border can have both edges fitted as one line; both show up
as small spikes or a missed frame, not as a wrong cursor. Recordings made before the per-side
code (`2026-09-22-coded`, `wow`, `wow2`, `2026-09-22-sidecode`, `consoom`) still exercise
the four-line path but their tabs no longer decode.

**Third recording (`corpus/consoom`, per-side code).** Roll works. Two things were left: 465
two-side frames refused because the vertical side showed three tabs with one uncertain read,
and a still hover jittered by 0.25% of screen (median; 0.7% at the 90th percentile), which is
the half-resolution mask's pixel quantisation. Two-symbol windows are now unique within a side,
so once another edge has fixed the side and the roll, two tabs place themselves; the decoder
falls back to the longest sub-run that places itself when one tab misreads; and the tracker
blends moves under 1% per frame (`display.hover_smoothing`), which leaves real motion untouched.

**Fourth recording (`corpus/mollywop`): what the full-resolution luma buys.** Two extractions
were built switchable and A/B-tested (`replay --no-subpixel-tabs`, `--no-subpixel-lines`).
Re-measuring each cluster-found tab from a brightness profile through the tab bodies, with
both crossings interpolated, keeps the solve rate at 96% and cuts aim spikes over 1% of screen
from 20 to 13, and its widths are unbiased. Refitting the edge lines to sub-pixel luma crossings
solved slightly fewer frames and spiked more, so it stays off. Neither moved the still-hover
jitter at all (0.24% of screen median, 0.7% at the 90th percentile), which settles that the
jitter is the hand, not the fit; the smoother is the answer. A first version that replaced
cluster detection with the profile outright lost 150 frames to spurious short runs near
corners, so detection stays with the clusters and the profile only refines. Two solver fixes
came out of the same recording: a placed sub-run must leave room on its side for the rest of
its contiguous stretch (a misread had put four bottom tabs on the right side's code), and the
border thickness is checked against the median bright run walked inward from the outer edge,
which catches the tab-tip line passing for the inner edge and gives lone edges a thickness.
Unusable frames at the bottom centre fell from 15% to 8%. Two rules that looked right and
measured wrong: dropping tab-less sides from partial solves lost the bottom-left corner, where
a real side shows no tabs, and a tighter extrapolation limit did the same, because at close
range a screen's far corners really are several frame widths away.

**Fifth recording (`corpus/riguma`).** Targets 1 to 7 measured at 0.03% to 0.38% of screen
error, and then target 8 could not be captured. Two causes, neither in the solver. The grid
put the outer targets 10% from the edge, and the target ring, cross and number were drawn
over the tab band (the outer 6% of the screen), so the bottom and top rows had their tabs
corrupted by the overlay itself; the grid is now 15% in with a smaller ring. And at the bottom
centre only the bottom edge is in view. A single side now solves: its inner edge and tab-tip
line are drawn lines at known screen offsets (`display.aspect`, `display.border_thickness`),
and with the tabs fixing the position along the side they fix the direction across it. The
last third of that session also shows the screen going bright for half-frames at a time, with
one frame's whole background white behind the target: something other than the overlay was on
the panel, and those frames are unsolvable by design. That turned out to be the camera's USB
link dropping data: the camera's sequence numbers show one frame in seven surviving by the
end, and truncated frames decode with the missing part as flat grey. The tracker now treats a
frame under 60% of the recent median size as corrupt and reports corrupt frames once a second.

**Sixth recording (`corpus/riguma2`).** All nine targets measured, at 0.09% to 0.59% error,
and no dropped frames. Half the bottom-left frames were lost to a gun rolled 90 degrees with
one border in view edge-on: the border's own tab-tip line became a bogus opposite side and its
corner exclusion erased the real tabs (anything parallel within three thicknesses inside an
edge is now consumed as that side's own), and the one-side solve was underdetermined, since
the constant tab pitch already fixes the vanishing point along the edge and the inner and tip
lines then add one constraint each, not two. A lone side is now solved explicitly from its
tabs and the inner edge's image distance against its known screen offset, which is the
thickness cue after all, used only where nothing else exists and flagged as the weakest
support. Riguma2 solves 96% of frames, the bottom left 92%. The remaining jumpiness was
0.5% to 2.5% frame-to-frame noise on two- and three-edge solves, which cleared the 1% gate
of the hover smoother; a velocity-tracking filter replaces it and smooths weaker solves harder.

**Button reports need command 50.** The gun sends nothing over serial until asked, and the
command that asks is the one the vendor labels "secondary serial output". With it off there
are no trigger or button events at all; with it on the gun sends `FE <state1> <state2> 96` on
every press and release. The driver enables it by default
(`[[gun]] buttons_over_serial = true`), since the trigger and offscreen reload both depend on
it. The mirrored position goes to a UART that nothing is listening to, so there is no cost.

## The overlay

The gun tracks a bright border drawn around the screen edge, so the driver draws one itself:
`sindenrs border`. It is a fullscreen window rendered with `winit` (windowing and input) and
`softbuffer` (a plain CPU pixel buffer), which works on Wayland and X11 today and compiles
for Windows. There is no GPU dependency and no font files; the few numbers on screen are
drawn as seven-segment glyphs.

Both libraries load their system libraries with `dlopen`, so they never appear as `NEEDED`
entries and rpath patching cannot find them. The flake puts them on `LD_LIBRARY_PATH` in the
dev shell and wraps the installed binary the same way.

## Measuring aim accuracy

`sindenrs aim-test` is the calibration overlay: it draws the border, a grid of crosshairs,
and a ring around the one you should be shooting. Aim at the ring, pull the trigger, and it
records; the ring moves to the next target. Everything you need is on the screen you are
already looking at, which matters because the terminal is invisible behind a fullscreen
window:

- a **cyan dot** where the driver thinks you are pointing,
- a **square in the bottom-left** that is green only while the whole border is in the
  camera's view, amber when it is clipped at the frame edge, red when it is lost,
- each measured target turning **green with a line** to where it actually read, so the shape
  of the error is visible at a glance,
- the current target **flashing red** if a shot could not be measured.

At the end the terminal prints the error at every point, the mean and worst error, the
systematic bias (which `display.offset_x` / `offset_y` can cancel) and the spread left after
removing that bias, which is what a curvature correction would have to fix.

A capture needs the whole border in view, because a border clipped at the edge of the camera
frame gives a partial quad and a badly wrong solve. A pull that cannot be measured says so
and waits for you to pull again, so the target numbering can never drift out of step with
where you are actually aiming.

Either gesture works: pull the trigger, or hold steady on the target (`--capture trigger`,
`dwell` or `either`). The trigger is the better measurement because it is the gesture used
when playing, and the pull cannot disturb the reading since the recorded point is the median
of the 0.6 s window around the event rather than the instant of it.

If recoil is enabled the gun kicks on every pull, which may disturb your aim, so set
`[gun.recoil] enabled = false` for a clean measurement.

Every shot saves the camera frame behind it under `~/.cache/sindenrs/aim/<stamp>/`, named by
target, shot number and outcome (`pull-ok`, `pull-clipped`, `pull-lost`, `measured`,
`unmeasurable`, `unsteady`), so a suspicious reading can be looked at rather than guessed
about. Pass `--debug-dir ""` to turn it off.

## Architecture

```
src/protocol/   wire format, auth hashes, event parser      (pure, tested, platform-neutral)
src/gun.rs      one serial session: handshake, queries, position writes, event drain
src/discovery/  find guns and cameras (sysfs on Linux; Windows stub)
src/camera/     capture types; v4l2/ is the Linux backend (hand-written ABI, size-tested)
src/usb.rs      hub port power cycling via usbfs
src/vision/     homography (verified), luma helpers
src/main.rs     CLI
```

The pointer is reported by the gun's own HID mouse, so the core driver never touches the
display server. Wayland, X11 and Windows only matter for the calibration/border overlay,
which will be one module behind `winit` + `softbuffer`. Serial uses the `serialport` crate
(cross-platform); capture and discovery have `cfg(target_os)` backends with Windows stubs.
The Windows target type-checks today (`cargo check --target x86_64-pc-windows-msvc`).

## Roadmap

1. Border acquisition (downsample, threshold, connected components, convex quad) against a
   white-border page on the OLED.
2. Shoot the coded border on hardware: measure at what distance tabs decode, and whether
   a zero-tab four-line solve should be rejected as "not our border". Then the sub-pixel
   gradient tracker.
3. Predictive filter and end-to-end latency measurement.
4. Correction field fitted from the aim test; virtual gamepad with rumble-to-recoil; hotplug in `run`.
5. Windows backends (Media Foundation capture, SetupAPI discovery).
6. CRT: curvature-aware calibration field, exposure vs refresh validation.

## Development

Nix flake with a pinned toolchain; see `CLAUDE.md` for the command list.

```shell
direnv allow            # or: nix develop
cargo build && cargo nextest run && cargo clippy --all-targets
nix build               # Linux package
nix build .#windows     # cross-compile
```
