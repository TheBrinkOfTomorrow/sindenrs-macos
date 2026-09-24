# sindenrs

A driver for the [Sinden Lightgun](https://sindenlightgun.com) on Linux, written in Rust
from scratch. It is an open-source alternative to the vendor's Linux software, which is a
closed .NET binary that runs under Mono and needs one process and one XML file per gun.
It reads the gun's camera, finds the border it draws around the screen, and tells the gun
where it points. The gun then reports the aim through its own USB mouse, so
games see a normal absolute mouse. Windows is planned; the serial and vision code already
compiles for it, but the camera and window backends do not exist yet.

**This fork also runs on macOS** (Apple silicon): discovery, camera, border overlay,
calibration, and a `Sindenrs.app` with a menu bar item and shortcuts (⌥B border, ⌃⌥⌘Q quit).
See [docs/macos-emulators.md](docs/macos-emulators.md) to set it up and play, and
[docs/macos-plan.md](docs/macos-plan.md) for the state of the port.

## What works today

| Piece | State |
|---|---|
| Find guns and cameras, talk to the gun (handshake, queries, events) | done, firmware 1.5 and 2.1 |
| Reset a gun that stopped answering | done |
| Track the border and aim the gun, 60 frames per second, one thread per gun | done, tested with two guns |
| The coded border: the gun locates itself from two or three visible edges | done, measured on hardware |
| Calibration: shoot targets, save the result into the gun | done |
| Border on screen above a running game (X11 with or without a window manager, Wayland with layer shell) | written, not yet tested against a game |
| Border as MAME artwork, for setups where MAME draws it | done |
| Button map and recoil from a small TOML file, per gun | done |
| Firmware backup and flash, joystick device | done |
| Hotplug: `run` picks up guns as they come and go | done |
| Rumble from games (virtual gamepad), Windows backends, CRT-specific correction | not started |

## Install

### NixOS

```nix
{
  inputs.sindenrs.url = "github:schlarpc/sindenrs";
  # ...
  imports = [ inputs.sindenrs.nixosModules.default ];
  services.sindenrs = {
    enable = true;               # udev rules and the sindenrs command
    users = [ "alice" ];         # dialout + video: recover a wedged gun, work off-seat
    session.enable = true;       # run the driver in your graphical session
    hideFromDesktop = true;      # cabinet: keep the gun off the desktop pointer
  };
}
```

The module installs udev rules that let the logged-in user open the gun, its camera and
its internal hub, and that keep ModemManager away from the gun's serial port. With
`session.enable`, `sindenrs run` starts as a user service when the graphical session
starts. If you start X with `startx` and no display manager, no session target exists.
Run `config.services.sindenrs.session.command` in the background from your session
script instead. `services.sindenrs.settings` writes the driver configuration from Nix (see
Configuration).

### Other distributions

1. Build with `cargo build --release`.
2. Copy `udev/70-sinden-lightgun.rules` to `/etc/udev/rules.d/`. If you want MAME to see
   the gun as a lightgun, copy `udev/71-sinden-lightgun-input.rules` too. Then run
   `udevadm control --reload && udevadm trigger`.
3. Add yourself to the `dialout` and `video` groups, and log in again. A game that reads
   the gun's input devices directly (MAME) also needs you in `input`.

## Use

1. Plug in the gun. Run `sindenrs list`. It prints each gun, its id, and whether you can
   open it.
2. Run `sindenrs config init`. It writes a short configuration file with one entry per
   attached gun.
3. Run `sindenrs calibrate`. A fullscreen window draws the border and a grid of targets.
   Shoot each target. At the end the tool prints the aim error and asks whether to save
   the result into the gun. With two guns, run it once per gun: `sindenrs calibrate --gun player2`.
4. Run `sindenrs run`. It draws the border, tracks every attached gun, and stops on Ctrl-C.
   It keeps looking for guns: a gun plugged in later is picked up, and an unplugged gun is
   picked up again when it returns. This is what the service runs.

The commands:

```
sindenrs list                       # guns, cameras, ids, access
sindenrs run [--no-overlay]         # the driver; --no-overlay if MAME artwork draws the border
sindenrs calibrate [--gun NAME]     # measure and save the aim calibration
sindenrs border                     # draw the border and nothing else (Ctrl-C stops)
sindenrs border export --dir DIR    # write the border as MAME artwork
sindenrs config init|show|path      # the configuration file
sindenrs gun reset                  # a gun that stopped answering
sindenrs gun recoil test|auto|off   # try the recoil
sindenrs gun firmware backup|flash  # the gun's firmware
sindenrs debug ...                  # tools for development (tracking, replay, raw frames)
```

`--gun` picks a gun by its name or id from the configuration, or by its serial port. With
one gun attached you can leave it out. With two guns and no `--gun`, the tool stops and
asks, so it never talks to the wrong gun.

## Two guns and games

`sindenrs run` tracks all guns at once. Each gun reports its aim on its own USB mouse
(absolute position events) and its buttons as mouse buttons and keys. Nothing goes through
the display server, so the same driver works on X11, Wayland and a bare console.

A game that supports several lightguns must read the devices directly (evdev on Linux).
MAME does this with its `udev` lightgun provider, which is a patch from Batocera and is not
in upstream MAME. Upstream MAME's `x11` provider tells guns apart by name, and two Sinden
guns have the same name. The `sdl` provider merges every gun into one mouse. If you use one
gun, any provider works.

The udev rules can tag the gun's devices `ID_INPUT_GUN=1` (what MAME's udev provider looks
for) and hide them from the desktop, so the guns do not move your pointer. The NixOS module
options are `tagAsGun` and `hideFromDesktop`.

## The border on screen

The gun tracks a white border with small tabs on its inner edge. The tabs encode which side
and which part of the side the camera sees, so the gun can aim from a position where only
two edges are in view. The driver draws this border itself, and `run` keeps it on screen
while a game runs:

- On X11 the border is a window that exists only where the border is (a shaped window).
  It takes no input and stays above other windows, with or without a window manager.
- On Wayland it is a layer-shell surface on the overlay layer with a transparent interior.
  GNOME has no layer shell, so an in-game border is not possible there. Use X11 or the
  artwork below.
- If you prefer MAME to draw the border, run `sindenrs border export --dir DIR
  --resolution 1280x960`, add `DIR` to MAME's `artpath`, start gun games with
  `-override_artwork sinden-border`, and set `display.overlay = false`. The artwork only
  fits when the game's view fills the screen (a 4:3 game on a 4:3 display).

Whether `run` draws the border is `display.overlay` in the configuration, or
`--overlay` / `--no-overlay` on the command line.

## Configuration

The file is `~/.config/sindenrs/config.toml` (`--config FILE` or `$SINDENRS_CONFIG`
override it; `sindenrs config path` prints the path in use). Every key has a default, and
the file only needs the keys you change. `sindenrs config show --defaults` lists them all.

```toml
[global]                     # log, profile, auto_recover, recoil_gap_ms, lens_k1

[display]                    # the screen in front of you
threshold = 48               # how bright the border must be (0-255)
exposure = 78                # camera exposure in 0.1 ms
border_thickness = 3.0       # percent of the shorter screen side
aspect = 1.7778              # width over height
overlay = true               # draw the border from `run`

[profiles.crt]               # any subset of [display]; --profile crt or global.profile
aspect = 1.3333
exposure = 167

[gun]                        # every gun
recoil.enabled = true
offscreen_reload = false     # point off screen to reload
buttons.trigger.onscreen = "mouse_left"

[guns."2146665221"]          # one gun, by the id `sindenrs list` prints; any [gun] key
name = "player1"
recoil.strength = 60
calibration = [-1.97, 0.07]  # optional; normally the value saved in the gun is used
```

Button actions are `mouse_left`, `mouse_middle`, `mouse_right`, `key:<char>`,
`key:f1`..`key:f12`, `key:up|down|left|right|return|escape|tab|space`, `joy:<1-20>`,
`turbo`, `turbo_reload`, `pause`, `border_toggle`, `none`. Each button has an `onscreen`
and an `offscreen` action. The button names are `trigger`, `front_left`, `rear_left`,
`front_right`, `rear_right`, `up`, `down`, `left`, `right`, `pump` and `pedal`.

Recoil `strength` is a percent. Measured on firmware 2.1, the kick saturates around 40, so
values above that all feel the same.

## Calibration

`sindenrs calibrate` measures the offset between the camera's axis and the barrel, which is
different for every gun. It draws targets, you shoot them, and it computes the offset from
where each shot landed. The overlay shows:

- a cyan ring where the driver thinks you point,
- a square at the bottom that is green while the whole border is in view, amber when the
  border is cut off at the edge of the camera frame, and red when it is lost,
- each measured target in green, with a line to where the shot actually read,
- the current target flashing red if a shot could not be measured.

Recoil is off during calibration. At the end the tool asks whether to save the result into
the gun's memory, where the vendor software also keeps it. Pass `--save` or `--no-save` to
skip the question. A `calibration` key in the configuration overrides the saved value.

## Firmware

The gun is an ATmega32U4 with the Arduino Leonardo bootloader. `sindenrs gun firmware
backup` saves the whole flash. `sindenrs gun firmware flash IMAGE.hex` compares first and
only writes with `--yes`. The vendor's Windows bundle ships the images. Firmware 1.9 and
later can expose a joystick device; `sindenrs gun joystick-device enable` turns it on.

If the gun stops answering, run `sindenrs gun reset`. It resets the gun through its
bootloader and needs only the serial port.

## Development

Nix flake with a pinned toolchain; `CLAUDE.md` has the command list. `docs/notes.md`
holds the measurements and decisions from the hardware sessions.

```shell
direnv allow            # or: nix develop
cargo build && cargo nextest run && cargo clippy --all-targets
nix build               # Linux package
nix build .#windows     # cross-compile (serial and vision only)
```

Roadmap: test the overlay against MAME on the cabinet, a virtual gamepad so game rumble
fires the recoil, hotplug in `run`, the Windows backends, then a curvature correction
for CRTs.
