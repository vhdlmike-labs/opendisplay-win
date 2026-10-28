//! OpenDisplay tray app: runs the sender in the background with an icon in
//! the notification area (status, options, quit) instead of a console window.

#![windows_subsystem = "windows"]
#![allow(non_snake_case)]

use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use opendisplay_win::app::{self, RunConfig, SingleInstance};
use opendisplay_win::driver;
use opendisplay_win::session::Status;
use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Controls::{LIM_SMALL, LoadIconMetric};
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

const WM_TRAY: u32 = WM_APP + 1;
const WM_STATUS: u32 = WM_APP + 2;
/// Open the menu next to the icon (keyboard activation, scripted screenshots).
const WM_MENU_AT_ICON: u32 = WM_APP + 3;

const ID_STATUS: usize = 1;
const ID_EXTEND: usize = 10;
const ID_MIRROR: usize = 11;
const ID_CONN_AUTO: usize = 20;
const ID_CONN_USB: usize = 21;
const ID_CONN_WIFI: usize = 22;
const ID_SAVER_OFF: usize = 40;
const ID_SAVER_ON: usize = 41;
const ID_SAVER_AUTO: usize = 42;
const ID_AUTOSTART: usize = 30;
const ID_LOG: usize = 31;
const ID_OPEN_LOG: usize = 32;
const ID_INSTALL_DRIVER: usize = 33;
const ID_QUIT: usize = 99;

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const RUN_VALUE: PCWSTR = w!("OpenDisplay");

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_into(dst: &mut [u16], s: &str) {
    let src: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..src.len()].copy_from_slice(&src);
    dst[src.len()] = 0;
}

// --- settings ---------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum DisplayMode {
    #[default]
    Extend,
    Mirror,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum Connection {
    #[default]
    Auto,
    Usb,
    Wifi,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
enum BatterySaver {
    #[default]
    Off,
    On,
    /// On while the PC runs on battery or Windows' energy saver is on.
    Auto,
}

/// Running on battery, or Windows' battery/energy saver is on.
fn on_battery() -> bool {
    let mut ps = windows::Win32::System::Power::SYSTEM_POWER_STATUS::default();
    let ok = unsafe { windows::Win32::System::Power::GetSystemPowerStatus(&mut ps).is_ok() };
    ok && (ps.ACLineStatus == 0 || ps.SystemStatusFlag == 1)
}

#[derive(Clone, Serialize, Deserialize, Default)]
#[serde(default)]
struct Settings {
    display: DisplayMode,
    connection: Connection,
    battery_saver: BatterySaver,
    log_file: bool,
}

fn data_dir(var: &str) -> PathBuf {
    let base = std::env::var_os(var).map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    base.join("OpenDisplay")
}

fn settings_path() -> PathBuf {
    data_dir("APPDATA").join("settings.json")
}

fn log_path() -> PathBuf {
    data_dir("LOCALAPPDATA").join("opendisplay.log")
}

impl Settings {
    fn load() -> Self {
        std::fs::read(settings_path())
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    fn save(&self) {
        let p = settings_path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        if let Ok(j) = serde_json::to_vec_pretty(self) {
            let _ = std::fs::write(p, j);
        }
    }

    fn saver_active(&self) -> bool {
        match self.battery_saver {
            BatterySaver::Off => false,
            BatterySaver::On => true,
            BatterySaver::Auto => on_battery(),
        }
    }

    fn run_config(&self) -> RunConfig {
        RunConfig {
            battery_saver: self.saver_active(),
            to: match self.connection {
                Connection::Auto => None,
                Connection::Usb => Some("usb".into()),
                Connection::Wifi => Some("wifi".into()),
            },
            display: match self.display {
                DisplayMode::Extend => "auto".into(),
                DisplayMode::Mirror => "primary".into(),
            },
            ..RunConfig::default()
        }
    }
}

// --- log file (off unless enabled in the menu) -------------------------------

static LOG_ON: AtomicBool = AtomicBool::new(false);
static LOG_FILE: Mutex<Option<std::fs::File>> = Mutex::new(None);

struct GatedLog;

impl Write for GatedLog {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if LOG_ON.load(Ordering::Relaxed) {
            let mut f = LOG_FILE.lock().unwrap_or_else(|e| e.into_inner());
            if f.is_none() {
                let p = log_path();
                let _ = std::fs::create_dir_all(p.parent().unwrap());
                // Start over when it gets big.
                if std::fs::metadata(&p).is_ok_and(|m| m.len() > 5 << 20) {
                    let _ = std::fs::remove_file(&p);
                }
                *f = std::fs::OpenOptions::new().create(true).append(true).open(p).ok();
            }
            if let Some(f) = f.as_mut() {
                let _ = f.write_all(buf);
            }
        }
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn init_logging() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::new("info"))
        .with_target(false)
        .with_ansi(false)
        .with_writer(|| GatedLog)
        .init();
}

// --- the sender, on a controller thread -----------------------------------

enum Control {
    Apply(RunConfig),
    Quit(Sender<()>),
}

#[derive(Clone, PartialEq)]
enum TrayStatus {
    Starting,
    Status(Status),
    Error(String),
}

static STATUS: Mutex<TrayStatus> = Mutex::new(TrayStatus::Starting);
static HWND_TRAY: AtomicIsize = AtomicIsize::new(0);

fn post_status(s: TrayStatus) {
    *STATUS.lock().unwrap_or_else(|e| e.into_inner()) = s;
    let h = HWND_TRAY.load(Ordering::Relaxed);
    if h != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(h as _)), WM_STATUS, WPARAM(0), LPARAM(0));
        }
    }
}

fn controller(rx: Receiver<Control>) {
    let mut current: Option<(Arc<AtomicBool>, JoinHandle<()>)> = None;
    let stop_current = |c: &mut Option<(Arc<AtomicBool>, JoinHandle<()>)>| {
        if let Some((stop, h)) = c.take() {
            stop.store(true, Ordering::Relaxed);
            let _ = h.join();
        }
    };
    while let Ok(cmd) = rx.recv() {
        match cmd {
            Control::Apply(cfg) => {
                stop_current(&mut current);
                let stop = Arc::new(AtomicBool::new(false));
                let s = stop.clone();
                let handle = std::thread::spawn(move || {
                    let status: opendisplay_win::session::StatusFn =
                        Arc::new(|st: Status| post_status(TrayStatus::Status(st)));
                    if let Err(e) = app::run(&cfg, s, Some(status)) {
                        tracing::error!("{e:#}");
                        post_status(TrayStatus::Error(format!("{e:#}")));
                    }
                });
                current = Some((stop, handle));
            }
            Control::Quit(ack) => {
                stop_current(&mut current);
                let _ = ack.send(());
                return;
            }
        }
    }
}

// --- UI state (UI thread only) ------------------------------------------------

struct Ui {
    hwnd: HWND,
    hinst: HINSTANCE,
    settings: Settings,
    control: Sender<Control>,
    last: TrayStatus,
    /// Battery saver state the running session was started with.
    saver_applied: bool,
}

thread_local! {
    static UI: std::cell::RefCell<Option<Ui>> = const { std::cell::RefCell::new(None) };
}

/// Run `f` on the UI state. Never call Win32 functions that pump messages
/// (TrackPopupMenu, MessageBox, waiting for the controller) inside `f`: the
/// window procedure re-enters and would find the state borrowed. A nested
/// call gets `None` instead of a panic.
fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|u| match u.try_borrow_mut() {
        Ok(mut ui) => ui.as_mut().map(f),
        Err(_) => {
            tracing::warn!("UI state busy; event dropped");
            None
        }
    })
}

static TASKBAR_CREATED: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// What a menu command asks for beyond changing settings; done by the caller
/// after the UI state is released.
enum Action {
    None,
    Quit,
}

impl Ui {
    fn status_text(&self) -> (String, u16) {
        let (text, icon) = self.base_status_text();
        if self.saver_applied && matches!(self.last, TrayStatus::Status(Status::Streaming { .. })) {
            (format!("{text} (battery saver)"), icon)
        } else {
            (text, icon)
        }
    }

    fn base_status_text(&self) -> (String, u16) {
        let mirror_chosen = self.settings.display == DisplayMode::Mirror;
        match &self.last {
            TrayStatus::Starting | TrayStatus::Status(Status::Searching) => (
                match self.settings.connection {
                    Connection::Usb => "Waiting for an iPad/iPhone on USB".into(),
                    Connection::Wifi => "Looking for an iPad/iPhone on Wi-Fi".into(),
                    Connection::Auto => "Waiting for an iPad/iPhone (open OpenDisplay on it)".into(),
                },
                1,
            ),
            TrayStatus::Status(Status::Streaming { device, usb, extended, width, height }) => {
                let link = if *usb { "USB" } else { "Wi-Fi" };
                if *extended {
                    (format!("{device} over {link}: second screen {width}x{height}"), 2)
                } else {
                    (format!("{device} over {link}: mirroring the main screen"), if mirror_chosen { 2 } else { 3 })
                }
            }
            TrayStatus::Status(Status::DeviceAsleep) => ("Device locked; reconnects when unlocked".into(), 1),
            TrayStatus::Status(Status::AppClosed) => ("OpenDisplay app closed on the device; waiting".into(), 1),
            TrayStatus::Status(Status::Stopped) => ("Stopped".into(), 1),
            TrayStatus::Error(e) => (format!("Error: {e}"), 3),
        }
    }

    fn icon(&self, id: u16) -> HICON {
        unsafe { LoadIconMetric(Some(self.hinst), PCWSTR(id as usize as *const u16), LIM_SMALL).unwrap_or_default() }
    }

    fn notify(&self, msg: NOTIFY_ICON_MESSAGE, balloon: Option<(&str, &str)>) {
        let (text, icon) = self.status_text();
        let mut nid = NOTIFYICONDATAW {
            cbSize: size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: 1,
            uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
            uCallbackMessage: WM_TRAY,
            hIcon: self.icon(icon),
            ..Default::default()
        };
        copy_into(&mut nid.szTip, &format!("OpenDisplay\n{text}"));
        if let Some((title, body)) = balloon {
            nid.uFlags |= NIF_INFO;
            nid.dwInfoFlags = NIIF_INFO;
            copy_into(&mut nid.szInfoTitle, title);
            copy_into(&mut nid.szInfo, body);
        }
        unsafe {
            let _ = Shell_NotifyIconW(msg, &nid);
            let _ = DestroyIcon(nid.hIcon);
        }
    }

    fn on_status(&mut self) {
        let new = STATUS.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let was_streaming = matches!(self.last, TrayStatus::Status(Status::Streaming { .. }));
        self.last = new;
        let balloon = match (&self.last, was_streaming) {
            (TrayStatus::Status(Status::Streaming { device, .. }), false) => {
                Some((format!("Connected to {device}"), self.status_text().0))
            }
            _ => None,
        };
        self.notify(NIM_MODIFY, balloon.as_ref().map(|(t, b)| (t.as_str(), b.as_str())));
    }

    fn apply(&mut self) {
        self.settings.save();
        self.saver_applied = self.settings.saver_active();
        LOG_ON.store(self.settings.log_file, Ordering::Relaxed);
        self.last = TrayStatus::Starting;
        self.notify(NIM_MODIFY, None);
        let _ = self.control.send(Control::Apply(self.settings.run_config()));
    }

    /// The tray menu for the current state; the caller tracks and destroys it.
    fn build_menu(&self) -> Option<HMENU> {
        unsafe {
            let menu = CreatePopupMenu().ok()?;
            let check = |on: bool| if on { MF_CHECKED } else { MF_UNCHECKED };
            let add = |m: HMENU, flags: MENU_ITEM_FLAGS, id: usize, text: &str| {
                let t = wide(text);
                let _ = AppendMenuW(m, flags, id, PCWSTR(t.as_ptr()));
            };
            add(menu, MF_STRING | MF_GRAYED, ID_STATUS, &self.status_text().0);
            add(menu, MF_SEPARATOR, 0, "");

            add(menu, MF_STRING | check(self.settings.display == DisplayMode::Extend), ID_EXTEND, "&Extend: use as a second screen");
            add(menu, MF_STRING | check(self.settings.display == DisplayMode::Mirror), ID_MIRROR, "&Mirror the main screen");
            add(menu, MF_SEPARATOR, 0, "");

            if let Ok(conn) = CreatePopupMenu() {
                let c = self.settings.connection;
                add(conn, MF_STRING | check(c == Connection::Auto), ID_CONN_AUTO, "&Automatic (USB, else Wi-Fi)");
                add(conn, MF_STRING | check(c == Connection::Usb), ID_CONN_USB, "&USB cable only");
                add(conn, MF_STRING | check(c == Connection::Wifi), ID_CONN_WIFI, "&Wi-Fi only");
                let t = wide("&Connection");
                let _ = AppendMenuW(menu, MF_POPUP, conn.0 as usize, PCWSTR(t.as_ptr()));
            }
            if let Ok(saver) = CreatePopupMenu() {
                let b = self.settings.battery_saver;
                add(saver, MF_STRING | check(b == BatterySaver::Off), ID_SAVER_OFF, "O&ff: smoothest (up to 60 fps)");
                add(saver, MF_STRING | check(b == BatterySaver::On), ID_SAVER_ON, "&On: 30 fps, less GPU and battery use");
                add(saver, MF_STRING | check(b == BatterySaver::Auto), ID_SAVER_AUTO, "&Automatic: on when running on battery");
                let label = if self.settings.saver_active() { "&Battery saver (active)" } else { "&Battery saver" };
                let t = wide(label);
                let _ = AppendMenuW(menu, MF_POPUP, saver.0 as usize, PCWSTR(t.as_ptr()));
            }
            add(menu, MF_STRING | check(autostart_enabled()), ID_AUTOSTART, "&Start with Windows");
            add(menu, MF_STRING | check(self.settings.log_file), ID_LOG, "Write &log file");
            if log_path().exists() {
                add(menu, MF_STRING, ID_OPEN_LOG, "O&pen log file");
            }
            if !driver::installed() {
                add(menu, MF_SEPARATOR, 0, "");
                add(menu, MF_STRING, ID_INSTALL_DRIVER, "&Install virtual display driver...");
            }
            add(menu, MF_SEPARATOR, 0, "");
            add(menu, MF_STRING, ID_QUIT, "&Quit OpenDisplay");

            Some(menu)
        }
    }

    fn command(&mut self, id: usize) -> Action {
        let s = &mut self.settings;
        match id {
            ID_EXTEND if s.display != DisplayMode::Extend => {
                s.display = DisplayMode::Extend;
                self.apply();
            }
            ID_MIRROR if s.display != DisplayMode::Mirror => {
                s.display = DisplayMode::Mirror;
                self.apply();
            }
            ID_CONN_AUTO | ID_CONN_USB | ID_CONN_WIFI => {
                let c = match id {
                    ID_CONN_USB => Connection::Usb,
                    ID_CONN_WIFI => Connection::Wifi,
                    _ => Connection::Auto,
                };
                if s.connection != c {
                    s.connection = c;
                    self.apply();
                }
            }
            ID_SAVER_OFF | ID_SAVER_ON | ID_SAVER_AUTO => {
                let b = match id {
                    ID_SAVER_ON => BatterySaver::On,
                    ID_SAVER_AUTO => BatterySaver::Auto,
                    _ => BatterySaver::Off,
                };
                if s.battery_saver != b {
                    s.battery_saver = b;
                    if self.settings.saver_active() != self.saver_applied {
                        self.apply();
                    } else {
                        self.settings.save();
                    }
                }
            }
            ID_AUTOSTART => set_autostart(!autostart_enabled()),
            ID_LOG => {
                s.log_file = !s.log_file;
                s.save();
                LOG_ON.store(s.log_file, Ordering::Relaxed);
                if !s.log_file {
                    *LOG_FILE.lock().unwrap_or_else(|e| e.into_inner()) = None;
                }
            }
            ID_OPEN_LOG => open_path(&log_path()),
            ID_INSTALL_DRIVER => install_driver(self.settings.run_config(), self.control.clone()),
            ID_QUIT => return Action::Quit,
            _ => {}
        }
        Action::None
    }
}

fn open_path(p: &std::path::Path) {
    let p = wide(&p.to_string_lossy());
    unsafe {
        ShellExecuteW(None, w!("open"), PCWSTR(p.as_ptr()), PCWSTR::null(), PCWSTR::null(), SW_SHOWNORMAL);
    }
}

/// Run `opendisplay-win install-driver` (it asks for admin rights itself and
/// shows the result), then restart the sender to pick the driver up.
fn install_driver(cfg: RunConfig, control: Sender<Control>) {
    let Some(cli) = std::env::current_exe().ok().and_then(|e| Some(e.parent()?.join("opendisplay-win.exe"))) else {
        return;
    };
    std::thread::spawn(move || {
        let exe = wide(&cli.to_string_lossy());
        let mut info = SHELLEXECUTEINFOW {
            cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
            fMask: SEE_MASK_NOCLOSEPROCESS,
            lpVerb: w!("open"),
            lpFile: PCWSTR(exe.as_ptr()),
            lpParameters: w!("install-driver"),
            nShow: SW_SHOWNORMAL.0,
            ..Default::default()
        };
        unsafe {
            if ShellExecuteExW(&mut info).is_ok() && !info.hProcess.is_invalid() {
                windows::Win32::System::Threading::WaitForSingleObject(info.hProcess, u32::MAX);
                let _ = windows::Win32::Foundation::CloseHandle(info.hProcess);
            }
        }
        let _ = control.send(Control::Apply(cfg));
    });
}

fn autostart_enabled() -> bool {
    unsafe {
        let mut len = 0u32;
        RegGetValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE, RRF_RT_REG_SZ, None, None, Some(&mut len)).is_ok()
    }
}

fn set_autostart(on: bool) {
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let cmd = wide(&format!("\"{}\"", exe.display()));
            let _ = RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                RUN_VALUE,
                REG_SZ.0,
                Some(cmd.as_ptr() as _),
                (cmd.len() * 2) as u32,
            );
        } else {
            let _ = RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, RUN_VALUE);
        }
    }
}

/// Stop streaming (taking the virtual monitor down), remove the icon, exit.
fn quit() {
    let control = with_ui(|ui| {
        ui.last = TrayStatus::Status(Status::Stopped);
        ui.notify(NIM_MODIFY, None);
        ui.control.clone()
    });
    if let Some(control) = control {
        let (ack, done) = mpsc::channel();
        if control.send(Control::Quit(ack)).is_ok() {
            let _ = done.recv_timeout(Duration::from_secs(8));
        }
    }
    with_ui(|ui| ui.notify(NIM_DELETE, None));
    unsafe { PostQuitMessage(0) };
}

/// Where the tray icon is on screen, if it is visible.
fn icon_anchor(hwnd: HWND) -> Option<POINT> {
    let id = NOTIFYICONIDENTIFIER {
        cbSize: size_of::<NOTIFYICONIDENTIFIER>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    let r = unsafe { Shell_NotifyIconGetRect(&id).ok()? };
    Some(POINT { x: (r.left + r.right) / 2, y: r.top })
}

/// Show the tray menu (at the cursor, or above the icon) and run the chosen command.
fn show_menu(hwnd: HWND, at_icon: bool) {
    let Some(menu) = with_ui(|ui| ui.build_menu()).flatten() else { return };
    let cmd = unsafe {
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        if at_icon {
            if let Some(p) = icon_anchor(hwnd) {
                pt = p;
            }
        }
        // Required so the menu closes when clicking elsewhere.
        let _ = SetForegroundWindow(hwnd);
        // Modal: pumps messages, so no UI borrow may be held here.
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_NONOTIFY, pt.x, pt.y, Some(0), hwnd, None);
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(menu);
        cmd.0 as usize
    };
    run_command(cmd);
}

fn run_command(id: usize) {
    if id == 0 {
        return;
    }
    if let Some(Action::Quit) = with_ui(|ui| ui.command(id)) {
        quit();
    }
}

fn handle(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    match msg {
        WM_TRAY => {
            let event = lparam.0 as u32;
            if event == WM_LBUTTONUP || event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_menu(hwnd, false);
            }
            Some(LRESULT(0))
        }
        WM_MENU_AT_ICON => {
            show_menu(hwnd, true);
            Some(LRESULT(0))
        }
        // Menu commands also arrive this way (e.g. posted by tests or scripts).
        WM_COMMAND => {
            run_command(wparam.0 & 0xffff);
            Some(LRESULT(0))
        }
        WM_STATUS => {
            with_ui(|ui| ui.on_status());
            Some(LRESULT(0))
        }
        // `taskkill` without /F, uninstallers: quit cleanly like the menu item.
        WM_CLOSE => {
            quit();
            Some(LRESULT(0))
        }
        // Charger plugged in/out or energy saver toggled: follow it in Automatic.
        WM_POWERBROADCAST => {
            if wparam.0 == PBT_APMPOWERSTATUSCHANGE as usize {
                with_ui(|ui| {
                    if ui.settings.battery_saver == BatterySaver::Auto && ui.settings.saver_active() != ui.saver_applied {
                        tracing::info!("power source changed; battery saver {}", if on_battery() { "on" } else { "off" });
                        ui.apply();
                    }
                });
            }
            Some(LRESULT(1))
        }
        WM_QUERYENDSESSION => Some(LRESULT(1)),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                quit();
            }
            Some(LRESULT(0))
        }
        // Explorer restarted: put the icon back.
        m if m != 0 && m == TASKBAR_CREATED.load(Ordering::Relaxed) => {
            with_ui(|ui| ui.notify(NIM_ADD, None));
            Some(LRESULT(0))
        }
        _ => None,
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // A panic must not unwind into Windows (that aborts the process).
    match std::panic::catch_unwind(|| handle(hwnd, msg, wparam, lparam)) {
        Ok(Some(r)) => r,
        Ok(None) => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
        Err(_) => {
            tracing::error!("internal error handling window message {msg:#x}");
            LRESULT(0)
        }
    }
}

fn message_box(text: &str) {
    let t = wide(text);
    unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), w!("OpenDisplay"), MB_OK | MB_ICONINFORMATION);
    }
}

fn main() {
    app::set_dpi_awareness();
    let _instance = match SingleInstance::acquire() {
        Ok(i) => i,
        Err(e) => {
            message_box(&format!("{e:#}"));
            return;
        }
    };
    init_logging();
    let settings = Settings::load();
    LOG_ON.store(settings.log_file, Ordering::Relaxed);
    tracing::info!("OpenDisplay {} starting", env!("CARGO_PKG_VERSION"));

    unsafe {
        let hinst: HINSTANCE = GetModuleHandleW(None).unwrap_or_default().into();
        let class = w!("OpenDisplayTray");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: hinst,
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("OpenDisplay"),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            Some(hinst),
            None,
        ) else {
            message_box("Could not create the tray window.");
            return;
        };
        HWND_TRAY.store(hwnd.0 as isize, Ordering::Relaxed);
        TASKBAR_CREATED.store(RegisterWindowMessageW(w!("TaskbarCreated")), Ordering::Relaxed);

        let (tx, rx) = mpsc::channel();
        let ctl = std::thread::Builder::new()
            .name("controller".into())
            .spawn(move || controller(rx))
            .expect("controller thread");

        UI.with(|u| {
            *u.borrow_mut() = Some(Ui {
                hwnd,
                hinst,
                settings,
                control: tx,
                last: TrayStatus::Starting,
                saver_applied: false,
            })
        });
        with_ui(|ui| {
            ui.notify(NIM_ADD, None);
            ui.saver_applied = ui.settings.saver_active();
            let _ = ui.control.send(Control::Apply(ui.settings.run_config()));
        });

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UI.with(|u| *u.borrow_mut() = None);
        let _ = ctl.join();
    }
}
