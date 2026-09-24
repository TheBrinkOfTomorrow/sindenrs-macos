//! sindenrs command-line tool: list hardware, calibrate, run the driver, talk to the gun.

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
#[command(name = "sindenrs", version, about = "Sinden Lightgun driver")]
struct Cli {
    /// Which gun, when more than one is attached: its name or id from the config, or its
    /// serial port (e.g. /dev/ttyACM1). `sindenrs list` shows them.
    #[arg(long, short = 'g', global = true)]
    gun: Option<String>,
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
    /// List attached guns and cameras, with each gun's id and config entry.
    List {
        /// Skip talking to the guns (no ids, no config matching).
        #[arg(long)]
        no_connect: bool,
    },
    /// Run the driver until Ctrl-C: track every attached gun, picking up guns as they are
    /// plugged in and letting go of ones that are unplugged. What the service runs.
    Run {
        /// Draw the border on screen while running (config: display.overlay).
        #[arg(long, overrides_with = "no_overlay")]
        overlay: bool,
        /// Do not draw the border; something else (MAME artwork) draws it.
        #[arg(long)]
        no_overlay: bool,
    },
    /// Measure and save the gun's aim calibration: shoot each target the overlay lights up.
    Calibrate(Box<CalibrateArgs>),
    /// Draw the tracking border on screen (Ctrl-C stops), or export it as MAME artwork.
    Border(BorderArgs),
    /// Configuration file management.
    Config {
        #[command(subcommand)]
        cmd: ConfigCmd,
    },
    /// Gun maintenance: reset, recoil, firmware, joystick device.
    Gun {
        #[command(subcommand)]
        cmd: GunCmd,
    },
    /// Development and diagnostic tools.
    Debug {
        #[command(subcommand)]
        cmd: DebugCmd,
    },
}

#[derive(Args)]
struct BorderArgs {
    /// Border thickness as a percentage of the shorter screen dimension (config:
    /// display.border_thickness).
    #[arg(long)]
    thickness: Option<f64>,
    #[command(subcommand)]
    cmd: Option<BorderCmd>,
}

#[derive(Subcommand)]
enum BorderCmd {
    /// Write the border as MAME artwork (<dir>/<name>/default.lay + border.png). Point
    /// MAME's artpath at <dir> and launch gun games with `-override_artwork <name>`;
    /// keep artwork_crop off. Then set display.overlay = false so `run` draws nothing.
    Export {
        /// Directory to write the artwork folder into.
        #[arg(long, default_value = "artwork")]
        dir: PathBuf,
        /// Artwork folder name.
        #[arg(long, default_value = "sinden-border")]
        name: String,
        /// Image size; use the screen's resolution.
        #[arg(long, default_value = "1280x960")]
        resolution: String,
    },
}

#[derive(Subcommand)]
enum DebugCmd {
    /// Live border tracking: camera -> corners -> aim point, optionally driving the gun.
    Track(Box<TrackArgs>),
    /// Run border detection over recorded frames (from `debug track --record`) and report
    /// how each frame was solved; optionally fit the lens distortion coefficient to them.
    Replay(ReplayArgs),
    /// Camera capture and measurement.
    Camera {
        #[command(subcommand)]
        cmd: CameraCmd,
    },
    /// Authenticate and read everything the gun will tell us, with reply timings.
    GunInfo,
    /// Power-cycle the gun's USB port on its internal hub (re-enumerates it; does not reset
    /// the microcontroller on this hardware, so prefer `gun reset`).
    PowerCycle,
    /// Authenticate, start streaming, hold a fixed position, and log every event byte.
    Monitor {
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
    /// Send one raw frame (command byte and up to four payload bytes) and dump whatever the
    /// gun replies within a window. For protocol exploration.
    Raw {
        /// Command byte, decimal or 0x-hex.
        command: String,
        /// Payload bytes p1..p4 (missing ones are zero).
        payload: Vec<String>,
        /// How long to listen for a reply, in ms.
        #[arg(long, default_value_t = 300)]
        window_ms: u64,
    },
    /// Send several raw frames in one session: each argument is `cmd[:p1[:p2[:p3[:p4]]]]`,
    /// decimal or 0x-hex; `sleep:<ms>` pauses. Replies are dumped as they arrive.
    RawSeq {
        frames: Vec<String>,
        /// Pause after each frame, in ms.
        #[arg(long, default_value_t = 20)]
        gap_ms: u64,
    },
    /// Send the full startup configuration (modes, button map, recoil) to the gun without
    /// tracking. `run` and `calibrate` do this themselves.
    Setup {
        /// Override global.recoil_gap_ms for this run.
        #[arg(long)]
        recoil_gap_ms: Option<u64>,
    },
    /// Write bore calibration offsets (percent of frame) to the gun's EEPROM by hand.
    /// `calibrate` measures and saves them for you.
    WriteCalibration {
        #[arg(long)]
        x: f64,
        #[arg(long)]
        y: f64,
    },
}

#[derive(Args)]
struct ReplayArgs {
    /// Directories of recorded frames (.jpg from Linux, .pgm from macOS).
    #[arg(required = true)]
    dirs: Vec<PathBuf>,
    /// Use every Nth frame.
    #[arg(long, default_value_t = 1)]
    every: usize,
    /// Luma threshold (config: display.threshold).
    #[arg(long)]
    threshold: Option<u8>,
    /// Lens distortion coefficient to detect with (config: global.lens_k1).
    #[arg(long, allow_hyphen_values = true)]
    lens_k1: Option<f64>,
    /// Search for the lens coefficient that straightens the border edges, and report it.
    #[arg(long)]
    fit_lens: bool,
    /// Print one line per frame.
    #[arg(long)]
    per_frame: bool,
    /// With --per-frame: also print every fitted edge line (side, support, residual).
    #[arg(long)]
    lines: bool,
    /// Skip the sub-pixel edge refit (A/B against the mask-only lines).
    #[arg(long)]
    no_subpixel_lines: bool,
    /// Skip the luma re-measurement of tab widths.
    #[arg(long)]
    no_subpixel_tabs: bool,
}

#[derive(Subcommand)]
enum ConfigCmd {
    /// Write a commented default config file.
    Init {
        #[arg(long)]
        force: bool,
    },
    /// Print the effective configuration (defaults merged with the file and --profile).
    Show {
        /// Print every key with its built-in default instead.
        #[arg(long)]
        defaults: bool,
    },
    /// Print the config file path in use.
    Path,
}

/// Loaded once in main and handed to commands.
struct Ctx {
    path: PathBuf,
    cfg: sindenrs::config::Config,
    display: sindenrs::config::Display,
    /// The `--gun` selector, if any.
    gun: Option<String>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
enum CaptureMode {
    /// Hold the aim steady on the target.
    Dwell,
    /// Pull the trigger.
    Trigger,
    /// Whichever happens first.
    Either,
}

#[derive(Args)]
struct CalibrateArgs {
    /// Grid size: 3, 4 or 5 targets per side.
    #[arg(long, default_value_t = 3)]
    grid: u32,
    /// How to capture a target: pull the trigger (the real gesture), hold steady on it, or
    /// whichever comes first.
    #[arg(long, value_enum, default_value = "trigger")]
    capture: CaptureMode,
    /// Save the result to the gun without asking.
    #[arg(long, overrides_with = "no_save")]
    save: bool,
    /// Only measure and report; never save.
    #[arg(long)]
    no_save: bool,
    /// Write the measurements here as CSV.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Save every frame (.jpg) and a frames.csv here while the test runs, for `debug replay`.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Where to save the camera frame behind every shot. Defaults to a timestamped
    /// directory under the cache dir; pass `--debug-dir ""` to turn it off.
    #[arg(long)]
    debug_dir: Option<PathBuf>,
    /// Aim points to average per target.
    #[arg(long, default_value_t = 24, hide = true)]
    samples: u32,
    /// Hold-steady tolerance in percent of screen; smaller demands a steadier hand.
    #[arg(long, default_value_t = 1.2, hide = true)]
    dwell_radius: f64,
    /// How far the aim must move off a captured point before the next can be captured.
    #[arg(long, default_value_t = 4.0, hide = true)]
    rearm_distance: f64,
    /// Reject a capture whose samples disagree by more than this, in percent of screen.
    #[arg(long, default_value_t = 4.0, hide = true)]
    steady_tolerance: f64,
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
    /// Lens distortion coefficient (config: global.lens_k1).
    #[arg(long, allow_hyphen_values = true)]
    lens_k1: Option<f64>,
    /// Save every frame (.jpg) and a frames.csv of results here, for regression replay.
    #[arg(long)]
    record: Option<PathBuf>,
    /// Stop after this many frames.
    #[arg(long, default_value_t = 600)]
    frames: u32,
    /// Print one line per frame.
    #[arg(long)]
    per_frame: bool,
    /// Show a full-screen page with the border, aiming targets, the camera feed and what the
    /// detector made of it, and log every click against the targets (macOS). Runs until Esc;
    /// --frames does not apply.
    #[arg(long)]
    preview: bool,
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
        /// Output path; default <data dir>/firmware/backup-<stamp>.hex
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Flash an image's application section, after backing up and with read-back verify.
    Flash {
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

#[derive(Subcommand)]
enum GunCmd {
    /// Reset a gun that stopped answering (via its bootloader; needs only the serial port).
    Reset,
    /// Fire recoil on demand (test), run automatic recoil, or switch recoil off.
    Recoil {
        #[arg(value_enum)]
        action: RecoilAction,
        /// Number of test pulses.
        #[arg(long, default_value_t = 3)]
        count: u32,
        /// Pause between test pulses, or duration of automatic recoil, in ms.
        #[arg(long, default_value_t = 500)]
        interval_ms: u64,
        /// Override the configured strength (0-100) for this run.
        #[arg(long)]
        strength: Option<u8>,
        /// A/B: fire `count` pulses at each of these strengths in turn, one session, with a
        /// pause between groups (e.g. 30,100).
        #[arg(long, value_delimiter = ',', hide = true)]
        strengths: Vec<u8>,
    },
    /// Enable, disable or query the gun's joystick HID device (firmware 1.9+, persistent;
    /// the gun is reset afterwards so the change takes effect).
    JoystickDevice {
        #[arg(value_enum)]
        action: JoystickDeviceAction,
    },
    /// Firmware: inspect an image, back up the gun's flash, or flash a new image.
    Firmware {
        #[command(subcommand)]
        cmd: FirmwareCmd,
    },
}

/// Every gun-touching command, user-facing or debug, dispatched by [`gun`].
enum AnyGunCmd {
    User(GunCmd),
    Info,
    PowerCycle,
    Monitor {
        seconds: f64,
        x: f64,
        y: f64,
        rate: f64,
    },
    Raw {
        command: String,
        payload: Vec<String>,
        window_ms: u64,
    },
    RawSeq {
        frames: Vec<String>,
        gap_ms: u64,
    },
    Setup {
        recoil_gap_ms: Option<u64>,
    },
    WriteCalibration {
        x: f64,
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
    let ctx = Ctx {
        path,
        cfg,
        display,
        gun: cli.gun.clone(),
    };
    if ctx.path.exists() {
        debug!(path = %ctx.path.display(), "config loaded");
    }

    match cli.cmd {
        Cmd::List { no_connect } => list(&ctx, no_connect),
        Cmd::Config { cmd } => config_cmd(&ctx, cmd),
        Cmd::Run {
            overlay,
            no_overlay,
        } => {
            let overlay = if overlay {
                true
            } else if no_overlay {
                false
            } else {
                ctx.display.overlay
            };
            run_all(&ctx, overlay)
        }
        Cmd::Border(a) => {
            use std::sync::atomic::{AtomicBool, Ordering};
            use std::sync::{Arc, Mutex};
            let frac = a.thickness.unwrap_or(ctx.display.border_thickness) / 100.0;
            if let Some(BorderCmd::Export {
                dir,
                name,
                resolution,
            }) = a.cmd
            {
                let (w, h) = resolution
                    .split_once('x')
                    .and_then(|(w, h)| Some((w.parse::<u32>().ok()?, h.parse::<u32>().ok()?)))
                    .ok_or_else(|| anyhow!("--resolution wants WIDTHxHEIGHT, e.g. 1280x960"))?;
                sindenrs::overlay::artwork::export(&dir, &name, w, h, frac)?;
                println!("wrote {}", dir.join(&name).display());
                return Ok(());
            }
            let scene = Arc::new(Mutex::new(sindenrs::overlay::Scene::border_only(frac)));
            let stop = Arc::new(AtomicBool::new(false));
            {
                let stop = stop.clone();
                ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
                    .context("installing Ctrl-C handler")?;
            }
            sindenrs::overlay::run(scene, stop)
        }
        Cmd::Calibrate(a) => calibrate(&ctx, *a),
        Cmd::Gun { cmd } => gun(&ctx, AnyGunCmd::User(cmd)),
        Cmd::Debug { cmd } => match cmd {
            DebugCmd::Track(args) => track(&ctx, *args),
            DebugCmd::Replay(a) => replay(&ctx, &a),
            DebugCmd::Camera { cmd } => camera(cmd),
            DebugCmd::GunInfo => gun(&ctx, AnyGunCmd::Info),
            DebugCmd::PowerCycle => gun(&ctx, AnyGunCmd::PowerCycle),
            DebugCmd::Monitor {
                seconds,
                x,
                y,
                rate,
            } => gun(
                &ctx,
                AnyGunCmd::Monitor {
                    seconds,
                    x,
                    y,
                    rate,
                },
            ),
            DebugCmd::Raw {
                command,
                payload,
                window_ms,
            } => gun(
                &ctx,
                AnyGunCmd::Raw {
                    command,
                    payload,
                    window_ms,
                },
            ),
            DebugCmd::RawSeq { frames, gap_ms } => gun(&ctx, AnyGunCmd::RawSeq { frames, gap_ms }),
            DebugCmd::Setup { recoil_gap_ms } => gun(&ctx, AnyGunCmd::Setup { recoil_gap_ms }),
            DebugCmd::WriteCalibration { x, y } => gun(&ctx, AnyGunCmd::WriteCalibration { x, y }),
        },
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
            // One [guns.<id>] table per attached gun, keyed by unique id, so the file
            // survives port renumbering and two guns of the same colour.
            let guns = discovery::find_guns()?;
            let mut cfg = sindenrs::config::Config::default();
            for (i, g) in guns.iter().enumerate() {
                let port = g.port.to_string_lossy().into_owned();
                if let Ok(mut gun) = connect(&port, ctx.cfg.global.auto_recover) {
                    if let Ok(id) = gun.unique_id() {
                        let mut t = toml::Table::new();
                        t.insert(
                            "name".into(),
                            toml::Value::String(format!("player{}", i + 1)),
                        );
                        cfg.guns.insert(id, t);
                    }
                }
            }
            let n_guns = cfg.guns.len();
            let text = format!("{}{}", sindenrs::config::example_header(), cfg.to_toml());
            std::fs::write(&ctx.path, text)?;
            println!(
                "wrote {} ({} gun{} detected)",
                ctx.path.display(),
                n_guns,
                if n_guns == 1 { "" } else { "s" }
            );
            Ok(())
        }
        ConfigCmd::Show { defaults } => {
            let mut c = if defaults {
                sindenrs::config::Config::default()
            } else {
                ctx.cfg.clone()
            };
            if !defaults {
                c.display = ctx.display.clone();
            }
            print!("{}", c.to_toml_full());
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

/// The settings for an attached gun: `[gun]` plus its `[guns.<id>]` overrides.
fn gun_config_for(cfg: &sindenrs::config::Config, id: Option<&str>) -> sindenrs::config::GunConfig {
    cfg.gun_for(id).unwrap_or_else(|e| {
        warn!("{e}; using the [gun] defaults");
        cfg.gun_for(None).unwrap_or_default()
    })
}

fn list(ctx: &Ctx, no_connect: bool) -> Result<()> {
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
        if !no_connect {
            let port = g.port.to_string_lossy().into_owned();
            match connect(&port, ctx.cfg.global.auto_recover) {
                Ok(mut gun) => {
                    let id = gun.unique_id().unwrap_or_default();
                    let fw = gun
                        .firmware_version()
                        .map(|((a, b), _)| format!("v{a}.{b}"))
                        .unwrap_or_default();
                    let gc = gun_config_for(&ctx.cfg, Some(&id));
                    println!(
                        "        id: {id}  firmware {fw}  config: {}",
                        if ctx.cfg.guns.contains_key(&id) {
                            format!("[guns.\"{id}\"] \"{}\"", gc.name)
                        } else {
                            "[gun] defaults (no [guns] table for this id)".to_owned()
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

/// The serial port of the gun this invocation is about.
///
/// `--gun` may be a serial port, a config name or a unique id; a name or id means talking to
/// each attached gun until one answers with that id. With no selector, one attached gun is
/// simply it, and more than one is an error rather than a silent guess.
fn select_port(ctx: &Ctx) -> Result<String> {
    let guns = discovery::find_guns()?;
    if let Some(sel) = ctx.gun.as_deref() {
        if let Some(g) = guns.iter().find(|g| g.port.to_string_lossy() == sel) {
            return Ok(g.port.to_string_lossy().into_owned());
        }
        if sel.starts_with('/') {
            bail!("{sel} is not an attached gun's serial port (see `sindenrs list`)");
        }
        let want_id = ctx
            .cfg
            .guns
            .iter()
            .find(|(id, t)| {
                id.as_str() == sel || t.get("name").and_then(toml::Value::as_str) == Some(sel)
            })
            .map_or(sel, |(id, _)| id.as_str());
        for g in &guns {
            let port = g.port.to_string_lossy().into_owned();
            let Ok(mut gun) = connect(&port, ctx.cfg.global.auto_recover) else {
                continue;
            };
            if gun.unique_id().ok().as_deref() == Some(want_id) {
                return Ok(port);
            }
        }
        bail!("no attached gun is {sel:?} (see `sindenrs list`)");
    }
    match guns.as_slice() {
        [] => bail!("no gun attached"),
        [g] => Ok(g.port.to_string_lossy().into_owned()),
        many => bail!(
            "{} guns attached; say which with --gun <name|id|port> (see `sindenrs list`)",
            many.len()
        ),
    }
}

/// The capture node of the camera inside the gun on `port`, else the first Sinden camera.
fn camera_for_port(port: &str) -> Result<PathBuf> {
    let guns = discovery::find_guns()?;
    let cams = discovery::find_cameras()?;
    if let Some(c) = guns
        .iter()
        .find(|g| g.port.to_string_lossy() == port)
        .and_then(|g| g.sibling_camera(&cams))
    {
        return Ok(c.node.clone());
    }
    default_camera(None)
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

/// macOS: AVFoundation delivers decoded luma and the UVC controls go over IOKit, so `info` lists
/// controls and `capture` applies them; format, frame rate and buffer options do not apply.
#[cfg(target_os = "macos")]
fn camera(cmd: CameraCmd) -> Result<()> {
    use sindenrs::camera::avfoundation::Device;
    use sindenrs::camera::uvc::{self, iokit::Controls};
    use sindenrs::vision::luma;

    let open_controls = |id: &Path| {
        Controls::open(id).with_context(|| format!("opening camera controls of {}", id.display()))
    };
    let (settings, frames, out, save_every, no_drain, stall_ms, per_frame) = match cmd {
        CameraCmd::Info { device } => {
            let id = default_camera(device)?;
            let ctl = open_controls(&id)?;
            let t = ctl.topology();
            println!(
                "{}: VideoControl interface {}, camera terminal {} (controls {:#x}), processing unit {} (controls {:#x})",
                id.display(),
                t.interface,
                t.camera_terminal,
                t.ct_controls,
                t.processing_unit,
                t.pu_controls
            );
            println!("controls (raw UVC values; exposure_auto shown as the V4L2 menu value):");
            for (cid, name) in uvc::CONTROLS {
                // On/off controls (the automatic modes) have no range in UVC.
                match (ctl.get_control(cid), ctl.range(cid)) {
                    (Err(e), _) if e.kind() == std::io::ErrorKind::Unsupported => {}
                    (Err(e), _) => println!("  {name:<28} {cid:#010x} error: {e}"),
                    (Ok(v), Ok((min, max, step, def))) => println!(
                        "  {name:<28} {cid:#010x} min={min} max={max} step={step} default={def} value={v}"
                    ),
                    (Ok(v), Err(_)) => println!("  {name:<28} {cid:#010x} value={v}"),
                }
            }
            return Ok(());
        }
        CameraCmd::Capture {
            settings,
            frames,
            out,
            save_every,
            no_drain,
            stall_ms,
            per_frame,
        } => (
            settings, frames, out, save_every, no_drain, stall_ms, per_frame,
        ),
        CameraCmd::SweepExposure { .. } => {
            bail!("`debug camera sweep-exposure` is not available on macOS yet")
        }
    };
    let id = default_camera(settings.device.clone())?;
    let ctl = open_controls(&id)?;
    match settings.exposure.as_deref() {
        None => {}
        Some("auto" | "A" | "a") => ctl.set_auto_exposure()?,
        Some(v) => {
            let v: i32 = v.parse().with_context(|| format!("bad --exposure {v:?}"))?;
            ctl.set_manual_exposure(v)?;
        }
    }
    for (cid, val, name) in [
        (cid::BRIGHTNESS, settings.brightness, "brightness"),
        (cid::CONTRAST, settings.contrast, "contrast"),
        (cid::GAIN, settings.gain, "gain"),
        (cid::GAMMA, settings.gamma, "gamma"),
        (cid::SHARPNESS, settings.sharpness, "sharpness"),
    ] {
        if let Some(v) = val {
            ctl.set_control(cid, v)
                .with_context(|| format!("setting {name}={v}"))?;
        }
    }
    if let (Ok(auto), Ok(exp)) = (
        ctl.get_control(cid::EXPOSURE_AUTO),
        ctl.get_control(cid::EXPOSURE_ABSOLUTE),
    ) {
        info!(
            "exposure: auto={auto} absolute={exp} (x100 µs = {:.1} ms)",
            f64::from(exp) / 10.0
        );
    }
    let dev = Device::open(&id).with_context(|| format!("opening camera {}", id.display()))?;
    let mut stream = dev.start_stream(settings.width, settings.height)?;
    let (w, h) = (stream.width(), stream.height());
    if let Some(dir) = &out {
        std::fs::create_dir_all(dir)?;
    }
    // Time from the first frame, so session start-up does not count against the rate.
    let mut start = Instant::now();
    let (mut n, mut skipped, mut age_sum, mut age_max) =
        (0u32, 0u64, Duration::ZERO, Duration::ZERO);
    let mut luma_sum = 0.0;
    while n < frames {
        let Some(f) = stream.next(Some(Duration::from_secs(3)), !no_drain)? else {
            warn!("frame timeout");
            continue;
        };
        n += 1;
        if n == 1 {
            start = Instant::now();
        }
        skipped += u64::from(f.dropped);
        let age = f.age().unwrap_or_default();
        age_sum += age;
        age_max = age_max.max(age);
        #[allow(clippy::cast_precision_loss)]
        let mean = f.data.iter().map(|&v| u64::from(v)).sum::<u64>() as f64 / f.data.len() as f64;
        luma_sum += mean;
        if per_frame {
            println!(
                "seq={:<6} age={} skipped={} luma={mean:.1}",
                f.sequence,
                fmt_ms(age),
                f.dropped
            );
        }
        if let Some(dir) = &out {
            if save_every > 0 && n % save_every == 0 {
                #[allow(clippy::cast_possible_truncation)]
                luma::write_pgm(
                    &dir.join(format!("frame_{:06}.pgm", f.sequence)),
                    w as u32,
                    h as u32,
                    f.data,
                )?;
            }
        }
        if stall_ms > 0 {
            std::thread::sleep(Duration::from_millis(stall_ms));
        }
    }
    let wall = start.elapsed();
    println!(
        "{n} frames {w}x{h} in {:.2}s = {:.2} fps (camera set to {:.1}); age mean {} max {}; \
         skipped by drain {skipped}, dropped in capture {}; luma mean {:.1}",
        wall.as_secs_f64(),
        f64::from(n.saturating_sub(1)) / wall.as_secs_f64(),
        stream.fps(),
        fmt_ms(age_sum / n.max(1)),
        fmt_ms(age_max),
        stream.dropped(),
        luma_sum / f64::from(n.max(1)),
    );
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

fn gun(ctx: &Ctx, cmd: AnyGunCmd) -> Result<()> {
    let recover = ctx.cfg.global.auto_recover;
    let port = select_port(ctx)?;
    match cmd {
        AnyGunCmd::RawSeq { frames, gap_ms } => {
            let parse = |t: &str| -> Result<u8> {
                let v = t
                    .strip_prefix("0x")
                    .map_or_else(|| t.parse::<u8>(), |h| u8::from_str_radix(h, 16));
                v.with_context(|| format!("bad byte {t:?}"))
            };
            let mut gun = connect(&port, recover)?;
            gun.discard_input()?;
            let t0 = Instant::now();
            for spec in &frames {
                if let Some(ms) = spec.strip_prefix("sleep:") {
                    std::thread::sleep(Duration::from_millis(
                        ms.parse().with_context(|| format!("bad sleep {ms:?}"))?,
                    ));
                    continue;
                }
                let parts: Vec<&str> = spec.split(':').collect();
                let c = parse(parts[0])?;
                let mut p = [0u8; 4];
                for (i, t) in parts.iter().skip(1).take(4).enumerate() {
                    p[i] = parse(t)?;
                }
                gun.send(sindenrs::protocol::Frame::new(c, p))?;
                std::thread::sleep(Duration::from_millis(gap_ms));
                let reply = gun.read_available()?;
                println!(
                    "{:>8.3}s cmd {c} {p:?}{}",
                    t0.elapsed().as_secs_f64(),
                    if reply.is_empty() {
                        String::new()
                    } else {
                        format!(" -> {reply:02x?}")
                    }
                );
            }
            Ok(())
        }
        AnyGunCmd::Raw {
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
        AnyGunCmd::Setup { recoil_gap_ms } => {
            let mut gun = connect(&port, recover)?;
            let id = gun.unique_id().ok();
            let gc = gun_config_for(&ctx.cfg, id.as_deref());
            let gap = recoil_gap_ms.map_or(ctx.cfg.recoil_gap(), Duration::from_millis);
            let t0 = Instant::now();
            gun.apply_config(&gc, gap)?;
            println!("applied config \"{}\" to {port} in {}: joystick={} offscreen_reload={} recoil={} ({}%, {:?})",
                gc.name, fmt_ms(t0.elapsed()), gc.joystick, gc.offscreen_reload, gc.recoil.enabled, gc.recoil.strength, gc.recoil.mode);
            Ok(())
        }
        AnyGunCmd::User(GunCmd::Recoil {
            action,
            count,
            interval_ms,
            strength,
            strengths,
        }) => {
            let mut gun = connect(&port, recover)?;
            let id = gun.unique_id().ok();
            let mut gc = gun_config_for(&ctx.cfg, id.as_deref());
            if let Some(st) = strength {
                gc.recoil.strength = st;
            }
            let gap = ctx.cfg.recoil_gap();
            match action {
                RecoilAction::Test if !strengths.is_empty() => {
                    let t_all = Instant::now();
                    for (gi, st) in strengths.iter().enumerate() {
                        let mut r = gc.recoil.clone();
                        r.strength = *st;
                        gun.send_recoil_burst(&r, gap)?;
                        gun.set_recoil_enabled(true)?;
                        std::thread::sleep(Duration::from_millis(50));
                        for i in 0..count {
                            gun.fire_recoil()?;
                            println!(
                                "{:>8.3}s group {} strength {st} (wire {}): pulse {}",
                                t_all.elapsed().as_secs_f64(),
                                gi + 1,
                                r.wire_level(),
                                i + 1
                            );
                            std::thread::sleep(Duration::from_millis(interval_ms));
                        }
                        std::thread::sleep(Duration::from_millis(1500));
                    }
                    gun.set_recoil_enabled(gc.recoil.enabled)?;
                    Ok(())
                }
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
        AnyGunCmd::User(GunCmd::Reset) => {
            reset_port(&port)?;
            println!("{port} reset and back");
            Ok(())
        }
        AnyGunCmd::PowerCycle => {
            power_cycle_port(&port)?;
            println!("{port} power-cycled and back");
            Ok(())
        }
        AnyGunCmd::Info => {
            let t0 = Instant::now();
            let mut gun = connect(&port, recover)?;
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
        AnyGunCmd::Monitor {
            seconds,
            x,
            y,
            rate,
        } => {
            let mut gun = connect(&port, recover)?;
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
        AnyGunCmd::User(GunCmd::JoystickDevice { action }) => {
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
        AnyGunCmd::User(GunCmd::Firmware { cmd }) => firmware(&port, cmd),
        AnyGunCmd::WriteCalibration { x, y } => {
            let mut gun = connect(&port, recover)?;
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn track(ctx: &Ctx, a: TrackArgs) -> Result<()> {
    use sindenrs::runtime::{run_tracker, Status, TrackerOptions};
    use std::sync::atomic::AtomicBool;
    use std::sync::{Arc, Mutex};

    let camera = default_camera(a.settings.device.clone())?;
    let mut gun = None;
    let mut name = "track".to_owned();
    if a.send {
        let port = select_port(ctx)?;
        let mut g = connect(&port, ctx.cfg.global.auto_recover)?;
        let id = g.unique_id().ok();
        let mut gc = gun_config_for(&ctx.cfg, id.as_deref());
        if a.joystick {
            gc.joystick = true;
        }
        sindenrs::runtime::prepare_gun(&mut g, &gc, ctx.cfg.recoil_gap())?;
        info!(
            "gun \"{}\" streaming (joystick={}, recoil={})",
            gc.name, gc.joystick, gc.recoil.enabled
        );
        name.clone_from(&gc.name);
        gun = Some((g, gc));
    }
    let mut display = ctx.display.clone();
    if let Some(g) = a.gunsight_y {
        display.gunsight_y = g;
    }
    // Camera flags on the CLI override the display profile.
    let cs = camera_settings_from(&display, &a.settings);
    if let Some(e) = &cs.exposure {
        display.exposure = if e == "auto" {
            sindenrs::config::Exposure::Auto(sindenrs::config::AutoWord::Auto)
        } else {
            sindenrs::config::Exposure::Manual(
                e.parse().with_context(|| format!("bad --exposure {e:?}"))?,
            )
        };
    }
    display.brightness = cs.brightness;
    display.contrast = cs.contrast;
    display.gain = cs.gain;
    display.gamma = cs.gamma;
    display.sharpness = cs.sharpness;
    display.fps = cs.fps;
    let opts = TrackerOptions {
        display,
        threshold: a.threshold,
        min_size: a.min_size,
        flip: a.flip.map(Into::into),
        orientation: a.orientation,
        cal_x: a.cal_x,
        cal_y: a.cal_y,
        lens_k1: a.lens_k1.unwrap_or(ctx.cfg.global.lens_k1),
        jump_limit: ctx.display.jump_limit,
        hover_smoothing: ctx.display.hover_smoothing,
        buffers: a.settings.buffers,
        frames: a.frames,
        record: a.record.clone(),
        per_frame: a.per_frame,
        report_every: Some(Duration::from_secs(1)),
    };
    let status = Arc::new(Mutex::new(Status::default()));
    if a.preview {
        let border = ctx.display.border_thickness / 100.0;
        track_with_preview(name, camera, gun, opts, status.clone(), border)?;
    } else {
        let stop = AtomicBool::new(false);
        run_tracker(&name, &camera, gun, &opts, &stop, &status)?;
    }
    let s = status.lock().map(|s| s.clone()).unwrap_or_default();
    println!(
        "frames={} found={} ({:.0}%) processing mean={} age mean={} events={}",
        s.frames,
        s.found,
        if s.frames > 0 {
            s.found as f64 * 100.0 / s.frames as f64
        } else {
            0.0
        },
        fmt_ms(s.proc_mean),
        fmt_ms(s.age_mean),
        s.events
    );
    if let Some(q) = s.last_quad {
        println!("last corners: {q:?}");
    }
    Ok(())
}

/// `debug track --preview`: the tracker on a worker thread feeding the preview page, which
/// owns the main thread until Esc (or the tracker stops).
#[cfg(target_os = "macos")]
fn track_with_preview(
    name: String,
    camera: PathBuf,
    gun: Option<(Gun, sindenrs::config::GunConfig)>,
    mut opts: sindenrs::runtime::TrackerOptions,
    status: std::sync::Arc<std::sync::Mutex<sindenrs::runtime::Status>>,
    border_frac: f64,
) -> Result<()> {
    use sindenrs::preview::{self, Shared};
    use sindenrs::protocol::event::Event;
    use sindenrs::runtime::{run_tracker_with, Flow, Sample};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    opts.frames = 0;
    let shared = Arc::new(Mutex::new(Shared::default()));
    let stop = Arc::new(AtomicBool::new(false));
    let worker = {
        let (shared, stop) = (shared.clone(), stop.clone());
        std::thread::spawn(move || {
            let (mut n, mut window, mut fps) = (0u64, (Instant::now(), 0u32), 0.0);
            let mut hook = |s: &Sample| -> Flow {
                n += 1;
                window.1 += 1;
                if window.0.elapsed() >= Duration::from_secs(1) {
                    fps = f64::from(window.1) / window.0.elapsed().as_secs_f64();
                    window = (Instant::now(), 0);
                }
                let Ok(mut sh) = shared.lock() else {
                    return Flow::Continue;
                };
                sh.aim = s.aim;
                for ev in s.events {
                    if let Event::Buttons { state1, state2, .. } = ev {
                        sh.push_log(format!("gun buttons s1={state1:08b} s2={state2:08b}"));
                    }
                }
                // Previews at half the frame rate are plenty to watch.
                if n % 2 == 0 {
                    let (camera, processed) = preview::images_for(s);
                    sh.camera = camera;
                    sh.processed = processed;
                    sh.serial += 1;
                }
                let solve = s.quad.map_or_else(
                    || "no border".to_owned(),
                    |q| {
                        format!(
                            "{} sides {} tabs{}",
                            q.sides.count_ones(),
                            q.tabs,
                            if q.from_lines { "" } else { " (hull)" }
                        )
                    },
                );
                let aim = s.aim.map_or_else(
                    || "-".to_owned(),
                    |a| format!("({:5.1}%, {:5.1}%)", a[0], a[1]),
                );
                sh.status = format!(
                    "{fps:4.1} fps  aim {aim}  {solve}  age {:.0} ms   Esc quits",
                    s.age.map_or(0.0, |a| a.as_secs_f64() * 1000.0)
                );
                Flow::Continue
            };
            let r = run_tracker_with(&name, &camera, gun, &opts, &stop, &status, &mut hook);
            if let Ok(mut sh) = shared.lock() {
                sh.done = true;
            }
            r
        })
    };
    let shown = preview::appkit::run(shared.clone(), stop.clone(), border_frac);
    stop.store(true, Ordering::Relaxed);
    let tracked = worker
        .join()
        .map_err(|_| anyhow!("the tracker thread panicked"))?;
    if let Ok(sh) = shared.lock() {
        for line in &sh.history {
            println!("{line}");
        }
    }
    shown?;
    tracked
}

#[cfg(target_os = "linux")]
fn track_with_preview(
    _name: String,
    _camera: PathBuf,
    _gun: Option<(Gun, sindenrs::config::GunConfig)>,
    _opts: sindenrs::runtime::TrackerOptions,
    _status: std::sync::Arc<std::sync::Mutex<sindenrs::runtime::Status>>,
    _border_frac: f64,
) -> Result<()> {
    bail!("--preview is only available on macOS so far")
}

/// Run every attached gun that has a config entry, each on its own thread, until Ctrl-C.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn run_all(ctx: &Ctx, overlay: bool) -> Result<()> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .context("installing Ctrl-C handler")?;
    }

    // The border the guns track. Its window is shaped to the border and takes no input, so
    // it can sit above a running game; if there is no display, tracking still runs (the
    // border may be coming from MAME artwork).
    let scene = Arc::new(Mutex::new(sindenrs::overlay::Scene::border_only(
        ctx.display.border_thickness / 100.0,
    )));

    // AppKit windows live on the main thread, so on macOS the overlay takes it and the guns
    // are supervised beside it; losing the overlay still leaves the guns tracking.
    #[cfg(target_os = "macos")]
    if overlay {
        return std::thread::scope(|sc| {
            let guns = sc.spawn(|| supervise_guns(ctx, &stop));
            if let Err(e) = sindenrs::overlay::run_until(scene, &stop) {
                warn!("overlay: {e:#}; tracking continues without it");
            }
            guns.join()
                .map_err(|_| anyhow!("the gun supervisor panicked"))?
        });
    }

    let overlay_thread = overlay.then(|| {
        let stop = stop.clone();
        std::thread::Builder::new()
            .name("overlay".into())
            .spawn(move || {
                if let Err(e) = sindenrs::overlay::run_until(scene, &stop) {
                    warn!("overlay: {e:#}; tracking continues without it");
                }
            })
    });

    let r = supervise_guns(ctx, &stop);
    if let Some(Ok(t)) = overlay_thread {
        let _ = t.join();
    }
    r
}

/// `run`'s gun supervisor: rediscover every second, start a tracker for each gun that
/// appears, reap the ones that end, print a status line, and stop them all once `stop` is set.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn supervise_guns(ctx: &Ctx, stop: &std::sync::Arc<std::sync::atomic::AtomicBool>) -> Result<()> {
    use sindenrs::runtime::{run_tracker, Status, TrackerOptions};
    use std::collections::HashMap;
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    // One tracker thread per attached gun, keyed by the gun's USB path (stable per physical
    // port while it stays plugged in). The loop below rediscovers every second: a gun that
    // appears gets a thread, a gun that goes away makes its thread fail and be reaped, and
    // a gun that failed waits a little before it is tried again, so a broken one does not
    // spin. This is what a cabinet needs: guns plugged in after boot, or unplugged and
    // replugged mid-session, without anything restarting the driver.
    struct Running {
        handle: std::thread::JoinHandle<Result<()>>,
        status: Arc<Mutex<Status>>,
    }
    let mut running: HashMap<String, Running> = HashMap::new();
    let mut retry_after: HashMap<String, Instant> = HashMap::new();
    let retry = Duration::from_secs(3);
    let started = Instant::now();
    let mut announced_empty = false;

    while !stop.load(Ordering::Relaxed) {
        // Reap trackers that ended: a gun unplugged, its camera gone, or a real fault.
        let finished: Vec<String> = running
            .iter()
            .filter(|(_, r)| r.handle.is_finished())
            .map(|(k, _)| k.clone())
            .collect();
        for key in finished {
            if let Some(r) = running.remove(&key) {
                let name = r.status.lock().map(|s| s.name.clone()).unwrap_or_default();
                match r.handle.join() {
                    Ok(Ok(())) => info!("[{name}] stopped"),
                    Ok(Err(e)) => warn!("[{name}] {e:#}; will retry if the gun is still there"),
                    Err(_) => warn!("[{name}] tracker panicked"),
                }
                retry_after.insert(key, Instant::now() + retry);
            }
        }

        // Start a tracker for every gun that has none.
        let guns = discovery::find_guns().unwrap_or_default();
        let cams = discovery::find_cameras().unwrap_or_default();
        for g in &guns {
            let key = g.usb_path.clone();
            if running.contains_key(&key)
                || retry_after.get(&key).is_some_and(|t| *t > Instant::now())
            {
                continue;
            }
            let port = g.port.to_string_lossy().into_owned();
            let Some(cam) = g.sibling_camera(&cams) else {
                if !retry_after.contains_key(&key) {
                    warn!("{port}: no camera paired on the same hub; waiting for one");
                }
                retry_after.insert(key, Instant::now() + retry);
                continue;
            };
            let mut gun = match connect(&port, ctx.cfg.global.auto_recover) {
                Ok(g) => g,
                Err(e) => {
                    warn!("{port}: {e:#}; retrying in {}s", retry.as_secs());
                    retry_after.insert(key, Instant::now() + retry);
                    continue;
                }
            };
            let id = gun.unique_id().ok();
            let gc = gun_config_for(&ctx.cfg, id.as_deref());
            if let Err(e) = sindenrs::runtime::prepare_gun(&mut gun, &gc, ctx.cfg.recoil_gap()) {
                warn!("{port}: {e:#}; retrying in {}s", retry.as_secs());
                retry_after.insert(key, Instant::now() + retry);
                continue;
            }
            info!(
                "[{}] {port} + {} (id {})",
                gc.name,
                cam.node.display(),
                id.as_deref().unwrap_or("?")
            );
            let opts = TrackerOptions {
                display: ctx.display.clone(),
                threshold: None,
                min_size: None,
                flip: None,
                orientation: None,
                cal_x: None,
                cal_y: None,
                lens_k1: ctx.cfg.global.lens_k1,
                jump_limit: ctx.display.jump_limit,
                hover_smoothing: ctx.display.hover_smoothing,
                buffers: 4,
                frames: 0,
                record: None,
                per_frame: false,
                report_every: None,
            };
            let status = Arc::new(Mutex::new(Status {
                name: gc.name.clone(),
                ..Default::default()
            }));
            let name = gc.name.clone();
            let camera = cam.node.clone();
            let (stop2, status2) = (stop.clone(), status.clone());
            let handle = std::thread::Builder::new()
                .name(name.clone())
                .spawn(move || {
                    run_tracker(&name, &camera, Some((gun, gc)), &opts, &stop2, &status2)
                })?;
            retry_after.remove(&key);
            running.insert(key, Running { handle, status });
        }

        if running.is_empty() {
            if !announced_empty {
                info!("no gun attached; waiting (Ctrl-C to stop)");
                announced_empty = true;
            }
        } else {
            announced_empty = false;
            let mut line = format!("{:>6.0}s", started.elapsed().as_secs_f64());
            let mut names: Vec<&String> = running.keys().collect();
            names.sort();
            for key in names {
                let s = running[key]
                    .status
                    .lock()
                    .map(|s| s.clone())
                    .unwrap_or_default();
                let aim = s.last_aim.map_or("   --  ,   --  ".into(), |a| {
                    format!("{:6.2}%,{:6.2}%", a[0], a[1])
                });
                line.push_str(&format!(
                    " | {}: {:.0}fps found {:.0}% aim {} proc {:.2}ms{}",
                    s.name,
                    s.fps,
                    if s.frames > 0 {
                        s.found as f64 * 100.0 / s.frames as f64
                    } else {
                        0.0
                    },
                    aim,
                    s.proc_mean.as_secs_f64() * 1000.0,
                    if s.clipped { " clipped" } else { "" },
                ));
            }
            println!("{line}");
        }
        // Sleep in short steps so Ctrl-C is prompt.
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    stop.store(true, Ordering::Relaxed);
    for (_, r) in running {
        match r.handle.join() {
            Ok(Ok(())) => {}
            Ok(Err(e)) => warn!("tracker failed: {e:#}"),
            Err(_) => warn!("tracker panicked"),
        }
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn run_all(_ctx: &Ctx, _overlay: bool) -> Result<()> {
    bail!("the runtime needs the camera backend, which is not implemented on this platform yet")
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
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

fn firmware(port: &str, cmd: FirmwareCmd) -> Result<()> {
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
        FirmwareCmd::Backup { out } => {
            let out = out.unwrap_or_else(|| {
                sindenrs::config::default_data_dir()
                    .join("firmware")
                    .join(format!("backup-{}.hex", chrono_like_stamp()))
            });
            let mut bl = open_bootloader(port)?;
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
            wait_for_gun_back(port)?;
            println!("gun is back on {port}");
            Ok(())
        }
        FirmwareCmd::Flash {
            image,
            yes,
            allow_id_mismatch,
        } => {
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
            let backup = sindenrs::config::default_data_dir()
                .join("firmware")
                .join(format!("backup-{}-before-flash.hex", chrono_like_stamp()));
            let mut bl = open_bootloader(port)?;
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
                wait_for_gun_back(port)?;
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
            wait_for_gun_back(port)?;
            let mut g = connect(port, true)?;
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

/// Target positions in screen percent, row-major and 1-indexed to match `tools/targets.html`.
fn grid_targets(n: u32) -> Vec<[f64; 2]> {
    let mut out = Vec::new();
    let d = f64::from(n - 1);
    for r in 0..n {
        for c in 0..n {
            // 15% in from the edges: the target ring and its number must stay clear of
            // the border's tab band (the outer 6% of the screen), or they read as tabs.
            out.push([
                15.0 + 70.0 * f64::from(c) / d,
                15.0 + 70.0 * f64::from(r) / d,
            ]);
        }
    }
    out
}

/// Detection over recorded frames, with an optional lens fit.
fn replay(ctx: &Ctx, a: &ReplayArgs) -> Result<()> {
    use sindenrs::vision::acquire::{acquire, edge_segments, flip_luma, solve, AcquireParams};
    use sindenrs::vision::code::Side;
    use sindenrs::vision::lens::Lens;
    use sindenrs::vision::lensfit::{fit_k1, score, FitFrame};
    use sindenrs::vision::luma::frame_to_luma;

    let flip = flip_from_config(ctx.display.flip);
    let mut params = AcquireParams {
        threshold: a.threshold.unwrap_or(ctx.display.threshold),
        min_size: ctx.display.min_size,
        lens_k1: a.lens_k1.unwrap_or(ctx.cfg.global.lens_k1),
        subpixel_lines: !a.no_subpixel_lines,
        subpixel_tabs: !a.no_subpixel_tabs,
        screen_aspect: ctx.display.aspect,
        border_frac: ctx.display.border_thickness / 100.0,
        ..Default::default()
    };
    let mut files: Vec<PathBuf> = Vec::new();
    for dir in &a.dirs {
        let mut in_dir: Vec<PathBuf> = std::fs::read_dir(dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jpg" || e == "pgm"))
            .collect();
        in_dir.sort();
        files.extend(in_dir.into_iter().step_by(a.every.max(1)));
    }
    if files.is_empty() {
        bail!("no .jpg or .pgm frames found");
    }
    let mut frames: Vec<(PathBuf, usize, usize, Vec<u8>)> = Vec::new();
    for f in &files {
        let data = std::fs::read(f)?;
        let Ok((w, h, mut l)) = frame_to_luma(f, &data) else {
            warn!("{}: decode failed", f.display());
            continue;
        };
        let (w, h) = (w as usize, h as usize);
        flip_luma(&mut l, w, flip);
        frames.push((f.clone(), w, h, l));
    }
    println!(
        "{} frames, threshold {}, flip {flip:?}",
        frames.len(),
        params.threshold
    );

    if a.fit_lens {
        let fit_frames: Vec<FitFrame> = frames
            .iter()
            .filter_map(|(_, w, h, l)| FitFrame::prepare(l, *w, *h, &params))
            .collect();
        println!(
            "fitting lens k1 on {} frames with a border blob",
            fit_frames.len()
        );
        let (best, sweep) = fit_k1(&fit_frames, -0.3, 0.1, &params);
        println!("{:>8} {:>8} {:>7}", "k1", "inliers", "rms_px");
        for s in &sweep {
            println!("{:>8.3} {:>8} {:>7.2}", s.k1, s.inliers, s.rms);
        }
        let zero = score(&fit_frames, 0.0, &params);
        println!(
            "\nbest k1 = {:.4} (inliers {}, rms {:.2} px); k1 = 0 gives inliers {}, rms {:.2} px",
            best.k1, best.inliers, best.rms, zero.inliers, zero.rms
        );
        println!(
            "set [global] lens_k1 = {:.4} in the config to use it",
            best.k1
        );
        params.lens_k1 = best.k1;
    }

    let (mut lines, mut hull, mut none) = (0u32, 0u32, 0u32);
    let t0 = Instant::now();
    for (path, w, h, l) in &frames {
        let q = acquire(l, *w, *h, &params);
        let tag = match q {
            Some(q) if q.from_lines => {
                lines += 1;
                "lines"
            }
            Some(_) => {
                hull += 1;
                "hull"
            }
            None => {
                none += 1;
                "none"
            }
        };
        if a.per_frame && a.lines {
            if let Some(f) = FitFrame::prepare(l, *w, *h, &params) {
                let lens = Lens::centred(params.lens_k1, *w, *h);
                let edges = edge_segments(&f.pts, &f.mask, &lens, &params, Some(l));
                let (_, report) = solve(&edges, f.mask.w, f.mask.h);
                if let Some(why) = report.refused {
                    println!("    refused: {why}");
                }
                for side in Side::ALL {
                    let Some(sl) = &report.sides[side as usize] else {
                        continue;
                    };
                    println!(
                        "    {side:?}: outer ({:.4},{:.4},{:.2}) thickness {} (walked {}, inner seg {:?}{}) tabs seen {} decoded {}",
                        sl.outer.a,
                        sl.outer.b,
                        sl.outer.c,
                        sl.thickness.map_or("?".into(), |t| format!("{t:.1}px")),
                        sl.walked.map_or("?".into(), |t| format!("{t:.1}px")),
                        sl.inner_seg,
                        if sl.inner_only { ", inner only" } else { "" },
                        report.seen[side as usize].len(),
                        report.decoded[side as usize]
                    );
                    for t in &report.seen[side as usize] {
                        println!(
                            "        tab at {:.1} width {:.1}px{}",
                            t.along,
                            t.width,
                            if t.partial { " (cut off)" } else { "" }
                        );
                    }
                }
                for (img, scr) in &report.points {
                    println!(
                        "    point ({:.2},{:.2}) -> ({:.2},{:.2})",
                        img[0], img[1], scr[0], scr[1]
                    );
                }
                for (i, s) in edges.segments.iter().enumerate() {
                    let role = Side::ALL
                        .iter()
                        .find_map(|&sd| {
                            let sl = report.sides[sd as usize]?;
                            if sl.outer_seg == i {
                                Some(format!("{sd:?} outer"))
                            } else if sl.inner_seg == Some(i) {
                                Some(format!("{sd:?} inner"))
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default();
                    let o = edges.outward[i];
                    print!(
                        "    [{i:>2}] dark side ({:+.2},{:+.2}) {role:<13}",
                        o.a, o.b
                    );
                    let (lo, hi) = s.inliers.iter().fold((f64::MAX, f64::MIN), |(lo, hi), p| {
                        let t = -s.line.b * p[0] + s.line.a * p[1];
                        (lo.min(t), hi.max(t))
                    });
                    println!(
                        " n={:<4} rms={:.2} normal=({:+.3},{:+.3}) c={:8.2} span={:.0}px",
                        s.inliers.len(),
                        s.rms,
                        s.line.a,
                        s.line.b,
                        s.line.c,
                        hi - lo
                    );
                }
            }
        }
        if a.per_frame {
            let c = q.map_or([[f64::NAN; 2]; 4], |q| q.corners);
            println!(
                "{} {tag:<12} tabs={:<2} TL({:.1},{:.1}) TR({:.1},{:.1}) BR({:.1},{:.1}) BL({:.1},{:.1})",
                path.file_name()
                    .map_or_else(String::new, |n| n.to_string_lossy().into_owned()),
                q.map_or(0, |q| q.tabs),
                c[0][0],
                c[0][1],
                c[1][0],
                c[1][1],
                c[2][0],
                c[2][1],
                c[3][0],
                c[3][1]
            );
        }
    }
    let n = frames.len().max(1);
    #[allow(clippy::cast_precision_loss)]
    let pct = |k: u32| f64::from(k) * 100.0 / n as f64;
    println!(
        "lens k1 {:.4}: {lines} solved from edge lines ({:.0}%), {hull} hull only, unreliable ({:.0}%), {none} not found ({:.0}%); {:.2} ms/frame",
        params.lens_k1,
        pct(lines),
        pct(hull),
        pct(none),
        t0.elapsed().as_secs_f64() * 1000.0 / n as f64
    );
    Ok(())
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.is_empty() {
        return f64::NAN;
    }
    if v.len() % 2 == 1 {
        v[v.len() / 2]
    } else {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    }
}

/// The image transform the tracker applies, from the display profile.
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

/// Short tag for a frame's tracking quality, used in debug capture filenames.
#[cfg(target_os = "linux")]
fn quality_tag(q: sindenrs::overlay::Quality) -> &'static str {
    use sindenrs::overlay::Quality;
    match q {
        Quality::Good => "ok",
        Quality::Clipped => "clipped",
        Quality::Lost => "lost",
    }
}

/// What the measuring thread hands back.
#[derive(Default)]
struct AimResult {
    measured: Vec<Option<[f64; 2]>>,
    /// The border quad seen at each capture, needed to turn a screen-space error back into a
    /// camera-space bore offset.
    quads: Vec<Option<[[f64; 2]; 4]>>,
    gun_id: Option<String>,
    frames: u64,
    usable: u64,
    clipped: u64,
    events: std::collections::BTreeMap<String, u32>,
}

#[cfg(target_os = "linux")]
#[allow(clippy::too_many_lines)]
fn calibrate(ctx: &Ctx, a: CalibrateArgs) -> Result<()> {
    use sindenrs::overlay::{Quality, Scene};
    use sindenrs::protocol::event::Event;
    use sindenrs::runtime::{run_tracker_with, Flow, Sample, Status, TrackerOptions};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    if !(3..=5).contains(&a.grid) {
        bail!("--grid must be 3, 4 or 5");
    }
    let targets = grid_targets(a.grid);
    let port = select_port(ctx)?;
    let camera = camera_for_port(&port)?;
    let opts = TrackerOptions {
        display: ctx.display.clone(),
        threshold: None,
        min_size: None,
        flip: None,
        orientation: None,
        cal_x: None,
        cal_y: None,
        lens_k1: ctx.cfg.global.lens_k1,
        jump_limit: ctx.display.jump_limit,
        hover_smoothing: ctx.display.hover_smoothing,
        buffers: 4,
        frames: 0,
        record: a.record.clone(),
        per_frame: false,
        report_every: None,
    };

    let scene = Arc::new(Mutex::new(Scene {
        border_frac: ctx.display.border_thickness / 100.0,
        targets: targets.clone(),
        current: Some(0),
        measured: vec![None; targets.len()],
        ..Default::default()
    }));
    let stop = Arc::new(AtomicBool::new(false));
    {
        let stop = stop.clone();
        ctrlc::set_handler(move || stop.store(true, Ordering::Relaxed))
            .context("installing Ctrl-C handler")?;
    }

    println!("Aim at the ringed target and pull the trigger. Escape quits.");
    println!("The overlay shows everything: the ring is the target, the dot is where the driver");
    println!("thinks you are pointing, and the square bottom-left is green only while the whole");
    println!("border is in the camera's view.");

    // The window event loop must own the main thread, so the measuring runs on a worker.
    let debug_dir = match &a.debug_dir {
        Some(p) if p.as_os_str().is_empty() => None,
        Some(p) => Some(p.clone()),
        None => Some(
            sindenrs::config::default_cache_dir()
                .join("aim")
                .join(chrono_like_stamp()),
        ),
    };
    if let Some(d) = &debug_dir {
        std::fs::create_dir_all(d).with_context(|| format!("creating {}", d.display()))?;
        println!("saving a frame per shot to {}", d.display());
    }
    let status = Arc::new(Mutex::new(Status::default()));
    let cfg = ctx.cfg.clone();
    let recoil_gap = ctx.cfg.recoil_gap();
    let recover = ctx.cfg.global.auto_recover;
    let worker = {
        let (scene, stop, targets) = (scene.clone(), stop.clone(), targets.clone());
        let debug_dir = debug_dir.clone();
        let status = status.clone();
        let port = port.clone();
        std::thread::Builder::new().name("calibrate".into()).spawn(move || -> Result<AimResult> {
            let finish = |stop: &AtomicBool| stop.store(true, Ordering::Relaxed);
            let mut gun = match connect(&port, recover) {
                Ok(g) => g,
                Err(e) => {
                    finish(&stop);
                    return Err(e);
                }
            };
            let id = gun.unique_id().ok();
            let gc = gun_config_for(&cfg, id.as_deref());
            if let Err(e) = sindenrs::runtime::prepare_gun(&mut gun, &gc, recoil_gap) {
                finish(&stop);
                return Err(e);
            }
            // The kick would disturb the aim being measured; `run` re-sends the config.
            if let Err(e) = gun.set_recoil_enabled(false) {
                finish(&stop);
                return Err(e.into());
            }

            // A capture needs the whole border in view: when it runs off the edge of the
            // camera frame the detected quad is only the visible part, so the solve is badly
            // wrong. The reading is the median of the usable aim points in a short window
            // around the pull, so the pull itself cannot drag it. The press is latched rather
            // than serviced immediately, because it usually lands on a frame with no usable
            // aim, and dropping it there silently desynchronises the target numbering from
            // where the shooter is actually pointing.
            let window = Duration::from_millis(600);
            // Six frames inside a 600 ms window can straddle a movement between targets when
            // most frames are unusable, which produced a 38%-error outlier; require enough of
            // them, and that they agree, before trusting a reading.
            let min_good = 12usize;
            let give_up = Duration::from_millis(1200);
            let use_dwell = matches!(a.capture, CaptureMode::Dwell | CaptureMode::Either);
            let use_trigger = matches!(a.capture, CaptureMode::Trigger | CaptureMode::Either);
            let mut out = AimResult {
                measured: vec![None; targets.len()],
                quads: vec![None; targets.len()],
                gun_id: id.clone(),
                ..Default::default()
            };
            // Every shot's frame is kept so a bad reading can be looked at afterwards.
            let mut shot = 0u32;
            let save = |dir: &Option<PathBuf>, name: &str, raw: &[u8]| {
                if let Some(d) = dir {
                    if let Err(e) = std::fs::write(d.join(name), raw) {
                        warn!("could not save {name}: {e}");
                    }
                }
            };
            let mut idx = 0usize;
            let mut trigger_was_down = false;
            let mut ring: std::collections::VecDeque<(Instant, [f64; 2])> = std::collections::VecDeque::new();
            let mut pending: Option<Instant> = None;
            let mut dwell_armed = true;
            let mut captured_at: Option<[f64; 2]> = None;
            let mut hook = |s: &Sample| -> Flow {
                out.frames += 1;
                let mut pressed = false;
                for ev in s.events {
                    *out.events.entry(format!("{ev:?}")).or_default() += 1;
                    match ev {
                        Event::Buttons { state1, .. } => {
                            let down = state1 & 1 != 0;
                            if down && !trigger_was_down {
                                pressed = true;
                            }
                            trigger_was_down = down;
                        }
                        Event::TriggerEnabled => pressed = true,
                        _ => {}
                    }
                }
                let now = Instant::now();
                let (usable, quality) = match (s.aim, s.quad.as_ref()) {
                    (Some(aim), Some(q)) if !q.clipped => {
                        out.usable += 1;
                        (Some(aim), Quality::Good)
                    }
                    (_, Some(q)) if q.clipped => {
                        out.clipped += 1;
                        (None, Quality::Clipped)
                    }
                    _ => (None, Quality::Lost),
                };
                if let Some(aim) = usable {
                    ring.push_back((now, aim));
                }
                while ring.front().is_some_and(|(t, _)| now.duration_since(*t) > window) {
                    ring.pop_front();
                }
                if let Ok(mut sc) = scene.lock() {
                    sc.aim = s.aim;
                    sc.quality = quality;
                    sc.solve = sindenrs::overlay::SolveInfo {
                        sides: s.quad.map_or(0, |q| q.sides),
                        tabs: s.quad.map_or(0, |q| q.tabs),
                        from_lines: s.quad.is_some_and(|q| q.from_lines),
                        view: s.view,
                    };
                }
                if use_trigger && pressed && pending.is_none() {
                    pending = Some(now);
                    shot += 1;
                    save(&debug_dir, &format!("t{:02}-shot{:02}-pull-{}.{}", idx + 1, shot, quality_tag(quality), s.raw_ext), s.raw);
                }
                let steady = use_dwell && dwell_armed && ring.len() >= a.samples.max(4) as usize && {
                    let mx = median(ring.iter().map(|(_, p)| p[0]).collect());
                    let my = median(ring.iter().map(|(_, p)| p[1]).collect());
                    ring.iter().all(|(_, p)| (p[0] - mx).hypot(p[1] - my) <= a.dwell_radius)
                };
                let ready = ring.len() >= min_good;
                if !(ready && (steady || pending.is_some())) {
                    if pending.is_some_and(|t| now.duration_since(t) > give_up) {
                        println!(
                            "  target {:>2}: could not measure, only {} usable frame(s); the border is not fully in the camera's view. Step back, then pull again.",
                            idx + 1,
                            ring.len()
                        );
                        if let Ok(mut sc) = scene.lock() {
                            sc.flash_until = Some(now + Duration::from_millis(700));
                        }
                        save(&debug_dir, &format!("t{:02}-shot{:02}-unmeasurable.{}", idx + 1, shot, s.raw_ext), s.raw);
                        pending = None;
                    }
                    if !dwell_armed
                        && captured_at.is_some_and(|c| usable.is_some_and(|aim| (aim[0] - c[0]).hypot(aim[1] - c[1]) > a.rearm_distance))
                    {
                        dwell_armed = true;
                    }
                    return Flow::Continue;
                }
                let got = [
                    median(ring.iter().map(|(_, p)| p[0]).collect()),
                    median(ring.iter().map(|(_, p)| p[1]).collect()),
                ];
                let scatter = ring
                    .iter()
                    .map(|(_, p)| (p[0] - got[0]).hypot(p[1] - got[1]))
                    .fold(0.0_f64, f64::max);
                if scatter > a.steady_tolerance {
                    println!(
                        "  target {:>2}: aim was not steady (samples spread {scatter:.1}% of screen); pull again.",
                        idx + 1
                    );
                    save(&debug_dir, &format!("t{:02}-shot{:02}-unsteady.{}", idx + 1, shot, s.raw_ext), s.raw);
                    if let Ok(mut sc) = scene.lock() {
                        sc.flash_until = Some(now + Duration::from_millis(700));
                    }
                    pending = None;
                    ring.clear();
                    return Flow::Continue;
                }
                let want = targets[idx];
                let err = (got[0] - want[0]).hypot(got[1] - want[1]);
                println!(
                    "  target {:>2}: want ({:5.1}%, {:5.1}%)  got ({:5.1}%, {:5.1}%)  dx {:+6.2} dy {:+6.2}  err {:5.2}%  [{}, {} frames]",
                    idx + 1,
                    want[0], want[1], got[0], got[1],
                    got[0] - want[0], got[1] - want[1], err,
                    if pending.is_some() { "trigger" } else { "held steady" },
                    ring.len()
                );
                save(&debug_dir, &format!("t{:02}-shot{:02}-measured.{}", idx + 1, shot, s.raw_ext), s.raw);
                out.measured[idx] = Some(got);
                out.quads[idx] = s.quad.as_ref().map(|q| q.corners);
                captured_at = Some(got);
                pending = None;
                dwell_armed = false;
                ring.clear();
                idx += 1;
                if let Ok(mut sc) = scene.lock() {
                    sc.measured[idx - 1] = Some(got);
                    sc.current = (idx < targets.len()).then_some(idx);
                    sc.done = idx >= targets.len();
                }
                if idx >= targets.len() {
                    return Flow::Stop;
                }
                Flow::Continue
            };
            let r = run_tracker_with(
                "calibrate",
                &camera,
                Some((gun, gc)),
                &opts,
                &stop,
                &status,
                &mut hook,
            );
            finish(&stop);
            r.map(|()| out)
        })?
    };

    sindenrs::overlay::run(scene, stop)?;
    let out = worker
        .join()
        .map_err(|_| anyhow!("the measuring thread panicked"))??;

    println!(
        "\nframes {}: {} usable ({:.0}%), {} with the border clipped at the frame edge ({:.0}%)",
        out.frames,
        out.usable,
        if out.frames > 0 {
            out.usable as f64 * 100.0 / out.frames as f64
        } else {
            0.0
        },
        out.clipped,
        if out.frames > 0 {
            out.clipped as f64 * 100.0 / out.frames as f64
        } else {
            0.0
        }
    );
    if out.events.is_empty() {
        println!("no events were received from the gun, so the trigger could not be used");
    }
    let done: Vec<(usize, [f64; 2], [f64; 2])> = out
        .measured
        .iter()
        .enumerate()
        .filter_map(|(i, m)| m.map(|g| (i, targets[i], g)))
        .collect();
    if done.is_empty() {
        println!("no targets measured");
        return Ok(());
    }
    let errs: Vec<f64> = done
        .iter()
        .map(|(_, w, g)| (g[0] - w[0]).hypot(g[1] - w[1]))
        .collect();
    let mean = errs.iter().sum::<f64>() / errs.len() as f64;
    let max = errs.iter().copied().fold(0.0_f64, f64::max);
    let dx: Vec<f64> = done.iter().map(|(_, w, g)| g[0] - w[0]).collect();
    let dy: Vec<f64> = done.iter().map(|(_, w, g)| g[1] - w[1]).collect();
    let bias = [
        dx.iter().sum::<f64>() / dx.len() as f64,
        dy.iter().sum::<f64>() / dy.len() as f64,
    ];
    println!("\n{} of {} targets measured", done.len(), targets.len());
    println!("error: mean {mean:.2}% of screen, max {max:.2}%");
    println!(
        "systematic bias: dx {:+.2}%, dy {:+.2}%  (cancel with display.offset_x / offset_y)",
        bias[0], bias[1]
    );
    let spread = [
        (dx.iter().map(|v| (v - bias[0]).powi(2)).sum::<f64>() / dx.len() as f64).sqrt(),
        (dy.iter().map(|v| (v - bias[1]).powi(2)).sum::<f64>() / dy.len() as f64).sqrt(),
    ];
    println!(
        "after removing that bias: dx spread {:.2}%, dy spread {:.2}%  (what a correction field would have to fix)",
        spread[0], spread[1]
    );
    // Turn the constant part of the error into a bore offset. It has to be expressed in the
    // camera's frame rather than as a screen-percent trim: the bore is an angular property of
    // the gun, so a frame-relative offset stays correct when you move closer or further away,
    // whereas a screen-percent trim only holds at the distance it was measured.
    let geom = status
        .lock()
        .map(|s| (s.aim_pixel, s.frame))
        .unwrap_or(([0.0; 2], (0, 0)));
    let (aim_px, (fw, fh)) = geom;
    if fw > 0 && fh > 0 {
        let d = &ctx.display;
        let flip = flip_from_config(d.flip);
        let mut cals = Vec::new();
        for (i, want, _got) in &done {
            let Some(corners) = out.quads[*i] else {
                continue;
            };
            // Undo the trims to get the raw solve the homography must produce.
            let raw = [
                (want[0] - d.offset_x) / d.ratio_x,
                (want[1] + d.gunsight_y - d.offset_y) / d.ratio_y,
            ];
            let to_camera = sindenrs::vision::homography::quad_to_quad(
                &sindenrs::vision::homography::SCREEN_PERCENT,
                &corners,
            );
            let Some(p_want) = to_camera.apply(raw) else {
                continue;
            };
            // Where the bore axis should sit, back in unflipped camera pixels.
            let unflipped = sindenrs::vision::acquire::flip_point(p_want, fw, fh, flip);
            #[allow(clippy::cast_precision_loss)]
            let (fwf, fhf) = (fw as f64, fh as f64);
            cals.push([
                (unflipped[0] - fwf / 2.0) * 100.0 / (fwf * d.orientation),
                (unflipped[1] - fhf / 2.0) * 100.0 / (fhf * d.orientation),
            ]);
        }
        if !cals.is_empty() {
            let n = cals.len() as f64;
            let cal = [
                cals.iter().map(|c| c[0]).sum::<f64>() / n,
                cals.iter().map(|c| c[1]).sum::<f64>() / n,
            ];
            let spread = [
                (cals.iter().map(|c| (c[0] - cal[0]).powi(2)).sum::<f64>() / n).sqrt(),
                (cals.iter().map(|c| (c[1] - cal[1]).powi(2)).sum::<f64>() / n).sqrt(),
            ];
            println!(
                "\nbore calibration from {} point(s): x {:+.2}%, y {:+.2}% of frame (agreement +/-{:.2}, {:.2})",
                cals.len(), cal[0], cal[1], spread[0], spread[1]
            );
            println!(
                "current aim pixel ({:.1}, {:.1}) of {fw}x{fh}",
                aim_px[0], aim_px[1]
            );
            let cal = [round2(cal[0]), round2(cal[1])];
            let save = if a.no_save {
                false
            } else if a.save {
                true
            } else {
                ask_yes_no(&format!(
                    "Save this calibration (x {:+.2}, y {:+.2}) to the gun?",
                    cal[0], cal[1]
                ))
            };
            if save {
                let mut gun = connect(&port, ctx.cfg.global.auto_recover)?;
                gun.write_calibration(cal[0], cal[1])?;
                println!("saved to the gun's EEPROM; run `calibrate` again to check the error drops to the spread above");
                if let Some(over) = gun_config_for(&ctx.cfg, out.gun_id.as_deref()).calibration {
                    println!(
                        "note: the config sets calibration = [{}, {}] for this gun, which overrides the saved value",
                        over[0], over[1]
                    );
                }
            } else {
                println!(
                    "not saved; to set it by hand: `sindenrs debug write-calibration --x {:.2} --y {:.2}` or `calibration = [{:.2}, {:.2}]` in the config",
                    cal[0], cal[1], cal[0], cal[1]
                );
            }
        }
    }

    if let Some(out_path) = &a.out {
        if let Some(d) = out_path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let mut f = std::fs::File::create(out_path)?;
        use std::io::Write as _;
        writeln!(f, "target,want_x,want_y,got_x,got_y,dx,dy,err")?;
        for (i, w, g) in &done {
            writeln!(
                f,
                "{},{:.2},{:.2},{:.2},{:.2},{:.3},{:.3},{:.3}",
                i + 1,
                w[0],
                w[1],
                g[0],
                g[1],
                g[0] - w[0],
                g[1] - w[1],
                (g[0] - w[0]).hypot(g[1] - w[1])
            )?;
        }
        println!("wrote {}", out_path.display());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn calibrate(_ctx: &Ctx, _a: CalibrateArgs) -> Result<()> {
    bail!("calibrate needs the camera backend, which is not implemented on this platform yet")
}

/// Two decimals of a percent-of-frame offset is finer than the measurement.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// A y/n question on the terminal. Anything but a terminal answers no, so a scripted run
/// has to say `--save` or `--no-save` explicitly.
fn ask_yes_no(question: &str) -> bool {
    use std::io::{BufRead, IsTerminal, Write};
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        println!("{question} (no terminal; pass --save or --no-save)");
        return false;
    }
    print!("{question} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}
