//! opendisplay-win: a Windows sender for OpenDisplay (https://opendisplay.app).
//! Extends (or mirrors) the Windows desktop onto an iPad/iPhone running the
//! OpenDisplay receiver app, over Wi-Fi or USB. Wire protocol: PROTOCOL.md pv 3,
//! implemented by the upstream `opendisplay-proto`/`opendisplay-session` crates.

use opendisplay_win::app::{self, RunConfig, SingleInstance};
use opendisplay_win::{capture, desktop, discover, display, driver, encoder, usbmux};

use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Args, FromArgMatches, Parser, Subcommand};

use desktop::{DesktopOptions, ScalePolicy};
use display::{DisplayBackend, ExistingMonitor};

#[derive(Parser)]
#[command(
    version,
    about = "Windows sender for OpenDisplay: use an iPad or iPhone as a second display",
    long_about = "Windows sender for OpenDisplay: use an iPad or iPhone as a second display.\n\n\
        Without a command it runs silently: waits for the OpenDisplay app on a device \
        (USB or Wi-Fi), extends the desktop onto it, and keeps going until this window is closed."
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    /// Print a log to the console: `--log` (info), or a filter such as `--log debug`.
    #[arg(long, global = true, num_args = 0..=1, default_missing_value = "info", value_name = "FILTER")]
    log: Option<String>,
    /// Append the log to this file (info level unless --log gives a filter).
    #[arg(long, global = true, value_name = "PATH")]
    log_file: Option<std::path::PathBuf>,
}

#[derive(Args, Clone)]
struct RunArgs {
    /// Receiver: Bonjour name or id fragment, `ip[:port]`, `usb`, or
    /// `usb:<udid>`. Default: the receiver over USB when cabled, else on Wi-Fi.
    #[arg(long)]
    to: Option<String>,
    /// What to stream: `auto` (a virtual display driver's monitor as an
    /// extended desktop when one is installed, else mirror the primary
    /// monitor), `virtual`, `primary`, or a monitor such as `DISPLAY2`.
    #[arg(long, default_value = "auto")]
    display: String,
    /// With `--display DISPLAYn`: switch that monitor to the receiver's
    /// desktop size while streaming, restoring it afterwards.
    #[arg(long)]
    resize: Option<bool>,
    /// Desktop scale policy: `passthrough`, `mac` or a number.
    #[arg(long, default_value = "mac")]
    scale: ScalePolicy,
    /// Lower the frame rate below the receiver's panel rate.
    #[arg(long)]
    max_fps: Option<f32>,
    /// Video bitrate in Mbit/s (default: from resolution and link).
    #[arg(long)]
    bitrate: Option<f64>,
    /// Use Microsoft's software H.264 encoder instead of the GPU.
    #[arg(long)]
    software_encoder: bool,
    /// Keep waiting for the receiver when its app is closed.
    #[arg(long)]
    stay: bool,
    /// Keep looking until a receiver appears instead of giving up.
    #[arg(long)]
    wait: bool,
    /// Battery saver: 30 fps cap, lower bitrate, fastest encoder preset.
    #[arg(long)]
    battery_saver: bool,
    /// Stop after this many seconds (scripted tests).
    #[arg(long = "for", value_name = "SECONDS")]
    duration: Option<f64>,
}

impl RunArgs {
    /// What a plain double-click runs.
    fn background() -> Self {
        let cmd = Self::augment_args(clap::Command::new("run"));
        let mut a = Self::from_arg_matches(&cmd.get_matches_from(["run"])).expect("defaults parse");
        a.stay = true;
        a.wait = true;
        a
    }
}

#[derive(Subcommand)]
enum Command {
    /// List receivers on the network and on USB, and local monitors.
    List {
        #[arg(long, default_value_t = 3.0)]
        timeout: f64,
    },
    /// List monitors and their modes.
    Monitors,
    /// Connect to a receiver and stream (the default without a command).
    Run(RunArgs),
    /// Switch the virtual monitor on (attached to the desktop, sized) or off,
    /// without a device: for diagnosing the virtual display setup.
    VirtualDisplay {
        #[arg(value_parser = ["on", "off"])]
        state: String,
        #[arg(long, default_value_t = 2224)]
        width: u32,
        #[arg(long, default_value_t = 1668)]
        height: u32,
        #[arg(long, default_value_t = 2.0)]
        scale: f64,
    },
    /// Install the bundled virtual display driver (asks for admin rights).
    InstallDriver,
    /// Remove the virtual display driver (asks for admin rights).
    UninstallDriver,
    /// Save a monitor as a lossless PNG (pointer not included), e.g. for docs.
    #[command(hide = true)]
    Screenshot {
        #[arg(long, default_value = "primary")]
        display: String,
        #[arg(long, default_value = "screenshot.png")]
        out: String,
    },
    /// Capture a monitor and encode it to an .h264 file (no receiver needed).
    Probe {
        #[arg(long, default_value = "primary")]
        display: String,
        #[arg(long, default_value_t = 1280)]
        width: u32,
        #[arg(long, default_value_t = 720)]
        height: u32,
        #[arg(long, default_value_t = 60)]
        frames: u32,
        #[arg(long, default_value = "probe.h264")]
        out: String,
        #[arg(long)]
        software_encoder: bool,
    },
}

static STOP: std::sync::OnceLock<Arc<AtomicBool>> = std::sync::OnceLock::new();
static CLEANED_UP: AtomicBool = AtomicBool::new(false);

/// Ctrl+C, closing the console window, logoff and shutdown all stop the
/// sender. For a closing window Windows kills the process once this returns,
/// so wait (briefly) until the virtual monitor has been taken down.
unsafe extern "system" fn on_console_event(kind: u32) -> windows::core::BOOL {
    use windows::Win32::System::Console::{CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT};
    if let Some(s) = STOP.get() {
        s.store(true, Ordering::Relaxed);
    }
    if [CTRL_CLOSE_EVENT, CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT].contains(&kind) {
        let deadline = Instant::now() + Duration::from_millis(4500);
        while !CLEANED_UP.load(Ordering::Relaxed) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    true.into()
}

fn init_logging(cli: &Cli) -> Result<()> {
    use tracing_subscriber::prelude::*;
    if cli.log.is_none() && cli.log_file.is_none() {
        return Ok(()); // silent by default
    }
    let filter = || tracing_subscriber::EnvFilter::new(cli.log.as_deref().unwrap_or("info"));
    let console = cli
        .log
        .is_some()
        .then(|| tracing_subscriber::fmt::layer().with_target(false).with_filter(filter()));
    let file = match &cli.log_file {
        Some(p) => {
            let f = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(p)
                .with_context(|| format!("opening {}", p.display()))?;
            Some(
                tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_ansi(false)
                    .with_writer(std::sync::Mutex::new(f))
                    .with_filter(filter()),
            )
        }
        None => None,
    };
    tracing_subscriber::registry().with(console).with(file).init();
    Ok(())
}

fn show_error(e: &anyhow::Error) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};
    let text: Vec<u16> = format!("{e:#}").encode_utf16().chain([0]).collect();
    let title: Vec<u16> = "OpenDisplay".encode_utf16().chain([0]).collect();
    unsafe {
        MessageBoxW(None, windows::core::PCWSTR(text.as_ptr()), windows::core::PCWSTR(title.as_ptr()), MB_OK | MB_ICONERROR);
    }
}

fn main() {
    let cli = Cli::parse();
    let silent = cli.command.is_none() && cli.log.is_none();
    if let Err(e) = real_main(cli) {
        if silent {
            show_error(&e);
        } else {
            eprintln!("Error: {e:#}");
        }
        std::process::exit(1);
    }
}

fn run(args: RunArgs, silent: bool) -> Result<()> {
    let RunArgs { to, display, resize, scale, max_fps, bitrate, software_encoder, stay, wait, battery_saver, duration } = args;
    let stop = STOP.get_or_init(|| Arc::new(AtomicBool::new(false))).clone();
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(Some(on_console_event), true);
        if silent {
            let title: Vec<u16> = "OpenDisplay - extending this PC to your iPad/iPhone. Close this window to stop."
                .encode_utf16()
                .chain([0])
                .collect();
            let _ = windows::Win32::System::Console::SetConsoleTitleW(windows::core::PCWSTR(title.as_ptr()));
        }
    }
    if let Some(secs) = duration {
        let s = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs_f64(secs));
            s.store(true, Ordering::Relaxed);
        });
    }
    let _instance = SingleInstance::acquire()?;
    let cfg = RunConfig {
        to,
        display,
        resize,
        desktop: DesktopOptions { scale, max_fps },
        bitrate: bitrate.map(|m| (m * 1e6) as u32),
        software_encoder,
        stay,
        wait,
        battery_saver,
    };
    let r = app::run(&cfg, stop, None);
    CLEANED_UP.store(true, Ordering::Relaxed);
    r
}

fn real_main(cli: Cli) -> Result<()> {
    init_logging(&cli)?;
    app::set_dpi_awareness();
    let silent = cli.command.is_none() && cli.log.is_none();
    let Some(command) = cli.command else {
        return run(RunArgs::background(), silent);
    };

    match command {
        Command::List { timeout } => {
            println!("Receivers on the network (_opensidecar._tcp):");
            match discover::browse(Duration::from_secs_f64(timeout)) {
                Ok(found) if found.is_empty() => println!("  (none)"),
                Ok(found) => {
                    for r in found {
                        println!("  {}  {}  id {}  pv {}", r.name, r.addr, r.id.as_deref().unwrap_or("-"), r.pv);
                    }
                }
                Err(e) => println!("  mDNS failed: {e:#}"),
            }
            println!("USB devices (usbmuxd):");
            match usbmux::list_devices() {
                Ok(d) if d.is_empty() => println!("  (none)"),
                Ok(d) => {
                    for d in d {
                        println!("  usb:{}  device {}  product {:#x}", d.serial, d.device_id, d.product_id.unwrap_or(0));
                    }
                }
                Err(e) => println!("  {e:#}"),
            }
            print_monitors(false);
        }
        Command::Monitors => {
            print_monitors(true);
            println!("Display paths (all):");
            for l in display::debug_paths() {
                println!("  {l}");
            }
        }
        Command::Run(args) => run(args, false)?,
        Command::VirtualDisplay { state, width, height, scale } => {
            println!("driver installed: {}", driver::installed());
            let mut v = display::VirtualMonitor::new();
            if state == "on" {
                let m = v.ensure(&desktop::OutputMode { width, height, hz: 60, scale })?;
                println!("on: {} {}x{} at ({}, {}), {}% scaling", m.device_name, m.size().0, m.size().1, m.rect.left, m.rect.top, m.dpi * 100 / 96);
            } else {
                match display::monitors().into_iter().find(|m| display::looks_virtual(m)) {
                    Some(m) => {
                        display::detach(&m.device_name)?;
                        println!("off: {} detached", m.device_name);
                    }
                    None => println!("off: no virtual monitor is attached"),
                }
            }
        }
        Command::InstallDriver => driver::install()?,
        Command::UninstallDriver => driver::uninstall()?,
        Command::Screenshot { display, out } => screenshot(&display, &out)?,
        Command::Probe { display, width, height, frames, out, software_encoder } => {
            probe(&display, width, height, frames, &out, software_encoder)?;
        }
    }
    Ok(())
}

fn print_monitors(with_modes: bool) {
    println!("Monitors:");
    for m in display::monitors() {
        let (w, h) = m.size();
        println!(
            "  {}{}  {}x{} at ({}, {}) {}%  {}  {}",
            m.device_name.trim_start_matches(r"\\.\"),
            if m.primary { " (primary)" } else { "" },
            w,
            h,
            m.rect.left,
            m.rect.top,
            m.dpi * 100 / 96,
            m.friendly,
            m.device_path
        );
        if with_modes {
            let modes: Vec<String> = display::modes(&m.device_name)
                .iter()
                .map(|(w, h, hz)| format!("{w}x{h}@{hz}"))
                .collect();
            println!("      modes: {}", modes.join(" "));
        }
    }
}

fn screenshot(display: &str, out: &str) -> Result<()> {
    use windows::Win32::Graphics::Direct3D11::*;
    let monitor = ExistingMonitor::new(app::monitor_name(display), false)
        .ensure(&desktop::OutputMode { width: 0, height: 0, hz: 0, scale: 1.0 })?;
    let mut cap = capture::Capturer::new(&monitor.device_name)?;
    // The first duplicated frame can be empty; take the latest real update
    // within a short window (a static desktop still delivers one at start).
    let start = Instant::now();
    while start.elapsed() < Duration::from_millis(1500) {
        cap.grab(100)?;
    }
    if !cap.have_frame {
        bail!("no desktop image");
    }
    let (w, h) = (cap.width, cap.height);
    let mut rgba = vec![0u8; (w * h * 4) as usize];
    unsafe {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        cap.frame.GetDesc(&mut desc);
        let desc = D3D11_TEXTURE2D_DESC {
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
            ..desc
        };
        let mut staging = None;
        cap.gpu.device.CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.context("no staging texture")?;
        cap.gpu.context.CopyResource(&staging, &cap.frame);
        let mut map = D3D11_MAPPED_SUBRESOURCE::default();
        cap.gpu.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut map))?;
        for y in 0..h as usize {
            let row = std::slice::from_raw_parts((map.pData as *const u8).add(y * map.RowPitch as usize), w as usize * 4);
            for (x, px) in row.chunks(4).enumerate() {
                let o = (y * w as usize + x) * 4;
                rgba[o..o + 4].copy_from_slice(&[px[2], px[1], px[0], 255]);
            }
        }
        cap.gpu.context.Unmap(&staging, 0);
    }
    let file = std::fs::File::create(out).with_context(|| format!("creating {out}"))?;
    let mut enc = png::Encoder::new(std::io::BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgba)?;
    println!("{out}: {w}x{h} of {} at ({}, {})", monitor.device_name, monitor.rect.left, monitor.rect.top);
    Ok(())
}

fn probe(display: &str, width: u32, height: u32, frames: u32, out: &str, software: bool) -> Result<()> {
    encoder::mf_startup()?;
    let monitor = ExistingMonitor::new(app::monitor_name(display), false).ensure(&desktop::OutputMode { width, height, hz: 60, scale: 1.0 })?;
    let mut cap = capture::Capturer::new(&monitor.device_name)?;
    let mut conv = encoder::Converter::new(&cap.gpu, cap.width, cap.height, width, height, 60)?;
    let mut enc = encoder::Encoder::new(
        &cap.gpu,
        &encoder::EncoderConfig { width, height, fps: 60, bitrate: 8_000_000, prefer_software: software, low_power: false },
    )?;
    println!("capturing {} ({}x{}) with {}", monitor.device_name, cap.width, cap.height, enc.name);
    let mut file = std::fs::File::create(out).with_context(|| format!("creating {out}"))?;
    let (mut n, mut bytes, mut idrs) = (0u32, 0usize, 0u32);
    let start = Instant::now();
    while n < frames {
        cap.grab(20)?;
        if !cap.have_frame {
            if start.elapsed() > Duration::from_secs(5) {
                bail!("no desktop image within 5 s");
            }
            continue;
        }
        let nv12 = conv.convert(&cap.frame)?;
        // An IDR first and every 30 frames, to exercise forced keyframes.
        for e in enc.encode(&nv12, n % 30 == 0)? {
            file.write_all(&e.annexb)?;
            bytes += e.annexb.len();
            idrs += e.idr as u32;
        }
        n += 1;
        std::thread::sleep(Duration::from_millis(16));
    }
    println!(
        "wrote {out}: {n} frames, {idrs} IDR, {bytes} bytes in {:.2}s",
        start.elapsed().as_secs_f64()
    );
    Ok(())
}
