# Engineering notes

Measurements and decisions from the hardware sessions, in the order they happened. The
README says what the driver does; this file says why it does it that way. Commands here
use the names they had at the time (`probe`, `aim-test`, `track`); today they are
`list`, `calibrate` and `debug track`.

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

**Seventh and eighth recordings (`corpus/riguma3`, `corpus/riguma4`).** The rolled hover at
the bottom centre that looked wrong was the projected field of view: a one-side solve knows
nothing about foreshortening across its side, so its far corners are a guess; the outline is
now drawn only when two or more edges pinned the solve. Deliberate shots on riguma4 measured
eight targets at 0.16% to 1.1% and the centre at 2.9%; the recording's raw aim (now saved
beside the tracked one) cleared the tracker, which sits 0.1% from the raw aim when still and
halves hover jitter. The centre shot saw only the bottom edge because the left border lay in
a one-to-six-pixel strip at the frame's edge. Four solver changes came out of that recording:
a lone inner edge (outer edge off the frame) is a side, with its correspondence on the inner
line one thickness in; thickness comes from luma profiles across the edge at a low percentile,
because the paired fits can converge along the span and the mask walk counts tabs; the
two-side least-squares solve's degenerate answer (one edge mapped to nothing) is detected by a
vanishing tab weight and replaced by the exact solve from two tabs per side; and partial solves
are judged locally rather than by convexity, since a steep view puts part of the screen beyond
the camera's horizon and the projected quad is legitimately a bow-tie. Riguma4 went from 83%
to 98% solved, its top-centre region from 95% to 2% unusable and bottom-left from 69% to 0%;
the earlier recordings sit at 98% too. Processing is 2 to 5 ms per frame.

**Button reports need command 50.** The gun sends nothing over serial until asked, and the
command that asks is the one the vendor labels "secondary serial output". With it off there
are no trigger or button events at all; with it on the gun sends `FE <state1> <state2> 96` on
every press and release. The driver enables it by default
(`[[gun]] buttons_over_serial = true`), since the trigger and offscreen reload both depend on
it. The mirrored position goes to a UART that nothing is listening to, so there is no cost.


## Recoil, measured

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

