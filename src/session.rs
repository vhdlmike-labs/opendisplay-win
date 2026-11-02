//! One sender run: dial, drive `opendisplay_session::SenderSession`, keep the
//! display, capture/encode pipeline, cursor and input in step with it, and
//! redial when the link drops (PROTOCOL.md §8.2, §9).

use std::io::{Read, Write};
use std::net::{IpAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use opendisplay_proto::control::{CursorImg, Ping, StreamConfig};
use opendisplay_session::sender::{SenderAction, SenderConfig, SenderEvent, SenderSession};
use opendisplay_session::{CloseReason, Now};
use tracing::{debug, error, info, warn};
use windows::Win32::Foundation::RECT;

use crate::capture::{Capturer, Grab};
use crate::cursor::{self, CursorPos};
use crate::desktop::{self, DesktopOptions, DesktopPlan, Debounce, HELLO_DEBOUNCE};
use crate::discover::Target;
use crate::display::{DisplayBackend, Monitor};
use crate::encoder::{self, Converter, Encoder, EncoderConfig};
use crate::input::Injector;

/// Encoded frames allowed in the writer queue before capture waits. Frames
/// are never dropped after encoding (that would break P-frame references);
/// a slow link lowers the capture rate instead.
const MAX_VIDEO_IN_FLIGHT: usize = 2;

#[derive(Clone)]
pub struct Options {
    pub desktop: DesktopOptions,
    pub bitrate: Option<u32>,
    pub software_encoder: bool,
    /// Exit when the receiver says `closing` instead of waiting for it.
    pub exit_on_close: bool,
    /// Told about state changes (the tray app shows them).
    pub status: Option<StatusFn>,
    /// Battery saver: lower bitrate, fastest encoder preset, slower cursor
    /// polling. (The frame-rate cap is applied through `desktop.max_fps`.)
    pub power_saving: bool,
}

/// Battery saver: frame-rate cap and bitrate share.
pub const POWER_SAVING_FPS: f32 = 30.0;
const POWER_SAVING_BITRATE: f64 = 0.6;

/// What the sender is doing, for a UI.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Looking for / dialing the receiver.
    Searching,
    /// Streaming to `device` (e.g. "iPad").
    Streaming { device: String, usb: bool, extended: bool, width: u32, height: u32 },
    /// The device is locked; it reconnects when unlocked.
    DeviceAsleep,
    /// The OpenDisplay app was closed on the device.
    AppClosed,
    Stopped,
}

pub type StatusFn = Arc<dyn Fn(Status) + Send + Sync>;

enum Event {
    Bytes(Vec<u8>),
    Eof(String),
    Encoded { annexb: Vec<u8>, idr: bool, cap_ms: f64 },
    PipelineFps(f64),
    Cursor(CursorPos),
    CursorImg(CursorImg),
    /// The display for `plan` is ready (or could not be prepared).
    Display { plan: DesktopPlan, result: Result<(Monitor, bool), String> },
}

/// The display backend, shared with the thread that prepares displays:
/// attaching/resizing a monitor can take seconds, and the session must keep
/// answering pings meanwhile (the receiver drops a sender silent for 5 s).
pub type SharedDisplay = Arc<Mutex<Box<dyn DisplayBackend>>>;

enum Outcome {
    /// A USB session reached a device other than the one asked for.
    WrongDevice,
    /// Ctrl+C / `--for` on this side.
    Stopped,
    Lost,
    Sleeping,
    Closing,
    /// Another Windows session took over the screen (fast user switching).
    Inactive,
}

/// Whether this process runs in the Windows session at the physical screen.
/// With several users signed in, each may run OpenDisplay ("Start with
/// Windows" is per user); the per-session lock can't stop them, and the
/// receiver always adopts the newest connection, so only the session at the
/// screen may stream.
pub fn session_is_active() -> bool {
    use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSGetActiveConsoleSessionId};
    use windows::Win32::System::Threading::GetCurrentProcessId;
    unsafe {
        let mut mine = 0u32;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut mine).is_err() {
            return true;
        }
        // 0xFFFFFFFF: between sessions (switching users); wait it out.
        WTSGetActiveConsoleSessionId() == mine
    }
}

pub struct Clock(Instant);

impl Clock {
    fn now(&self) -> Now {
        let wall = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64() * 1000.0;
        Now::new(self.0.elapsed(), wall)
    }
}

fn wall_ms() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64() * 1000.0
}

pub fn run(target: Target, backend: Box<dyn DisplayBackend>, opts: Options, stop: Arc<AtomicBool>) -> Result<()> {
    let report = |s: Status| {
        if let Some(f) = &opts.status {
            f(s)
        }
    };
    let backend: SharedDisplay = Arc::new(Mutex::new(backend));
    let release = || backend.lock().unwrap_or_else(|e| e.into_inner()).release();
    let clock = Clock(Instant::now());
    let mut failures = 0u32;
    let mut allow_usb = true;
    // Sessions the receiver ended within seconds of starting, in a row.
    let mut short_lived = 0u32;
    // Sessions in a row that began while another sender held the receiver.
    let mut contested = 0u32;
    let mut waiting_for_screen = false;
    while !stop.load(Ordering::Relaxed) {
        if !session_is_active() {
            if !waiting_for_screen {
                info!("another Windows user is at the screen; waiting until this session is active again");
                report(Status::Searching);
                release();
                waiting_for_screen = true;
            }
            std::thread::sleep(Duration::from_secs(1));
            continue;
        }
        if waiting_for_screen {
            info!("this Windows session is active again");
            waiting_for_screen = false;
            failures = 0;
        }
        if failures == 0 {
            info!("dialing {}", target.describe());
        }
        let (stream, usb) = match target.dial(allow_usb) {
            Ok(s) => s,
            Err(e) => {
                failures += 1;
                if failures == 1 || failures % 10 == 0 {
                    warn!("{e:#}");
                }
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        failures = 0;
        let peer = stream.peer_addr().ok().map(|a| a.ip());
        info!("connected over {}", if usb { "USB" } else { "Wi-Fi/LAN" });
        let started = Instant::now();
        let mut rival = false;
        let outcome = connection(&clock, stream, peer, usb, target.expected_id(), &backend, &opts, &stop, &mut rival);
        if rival && matches!(outcome, Ok(Outcome::Lost)) {
            contested += 1;
            if contested == 3 {
                warn!(
                    "another sender keeps taking this device over (it was connected to it each time this \
                     sender reconnected). Quit OpenDisplay on the other sender: another Windows user \
                     signed in on this PC, another PC, or a Mac"
                );
            }
        } else {
            contested = 0;
        }
        match outcome {
            Ok(Outcome::WrongDevice) => {
                warn!("the device on USB is not the requested receiver; using Wi-Fi");
                allow_usb = false;
                continue;
            }
            Ok(Outcome::Stopped) => break,
            Ok(Outcome::Closing) => {
                info!("receiver closed the app");
                report(Status::AppClosed);
                if opts.exit_on_close {
                    break;
                }
                release();
            }
            Ok(Outcome::Inactive) => {
                // The loop above waits for the screen to come back.
                report(Status::Searching);
            }
            Ok(Outcome::Sleeping) => {
                info!("receiver is asleep; waiting for it to come back");
                report(Status::DeviceAsleep);
                release();
            }
            Ok(Outcome::Lost) => {
                info!("connection lost; redialing");
                report(Status::Searching);
                if started.elapsed() < Duration::from_secs(5) {
                    short_lived += 1;
                    if short_lived == 3 {
                        warn!(
                            "the receiver keeps dropping this sender right after connecting. Is another \
                             sender (a second opendisplay-win, or a Mac) connected to it? A receiver \
                             always switches to the newest connection (PROTOCOL.md §1)"
                        );
                    }
                } else {
                    short_lived = 0;
                }
            }
            Err(e) => error!("session failed: {e:#}"),
        }
        std::thread::sleep(Duration::from_secs(1));
    }
    release();
    Ok(())
}

/// The capture -> convert -> encode thread for one plan.
struct Pipeline {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Pipeline {
    #[allow(clippy::too_many_arguments)]
    fn start(
        device_name: String,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        software: bool,
        low_power: bool,
        force_idr: Arc<AtomicBool>,
        in_flight: Arc<AtomicUsize>,
        events: Sender<Event>,
    ) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let handle = std::thread::Builder::new()
            .name("pipeline".into())
            .spawn(move || {
                if let Err(e) = encoder::mf_startup() {
                    error!("{e:#}");
                    return;
                }
                let cfg = EncoderConfig { width, height, fps, bitrate, prefer_software: software, low_power };
                while !s.load(Ordering::Relaxed) {
                    if let Err(e) = pipeline_loop(&device_name, &cfg, &s, &force_idr, &in_flight, &events) {
                        warn!("capture/encode restarting: {e:#}");
                        force_idr.store(true, Ordering::Relaxed);
                        std::thread::sleep(Duration::from_millis(500));
                    }
                }
            })
            .expect("spawn pipeline");
        Self { stop, handle: Some(handle) }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn pipeline_loop(
    device_name: &str,
    cfg: &EncoderConfig,
    stop: &AtomicBool,
    force_idr: &AtomicBool,
    in_flight: &AtomicUsize,
    events: &Sender<Event>,
) -> Result<()> {
    let mut cap = Capturer::new(device_name)?;
    let mut conv = Converter::new(&cap.gpu, cap.width, cap.height, cfg.width, cfg.height, cfg.fps)?;
    warn_if_discrete(&cap.gpu);
    let mut enc = Encoder::new(&cap.gpu, cfg)?;
    info!(
        "capturing {device_name} {}x{} on {} -> {} {}x{} @{} {:.1} Mbit/s",
        cap.width, cap.height, cap.gpu.adapter, enc.name, cfg.width, cfg.height, enc.fps, cfg.bitrate as f64 / 1e6
    );
    let interval = Duration::from_secs_f64(1.0 / enc.fps as f64);
    let mut next_due = Instant::now();
    let mut dirty = false;
    let mut cap_ms = wall_ms();
    let (mut frames, mut window) = (0u32, Instant::now());
    while !stop.load(Ordering::Relaxed) {
        let wait = next_due.saturating_duration_since(Instant::now()).max(Duration::from_millis(1));
        match cap.grab(wait.as_millis().min(50) as u32)? {
            Grab::New => {
                dirty = true;
                cap_ms = wall_ms();
            }
            Grab::Idle => {}
        }
        if !cap.have_frame {
            continue;
        }
        let idr = force_idr.load(Ordering::Relaxed);
        if !(dirty || idr) || Instant::now() < next_due {
            continue;
        }
        if in_flight.load(Ordering::Relaxed) >= MAX_VIDEO_IN_FLIGHT {
            continue; // the link is behind; encode the latest image once it drains
        }
        if idr {
            force_idr.store(false, Ordering::Relaxed);
        }
        dirty = false;
        next_due = Instant::now().max(next_due) + interval;
        let nv12 = conv.convert(&cap.frame)?;
        for e in enc.encode(&nv12, idr)? {
            in_flight.fetch_add(1, Ordering::Relaxed);
            frames += 1;
            if events.send(Event::Encoded { annexb: e.annexb, idr: e.idr, cap_ms }).is_err() {
                return Ok(());
            }
        }
        if window.elapsed() >= Duration::from_secs(2) {
            let _ = events.send(Event::PipelineFps(frames as f64 / window.elapsed().as_secs_f64()));
            frames = 0;
            window = Instant::now();
        }
    }
    Ok(())
}

/// A virtual monitor rendered on the discrete GPU keeps it awake (and the
/// driver picks the GPU with the most memory unless told otherwise).
fn warn_if_discrete(gpu: &crate::capture::Gpu) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if let Some((name, luid)) = crate::capture::power_saving_adapter() {
        if !crate::capture::same_luid(luid, gpu.luid) && !WARNED.swap(true, Ordering::Relaxed) {
            warn!(
                "the screen being streamed renders on {}, not on the power-saving {name}; \
                 run \"Install virtual display.cmd\" (or `opendisplay-win install-driver`) again \
                 to move the virtual monitor to {name}",
                gpu.adapter
            );
        }
    }
}

/// Polls the pointer and sprite for the streamed monitor.
fn cursor_thread(
    rect: Arc<Mutex<RECT>>,
    resend: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
    events: Sender<Event>,
    poll: Duration,
) {
    let mut last: Option<CursorPos> = None;
    let mut last_handle = None;
    // Re-send a still pointer now and then: a lost first datagram would
    // otherwise leave the UDP channel unacknowledged and abandoned (§6.3).
    let mut last_sent = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) {
        let r = *rect.lock().unwrap();
        let (pos, handle) = cursor::poll(&r);
        if resend.swap(false, Ordering::Relaxed) {
            last = None;
            last_handle = None;
        }
        if let Some(h) = handle {
            if pos.visible && last_handle != Some(h.0 as usize) {
                last_handle = Some(h.0 as usize);
                if let Some(img) = cursor::sprite(h, &r) {
                    let _ = events.send(Event::CursorImg(img));
                }
            }
        }
        let moved = match last {
            None => true,
            Some(l) => {
                l.visible != pos.visible
                    || (pos.visible && ((l.x - pos.x).abs() > 0.0002 || (l.y - pos.y).abs() > 0.0002))
            }
        };
        if moved || last_sent.elapsed() >= Duration::from_millis(500) {
            last = Some(pos);
            last_sent = std::time::Instant::now();
            if events.send(Event::Cursor(pos)).is_err() {
                return;
            }
        }
        std::thread::sleep(poll);
    }
}

fn default_bitrate(w: u32, h: u32, fps: u32, usb: bool) -> u32 {
    let bits = w as f64 * h as f64 * fps as f64 * 0.09;
    let (lo, hi) = if usb { (8e6, 80e6) } else { (6e6, 40e6) };
    bits.clamp(lo, hi) as u32
}

/// Fit an existing monitor's raster inside the planned stream bounds.
fn fit_stream(src: (u32, u32), bound: (u32, u32)) -> (u32, u32) {
    let s = (bound.0 as f64 / src.0 as f64).min(bound.1 as f64 / src.1 as f64).min(1.0);
    let even = |v: f64| ((v as u32) & !1).max(2);
    (even(src.0 as f64 * s), even(src.1 as f64 * s))
}

fn prepare_display(backend: SharedDisplay, plan: DesktopPlan, tx: Sender<Event>) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut b = backend.lock().unwrap_or_else(|e| e.into_inner());
        let result = b
            .ensure(&plan.output)
            .map(|m| (m, b.follows_plan()))
            .map_err(|e| format!("{}: {e:#}", b.name()));
        let _ = tx.send(Event::Display { plan, result });
    })
}

#[allow(clippy::too_many_arguments)]
fn connection(
    clock: &Clock,
    stream: TcpStream,
    peer: Option<IpAddr>,
    usb: bool,
    expected_id: Option<&str>,
    backend: &SharedDisplay,
    opts: &Options,
    stop: &AtomicBool,
    rival: &mut bool,
) -> Result<Outcome> {
    let (tx, rx): (Sender<Event>, Receiver<Event>) = mpsc::channel();
    let connected_at = Instant::now();
    let mut hellos = 0u32;
    let mut active_checked = Instant::now();

    // Reader.
    let mut reader = stream.try_clone()?;
    let rtx = tx.clone();
    std::thread::Builder::new().name("reader".into()).spawn(move || {
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => {
                    let _ = rtx.send(Event::Eof("closed by receiver".into()));
                    return;
                }
                Ok(n) => {
                    if rtx.send(Event::Bytes(buf[..n].to_vec())).is_err() {
                        return;
                    }
                }
                Err(e) => {
                    let _ = rtx.send(Event::Eof(e.to_string()));
                    return;
                }
            }
        }
    })?;

    // Writer: one ordered queue; video frames count against `in_flight`.
    let in_flight = Arc::new(AtomicUsize::new(0));
    let (wtx, wrx) = mpsc::channel::<(Vec<u8>, bool)>();
    let mut writer = stream.try_clone()?;
    let wflight = in_flight.clone();
    let wtx_err = tx.clone();
    let writer_thread = std::thread::Builder::new().name("writer".into()).spawn(move || {
        while let Ok((bytes, video)) = wrx.recv() {
            let r = writer.write_all(&bytes);
            if video {
                wflight.fetch_sub(1, Ordering::Relaxed);
            }
            if let Err(e) = r {
                let _ = wtx_err.send(Event::Eof(format!("write: {e}")));
                return;
            }
        }
    })?;

    // Same source address as the TCP session, so the receiver sees one host.
    let udp = match (usb, stream.local_addr()) {
        (false, Ok(local)) => UdpSocket::bind((local.ip(), 0)).ok(),
        _ => None,
    };
    let mut session = SenderSession::new(SenderConfig {
        use_cursor_udp: udp.is_some(),
        ..Default::default()
    });
    let force_idr = Arc::new(AtomicBool::new(true));
    let cursor_rect = Arc::new(Mutex::new(RECT::default()));
    let cursor_resend = Arc::new(AtomicBool::new(false));
    let cursor_stop = Arc::new(AtomicBool::new(false));
    let mut cursor_handle: Option<JoinHandle<()>> = None;
    let mut injector: Option<Injector> = None;
    let mut pipeline: Option<Pipeline> = None;
    let mut debounce: Debounce<DesktopPlan> = Debounce::new(HELLO_DEBOUNCE);
    let mut capture_fps = 0.0;
    let mut display_job: Option<JoinHandle<()>> = None;
    let mut device_name = String::from("device");
    let mut queued_plan: Option<DesktopPlan> = None;

    let mut pending = session.handle(clock.now(), SenderEvent::Connected);
    let outcome = 'outer: loop {
        if stop.load(Ordering::Relaxed) {
            break Outcome::Stopped;
        }
        // Execute what the session asked for.
        let mut plan_now: Option<DesktopPlan> = None;
        for action in pending.drain(..) {
            match action {
                SenderAction::Send(bytes) => {
                    let _ = wtx.send((bytes, false));
                }
                SenderAction::SendCursorDatagram(d) => {
                    if let (Some(u), Some(ip), Some(port)) = (&udp, peer, session.cursor_udp_port()) {
                        if let Err(e) = u.send_to(&d, (ip, port)) {
                            debug!("cursor datagram to {ip}:{port}: {e}");
                        }
                    }
                }
                SenderAction::Hello(h) => {
                    // A receiver that is still connected to another sender
                    // greets a newcomer twice: provisionally, then again once
                    // the newcomer has proved itself and is adopted.
                    hellos += 1;
                    if hellos == 2 && connected_at.elapsed() < Duration::from_millis(500) {
                        *rival = true;
                        debug!("the receiver was connected to another sender");
                    }
                    device_name = h.device.clone().unwrap_or_else(|| "device".into());
                    if let (true, Some(want), Some(got)) = (usb, expected_id, h.id.as_deref()) {
                        if !want.eq_ignore_ascii_case(got) {
                            break 'outer Outcome::WrongDevice;
                        }
                    }
                    info!(
                        "hello from {} ({}), pv {}",
                        h.device.as_deref().unwrap_or("receiver"),
                        h.id.as_deref().unwrap_or("-"),
                        h.pv.unwrap_or(1)
                    );
                    match desktop::plan(&h, &opts.desktop) {
                        Ok(p) => {
                            info!("plan: {p}");
                            if let Some(p) = debounce.on_hello(p, Instant::now()) {
                                plan_now = Some(p);
                            }
                        }
                        Err(e) => error!("cannot stream to this receiver: {e}"),
                    }
                }
                SenderAction::RequestKeyframe => force_idr.store(true, Ordering::Relaxed),
                SenderAction::Touch(t) => {
                    if let Some(i) = injector.as_mut() {
                        i.touch(&t)
                    }
                }
                SenderAction::Scroll(s) => {
                    if let Some(i) = injector.as_mut() {
                        i.scroll(&s)
                    }
                }
                SenderAction::Pencil(p) => {
                    if let Some(i) = injector.as_mut() {
                        i.pencil(&p)
                    }
                }
                SenderAction::Proximity(p) => {
                    if let Some(i) = injector.as_mut() {
                        i.proximity(&p)
                    }
                }
                SenderAction::Stats(v) => info!("PHONE-STATS {v}"),
                SenderAction::ReceiverSleeping => break 'outer Outcome::Sleeping,
                SenderAction::ReceiverClosing => break 'outer Outcome::Closing,
                SenderAction::Close(CloseReason::Timeout) => {
                    warn!("receiver silent for over 5 s");
                    break 'outer Outcome::Lost;
                }
                SenderAction::Close(r) => {
                    warn!("closing: {r:?}");
                    break 'outer Outcome::Lost;
                }
                SenderAction::Warn(w) => warn!("{w}"),
            }
        }
        if plan_now.is_none() {
            plan_now = debounce.poll(Instant::now());
        }

        // A new plan: prepare its display off this thread (see SharedDisplay).
        if let Some(plan) = plan_now {
            if display_job.is_some() {
                queued_plan = Some(plan);
            } else {
                drop(pipeline.take());
                display_job = Some(prepare_display(backend.clone(), plan, tx.clone()));
            }
            continue;
        }

        let timeout = debounce
            .deadline()
            .map(|d| d.saturating_duration_since(Instant::now()))
            .unwrap_or(Duration::from_millis(100))
            .min(Duration::from_millis(100));
        let now = clock.now();
        pending = match rx.recv_timeout(timeout) {
            Ok(Event::Bytes(b)) => session.handle(now, SenderEvent::Bytes(&b)),
            Ok(Event::Display { plan, result }) => {
                if let Some(h) = display_job.take() {
                    let _ = h.join();
                }
                let (monitor, follows_plan) = match result {
                    Ok(r) => r,
                    Err(e) => {
                        error!("display: {e}");
                        break Outcome::Lost;
                    }
                };
                if let Some(next) = queued_plan.take() {
                    // Superseded while it was being prepared (e.g. rotation).
                    display_job = Some(prepare_display(backend.clone(), next, tx.clone()));
                    continue;
                }
                let (sw, sh) = if follows_plan {
                    (plan.stream.width, plan.stream.height)
                } else {
                    fit_stream(monitor.size(), (plan.stream.width, plan.stream.height))
                };
                let fps = plan.stream.fps;
                let bitrate = opts.bitrate.unwrap_or_else(|| {
                    let b = default_bitrate(sw, sh, fps, usb);
                    if opts.power_saving { (b as f64 * POWER_SAVING_BITRATE) as u32 } else { b }
                });
                if opts.power_saving {
                    info!("battery saver: {fps} fps cap, {:.1} Mbit/s, fastest encoder preset", bitrate as f64 / 1e6);
                }
                info!(
                    "streaming {} {} ({}x{} at {:?}) as {sw}x{sh}@{fps}",
                    monitor.device_name, monitor.friendly, monitor.size().0, monitor.size().1,
                    (monitor.rect.left, monitor.rect.top)
                );
                if let Some(f) = &opts.status {
                    f(Status::Streaming {
                        device: device_name.clone(),
                        usb,
                        extended: crate::display::looks_virtual(&monitor),
                        width: monitor.size().0,
                        height: monitor.size().1,
                    });
                }
                pending.push(session.stream_config(StreamConfig {
                    codec: desktop::H264.into(),
                    width: sw,
                    height: sh,
                    frames_per_second: fps,
                }));
                *cursor_rect.lock().unwrap() = monitor.rect;
                cursor_resend.store(true, Ordering::Relaxed);
                match injector.as_mut() {
                    Some(i) => i.set_rect(monitor.rect),
                    None => injector = Some(Injector::new(monitor.rect)),
                }
                if cursor_handle.is_none() {
                    let (r, s, st, t) = (cursor_rect.clone(), cursor_resend.clone(), cursor_stop.clone(), tx.clone());
                    let poll = Duration::from_millis(if opts.power_saving { 16 } else { 8 });
                    cursor_handle = Some(std::thread::spawn(move || cursor_thread(r, s, st, t, poll)));
                }
                force_idr.store(true, Ordering::Relaxed);
                pipeline = Some(Pipeline::start(
                    monitor.device_name.clone(),
                    sw,
                    sh,
                    fps,
                    bitrate,
                    opts.software_encoder,
                    opts.power_saving,
                    force_idr.clone(),
                    in_flight.clone(),
                    tx.clone(),
                ));
                Vec::new()
            }
            Ok(Event::Eof(why)) => {
                info!("link closed: {why}");
                break Outcome::Lost;
            }
            Ok(Event::Encoded { annexb, idr, cap_ms }) => {
                let actions = session.push_encoded_frame(now, &annexb, idr, Some(cap_ms));
                if actions.is_empty() {
                    in_flight.fetch_sub(1, Ordering::Relaxed);
                }
                for a in actions {
                    if let SenderAction::Send(bytes) = a {
                        let _ = wtx.send((bytes, true));
                    }
                }
                Vec::new()
            }
            Ok(Event::PipelineFps(f)) => {
                capture_fps = f;
                debug!("capture {f:.1} fps, {} frames sent", session.counters.video_frames);
                Vec::new()
            }
            Ok(Event::Cursor(p)) => session.cursor(now, p.x, p.y, p.visible),
            Ok(Event::CursorImg(img)) => {
                if session.ready_for_video() {
                    vec![session.cursor_image(img)]
                } else {
                    Vec::new()
                }
            }
            Err(RecvTimeoutError::Timeout) => {
                session.set_health(Ping {
                    cap_fps: Some(capture_fps),
                    pending: Some(in_flight.load(Ordering::Relaxed) as f64),
                    ..Default::default()
                });
                session.handle(now, SenderEvent::Tick)
            }
            Err(RecvTimeoutError::Disconnected) => break Outcome::Lost,
        };
        // A busy queue never times out: tick on every event so pings and
        // the liveness check stay on schedule (the session pings only when due).
        let mut t = session.handle(clock.now(), SenderEvent::Tick);
        pending.append(&mut t);
        if active_checked.elapsed() >= Duration::from_secs(1) {
            active_checked = Instant::now();
            if !session_is_active() {
                info!("another Windows user is at the screen; disconnecting");
                break Outcome::Inactive;
            }
        }
    };

    session.handle(clock.now(), SenderEvent::Disconnected);
    if let Some(h) = display_job {
        let _ = h.join();
    }
    drop(pipeline);
    cursor_stop.store(true, Ordering::Relaxed);
    if let Some(h) = cursor_handle {
        let _ = h.join();
    }
    drop(injector);
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(wtx);
    let _ = writer_thread.join();
    Ok(outcome)
}
