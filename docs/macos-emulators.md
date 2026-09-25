# Sinden Lightgun with emulators on macOS

How to play light-gun games on macOS with sindenrs. Each section is an emulator that has been
set up and checked with the gun. More will follow (Flycast, Mednafen, RPCS3).

## How it fits together

`Sindenrs.app` (`sindenrs run`) draws the white border around the screen, tracks it through the
gun's camera, and tells the gun where it is aiming. The gun reports that as an **absolute USB
mouse**, and its controls as mouse buttons and keys. So an emulator only needs a light gun
that follows the mouse pointer. With the default button map the gun sends:

| Sinden control | Arrives as |
|----------------|------------|
| trigger | left mouse button |
| pump | right mouse button |
| front left | right mouse button |
| front right | middle mouse button |
| rear left | key `1` |
| rear right | key `5` |
| d-pad | arrow keys |

A click lands where the gun is aiming, not where the pointer was.

## Starting and stopping sindenrs

- **Start:** open `Sindenrs.app` (Applications, Spotlight or Launchpad). It has no Dock icon;
  the border and a crosshair in the menu bar show it is running. The first start asks for
  camera access.
- **⌃⌥B** shows or hides the border, from anywhere, including a full-screen game. With the
  border hidden the menu bar is visible, and so is the crosshair item.
- **⌃⌥⌘Q** quits, from anywhere. So does *Quit Sindenrs* in the crosshair menu, or
  `tools/macos/stop.sh`.
- Log: `~/Library/Logs/Sindenrs.log`. Install or update the app with `tools/macos/install.sh`,
  or copy `target/macos/Sindenrs.app` (from `tools/macos/bundle.sh`) into Applications.

The gun's aim calibration is stored on the gun, and the factory values measured right on this
setup (`calibrate --no-save`: 1.1% mean error), so nothing needs calibrating in sindenrs.

## ARMSX2 (PS2): GunCon 2

Checked with **Virtua Cop Elite Edition**, full screen, 4:3 picture centred on a 16:9 display:
shooting is accurate and the border stays on top of the full-screen game.

ARMSX2 is the Apple-silicon fork of PCSX2, so the same settings apply to PCSX2 2.x.

1. Start sindenrs first, so the gun's controls work while you bind them.
2. **Settings → Controllers**, then **USB Port 1**, and set the device to **GunCon 2**.
3. Bind (click a binding, then press the control on the gun while aiming at the ARMSX2 window,
   or press the same mouse button or key yourself):

   | GunCon 2 | Binding | Sinden control |
   |----------|---------|----------------|
   | Trigger | Pointer-0 Left Button | trigger |
   | Shoot Offscreen | Pointer-0 Right Button | pump (or front left) |
   | Calibration Shot | Keyboard 5 | rear right |
   | A | Pointer-0 Right Button | pump (or front left) |
   | B | Pointer-0 Middle Button | front right |
   | Start | Keyboard 1 | rear left |
   | D-Pad Up / Down / Left / Right | Keyboard Up / Down / Left / Right | d-pad |

   Leave **Relative Aiming**, **C** and **Select** unbound: the GunCon 2 follows the mouse
   pointer by itself.
4. Save it as an input profile (**New Profile**, e.g. `Sinden-P1`) and **Apply Profile** to use
   it, or pick it per game. It is stored in
   `~/Library/Application Support/ARMSX2/inputprofiles/Sinden-P1.ini`:

   ```ini
   [USB1]
   Type = guncon2
   guncon2_Trigger = Pointer-0/LeftButton
   guncon2_Up = Keyboard/Up
   guncon2_Left = Keyboard/Left
   guncon2_Right = Keyboard/Right
   guncon2_Down = Keyboard/Down
   guncon2_A = Pointer-0/RightButton
   guncon2_B = Pointer-0/MiddleButton
   guncon2_ShootOffscreen = Pointer-0/RightButton
   guncon2_Start = Keyboard/1
   guncon2_Recalibrate = Keyboard/5
   ```
5. Play full screen (View → Fullscreen, ⌃⌘F). Keep the 4:3 aspect ratio: the GunCon 2 maps
   the pointer into the game picture, so the full-screen border works for 4:3 games too.

While binding, keep the gun aimed at the ARMSX2 window: a press elsewhere clicks there and
takes the focus away from the binding prompt.
