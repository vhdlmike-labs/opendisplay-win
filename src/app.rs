//! What both front ends (the `opendisplay-win` CLI and the `OpenDisplay`
//! tray app) share: the run configuration, display backend choice, the
//! find-then-stream loop, and the one-sender-per-PC guard.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::{Result, bail};
use tracing::info;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;
use windows::core::w;

use crate::desktop::{DesktopOptions, ScalePolicy};
use crate::display::{self, DisplayBackend, ExistingMonitor};
use crate::session::{self, Status, StatusFn};
use crate::{discover, driver};

#[derive(Clone)]
pub struct RunConfig {
    /// See `discover::resolve`: name/id, ip, `usb`, `wifi`, or `None` (USB, else Wi-Fi).
    pub to: Option<String>,
    /// `auto`, `virtual`, `primary` or `DISPLAYn`.
    pub display: String,
    pub resize: Option<bool>,
    pub desktop: DesktopOptions,
    pub bitrate: Option<u32>,
    pub software_encoder: bool,
    /// Keep waiting when the receiver's app is closed.
    pub stay: bool,
    /// Keep looking until a receiver appears.
    pub wait: bool,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            to: None,
            display: "auto".into(),
            resize: None,
            desktop: DesktopOptions { scale: ScalePolicy::Mac, max_fps: None },
            bitrate: None,
            software_encoder: false,
            stay: true,
            wait: true,
        }
    }
}

pub fn monitor_name(display: &str) -> Option<String> {
    match display {
        "primary" => None,
        d if d.starts_with(r"\\.\") => Some(d.to_string()),
        d => Some(format!(r"\\.\{}", d.to_uppercase())),
    }
}

pub fn backend(display: &str, resize: Option<bool>) -> Box<dyn DisplayBackend> {
    match display {
        "auto" => return Box::new(display::AutoDisplay::new(driver::installed)),
        "virtual" => return Box::new(display::VirtualMonitor::new()),
        _ => {}
    }
    let name = monitor_name(display);
    let resize = resize.unwrap_or(false) && name.is_some();
    Box::new(ExistingMonitor::new(name, resize))
}

/// Physical pixels everywhere: capture, cursor and input share one space.
pub fn set_dpi_awareness() {
    unsafe {
        let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
            windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        );
    }
}

/// Find the receiver (retrying with `wait`) and stream until `stop`.
pub fn run(cfg: &RunConfig, stop: Arc<AtomicBool>, status: Option<StatusFn>) -> Result<()> {
    let report = |s: Status| {
        if let Some(f) = &status {
            f(s)
        }
    };
    report(Status::Searching);
    let target = loop {
        if stop.load(Ordering::Relaxed) {
            report(Status::Stopped);
            return Ok(());
        }
        match discover::resolve(cfg.to.as_deref(), Duration::from_secs(3)) {
            Ok(t) => break t,
            Err(e) if cfg.wait => {
                info!("{e:#}; looking again");
                std::thread::sleep(Duration::from_secs(2));
            }
            Err(e) => return Err(e),
        }
    };
    let backend = backend(&cfg.display, cfg.resize);
    info!("display: {}", backend.name());
    let r = session::run(
        target,
        backend,
        session::Options {
            desktop: cfg.desktop,
            bitrate: cfg.bitrate,
            software_encoder: cfg.software_encoder,
            exit_on_close: !cfg.stay,
            status: status.clone(),
        },
        stop,
    );
    report(Status::Stopped);
    r
}

/// Only one sender may drive a receiver (it always takes the newest
/// connection), so only one runs per user session, CLI or tray.
pub struct SingleInstance(HANDLE);

impl SingleInstance {
    pub fn acquire() -> Result<Self> {
        unsafe {
            let h = CreateMutexW(None, true, w!("Local\\OpenDisplaySender"))?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(h);
                bail!("OpenDisplay is already running (see the tray icon, or another opendisplay-win window)");
            }
            Ok(Self(h))
        }
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
