//! Driver configuration: a TOML file with global, per-display and per-gun sections.
//!
//! Display tuning (threshold, exposure, gunsight, trims) is keyed by display, not gun,
//! because that is where it varies. Per-gun settings are what the vendor driver re-sends to
//! the gun at every start: button map, recoil, modes. Everything has a default so the file is
//! optional; `sindenrs config init` writes a commented starting point.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::protocol::{button_map, cmd, Frame};

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    #[error("{0}")]
    Invalid(String),
}

pub type Result<T> = std::result::Result<T, ConfigError>;

/// Where the config lives by default.
pub fn default_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SINDENRS_CONFIG") {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("APPDATA").map(PathBuf::from);
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")));
    base.unwrap_or_else(|| PathBuf::from("."))
        .join("sindenrs")
        .join("config.toml")
}

/// Where backups and recordings go by default.
pub fn default_data_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SINDENRS_DATA") {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local").join("share"))
        });
    base.unwrap_or_else(|| PathBuf::from(".")).join("sindenrs")
}

/// Where throwaway diagnostic captures go.
pub fn default_cache_dir() -> PathBuf {
    if let Some(p) = std::env::var_os("SINDENRS_CACHE") {
        return PathBuf::from(p);
    }
    #[cfg(target_os = "windows")]
    let base = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    #[cfg(not(target_os = "windows"))]
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")));
    base.unwrap_or_else(|| PathBuf::from(".")).join("sindenrs")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub global: Global,
    pub display: Display,
    /// Named partial overrides of `display`, selected with `--profile`.
    pub profiles: std::collections::BTreeMap<String, DisplayOverrides>,
    pub gun: Vec<GunConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            global: Global::default(),
            display: Display::default(),
            profiles: Default::default(),
            gun: vec![GunConfig::default()],
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Global {
    /// Log filter, e.g. "info" or "sindenrs=debug".
    pub log: String,
    /// Reset a gun that does not answer the handshake (bootloader touch, then hub power).
    pub auto_recover: bool,
    /// Pause between the frames of the recoil configuration burst, in ms. The vendor uses
    /// 100; measured: three of the frames answer within 2 ms and are drained, so 5 suffices.
    pub recoil_gap_ms: u64,
    /// Camera lens radial distortion (division model, radius in half-frame-widths; negative
    /// is barrel). A property of the camera module, so it lives here rather than per display.
    /// The default was fitted on one gun with `sindenrs replay --fit-lens <recorded frames>`;
    /// refit if the edges still bow in `replay --per-frame` output.
    pub lens_k1: f64,
}

impl Default for Global {
    fn default() -> Self {
        Self {
            log: "info".into(),
            auto_recover: true,
            recoil_gap_ms: 100,
            lens_k1: -0.178,
        }
    }
}

/// How the camera frame must be transformed to match screen orientation.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Flip {
    None,
    Horizontal,
    Vertical,
    Both,
}

/// Exposure: manual in 100 µs units, or the camera's auto mode.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum Exposure {
    Manual(i32),
    Auto(AutoWord),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AutoWord {
    Auto,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Display {
    /// Luma threshold (0-255) for the border.
    pub threshold: u8,
    /// Minimum border blob width/height in half-resolution pixels.
    pub min_size: u32,
    pub exposure: Exposure,
    pub brightness: Option<i32>,
    pub contrast: Option<i32>,
    pub gain: Option<i32>,
    pub gamma: Option<i32>,
    pub sharpness: Option<i32>,
    /// Frame transform; the Sinden camera board is mounted upside down.
    pub flip: Flip,
    /// Gunsight offset in screen percent, subtracted from Y (vendor default 4.9).
    pub gunsight_y: f64,
    /// Linear aim trims applied after the solve: x' = x * ratio_x + offset_x.
    pub offset_x: f64,
    pub offset_y: f64,
    pub ratio_x: f64,
    pub ratio_y: f64,
    /// Sign applied to the bore offset (-1 modern camera boards, 1 legacy).
    pub orientation: f64,
    /// Requested camera frame rate; None leaves the camera default.
    pub fps: Option<u32>,
    /// Aim jump, in percent of screen between consecutive frames, above which a frame whose
    /// solve rests on fewer edges or tabs than the previous one is held for a frame.
    pub jump_limit: f64,
    /// While the aim moves less than 1% per frame, only this fraction of each move is
    /// applied, which averages away pixel jitter when hovering; 1 turns it off.
    pub hover_smoothing: f64,
    /// Screen width over height, and the tracked border's thickness as a percentage of the
    /// shorter screen dimension (what the overlay draws): where the border's inner edge and
    /// tab tips lie on screen, which lets a single visible side solve near that side.
    pub aspect: f64,
    pub border_thickness: f64,
}

impl Default for Display {
    fn default() -> Self {
        Self {
            threshold: 48,
            min_size: 60,
            exposure: Exposure::Manual(78),
            brightness: None,
            contrast: Some(50),
            gain: None,
            gamma: None,
            sharpness: None,
            flip: Flip::Both,
            gunsight_y: 4.9,
            offset_x: 0.0,
            offset_y: 0.0,
            ratio_x: 1.0,
            ratio_y: 1.0,
            orientation: -1.0,
            fps: None,
            jump_limit: 5.0,
            hover_smoothing: 0.3,
            aspect: 16.0 / 9.0,
            border_thickness: 3.0,
        }
    }
}

/// Every field of [`Display`], optional, for named profiles.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayOverrides {
    pub threshold: Option<u8>,
    pub min_size: Option<u32>,
    pub exposure: Option<Exposure>,
    pub brightness: Option<i32>,
    pub contrast: Option<i32>,
    pub gain: Option<i32>,
    pub gamma: Option<i32>,
    pub sharpness: Option<i32>,
    pub flip: Option<Flip>,
    pub gunsight_y: Option<f64>,
    pub offset_x: Option<f64>,
    pub offset_y: Option<f64>,
    pub ratio_x: Option<f64>,
    pub ratio_y: Option<f64>,
    pub orientation: Option<f64>,
    pub fps: Option<u32>,
    pub jump_limit: Option<f64>,
    pub hover_smoothing: Option<f64>,
    pub aspect: Option<f64>,
    pub border_thickness: Option<f64>,
}

impl Display {
    pub fn with_overrides(&self, o: &DisplayOverrides) -> Self {
        Self {
            threshold: o.threshold.unwrap_or(self.threshold),
            min_size: o.min_size.unwrap_or(self.min_size),
            exposure: o.exposure.unwrap_or(self.exposure),
            brightness: o.brightness.or(self.brightness),
            contrast: o.contrast.or(self.contrast),
            gain: o.gain.or(self.gain),
            gamma: o.gamma.or(self.gamma),
            sharpness: o.sharpness.or(self.sharpness),
            flip: o.flip.unwrap_or(self.flip),
            gunsight_y: o.gunsight_y.unwrap_or(self.gunsight_y),
            offset_x: o.offset_x.unwrap_or(self.offset_x),
            offset_y: o.offset_y.unwrap_or(self.offset_y),
            ratio_x: o.ratio_x.unwrap_or(self.ratio_x),
            ratio_y: o.ratio_y.unwrap_or(self.ratio_y),
            orientation: o.orientation.unwrap_or(self.orientation),
            fps: o.fps.or(self.fps),
            jump_limit: o.jump_limit.unwrap_or(self.jump_limit),
            hover_smoothing: o.hover_smoothing.unwrap_or(self.hover_smoothing),
            aspect: o.aspect.unwrap_or(self.aspect),
            border_thickness: o.border_thickness.unwrap_or(self.border_thickness),
        }
    }

    /// Apply trims and the gunsight offset to a raw solve.
    ///
    /// Deliberately **not** clamped to the screen: a reading of 104% is information, and
    /// clamping it to 100% silently understates the error during calibration. The wire
    /// encoding clamps when the position is actually sent to the gun.
    pub fn finish_aim(&self, x: f64, y: f64) -> (f64, f64) {
        (
            x * self.ratio_x + self.offset_x,
            y * self.ratio_y + self.offset_y - self.gunsight_y,
        )
    }
}

/// How a config entry is matched to an attached gun. Empty matches any gun.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GunMatch {
    /// The gun's unique id (command 111), as `sindenrs probe` prints it. The robust choice.
    pub id: Option<String>,
    /// Firmware variant: "blue", "red", "black", "player2".
    pub variant: Option<String>,
    /// USB bus path, e.g. "1-3.2" (stable per physical port).
    pub usb_path: Option<String>,
    /// Serial port path, e.g. "/dev/ttyACM0".
    pub port: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct GunConfig {
    pub name: String,
    #[serde(rename = "match")]
    pub matcher: GunMatch,
    /// Bore offset overrides in percent of frame; None reads the gun's EEPROM.
    pub calibration_x: Option<f64>,
    pub calibration_y: Option<f64>,
    /// Report positions as joystick axes (firmware 1.9+ with the joystick device enabled).
    pub joystick: bool,
    /// Pointing off-screen acts as reload.
    pub offscreen_reload: bool,
    /// Allow the gun's own button combo to enter calibration mode.
    pub calibration_mode: bool,
    /// Whether D-pad up toggles recoil on the gun.
    pub recoil_toggle: bool,
    /// Ask the gun to report button presses to the host (command 50).
    ///
    /// The vendor calls this "secondary serial output" and only enables it when a PS2/PSX
    /// adapter is configured, but measured on firmware 2.1 it is also what gates the gun's
    /// button reports over USB: with it off the gun sends nothing at all, with it on it sends
    /// `FE <state1> <state2> 96` on every press and release. The driver needs those for the
    /// trigger and for offscreen reload, so it defaults to on; the mirrored position simply
    /// goes to a UART nothing is listening to.
    pub buttons_over_serial: bool,
    pub buttons: Buttons,
    pub recoil: Recoil,
}

impl Default for GunConfig {
    fn default() -> Self {
        Self {
            name: "player1".into(),
            matcher: GunMatch::default(),
            calibration_x: None,
            calibration_y: None,
            joystick: false,
            offscreen_reload: false,
            calibration_mode: true,
            recoil_toggle: true,
            buttons_over_serial: true,
            buttons: Buttons::default(),
            recoil: Recoil::default(),
        }
    }
}

/// A button action, written as a string in the config:
/// `none`, `mouse_left`, `mouse_middle`, `mouse_right`, `pause`, `turbo`, `turbo_reload`,
/// `border_toggle`, `key:<char>` (letters, digits, space, `+ , - .`), `key:return|escape|tab|
/// up|down|left|right|f1..f12`, or `joy:<1-20>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Action(pub String);

impl Action {
    pub fn new(s: &str) -> Self {
        Self(s.to_owned())
    }

    /// The byte the gun expects (Arduino keycodes plus the vendor's private range).
    pub fn wire_value(&self) -> Result<u8> {
        let raw = self.0.trim();
        let s = raw.to_ascii_lowercase();
        let named = |n: &str| -> Option<u8> {
            Some(match n {
                "none" => 0,
                "mouse_left" => 255,
                "mouse_middle" => 254,
                "mouse_right" => 253,
                "pause" => 252,
                "turbo" => 251,
                "turbo_reload" => 250,
                "border_toggle" => 249,
                _ => return None,
            })
        };
        if let Some(v) = named(&s) {
            return Ok(v);
        }
        if let Some(k) = s.strip_prefix("key:") {
            let v = match k {
                "return" | "enter" => 176,
                "escape" | "esc" => 177,
                "tab" => 179,
                "space" => 32,
                "right" => 215,
                "left" => 216,
                "down" => 217,
                "up" => 218,
                _ if k.len() == 1 => {
                    // Single characters keep their case: the gun has separate codes for A-Z and a-z.
                    let c = raw.as_bytes()[raw.len() - 1];
                    if c.is_ascii_alphanumeric() || b"+,-. ".contains(&c) {
                        c
                    } else {
                        return Err(ConfigError::Invalid(format!("unsupported key {k:?}")));
                    }
                }
                _ if k.starts_with('f') => {
                    let n: u8 = k[1..]
                        .parse()
                        .map_err(|_| ConfigError::Invalid(format!("bad function key {k:?}")))?;
                    if !(1..=12).contains(&n) {
                        return Err(ConfigError::Invalid(format!(
                            "function key out of range: {k}"
                        )));
                    }
                    193 + n
                }
                _ => return Err(ConfigError::Invalid(format!("unknown key {k:?}"))),
            };
            return Ok(v);
        }
        if let Some(j) = s.strip_prefix("joy:") {
            let n: u8 = j
                .parse()
                .map_err(|_| ConfigError::Invalid(format!("bad joystick button {j:?}")))?;
            if !(1..=20).contains(&n) {
                return Err(ConfigError::Invalid(format!(
                    "joystick button out of range: {n}"
                )));
            }
            return Ok(n);
        }
        Err(ConfigError::Invalid(format!("unknown action {:?}", self.0)))
    }
}

impl Serialize for Action {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        let a = Action(s);
        a.wire_value().map_err(serde::de::Error::custom)?;
        Ok(a)
    }
}

/// One physical input's assignments.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Slot {
    pub onscreen: Action,
    pub offscreen: Action,
    /// Modifier index sent alongside (0 = none; the firmware's own small table).
    pub modifier: u8,
    pub offscreen_modifier: u8,
}

impl Slot {
    fn new(on: &str, off: &str) -> Self {
        Self {
            onscreen: Action::new(on),
            offscreen: Action::new(off),
            modifier: 0,
            offscreen_modifier: 0,
        }
    }
}

impl Default for Slot {
    fn default() -> Self {
        Self::new("none", "none")
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Buttons {
    pub trigger: Slot,
    pub front_left: Slot,
    pub rear_left: Slot,
    pub front_right: Slot,
    pub rear_right: Slot,
    pub up: Slot,
    pub down: Slot,
    pub left: Slot,
    pub right: Slot,
    pub pump: Slot,
    /// Pedal accessory (slot 140, on-screen only).
    pub pedal: Action,
}

impl Default for Buttons {
    /// The vendor's shipped player-1 defaults (MAME: 1 = start, 5 = coin).
    fn default() -> Self {
        Self {
            trigger: Slot::new("mouse_left", "mouse_left"),
            front_left: Slot::new("mouse_right", "mouse_right"),
            rear_left: Slot::new("key:1", "key:1"),
            front_right: Slot::new("mouse_middle", "mouse_middle"),
            rear_right: Slot::new("key:5", "key:5"),
            up: Slot::new("key:up", "key:up"),
            down: Slot::new("key:down", "key:down"),
            left: Slot::new("key:left", "key:left"),
            right: Slot::new("key:right", "key:right"),
            pump: Slot::new("mouse_right", "mouse_right"),
            pedal: Action::new("none"),
        }
    }
}

impl Buttons {
    /// The 40 command-60 frames the gun expects, in slot order.
    pub fn frames(&self) -> Result<Vec<Frame>> {
        let slots = [
            &self.trigger,
            &self.front_left,
            &self.rear_left,
            &self.front_right,
            &self.rear_right,
            &self.up,
            &self.down,
            &self.left,
            &self.right,
            &self.pump,
        ];
        let mut out = Vec::with_capacity(41);
        for (i, s) in slots.iter().enumerate() {
            let i = u8::try_from(i).unwrap_or(0);
            out.push(button_map(100 + i, s.onscreen.wire_value()?));
        }
        for (i, s) in slots.iter().enumerate() {
            let i = u8::try_from(i).unwrap_or(0);
            out.push(button_map(110 + i, s.offscreen.wire_value()?));
        }
        for (i, s) in slots.iter().enumerate() {
            let i = u8::try_from(i).unwrap_or(0);
            out.push(button_map(120 + i, s.modifier));
        }
        for (i, s) in slots.iter().enumerate() {
            let i = u8::try_from(i).unwrap_or(0);
            out.push(button_map(130 + i, s.offscreen_modifier));
        }
        out.push(button_map(140, self.pedal.wire_value()?));
        Ok(out)
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecoilMode {
    /// One pulse per trigger pull.
    Single,
    /// Keep pulsing while the trigger is held.
    Repeat,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Recoil {
    /// Master enable. Off means the gun never fires the solenoid on its own.
    pub enabled: bool,
    /// Solenoid strength as a percentage, 0-100, sent to the gun as 0-250 (see
    /// [`Recoil::wire_level`]). Command 172 is what actually controls the kick.
    pub strength: u8,
    pub mode: RecoilMode,
    pub on_trigger: bool,
    pub on_trigger_offscreen: bool,
    pub on_pump_on: bool,
    pub on_pump_off: bool,
    pub on_front_left: bool,
    pub on_rear_left: bool,
    pub on_front_right: bool,
    pub on_rear_right: bool,
    /// Automatic (repeat) mode parameters, 0-200 each; delays are firmware ticks.
    pub auto_strength: u8,
    pub auto_start_delay: u8,
    pub auto_pulse_delay: u8,
}

impl Default for Recoil {
    fn default() -> Self {
        Self {
            enabled: false,
            strength: 100,
            mode: RecoilMode::Single,
            on_trigger: true,
            on_trigger_offscreen: false,
            on_pump_on: false,
            on_pump_off: false,
            on_front_left: false,
            on_rear_left: false,
            on_front_right: false,
            on_rear_right: false,
            auto_strength: 40,
            auto_start_delay: 0,
            auto_pulse_delay: 13,
        }
    }
}

impl Recoil {
    /// The solenoid drive level on the wire, 0..=250.
    ///
    /// Both strength commands take this scale. The Windows app's slider is 0..=25 and it
    /// sends `value * 10` to **both** 167 and 172 (its default 10 gives 100). The Linux
    /// driver instead sends a raw 0..=100 to 167, always near the bottom of the range, and
    /// only `* 2.5` to 172 — which is why 167 appears to do nothing there. Measured on
    /// firmware 2.1 with a microphone: 0 gives no kick, 60 a weak one, 125 and 250 full ones,
    /// and 172 latches (zero it and nothing else revives recoil until it is set again).
    pub fn wire_level(&self) -> u8 {
        // `strength` is a percentage; 100% -> 250.
        u8::try_from(u16::from(self.strength.min(100)) * 5 / 2).unwrap_or(250)
    }

    /// The vendor's configuration burst, in its order. Send with a pause between frames.
    pub fn frames(&self) -> Vec<Frame> {
        let b = |v: bool| u8::from(v);
        let auto = (
            self.auto_strength.min(200),
            self.auto_start_delay.min(200),
            self.auto_pulse_delay.min(200),
        );
        let level = self.wire_level();
        let ext = level;
        vec![
            Frame::new(cmd::RECOIL_AUTO_PARAMS, [auto.0, auto.1, auto.0, auto.2]),
            Frame::flag(cmd::RECOIL_ENABLE, true),
            Frame::new(cmd::RECOIL_STRENGTH, [level, 0, 0, 0]),
            Frame::flag(
                cmd::RECOIL_TRIGGER_MODE,
                matches!(self.mode, RecoilMode::Repeat),
            ),
            Frame::new(
                cmd::RECOIL_EVENTS,
                [
                    b(self.on_trigger),
                    b(self.on_trigger_offscreen),
                    b(self.on_pump_on),
                    b(self.on_pump_off),
                ],
            ),
            Frame::new(
                cmd::RECOIL_BUTTONS,
                [
                    b(self.on_front_left),
                    b(self.on_rear_left),
                    b(self.on_front_right),
                    b(self.on_rear_right),
                ],
            ),
            Frame::new(cmd::RECOIL_TIMING, [4, 4, 4, 0]),
            Frame::flag(cmd::RECOIL_ENABLE, self.enabled),
            Frame::new(cmd::RECOIL_STRENGTH_EXT, [ext, 0, 0, 0]),
        ]
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Load `path` if it exists, otherwise defaults.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        if path.exists() {
            Self::load(path)
        } else {
            Ok(Self::default())
        }
    }

    pub fn to_toml(&self) -> String {
        toml::to_string_pretty(self).unwrap_or_default()
    }

    /// The effective display settings for an optional profile name.
    pub fn display_for(&self, profile: Option<&str>) -> Result<Display> {
        match profile {
            None => Ok(self.display.clone()),
            Some(name) => self
                .profiles
                .get(name)
                .map(|o| self.display.with_overrides(o))
                .ok_or_else(|| ConfigError::Invalid(format!("no display profile named {name:?}"))),
        }
    }

    /// The gun entry that matches an attached gun, if any: first entry whose every set
    /// matcher field agrees. An entry with no matcher fields matches anything.
    /// The gun entry that matches an attached gun, if any: first entry whose every set
    /// matcher field agrees. An entry with no matcher fields matches anything. Entries with
    /// an `id` are considered before entries without, so a catch-all can come first in the file.
    pub fn gun_for(
        &self,
        id: Option<&str>,
        variant: Option<&str>,
        usb_path: &str,
        port: &str,
    ) -> Option<&GunConfig> {
        let matches = |g: &&GunConfig| {
            let m = &g.matcher;
            m.id.as_deref().is_none_or(|i| Some(i) == id)
                && m.variant.as_deref().is_none_or(|v| {
                    Some(v.to_ascii_lowercase().as_str())
                        == variant.map(str::to_ascii_lowercase).as_deref()
                })
                && m.usb_path.as_deref().is_none_or(|p| p == usb_path)
                && m.port.as_deref().is_none_or(|p| p == port)
        };
        self.gun
            .iter()
            .filter(|g| g.matcher.id.is_some())
            .find(matches)
            .or_else(|| {
                self.gun
                    .iter()
                    .filter(|g| g.matcher.id.is_none())
                    .find(matches)
            })
    }

    /// Recoil configuration frames are sent with this pause between them.
    pub fn recoil_gap(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.global.recoil_gap_ms)
    }
}

/// A commented starting config.
pub fn example_toml() -> String {
    let body = Config::default().to_toml();
    format!(
        "# sindenrs configuration. Every key is optional; these are the defaults.\n\
         # Display tuning is per display (threshold 48 / contrast 50 suit an OLED at exposure 7.8 ms).\n\
         # Add [profiles.<name>] tables with any subset of [display] keys and pick one with --profile.\n\
         # Add more [[gun]] entries with a [gun.match] table (variant / usb_path / port) for player 2.\n\
         # Button actions: none, mouse_left|middle|right, pause, turbo, turbo_reload, border_toggle,\n\
         #   key:<char>, key:return|escape|tab|space|up|down|left|right|f1..f12, joy:<1-20>.\n\n{body}"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_round_trip() {
        let c = Config::default();
        let text = c.to_toml();
        let back: Config = toml::from_str(&text).expect("parse");
        assert_eq!(back, c);
        let ex: Config = toml::from_str(&example_toml()).expect("example parses");
        assert_eq!(ex, c);
    }

    #[test]
    fn action_values_match_vendor_table() {
        for (s, v) in [
            ("mouse_left", 255),
            ("mouse_right", 253),
            ("key:1", 49),
            ("key:5", 53),
            ("key:a", 97),
            ("key:A", 65),
            ("key:up", 218),
            ("key:right", 215),
            ("key:f1", 194),
            ("key:f12", 205),
            ("key:return", 176),
            ("key:space", 32),
            ("joy:20", 20),
            ("border_toggle", 249),
            ("none", 0),
        ] {
            assert_eq!(Action::new(s).wire_value().unwrap_or(0xEE), v, "{s}");
        }
        assert!(Action::new("key:f13").wire_value().is_err());
        assert!(Action::new("joy:21").wire_value().is_err());
        assert!(Action::new("bogus").wire_value().is_err());
        assert!(toml::from_str::<Slot>("onscreen = \"bogus\"").is_err());
    }

    #[test]
    fn button_frames_layout() {
        let f = Buttons::default().frames().expect("frames");
        assert_eq!(f.len(), 41);
        assert_eq!(f[0].as_bytes(), &[0xAA, 60, 0, 100, 0, 255, 0xBB]); // trigger -> mouse left
        assert_eq!(f[2].as_bytes(), &[0xAA, 60, 0, 102, 0, 49, 0xBB]); // rear left -> '1'
        assert_eq!(f[10].as_bytes()[3], 110); // first offscreen slot
        assert_eq!(f[20].as_bytes()[3], 120); // first modifier
        assert_eq!(f[40].as_bytes()[3], 140); // pedal
    }

    #[test]
    fn recoil_burst_matches_vendor_order() {
        let r = Recoil {
            enabled: true,
            strength: 80,
            ..Default::default()
        };
        let f = r.frames();
        let cmds: Vec<u8> = f.iter().map(Frame::command).collect();
        assert_eq!(cmds, vec![162, 161, 167, 163, 164, 165, 171, 161, 172]);
        // Both strength commands carry the same 0..=250 level, as the Windows app does.
        assert_eq!(f[2].as_bytes()[2], 200);
        assert_eq!(f[8].as_bytes()[2], 200);
        assert_eq!(
            Recoil {
                strength: 100,
                ..Default::default()
            }
            .wire_level(),
            250
        );
        assert_eq!(
            Recoil {
                strength: 0,
                ..Default::default()
            }
            .wire_level(),
            0
        );
        assert_eq!(
            Recoil {
                strength: 40,
                ..Default::default()
            }
            .wire_level(),
            100
        );
        assert_eq!(f[7].as_bytes()[2], 1);
        let off = Recoil::default().frames();
        assert_eq!(off[7].as_bytes()[2], 0);
    }

    #[test]
    fn profiles_and_gun_matching() {
        let text = r#"
[display]
threshold = 40
[profiles.crt]
exposure = 120
gunsight_y = 3.0
[[gun]]
name = "p2"
match = { variant = "player2" }
[[gun]]
name = "any"
"#;
        let c: Config = toml::from_str(text).expect("parse");
        let crt = c.display_for(Some("crt")).expect("profile");
        assert_eq!(crt.threshold, 40);
        assert_eq!(crt.exposure, Exposure::Manual(120));
        assert!((crt.gunsight_y - 3.0).abs() < 1e-9);
        assert!(c.display_for(Some("nope")).is_err());
        assert_eq!(
            c.gun_for(None, Some("Blue"), "1-3.2", "/dev/ttyACM0")
                .map(|g| g.name.as_str()),
            Some("any")
        );
        assert_eq!(
            c.gun_for(None, Some("player2"), "x", "y")
                .map(|g| g.name.as_str()),
            Some("p2")
        );
        let by_id: Config = toml::from_str(
            "[[gun]]\nname = \"catchall\"\n[[gun]]\nname = \"mine\"\n[gun.match]\nid = \"42\"\n",
        )
        .expect("parse");
        assert_eq!(
            by_id
                .gun_for(Some("42"), None, "", "")
                .map(|g| g.name.as_str()),
            Some("mine")
        );
        assert_eq!(
            by_id
                .gun_for(Some("7"), None, "", "")
                .map(|g| g.name.as_str()),
            Some("catchall")
        );
        let (x, y) = Display {
            offset_x: 1.0,
            ratio_y: 0.5,
            ..Default::default()
        }
        .finish_aim(50.0, 50.0);
        assert!((x - 51.0).abs() < 1e-9 && (y - 20.1).abs() < 1e-9);
    }
}
