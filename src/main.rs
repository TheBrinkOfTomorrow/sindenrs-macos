//! sindenrs command-line tool: probe hardware, measure the camera, talk to the gun.

// The camera commands only have a Linux backend so far; keep the shared helpers quiet elsewhere.
#![cfg_attr(not(target_os = "linux"), allow(dead_code, unused_imports))]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::Path;
use tracing::{debug, info, warn};

use sindenrs::camera::{cid, PixelFormat};
use sindenrs::discovery;
use sindenrs::gun::Gun;
use sindenrs::protocol::{cmd, percent_to_axis};

#[derive(Parser)]
#[command(
    name = "sindenrs",
    version,
    about = "Clean-room Sinden Lightgun driver and measurement tool"
)]
struct Cli {
    /// Log level filter (also honours RUST_LOG), e.g. `debug` or `sindenrs=trace`.
    #[arg(long, global = true)]
    log: Option<String>,
    /// Config file (default: $XDG_CONFIG_HOME/sindenrs/config.toml, or $SINDENRS_CONFIG).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Display profile from the config's [profiles.<name>] tables.
    #[arg(long, global = true)]
    profile: Option<String>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List attached guns and cameras, with each gun's unique id and config entry.
    Probe {
        /// Skip talking to the guns (no ids, no config matching).
        #[arg(long)]
        quick: bool,
    },
    /// Configuration file management.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Live border tracking: camera -> corners -> aim point, optionally driving the gun.
    Track(Box<TrackArgs>),
    /// Camera capture and measurement.
    Camera {
        #[command(subcommand)]
        cmd: CameraCmd,
    },
    /// Talk to a gun over its serial port.
    Gun {
        #[command(subcommand)]
        cmd: GunCmd,
    },
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Write a commented default config file.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Print the effective configuration (defaults merged with the file and --profile).
    Show,
    /// Print the config file path in use.
    Path,
}

/// Loaded once in main and handed to commands.
struct Ctx {
    path: PathBuf,
    cfg: sindenrs::config::Config,
    display: sindenrs::config::Display,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FormatArg {
    Mjpeg,
    Yuyv,
}

impl From<FormatArg> for PixelFormat {
    fn from(f: FormatArg) -> Self {
        match f {
            FormatArg::Mjpeg => PixelFormat::Mjpeg,
            FormatArg::Yuyv => PixelFormat::Yuyv,
        }
    }
}

#[derive(Args, Clone)]
struct CameraSettings {
    /// Video device; defaults to the first Sinden capture node found.
    #[arg(short, long)]
    device: Option<PathBuf>,
    #[arg(long, value_enum, default_value = "mjpeg")]
    format: FormatArg,
    #[arg(long, default_value_t = 640)]
    width: u32,
    #[arg(long, default_value_t = 480)]
    height: u32,
    /// Requested frame rate (VIDIOC_S_PARM). Omit to leave the camera's default.
    #[arg(long)]
    fps: Option<u32>,
    /// Exposure in 100 µs units (e.g. 39 = 3.9 ms), or `auto`.
    #[arg(long)]
    exposure: Option<String>,
    #[arg(long)]
    brightness: Option<i32>,
    #[arg(long)]
    contrast: Option<i32>,
    #[arg(long)]
    gain: Option<i32>,
    #[arg(long)]
    gamma: Option<i32>,
    #[arg(long)]
    sharpness: Option<i32>,
    /// Number of mmap buffers to request.
    #[arg(long, default_value_t = 4)]
    buffers: u32,
}

#[derive(Subcommand)]
enum CameraCmd {
    /// Show capabilities, formats, frame rates and every control with its range.
    Info {
        #[arg(short, long)]
        device: Option<PathBuf>,
    },
    /// Stream frames, log capture timing, optionally save frames.
    Capture {
        #[command(flatten)]
        settings: CameraSettings,
        /// Stop after this many frames.
        #[arg(long, default_value_t = 300)]
        frames: u32,
        /// Directory to save frames into (raw .jpg for MJPEG, .yuyv for YUYV, plus .pgm luma).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Save every Nth frame (only with --out).
        #[arg(long, default_value_t = 30)]
        save_every: u32,
        /// Process every frame in order instead of draining to the newest.
        #[arg(long)]
        no_drain: bool,
        /// Simulate a slow consumer: sleep this many ms per frame, to demonstrate the drain.
        #[arg(long, default_value_t = 0)]
        stall_ms: u64,
        /// Print one line per frame.
        #[arg(long)]
        per_frame: bool,
    },
    /// Measure achieved frame rate across a list of exposure values.
    SweepExposure {
        #[command(flatten)]
        settings: CameraSettings,
        /// Comma-separated exposure values in 100 µs units.
        #[arg(long, default_value = "19,39,78,120,167,200,250,333,500")]
        values: String,
        #[arg(long, default_value_t = 120)]
        frames: u32,
    },
}

#[derive(Args)]
struct TrackArgs {
    #[command(flatten)]
    settings: CameraSettings,
    /// Send the aim point to the gun (moves the real cursor). Without it, only prints.
    #[arg(long)]
    send: bool,
    /// With --send: put the gun in joystick mode (firmware 1.9+) instead of mouse mode.
    #[arg(long)]
    joystick: bool,
    #[arg(short, long)]
    port: Option<String>,
    /// Luma threshold for the border (default from config: display.threshold).
    #[arg(long)]
    threshold: Option<u8>,
    /// Minimum blob width and height in half-resolution pixels (config: display.min_size).
    #[arg(long)]
    min_size: Option<u32>,
    /// Bore offset X in percent of frame; default: config, else the gun's EEPROM (with --send), else 0.
    #[arg(long)]
    cal_x: Option<f64>,
    #[arg(long)]
    cal_y: Option<f64>,
    /// Gunsight Y offset in screen percent (config: display.gunsight_y).
    #[arg(long)]
    gunsight_y: Option<f64>,
    /// Camera orientation sign applied to the bore offset (config: display.orientation).
    #[arg(long, allow_hyphen_values = true)]
    orientation: Option<f64>,
    /// Image flip applied before detection (config: display.flip).
    #[arg(long, value_enum)]
    flip: Option<FlipArg>,
    /// Save every frame (.jpg) and a frames.csv of results here, for regression replay.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Stop after this many frames.
    #[arg(long, default_value_t = 600)]
    frames: u32,
    /// Print one line per frame.
    #[arg(long)]
    per_frame: bool,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum RecoilAction {
    Test,
    Auto,
    Off,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum JoystickDeviceAction {
    Enable,
    Disable,
    Status,
}

#[derive(Subcommand)]
enum FirmwareCmd {
    /// Show what an image contains (address range, embedded USB IDs, bootloader section).
    Info { image: PathBuf },
    /// Enter the bootloader and dump the whole flash to a file (.hex or .bin by extension).
    Backup {
        #[arg(short, long)]
        port: Option<String>,
        /// Output path; default corpus/firmware/backup-<date>.hex
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Flash an image's application section, after backing up and with read-back verify.
    Flash {
        #[arg(short, long)]
        port: Option<String>,
        image: PathBuf,
        /// Actually write. Without this the command only backs up, compares, and reports.
        #[arg(long)]
        yes: bool,
        /// Allow an image whose embedded product ID differs from the attached gun's.
        #[arg(long)]
        allow_id_mismatch: bool,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum FlipArg {
    None,
    Horizontal,
    Vertical,
    Both,
}

impl From<FlipArg> for sindenrs::vision::acquire::Flip {
    fn from(f: FlipArg) -> Self {
        use sindenrs::vision::acquire::Flip;
        match f {
            FlipArg::None => Flip::None,
            FlipArg::Horizontal => Flip::Horizontal,
            FlipArg::Vertical => Flip::Vertical,
            FlipArg::Both => Flip::Both,
        }
    }
}

fn flip_from_config(f: sindenrs::config::Flip) -> sindenrs::vision::acquire::Flip {
    use sindenrs::config::Flip as C;
    use sindenrs::vision::acquire::Flip as V;
    match f {
        C::None => V::None,
        C::Horizontal => V::Horizontal,
        C::Vertical => V::Vertical,
        C::Both => V::Both,
    }
}

/// Camera settings from the display config, with CLI flags taking precedence.
fn camera_settings_from(
    display: &sindenrs::config::Display,
    cli: &CameraSettings,
) -> CameraSettings {
    use sindenrs::config::Exposure;
    let mut s = cli.clone();
    if s.exposure.is_none() {
        s.exposure = Some(match display.exposure {
            Exposure::Manual(v) => v.to_string(),
            Exposure::Auto(_) => "auto".into(),
        });
    }
    s.brightness = s.brightness.or(display.brightness);
    s.contrast = s.contrast.or(display.contrast);
    s.gain = s.gain.or(display.gain);
    s.gamma = s.gamma.or(display.gamma);
    s.sharpness = s.sharpness.or(display.sharpness);
    s.fps = s.fps.or(display.fps);
    s
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Pattern {
    Circle,
    Box,
    Lissajous,
}

#[derive(Subcommand)]
enum GunCmd {
    /// Authenticate and read everything the gun will tell us, with reply timings.
    Info {
        #[arg(short, long)]
        port: Option<String>,
        /// Do not power-cycle the gun if it fails to answer the handshake.
        #[arg(long)]
        no_recover: bool,
    },
    /// Reset the gun's microcontroller via its bootloader (1200-baud touch). Recovers a
    /// wedged firmware; needs only the serial port.
    Reset {
        #[arg(short, long)]
        port: Option<String>,
    },
    /// Power-cycle the gun's USB port on its internal hub (re-enumerates it; does not reset the
    /// microcontroller on this hardware, so prefer `reset`).
    PowerCycle {
        #[arg(short, long)]
        port: Option<String>,
    },
    /// Authenticate, start streaming, hold a fixed position, and log every event byte.
    Monitor {
        #[arg(short, long)]
        port: Option<String>,
        /// Do not power-cycle the gun if it fails to answer the handshake.
        #[arg(long)]
        no_recover: bool,
        #[arg(long, default_value_t = 10.0)]
        seconds: f64,
        /// Position to hold, in screen percent.
        #[arg(long, default_value_t = 50.0)]
        x: f64,
        #[arg(long, default_value_t = 50.0)]
        y: f64,
        /// Position report rate in Hz.
        #[arg(long, default_value_t = 60.0)]
        rate: f64,
    },
    /// Move the pointer through a pattern to validate the position path end to end.
    Sweep {
        #[arg(short, long)]
        port: Option<String>,
        /// Do not power-cycle the gun if it fails to answer the handshake.
        #[arg(long)]
        no_recover: bool,
        /// Put the gun in joystick mode (firmware 1.9+): positions go out as joystick axes.
        #[arg(long)]
        joystick: bool,
        #[arg(long, default_value_t = 5.0)]
        seconds: f64,
        #[arg(long, value_enum, default_value = "circle")]
        pattern: Pattern,
        #[arg(long, default_value_t = 60.0)]
        rate: f64,
    },
    /// Send one raw frame (command byte and up to four payload bytes) and dump whatever the
    /// gun replies within a window. For protocol exploration.
    Raw {
        #[arg(short, long)]
        port: Option<String>,
        /// Command byte, decimal or 0x-hex.
        command: String,
        /// Payload bytes p1..p4 (missing ones are zero).
        payload: Vec<String>,
        /// How long to listen for a reply, in ms.
        #[arg(long, default_value_t = 300)]
        window_ms: u64,
    },
    /// Send the full startup configuration (modes, button map, recoil) from the config file.
    Setup {
        #[arg(short, long)]
        port: Option<String>,
        /// Override global.recoil_gap_ms for this run.
        #[arg(long)]
        recoil_gap_ms: Option<u64>,
    },
    /// Fire recoil on demand (test), run automatic recoil, or switch recoil off.
    Recoil {
        #[arg(short, long)]
        port: Option<String>,
        #[arg(value_enum)]
        action: RecoilAction,
        /// Number of test pulses.
        #[arg(long, default_value_t = 3)]
        count: u32,
        /// Pause between test pulses, or duration of automatic recoil, in ms.
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        /// Override global.recoil_gap_ms for this run.
        #[arg(long)]
        recoil_gap_ms: Option<u64>,
        /// Override the configured strength (0-100) for this run.
        #[arg(long)]
        strength: Option<u8>,
    },
    /// Enable, disable or query the gun's joystick HID device (firmware 1.9+, persistent;
    /// the gun is reset afterwards so the change takes effect).
    JoystickDevice {
        #[arg(short, long)]
        port: Option<String>,
        #[arg(value_enum)]
        action: JoystickDeviceAction,
    },
    /// Firmware: inspect an image, back up the gun's flash, or flash a new image.
    Firmware {
        #[command(subcommand)]
        cmd: FirmwareCmd,
    },
    /// Write bore calibration offsets (percent of frame) to the gun's EEPROM.
    WriteCalibration {
        #[arg(short, long)]
        port: Option<String>,
        /// Do not power-cycle the gun if it fails to answer the handshake.
        #[arg(long)]
        no_recover: bool,
        #[arg(long)]
        x: f64,
        #[arg(long)]
        y: f64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let path = cli
        .config
        .clone()
        .unwrap_or_else(sindenrs::config::default_path);
    // `config init` must work even when the current file is unparsable.
    let cfg = if matches!(
        cli.cmd,
        Cmd::Config {
            cmd: ConfigCmd::Init { .. }
        }
    ) {
        sindenrs::config::Config::default()
    } else {
        sindenrs::config::Config::load_or_default(&path)?
    };
    let log = cli.log.clone().unwrap_or_else(|| cfg.global.log.clone());
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(&log));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
    let display = cfg.display_for(cli.profile.as_deref())?;
    let ctx = Ctx { path, cfg, display };
    if ctx.path.exists() {
        debug!(path = %ctx.path.display(), "config loaded");
    }

    match cli.cmd {
        Cmd::Probe { quick } => probe(&ctx, quick),
        Cmd::Config { cmd } => config_cmd(&ctx, cmd),
        Cmd::Track(args) => track(&ctx, *args),
        Cmd::Camera { cmd } => camera(cmd),
        Cmd::Gun { cmd } => gun(&ctx, cmd),
    }
}

fn config_cmd(ctx: &Ctx, cmd: ConfigCmd) -> Result<()> {
    match cmd {
        ConfigCmd::Init { force } => {
            if ctx.path.exists() && !force {
                bail!("{} exists; pass --force to overwrite", ctx.path.display());
            }
            if let Some(dir) = ctx.path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let mut text = sindenrs::config::example_toml();
            // One [[gun]] entry per attached gun, keyed by unique id, so the file is legible
            // and survives port renumbering.
            let guns = discovery::find_guns()?;
            let mut entries = Vec::new();
            for (i, g) in guns.iter().enumerate() {
                let port = g.port.to_string_lossy().into_owned();
                if let Ok(mut gun) = connect(&port, ctx.cfg.global.auto_recover) {
                    if let Ok(id) = gun.unique_id() {
                        let mut gc = sindenrs::config::GunConfig {
                            name: format!("player{}", i + 1),
                            ..Default::default()
                        };
                        gc.matcher.id = Some(id);
                        entries.push(gc);
                    }
                }
            }
            let n_guns = entries.len();
            if !entries.is_empty() {
                let c = sindenrs::config::Config {
                    gun: entries,
                    ..Default::default()
                };
                // Replace the default [[gun]] block with the detected ones.
                // Match a table header at line start, not the "[[gun]]" mentioned in the comments.
                if let Some(i) = text.find("\n[[gun]]\n") {
                    text.truncate(i + 1);
                }
                let guns_toml = c.to_toml();
                if let Some(i) = guns_toml.find("\n[[gun]]\n") {
                    text.push_str(&guns_toml[i + 1..]);
                } else if let Some(i) = guns_toml.find("[[gun]]\n") {
                    text.push_str(&guns_toml[i..]);
                }
            }
            std::fs::write(&ctx.path, text)?;
            println!(
                "wrote {} ({} gun{} detected)",
                ctx.path.display(),
                n_guns,
                if n_guns == 1 { "" } else { "s" }
            );
            Ok(())
        }
        ConfigCmd::Show => {
            let mut c = ctx.cfg.clone();
            c.display = ctx.display.clone();
            print!("{}", c.to_toml());
            Ok(())
        }
        ConfigCmd::Path => {
            println!(
                "{}{}",
                ctx.path.display(),
                if ctx.path.exists() {
                    ""
                } else {
                    " (not present; defaults in use)"
                }
            );
            Ok(())
        }
    }
}

/// The config entry for an attached gun, or the built-in default. `id` is the gun's unique
/// id when the caller has already talked to it.
fn gun_config_for(ctx: &Ctx, port: &str, id: Option<&str>) -> sindenrs::config::GunConfig {
    let guns = discovery::find_guns().unwrap_or_default();
    let dev = guns.iter().find(|g| g.port.to_string_lossy() == port);
    let variant = dev.and_then(|g| g.variant).map(|v| match v {
        sindenrs::ids::GunVariant::Blue => "blue",
        sindenrs::ids::GunVariant::Red => "red",
        sindenrs::ids::GunVariant::Black => "black",
        sindenrs::ids::GunVariant::Player2 => "player2",
    });
    let usb_path = dev.map_or("", |g| g.usb_path.as_str());
    ctx.cfg
        .gun_for(id, variant, usb_path, port)
        .cloned()
        .unwrap_or_default()
}

fn probe(ctx: &Ctx, quick: bool) -> Result<()> {
    let guns = discovery::find_guns()?;
    let cams = discovery::find_cameras()?;
    if guns.is_empty() {
        println!("no guns found");
    }
    for g in &guns {
        println!(
            "gun     {}  {:04x}:{:04x}  {}  product={:?} serial={:?}  usb={}",
            g.port.display(),
            g.vid,
            g.pid,
            g.variant.map_or("unknown variant", |v| v.name()),
            g.product.as_deref().unwrap_or("-"),
            g.serial.as_deref().unwrap_or("-"),
            g.usb_path
        );
        let access = match std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&g.port)
        {
            Ok(_) => "ok".to_owned(),
            Err(e) => format!("NO ACCESS ({e}) — see README: udev rules / dialout group"),
        };
        println!("        access: {access}");
        if let Some(c) = g.sibling_camera(&cams) {
            println!("        camera: {} ({})", c.node.display(), c.name);
        }
        if !quick {
            let port = g.port.to_string_lossy().into_owned();
            match connect(&port, ctx.cfg.global.auto_recover) {
                Ok(mut gun) => {
                    let id = gun.unique_id().unwrap_or_default();
                    let fw = gun
                        .firmware_version()
                        .map(|((a, b), _)| format!("v{a}.{b}"))
                        .unwrap_or_default();
                    let gc = gun_config_for(ctx, &port, Some(&id));
                    let matched = gc.matcher.id.as_deref() == Some(id.as_str());
                    println!(
                        "        id: {id}  firmware {fw}  config: [[gun]] \"{}\"{}",
                        gc.name,
                        if matched {
                            ""
                        } else {
                            "  (not matched by id; `config init --force` or add [gun.match] id)"
                        }
                    );
                }
                Err(e) => println!("        could not talk to the gun: {e}"),
            }
        }
    }
    if cams.is_empty() {
        println!("no Sinden cameras found");
    }
    for c in &cams {
        println!(
            "camera  {}  {:04x}:{:04x}  {:?}  usb={}  {}",
            c.node.display(),
            c.vid,
            c.pid,
            c.name,
            c.usb_path,
            match c.is_capture {
                Some(true) => "video capture",
                Some(false) => "metadata node (not for capture)",
                None => "could not open (permissions?)",
            }
        );
    }
    Ok(())
}

fn default_camera(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(d) = explicit {
        return Ok(d);
    }
    let cams = discovery::find_cameras()?;
    cams.into_iter()
        .find(|c| c.is_capture == Some(true))
        .map(|c| c.node)
        .ok_or_else(|| anyhow!("no Sinden capture camera found; pass --device"))
}

fn default_port(explicit: Option<String>) -> Result<String> {
    if let Some(p) = explicit {
        return Ok(p);
    }
    let guns = discovery::find_guns()?;
    guns.first()
        .map(|g| g.port.to_string_lossy().into_owned())
        .ok_or_else(|| anyhow!("no gun found; pass --port"))
}

#[cfg(target_os = "linux")]
fn camera(cmd: CameraCmd) -> Result<()> {
    use sindenrs::camera::v4l2::{control_name, Device};

    match cmd {
        CameraCmd::Info { device } => {
            let path = default_camera(device)?;
            let dev = Device::open(&path).with_context(|| format!("opening {}", path.display()))?;
            let cap = dev.capability()?;
            println!(
                "{}: driver={} card={:?} bus={} caps={:#010x} device_caps={:#010x} capture={}",
                path.display(),
                cap.driver,
                cap.card,
                cap.bus_info,
                cap.capabilities,
                cap.device_caps,
                cap.is_video_capture()
            );
            if !cap.is_video_capture() {
                bail!("not a video capture node");
            }
            let fmt = dev.get_format()?;
            println!(
                "current format: {}x{} {} ({} bytes/line, {} bytes/frame)",
                fmt.width, fmt.height, fmt.format, fmt.bytes_per_line, fmt.size_image
            );
            if let Ok((n, d)) = dev.get_frame_interval() {
                println!("current interval: {n}/{d} s");
            }
            for f in dev.formats()? {
                println!("format {} ({})", f.format, f.description);
                for s in &f.sizes {
                    let rates: Vec<String> = s
                        .intervals
                        .iter()
                        .map(|&(n, d)| {
                            if n == 0 {
                                "?".into()
                            } else {
                                format!("{:.1} fps", f64::from(d) / f64::from(n))
                            }
                        })
                        .collect();
                    println!("  {}x{}: {}", s.width, s.height, rates.join(", "));
                }
            }
            println!("controls:");
            for c in dev.controls()? {
                let name = control_name(c.id).map_or(c.name.clone(), str::to_owned);
                let val = c.value.map_or("?".into(), |v| v.to_string());
                let inactive = if c.inactive { " [inactive]" } else { "" };
                println!(
                    "  {:<28} {:#010x} {:?} min={} max={} step={} default={} value={}{}",
                    name, c.id, c.kind, c.min, c.max, c.step, c.default, val, inactive
                );
                for (i, label) in &c.menu {
                    println!("      {i}: {label}");
                }
            }
            Ok(())
        }
        CameraCmd::Capture {
            settings,
            frames,
            out,
            save_every,
            no_drain,
            stall_ms,
            per_frame,
        } => {
            let path = default_camera(settings.device.clone())?;
            let dev = Device::open(&path).with_context(|| format!("opening {}", path.display()))?;
            apply_settings(&dev, &settings)?;
            if let Some(dir) = &out {
                std::fs::create_dir_all(dir)?;
            }
            let stats = run_capture(
                &dev,
                settings.buffers,
                frames,
                !no_drain,
                stall_ms,
                per_frame,
                out.as_deref(),
                save_every,
            )?;
            stats.print();
            Ok(())
        }
        CameraCmd::SweepExposure {
            settings,
            values,
            frames,
        } => {
            let path = default_camera(settings.device.clone())?;
            let dev = Device::open(&path).with_context(|| format!("opening {}", path.display()))?;
            apply_settings(&dev, &settings)?;
            println!(
                "{:>9} {:>8} {:>9} {:>9} {:>9} {:>8} {:>7}",
                "exposure", "fps", "age_mean", "age_max", "ivl_p95", "luma", "dropped"
            );
            for v in values.split(',') {
                let v: i32 = v
                    .trim()
                    .parse()
                    .with_context(|| format!("bad exposure value {v:?}"))?;
                dev.set_manual_exposure(v)?;
                std::thread::sleep(Duration::from_millis(200));
                let applied = dev.get_control(cid::EXPOSURE_ABSOLUTE).unwrap_or(-1);
                let s = run_capture(&dev, settings.buffers, frames, true, 0, false, None, 0)?;
                println!(
                    "{:>9} {:>8.2} {:>9} {:>9} {:>9} {:>8.1} {:>7}{}",
                    v,
                    s.fps(),
                    fmt_ms(s.age_mean()),
                    fmt_ms(s.age_max),
                    fmt_ms(s.interval_p95()),
                    s.luma_mean,
                    s.dropped,
                    if applied == v {
                        String::new()
                    } else {
                        format!("  (camera reports {applied})")
                    }
                );
            }
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
fn apply_settings(dev: &sindenrs::camera::v4l2::Device, s: &CameraSettings) -> Result<()> {
    let fmt = dev.set_format(s.width, s.height, s.format.into())?;
    info!(
        "format {}x{} {} sizeimage={}",
        fmt.width, fmt.height, fmt.format, fmt.size_image
    );
    if let Some(fps) = s.fps {
        let (n, d) = dev.set_frame_interval(1, fps)?;
        info!("frame interval {n}/{d} s");
    }
    match s.exposure.as_deref() {
        None => {}
        Some("auto") | Some("A") | Some("a") => dev.set_auto_exposure()?,
        Some(v) => {
            let v: i32 = v.parse().with_context(|| format!("bad --exposure {v:?}"))?;
            dev.set_manual_exposure(v)?;
        }
    }
    for (id, val, name) in [
        (cid::BRIGHTNESS, s.brightness, "brightness"),
        (cid::CONTRAST, s.contrast, "contrast"),
        (cid::GAIN, s.gain, "gain"),
        (cid::GAMMA, s.gamma, "gamma"),
        (cid::SHARPNESS, s.sharpness, "sharpness"),
    ] {
        if let Some(v) = val {
            dev.set_control(id, v)
                .with_context(|| format!("setting {name}={v}"))?;
        }
    }
    if let (Ok(auto), Ok(exp)) = (
        dev.get_control(cid::EXPOSURE_AUTO),
        dev.get_control(cid::EXPOSURE_ABSOLUTE),
    ) {
        info!(
            "exposure: auto={auto} absolute={exp} (x100 µs = {:.1} ms)",
            f64::from(exp) / 10.0
        );
    }
    Ok(())
}

#[derive(Default)]
struct CaptureStats {
    frames: u32,
    first_ts: Option<Duration>,
    last_ts: Option<Duration>,
    ages: Vec<Duration>,
    age_max: Duration,
    intervals: Vec<Duration>,
    dropped: u32,
    seq_gaps: u32,
    errors: u32,
    bytes_total: u64,
    luma_mean: f64,
    luma_max: u8,
    timestamp_source: Option<&'static str>,
    wall: Duration,
}

impl CaptureStats {
    fn fps(&self) -> f64 {
        match (self.first_ts, self.last_ts) {
            (Some(a), Some(b)) if b > a && self.frames > 1 => {
                f64::from(self.frames - 1) / (b - a).as_secs_f64()
            }
            _ => f64::from(self.frames) / self.wall.as_secs_f64().max(1e-9),
        }
    }
    fn age_mean(&self) -> Duration {
        if self.ages.is_empty() {
            return Duration::ZERO;
        }
        self.ages.iter().sum::<Duration>() / u32::try_from(self.ages.len()).unwrap_or(u32::MAX)
    }
    fn percentile(v: &[Duration], p: f64) -> Duration {
        if v.is_empty() {
            return Duration::ZERO;
        }
        let mut s = v.to_vec();
        s.sort_unstable();
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let i = ((s.len() - 1) as f64 * p).round() as usize;
        s[i]
    }
    fn interval_p95(&self) -> Duration {
        Self::percentile(&self.intervals, 0.95)
    }
    fn print(&self) {
        println!(
            "frames={} wall={:.2}s fps={:.2} (from {} timestamps)",
            self.frames,
            self.wall.as_secs_f64(),
            self.fps(),
            self.timestamp_source.unwrap_or("no")
        );
        println!(
            "frame age at dequeue: mean={} p50={} p95={} max={}",
            fmt_ms(self.age_mean()),
            fmt_ms(Self::percentile(&self.ages, 0.5)),
            fmt_ms(Self::percentile(&self.ages, 0.95)),
            fmt_ms(self.age_max)
        );
        println!(
            "capture interval:     p50={} p95={} max={}",
            fmt_ms(Self::percentile(&self.intervals, 0.5)),
            fmt_ms(self.interval_p95()),
            fmt_ms(Self::percentile(&self.intervals, 1.0))
        );
        println!(
            "dropped by drain={} sequence gaps={} error frames={} mean bytes/frame={}",
            self.dropped,
            self.seq_gaps,
            self.errors,
            if self.frames > 0 {
                self.bytes_total / u64::from(self.frames)
            } else {
                0
            }
        );
        println!(
            "last frame luma: mean={:.1} max={}",
            self.luma_mean, self.luma_max
        );
    }
}

fn fmt_ms(d: Duration) -> String {
    format!("{:.2}ms", d.as_secs_f64() * 1000.0)
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_arguments)]
fn run_capture(
    dev: &sindenrs::camera::v4l2::Device,
    buffers: u32,
    frames: u32,
    drain: bool,
    stall_ms: u64,
    per_frame: bool,
    out: Option<&std::path::Path>,
    save_every: u32,
) -> Result<CaptureStats> {
    use sindenrs::vision::luma;

    let fmt = dev.get_format()?;
    let mut stream = dev.start_stream(buffers)?;
    let mut st = CaptureStats::default();
    let start = Instant::now();
    let mut last_seq: Option<u32> = None;
    let mut last_ts: Option<Duration> = None;
    let mut luma_buf = Vec::new();
    while st.frames < frames {
        let Some(frame) = stream.next(Some(Duration::from_secs(3)), drain)? else {
            warn!("timed out waiting for a frame");
            break;
        };
        st.frames += 1;
        st.dropped += frame.dropped;
        st.bytes_total += frame.data.len() as u64;
        if frame.error {
            st.errors += 1;
        }
        if let Some(prev) = last_seq {
            let expected = prev.wrapping_add(1 + frame.dropped);
            if frame.sequence != expected {
                st.seq_gaps += frame.sequence.wrapping_sub(expected);
            }
        }
        last_seq = Some(frame.sequence);
        if let Some(age) = frame.age() {
            st.ages.push(age);
            st.age_max = st.age_max.max(age);
        }
        if let (Some(ts), Some(prev)) = (frame.timestamp, last_ts) {
            st.intervals.push(ts.saturating_sub(prev));
        }
        if let Some(ts) = frame.timestamp {
            st.first_ts.get_or_insert(ts);
            st.last_ts = Some(ts);
            last_ts = Some(ts);
        }
        if per_frame {
            println!(
                "seq={:<6} bytes={:<7} age={} dropped={} err={}",
                frame.sequence,
                frame.data.len(),
                frame.age().map_or("-".into(), fmt_ms),
                frame.dropped,
                frame.error
            );
        }
        let want_save = out.is_some() && save_every > 0 && st.frames % save_every == 0;
        let is_last = st.frames == frames;
        if want_save || is_last {
            let decoded = match fmt.format {
                PixelFormat::Mjpeg => match luma::mjpeg_to_luma(frame.data) {
                    Ok(v) => Some(v),
                    Err(e) => {
                        warn!("jpeg decode failed: {e}");
                        None
                    }
                },
                PixelFormat::Yuyv => {
                    luma::yuyv_to_luma(frame.data, &mut luma_buf);
                    Some((fmt.width, fmt.height, luma_buf.clone()))
                }
                PixelFormat::Other(_) => None,
            };
            if let Some((w, h, l)) = &decoded {
                let (mean, max) = luma::luma_stats(l);
                st.luma_mean = mean;
                st.luma_max = max;
                if want_save {
                    if let Some(dir) = out {
                        let stem = format!("frame_{:06}", frame.sequence);
                        let ext = match fmt.format {
                            PixelFormat::Mjpeg => "jpg",
                            _ => "yuyv",
                        };
                        std::fs::write(dir.join(format!("{stem}.{ext}")), frame.data)?;
                        luma::write_pgm(&dir.join(format!("{stem}.pgm")), *w, *h, l)?;
                    }
                }
            }
        }
        if stall_ms > 0 {
            std::thread::sleep(Duration::from_millis(stall_ms));
        }
    }
    st.timestamp_source = stream.timestamp_source();
    st.wall = start.elapsed();
    Ok(st)
}

#[cfg(not(target_os = "linux"))]
fn camera(_cmd: CameraCmd) -> Result<()> {
    bail!("camera capture is not implemented on this platform yet")
}

/// Open and authenticate. If the gun does not answer the handshake at all, its firmware has
/// wedged (it does this after a truncated auth command); reset it through the bootloader and
/// try again. If the reset cannot even be delivered, fall back to power-cycling the port.
fn connect(port: &str, allow_reset: bool) -> Result<Gun> {
    let mut gun = Gun::open(port).with_context(|| format!("opening {port}"))?;
    match gun.authenticate() {
        Ok(_) => Ok(gun),
        Err(e)
            if allow_reset
                && (looks_wedged(&e) || matches!(e, sindenrs::gun::GunError::GunAuthFailed)) =>
        {
            // Cheapest fix first: satisfy a pending 32-byte read, no re-enumeration.
            if gun.unwedge().is_ok() {
                if let Ok(report) = gun.authenticate() {
                    warn!(
                        "{e}; recovered by completing the firmware's pending read ({:?})",
                        report.leg1.complete
                    );
                    return Ok(gun);
                }
            }
            warn!("{e}; resetting the gun and retrying");
            drop(gun);
            if let Err(e) = reset_port(port) {
                warn!("bootloader reset failed ({e}); power-cycling the port instead");
                power_cycle_port(port)?;
            }
            let mut gun = open_with_retry(port)?;
            match gun.authenticate() {
                Ok(_) => Ok(gun),
                Err(e) => {
                    warn!("{e}; one more try after a longer settle");
                    std::thread::sleep(BOOT_SETTLE);
                    gun.authenticate().context("authentication after reset")?;
                    Ok(gun)
                }
            }
        }
        Err(e) => Err(e).context("authentication"),
    }
}

/// Open the port, waiting briefly for udev to grant access to a re-created node.
fn open_with_retry(port: &str) -> Result<Gun> {
    for _ in 0..20 {
        match Gun::open(port) {
            Ok(g) => return Ok(g),
            Err(sindenrs::gun::GunError::Serial(e)) if e.kind() == serialport_kind_permission() => {
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(e).with_context(|| format!("re-opening {port}")),
        }
    }
    bail!("{port} came back but is not accessible; are the udev rules installed?")
}

fn serialport_kind_permission() -> serialport::ErrorKind {
    serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied)
}

/// The ways a gun whose firmware is stuck in a pending auth read shows up: no reply at all,
/// our bytes still queued, the kernel's write queue already full so the write times out, or
/// (when it still accepts packets) a mismatched hash because it consumed our packet as the
/// stale payload — that last one is matched separately at the call site.
fn looks_wedged(e: &sindenrs::gun::GunError) -> bool {
    use sindenrs::gun::GunError;
    match e {
        GunError::Timeout { got: 0, .. } | GunError::NotReading { .. } => true,
        GunError::Io(io) => io.kind() == std::io::ErrorKind::TimedOut,
        GunError::Serial(se) => {
            matches!(
                se.kind(),
                serialport::ErrorKind::Io(std::io::ErrorKind::TimedOut)
            )
        }
        _ => false,
    }
}

/// How long a freshly rebooted gun needs after USB enumeration before it answers serial.
const BOOT_SETTLE: Duration = Duration::from_secs(3);

/// Reset the gun via the bootloader touch and wait until it is enumerated as a gun again
/// (it spends a few seconds as the Caterina bootloader in between).
fn reset_port(port: &str) -> Result<()> {
    let before = discovery::find_guns()?;
    let usb_path = before
        .iter()
        .find(|g| g.port.to_string_lossy() == port)
        .map(|g| g.usb_path.clone());
    sindenrs::gun::bootloader_touch(port).with_context(|| format!("1200-baud touch on {port}"))?;
    // Wait for it to go away (bootloader) and come back as a gun.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut seen_gone = false;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let guns = discovery::find_guns()?;
        let present = guns.iter().any(|g| match &usb_path {
            Some(p) => &g.usb_path == p,
            None => g.port.to_string_lossy() == port,
        });
        if !present {
            seen_gone = true;
        } else if seen_gone {
            std::thread::sleep(BOOT_SETTLE);
            return Ok(());
        }
    }
    if seen_gone {
        bail!("gun did not re-enumerate after the bootloader reset");
    }
    bail!("gun never left the bus after the 1200-baud touch; firmware may not honour it")
}

#[cfg(target_os = "linux")]
fn power_cycle_port(port: &str) -> Result<()> {
    use sindenrs::usb;
    let guns = discovery::find_guns()?;
    let dev = guns
        .iter()
        .find(|g| g.port.to_string_lossy() == port)
        .ok_or_else(|| anyhow!("{port} is not a known gun; cannot locate its hub"))?;
    let hp = usb::hub_port_of(&dev.usb_path)?;
    // The recoil circuit holds enough charge that a short outage does not reset the MCU;
    // 2 s was observed to be insufficient, 3 s sufficient.
    usb::power_cycle(&hp, Duration::from_secs(4))?;
    if !usb::wait_for_node(&dev.port, Duration::from_secs(8)) {
        bail!(
            "{} did not come back after the power cycle",
            dev.port.display()
        );
    }
    std::thread::sleep(BOOT_SETTLE);
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn power_cycle_port(_port: &str) -> Result<()> {
    Err(sindenrs::usb::power_cycle_unsupported().into())
}

fn gun(ctx: &Ctx, cmd: GunCmd) -> Result<()> {
    let recover = ctx.cfg.global.auto_recover;
    match cmd {
        GunCmd::Raw {
            port,
            command,
            payload,
            window_ms,
        } => {
            let parse = |t: &str| -> Result<u8> {
                let v = t
                    .strip_prefix("0x")
                    .map_or_else(|| t.parse::<u8>(), |h| u8::from_str_radix(h, 16));
                v.with_context(|| format!("bad byte {t:?}"))
            };
            let port = default_port(port)?;
            let c = parse(&command)?;
            let mut p = [0u8; 4];
            for (i, t) in payload.iter().take(4).enumerate() {
                p[i] = parse(t)?;
            }
            let mut gun = connect(&port, recover)?;
            gun.discard_input()?;
            let t0 = Instant::now();
            gun.send(sindenrs::protocol::Frame::new(c, p))?;
            let mut got = Vec::new();
            let mut first: Option<Duration> = None;
            let mut last = t0;
            while t0.elapsed() < Duration::from_millis(window_ms) {
                let bytes = gun.read_available()?;
                if !bytes.is_empty() {
                    first.get_or_insert(t0.elapsed());
                    last = Instant::now();
                    got.extend(bytes);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
            match first {
                Some(f) => println!(
                    "cmd {c} {p:?}: {} byte(s) {:02x?} first after {}, last after {}",
                    got.len(),
                    got,
                    fmt_ms(f),
                    fmt_ms(last - t0)
                ),
                None => println!("cmd {c} {p:?}: no reply within {window_ms} ms"),
            }
            Ok(())
        }
        GunCmd::Setup {
            port,
            recoil_gap_ms,
        } => {
            let port = default_port(port)?;
            let mut gun = connect(&port, recover)?;
            let id = gun.unique_id().ok();
            let gc = gun_config_for(ctx, &port, id.as_deref());
            let gap = recoil_gap_ms.map_or(ctx.cfg.recoil_gap(), Duration::from_millis);
            let t0 = Instant::now();
            gun.apply_config(&gc, gap)?;
            println!("applied config \"{}\" to {port} in {}: joystick={} offscreen_reload={} recoil={} ({}%, {:?})",
                gc.name, fmt_ms(t0.elapsed()), gc.joystick, gc.offscreen_reload, gc.recoil.enabled, gc.recoil.strength, gc.recoil.mode);
            Ok(())
        }
        GunCmd::Recoil {
            port,
            action,
            count,
            interval_ms,
            recoil_gap_ms,
            strength,
        } => {
            let port = default_port(port)?;
            let mut gun = connect(&port, recover)?;
            let id = gun.unique_id().ok();
            let mut gc = gun_config_for(ctx, &port, id.as_deref());
            if let Some(st) = strength {
                gc.recoil.strength = st;
            }
            let gap = recoil_gap_ms.map_or(ctx.cfg.recoil_gap(), Duration::from_millis);
            match action {
                RecoilAction::Test => {
                    // Enable with the configured strength, pulse, then restore the configured state.
                    let t0 = Instant::now();
                    gun.send_recoil_burst(&gc.recoil, gap)?;
                    gun.set_recoil_enabled(true)?;
                    // Prove the parser is still in sync after the burst: a query must answer.
                    let ((maj, min), t) = gun.firmware_version()?;
                    println!("recoil configured in {} with {:?} gaps; gun still answers (v{maj}.{min} in {})", fmt_ms(t0.elapsed()), gap, fmt_ms(t.complete));
                    std::thread::sleep(Duration::from_millis(50));
                    for i in 0..count {
                        let t0 = Instant::now();
                        gun.fire_recoil()?;
                        println!(
                            "pulse {} sent ({}, strength {})",
                            i + 1,
                            fmt_ms(t0.elapsed()),
                            gc.recoil.strength
                        );
                        std::thread::sleep(Duration::from_millis(interval_ms));
                    }
                    gun.set_recoil_enabled(gc.recoil.enabled)?;
                    Ok(())
                }
                RecoilAction::Auto => {
                    gun.send_recoil_burst(&gc.recoil, gap)?;
                    gun.set_recoil_enabled(true)?;
                    gun.start_auto_recoil()?;
                    println!("automatic recoil started for {} ms", interval_ms);
                    std::thread::sleep(Duration::from_millis(interval_ms));
                    gun.set_recoil_enabled(gc.recoil.enabled)?;
                    Ok(())
                }
                RecoilAction::Off => {
                    gun.set_recoil_enabled(false)?;
                    println!("recoil disabled on the gun");
                    Ok(())
                }
            }
        }
        GunCmd::Reset { port } => {
            let port = default_port(port)?;
            reset_port(&port)?;
            println!("{port} reset and back");
            Ok(())
        }
        GunCmd::PowerCycle { port } => {
            let port = default_port(port)?;
            power_cycle_port(&port)?;
            println!("{port} power-cycled and back");
            Ok(())
        }
        GunCmd::Info { port, no_recover } => {
            let port = default_port(port)?;
            let t0 = Instant::now();
            let mut gun = connect(&port, recover && !no_recover)?;
            let auth = gun.last_auth().unwrap_or_default();
            println!(
                "authenticated in {}: leg1 reply {} | leg2 challenge {} | leg2 verdict {}",
                fmt_ms(t0.elapsed()),
                fmt_ms(auth.leg1.complete),
                fmt_ms(auth.leg2_challenge.complete),
                fmt_ms(auth.leg2_verdict.complete)
            );
            let ((maj, min), t) = gun.firmware_version()?;
            println!(
                "firmware v{maj}.{min}  (reply {} / {})",
                fmt_ms(t.first_byte),
                fmt_ms(t.complete)
            );
            let (name, t) = gun.camera_name()?;
            println!(
                "stored camera name {name:?}  (reply {} / {})",
                fmt_ms(t.first_byte),
                fmt_ms(t.complete)
            );
            let (x, t) = gun.calibration_x()?;
            println!(
                "calibration X {x:+.2}%  (reply {} / {})",
                fmt_ms(t.first_byte),
                fmt_ms(t.complete)
            );
            let (y, t) = gun.calibration_y()?;
            println!(
                "calibration Y {y:+.2}%  (reply {} / {})",
                fmt_ms(t.first_byte),
                fmt_ms(t.complete)
            );
            for (c, label) in [
                (cmd::UNIQUE_ID, "unique id"),
                (cmd::FACTORY_COLOUR, "factory colour"),
                (cmd::MANUFACTURE_DATE, "manufacture date"),
            ] {
                let (bytes, t) = gun.identity(c)?;
                if bytes.is_empty() {
                    println!("{label} (cmd {c}): no reply");
                    continue;
                }
                let rendered = match c {
                    // Digits, one per byte.
                    cmd::UNIQUE_ID => bytes.iter().map(ToString::to_string).collect::<String>(),
                    // Two decimal digits per byte; observed order is dd mm yy hh mm ss.
                    cmd::MANUFACTURE_DATE => bytes
                        .iter()
                        .map(|b| format!("{b:02}"))
                        .collect::<Vec<_>>()
                        .join(" "),
                    _ => String::from_utf8_lossy(&bytes).trim().to_owned(),
                };
                println!(
                    "{label} (cmd {c}): {rendered}  raw={:02x?} (reply {})",
                    bytes,
                    fmt_ms(t.first_byte)
                );
            }
            match gun.joystick_probe() {
                Ok((present, t)) => println!(
                    "joystick hardware: {present} (reply {})",
                    fmt_ms(t.first_byte)
                ),
                Err(e) => println!("joystick probe: {e}"),
            }
            Ok(())
        }
        GunCmd::Monitor {
            port,
            no_recover,
            seconds,
            x,
            y,
            rate,
        } => {
            let port = default_port(port)?;
            let mut gun = connect(&port, recover && !no_recover)?;
            gun.start_streaming(Duration::from_millis(150))?;
            gun.set_offscreen_reload(false)?;
            gun.set_calibration_mode_enabled(false)?;
            gun.set_recoil_enabled(false)?;
            info!("streaming; holding ({x}%, {y}%) at {rate} Hz for {seconds}s — pull the trigger and press buttons");
            let (ax, ay) = (percent_to_axis(x), percent_to_axis(y));
            let period = Duration::from_secs_f64(1.0 / rate);
            let start = Instant::now();
            let mut next = start;
            let mut sent = 0u32;
            let mut n_events = 0u32;
            while start.elapsed().as_secs_f64() < seconds {
                gun.set_position(ax, ay)?;
                sent += 1;
                for ev in gun.poll_events()? {
                    n_events += 1;
                    println!("{:>9.3}s  {:?}", start.elapsed().as_secs_f64(), ev);
                }
                next += period;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
            println!("sent {sent} position reports, received {n_events} events");
            Ok(())
        }
        GunCmd::Sweep {
            port,
            no_recover,
            joystick,
            seconds,
            pattern,
            rate,
        } => {
            let port = default_port(port)?;
            let mut gun = connect(&port, recover && !no_recover)?;
            gun.start_streaming(Duration::from_millis(150))?;
            gun.set_recoil_enabled(false)?;
            gun.set_joystick_mode(joystick)?;
            info!("sweeping the pointer in a {pattern:?} for {seconds}s at {rate} Hz (joystick mode: {joystick})");
            let period = Duration::from_secs_f64(1.0 / rate);
            let start = Instant::now();
            let mut next = start;
            let mut sent = 0u32;
            while start.elapsed().as_secs_f64() < seconds {
                let t = start.elapsed().as_secs_f64();
                let (x, y) = match pattern {
                    Pattern::Circle => {
                        let a = t * std::f64::consts::TAU / 2.0;
                        (50.0 + 30.0 * a.cos(), 50.0 + 30.0 * a.sin())
                    }
                    Pattern::Lissajous => {
                        (50.0 + 40.0 * (t * 1.3).sin(), 50.0 + 40.0 * (t * 2.1).cos())
                    }
                    Pattern::Box => {
                        let phase = (t / 2.0).fract() * 4.0;
                        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                        let side = phase.floor() as u32;
                        let f = phase.fract() * 80.0;
                        match side {
                            0 => (10.0 + f, 10.0),
                            1 => (90.0, 10.0 + f),
                            2 => (90.0 - f, 90.0),
                            _ => (10.0, 90.0 - f),
                        }
                    }
                };
                gun.set_position(percent_to_axis(x), percent_to_axis(y))?;
                sent += 1;
                for ev in gun.poll_events()? {
                    println!("{:>9.3}s  {:?}", t, ev);
                }
                next += period;
                let now = Instant::now();
                if next > now {
                    std::thread::sleep(next - now);
                }
            }
            gun.set_position(percent_to_axis(50.0), percent_to_axis(50.0))?;
            println!("sent {sent} position reports");
            Ok(())
        }
        GunCmd::JoystickDevice { port, action } => {
            let port = default_port(port)?;
            let mut gun = connect(&port, recover)?;
            match action {
                JoystickDeviceAction::Status => {
                    let (present, _) = gun.joystick_probe()?;
                    println!("joystick device enabled: {present}");
                    Ok(())
                }
                JoystickDeviceAction::Enable | JoystickDeviceAction::Disable => {
                    let on = matches!(action, JoystickDeviceAction::Enable);
                    gun.set_joystick_device(on)?;
                    drop(gun);
                    println!(
                        "joystick device {}; resetting the gun so it re-enumerates",
                        if on { "enabled" } else { "disabled" }
                    );
                    reset_port(&port)?;
                    let mut gun = connect(&port, true)?;
                    let (present, _) = gun.joystick_probe()?;
                    println!("joystick device enabled now: {present}");
                    Ok(())
                }
            }
        }
        GunCmd::Firmware { cmd } => firmware(cmd),
        GunCmd::WriteCalibration {
            port,
            no_recover,
            x,
            y,
        } => {
            let port = default_port(port)?;
            let mut gun = connect(&port, recover && !no_recover)?;
            let (bx, _) = gun.calibration_x()?;
            let (by, _) = gun.calibration_y()?;
            println!("before: X {bx:+.2}% Y {by:+.2}%");
            gun.write_calibration(x, y)?;
            std::thread::sleep(Duration::from_millis(200));
            let (ax, _) = gun.calibration_x()?;
            let (ay, _) = gun.calibration_y()?;
            println!("after:  X {ax:+.2}% Y {ay:+.2}%");
            Ok(())
        }
    }
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_lines)]
fn track(ctx: &Ctx, a: TrackArgs) -> Result<()> {
    use sindenrs::camera::v4l2::Device;
    use sindenrs::vision::acquire::{acquire, aim_pixel, AcquireParams};
    use sindenrs::vision::luma;
    use std::io::Write as _;

    let display = &ctx.display;
    let path = default_camera(a.settings.device.clone())?;
    let dev = Device::open(&path).with_context(|| format!("opening {}", path.display()))?;
    let settings = camera_settings_from(display, &a.settings);
    apply_settings(&dev, &settings)?;
    let fmt = dev.get_format()?;
    let (w, h) = (fmt.width as usize, fmt.height as usize);

    let threshold = a.threshold.unwrap_or(display.threshold);
    let min_size = a.min_size.unwrap_or(display.min_size);
    let orientation = a.orientation.unwrap_or(display.orientation);
    let flip = a.flip.map_or(flip_from_config(display.flip), Into::into);
    let mut display = display.clone();
    if let Some(g) = a.gunsight_y {
        display.gunsight_y = g;
    }

    let mut gun = None;
    let (mut cal_x, mut cal_y) = (a.cal_x, a.cal_y);
    if a.send {
        let port = default_port(a.port.clone())?;
        let mut g = connect(&port, ctx.cfg.global.auto_recover)?;
        let id = g.unique_id().ok();
        let mut gun_cfg = gun_config_for(ctx, &port, id.as_deref());
        if a.joystick {
            gun_cfg.joystick = true;
        }
        cal_x = cal_x.or(gun_cfg.calibration_x);
        cal_y = cal_y.or(gun_cfg.calibration_y);
        if cal_x.is_none() {
            cal_x = Some(g.calibration_x()?.0);
        }
        if cal_y.is_none() {
            cal_y = Some(g.calibration_y()?.0);
        }
        g.apply_config(&gun_cfg, ctx.cfg.recoil_gap())?;
        g.start_streaming(Duration::from_millis(150))?;
        info!(
            "gun \"{}\" streaming (joystick={}, recoil={}); bore offset X {:+.2}% Y {:+.2}%",
            gun_cfg.name,
            gun_cfg.joystick,
            gun_cfg.recoil.enabled,
            cal_x.unwrap_or(0.0),
            cal_y.unwrap_or(0.0)
        );
        gun = Some(g);
    }
    let (cal_x, cal_y) = (cal_x.unwrap_or(0.0), cal_y.unwrap_or(0.0));
    let aim_px = aim_pixel(w, h, cal_x, cal_y, orientation);
    let params = AcquireParams {
        threshold,
        min_size,
        ..Default::default()
    };
    info!(
        "tracking {}x{} {}, aim pixel ({:.1}, {:.1}), threshold {}, flip {:?}",
        w, h, fmt.format, aim_px[0], aim_px[1], threshold, flip
    );

    let mut csv = None;
    if let Some(dir) = &a.record {
        std::fs::create_dir_all(dir)?;
        let mut f = std::io::BufWriter::new(std::fs::File::create(dir.join("frames.csv"))?);
        writeln!(
            f,
            "seq,ts_us,age_us,proc_us,found,clipped,tlx,tly,trx,try,brx,bry,blx,bly,aimx,aimy"
        )?;
        csv = Some(f);
    }

    let mut stream = dev.start_stream(a.settings.buffers)?;
    let start = Instant::now();
    let mut n = 0u32;
    let mut found = 0u32;
    let mut proc_times: Vec<Duration> = Vec::new();
    let mut last_report = Instant::now();
    let mut last_aim: Option<[f64; 2]> = None;
    let mut last_quad = None;
    let mut luma_buf = Vec::new();
    while n < a.frames {
        let Some(frame) = stream.next(Some(Duration::from_secs(3)), true)? else {
            warn!("frame timeout");
            break;
        };
        n += 1;
        let t0 = Instant::now();
        let decoded = match fmt.format {
            PixelFormat::Mjpeg => match luma::mjpeg_to_luma(frame.data) {
                Ok((_, _, l)) => Some(l),
                Err(e) => {
                    warn!("jpeg decode failed: {e}");
                    None
                }
            },
            PixelFormat::Yuyv => {
                luma::yuyv_to_luma(frame.data, &mut luma_buf);
                Some(luma_buf.clone())
            }
            PixelFormat::Other(_) => None,
        };
        let Some(mut l) = decoded else { continue };
        if l.len() != w * h {
            warn!("decoded {} bytes, expected {}", l.len(), w * h);
            continue;
        }
        sindenrs::vision::acquire::flip_luma(&mut l, w, flip);
        let quad = acquire(&l, w, h, &params);
        let mut aim = None;
        if let Some(q) = &quad {
            found += 1;
            // A solve that lands far outside the screen means the quad was not the border.
            if let Some(p) = q
                .to_screen()
                .apply(aim_px)
                .filter(|p| (-25.0..=125.0).contains(&p[0]) && (-25.0..=125.0).contains(&p[1]))
            {
                let (x, y) = display.finish_aim(p[0], p[1]);
                aim = Some([x, y]);
                if let Some(g) = gun.as_mut() {
                    g.set_position(percent_to_axis(x), percent_to_axis(y))?;
                }
            }
            last_quad = Some(*q);
        } else if let (Some(g), Some(prev)) = (gun.as_mut(), last_aim) {
            // Stock behaviour: keep the last position when the border is lost.
            g.set_position(percent_to_axis(prev[0]), percent_to_axis(prev[1]))?;
        }
        if aim.is_some() {
            last_aim = aim;
        }
        let proc = t0.elapsed();
        proc_times.push(proc);
        if let Some(g) = gun.as_mut() {
            for ev in g.poll_events()? {
                println!("{:>8.3}s  event {:?}", start.elapsed().as_secs_f64(), ev);
            }
        }
        if let Some(f) = csv.as_mut() {
            let (c, clipped) = quad.map_or(([[f64::NAN; 2]; 4], false), |q| (q.corners, q.clipped));
            let am = aim.unwrap_or([f64::NAN; 2]);
            writeln!(
                f,
                "{},{},{},{},{},{},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.1},{:.2},{:.2}",
                frame.sequence,
                frame.timestamp.map_or(0, |t| t.as_micros()),
                frame.age().map_or(0, |t| t.as_micros()),
                proc.as_micros(),
                u8::from(quad.is_some()),
                u8::from(clipped),
                c[0][0],
                c[0][1],
                c[1][0],
                c[1][1],
                c[2][0],
                c[2][1],
                c[3][0],
                c[3][1],
                am[0],
                am[1]
            )?;
            if let Some(dir) = &a.record {
                std::fs::write(
                    dir.join(format!("frame_{:06}.jpg", frame.sequence)),
                    frame.data,
                )?;
            }
        }
        if a.per_frame || last_report.elapsed() >= Duration::from_secs(1) {
            last_report = Instant::now();
            match (&quad, aim) {
                (Some(q), Some(am)) => println!(
                    "{:>7.2}s seq={:<6} aim=({:6.2}%, {:6.2}%) corners TL({:.0},{:.0}) TR({:.0},{:.0}) BR({:.0},{:.0}) BL({:.0},{:.0}){} blob {}x{} proc={} age={}",
                    start.elapsed().as_secs_f64(), frame.sequence, am[0], am[1],
                    q.corners[0][0], q.corners[0][1], q.corners[1][0], q.corners[1][1],
                    q.corners[2][0], q.corners[2][1], q.corners[3][0], q.corners[3][1],
                    if q.clipped { " CLIPPED" } else { "" }, q.blob.width() * 2, q.blob.height() * 2,
                    fmt_ms(proc), frame.age().map_or("-".into(), fmt_ms)
                ),
                _ => {
                    let (mean, max) = luma::luma_stats(&l);
                    println!("{:>7.2}s seq={:<6} no border (luma mean {:.1} max {}) proc={}", start.elapsed().as_secs_f64(), frame.sequence, mean, max, fmt_ms(proc));
                }
            }
        }
    }
    let wall = start.elapsed();
    let mut pt = proc_times.clone();
    pt.sort_unstable();
    let p95 = pt
        .get(pt.len().saturating_sub(1) * 95 / 100)
        .copied()
        .unwrap_or_default();
    println!(
        "frames={} found={} ({:.0}%) fps={:.1} processing mean={} p95={} max={}",
        n,
        found,
        if n > 0 {
            f64::from(found) * 100.0 / f64::from(n)
        } else {
            0.0
        },
        f64::from(n) / wall.as_secs_f64().max(1e-9),
        fmt_ms(pt.iter().sum::<Duration>() / u32::try_from(pt.len().max(1)).unwrap_or(1)),
        fmt_ms(p95),
        fmt_ms(pt.last().copied().unwrap_or_default())
    );
    if let Some(q) = last_quad {
        println!("last corners: {:?}", q.corners);
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn track(_ctx: &Ctx, _a: TrackArgs) -> Result<()> {
    bail!("tracking needs the camera backend, which is not implemented on this platform yet")
}

/// Enter the bootloader through the 1200-baud touch and open its CDC port.
fn open_bootloader(port: &str) -> Result<sindenrs::firmware::Bootloader> {
    use sindenrs::firmware::{Bootloader, BOOTLOADER_PID, BOOTLOADER_VID};
    // Already in the bootloader? (e.g. a previous attempt was interrupted)
    let existing = discovery::find_ttys_by_ids(BOOTLOADER_VID, BOOTLOADER_PID)?;
    let bl_port = if let Some(p) = existing.first() {
        info!("bootloader already present at {}", p.display());
        p.to_string_lossy().into_owned()
    } else {
        sindenrs::gun::bootloader_touch(port)
            .with_context(|| format!("1200-baud touch on {port}"))?;
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(p) = discovery::find_ttys_by_ids(BOOTLOADER_VID, BOOTLOADER_PID)?.first() {
                break p.to_string_lossy().into_owned();
            }
            if Instant::now() > deadline {
                bail!("bootloader (2341:0036) did not appear after the touch");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    };
    // udev needs a moment to create the node and apply access rules.
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match Bootloader::open(&bl_port) {
            Ok(b) => {
                info!(
                    "bootloader {} v{}.{} at {bl_port}, block size {}",
                    b.software_id, b.version.0, b.version.1, b.buffer_size
                );
                return Ok(b);
            }
            Err(e) if Instant::now() < deadline => {
                debug!("bootloader open not ready yet: {e}");
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(e).with_context(|| format!("opening bootloader at {bl_port}")),
        }
    }
}

fn wait_for_gun_back(port: &str) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        if discovery::find_guns()?
            .iter()
            .any(|g| g.port.to_string_lossy() == port)
        {
            std::thread::sleep(BOOT_SETTLE);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    bail!("gun did not re-enumerate after leaving the bootloader")
}

fn save_image(path: &Path, img: &sindenrs::firmware::Image) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.extension().is_some_and(|e| e == "bin") {
        std::fs::write(path, img.flat(0, sindenrs::firmware::FLASH_SIZE))?;
    } else {
        std::fs::write(path, img.to_intel_hex())?;
    }
    Ok(())
}

fn firmware(cmd: FirmwareCmd) -> Result<()> {
    use sindenrs::firmware::{Image, BOOT_START, FLASH_SIZE, PAGE_SIZE};
    match cmd {
        FirmwareCmd::Info { image } => {
            let img = Image::load(&image)?;
            println!(
                "{}: {} bytes, range {:#06x}..{:#06x}",
                image.display(),
                img.bytes.len(),
                img.bytes.keys().next().copied().unwrap_or(0),
                img.max_addr().unwrap_or(0)
            );
            println!(
                "application section: up to {:#06x} ({} bytes of {:#06x})",
                img.app_max_addr().unwrap_or(0),
                img.app_max_addr().map_or(0, |a| a + 1),
                BOOT_START
            );
            println!(
                "bootloader section present in image: {}",
                img.has_boot_section()
            );
            match img.usb_ids() {
                Some((vid, pid)) => println!(
                    "embedded USB id {vid:04x}:{pid:04x} ({})",
                    sindenrs::ids::GunVariant::from_pid(pid)
                        .map_or("unknown variant", |v| v.name())
                ),
                None => println!("no USB device descriptor found in image"),
            }
            Ok(())
        }
        FirmwareCmd::Backup { port, out } => {
            let port = default_port(port)?;
            let out = out.unwrap_or_else(|| {
                PathBuf::from(format!(
                    "corpus/firmware/backup-{}.hex",
                    chrono_like_stamp()
                ))
            });
            let mut bl = open_bootloader(&port)?;
            let data = bl.read_flash(0, FLASH_SIZE, |done| {
                if done % 4096 == 0 {
                    info!("read {done}/{FLASH_SIZE}")
                }
            })?;
            bl.exit()?;
            let img = Image::from_binary(&data);
            save_image(&out, &img)?;
            println!("saved {} bytes to {}", data.len(), out.display());
            if let Some((vid, pid)) = img.usb_ids() {
                println!("embedded USB id {vid:04x}:{pid:04x}");
            }
            wait_for_gun_back(&port)?;
            println!("gun is back on {port}");
            Ok(())
        }
        FirmwareCmd::Flash {
            port,
            image,
            yes,
            allow_id_mismatch,
        } => {
            let port = default_port(port)?;
            let img = Image::load(&image)?;
            let app_end = img
                .app_max_addr()
                .ok_or_else(|| anyhow!("image has no application data"))?
                + 1;
            let app_end = app_end.div_ceil(PAGE_SIZE) * PAGE_SIZE;
            let guns = discovery::find_guns()?;
            let gun = guns
                .iter()
                .find(|g| g.port.to_string_lossy() == port)
                .ok_or_else(|| anyhow!("{port} is not a known gun"))?;
            if let Some((_, pid)) = img.usb_ids() {
                if pid != gun.pid && !allow_id_mismatch {
                    bail!("image is for product id {pid:04x} but the gun is {:04x}; pass --allow-id-mismatch to change the gun's identity", gun.pid);
                }
            }
            println!(
                "image {}: application {:#06x} bytes ({} pages)",
                image.display(),
                app_end,
                app_end / PAGE_SIZE
            );
            let backup = PathBuf::from(format!(
                "corpus/firmware/backup-{}-before-flash.hex",
                chrono_like_stamp()
            ));
            let mut bl = open_bootloader(&port)?;
            let current = bl.read_flash(0, FLASH_SIZE, |_| {})?;
            save_image(&backup, &Image::from_binary(&current))?;
            println!("backed up current flash to {}", backup.display());
            let boot_same = img.has_boot_section()
                && img.flat(BOOT_START, FLASH_SIZE) == current[BOOT_START as usize..];
            println!(
                "image bootloader section matches the gun's: {} (never written either way)",
                if img.has_boot_section() {
                    boot_same.to_string()
                } else {
                    "n/a".into()
                }
            );
            let wanted = img.flat(0, app_end);
            let differing = wanted
                .iter()
                .zip(&current[..app_end as usize])
                .filter(|(a, b)| a != b)
                .count();
            println!("application bytes differing from current: {differing}");
            if !yes {
                bl.exit()?;
                println!("dry run: pass --yes to write");
                wait_for_gun_back(&port)?;
                return Ok(());
            }
            bl.enter_programming()?;
            info!("erasing application section");
            bl.chip_erase()?;
            info!("writing {} bytes", wanted.len());
            bl.write_flash(0, &wanted, |done| {
                if done % 8192 == 0 {
                    info!("wrote {done}/{}", wanted.len())
                }
            })?;
            info!("verifying");
            let back = bl.read_flash(0, app_end, |_| {})?;
            for (i, (w, r)) in wanted.iter().zip(&back).enumerate() {
                if w != r {
                    bl.leave_programming()?;
                    bail!(sindenrs::firmware::FirmwareError::Verify {
                        addr: u32::try_from(i).unwrap_or(u32::MAX),
                        expected: *w,
                        got: *r
                    });
                }
            }
            bl.leave_programming()?;
            bl.exit()?;
            println!("flashed and verified {} bytes", wanted.len());
            wait_for_gun_back(&port)?;
            let mut g = connect(&port, true)?;
            let ((maj, min), _) = g.firmware_version()?;
            println!("gun reports firmware v{maj}.{min}");
            Ok(())
        }
    }
}

/// A sortable timestamp without pulling in a date crate.
fn chrono_like_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs());
    format!("{secs}")
}
