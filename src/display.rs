//! The display that gets streamed: an existing monitor, or a virtual monitor
//! created for the receiver (sized from its `hello`, §6.7).

use anyhow::{Context, Result, bail};
use tracing::info;
use windows::Win32::Devices::Display::*;
use windows::Win32::Foundation::{LPARAM, LUID, RECT};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::Graphics::Gdi::*;
use windows::core::{BOOL, PCWSTR};

use crate::desktop::OutputMode;

#[derive(Debug, Clone)]
pub struct Monitor {
    /// GDI name, `\\.\DISPLAYn`: what DXGI and ChangeDisplaySettings use.
    pub device_name: String,
    /// Desktop coordinates in physical pixels (the process is per-monitor DPI aware).
    pub rect: RECT,
    pub primary: bool,
    pub friendly: String,
    /// `\\?\DISPLAY#<PnP id>#...`, identifies virtual monitors.
    pub device_path: String,
    /// Effective DPI (96 = 100% scaling).
    pub dpi: u32,
}

impl Monitor {
    pub fn size(&self) -> (u32, u32) {
        ((self.rect.right - self.rect.left) as u32, (self.rect.bottom - self.rect.top) as u32)
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn from_wide(w: &[u16]) -> String {
    let end = w.iter().position(|&c| c == 0).unwrap_or(w.len());
    String::from_utf16_lossy(&w[..end])
}

/// One display path: a desktop source driving a monitor target.
#[derive(Debug, Clone)]
struct PathInfo {
    gdi: String,
    friendly: String,
    device_path: String,
    source: (LUID, u32),
}

/// Every target in the display configuration, raw, for diagnostics:
/// (source gdi name or error, target friendly name, device path, available, active).
pub fn debug_paths() -> Vec<String> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if let Err(e) = GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut np, &mut nm).ok() {
            return vec![format!("GetDisplayConfigBufferSizes: {e}")];
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if let Err(e) = QueryDisplayConfig(QDC_ALL_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).ok() {
            return vec![format!("QueryDisplayConfig: {e}")];
        }
        for p in &paths[..np as usize] {
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of_val(&src) as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            let rs = DisplayConfigGetDeviceInfo(&mut src.header);
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = size_of_val(&tgt) as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            let rt = DisplayConfigGetDeviceInfo(&mut tgt.header);
            out.push(format!(
                "src {} [{:x}:{}] (rc {rs}) -> tgt [{:x}:{}] {:?} {} (rc {rt}) available={} active={}",
                from_wide(&src.viewGdiDeviceName),
                p.sourceInfo.adapterId.LowPart,
                p.sourceInfo.id,
                p.targetInfo.adapterId.LowPart,
                p.targetInfo.id,
                from_wide(&tgt.monitorFriendlyDeviceName),
                from_wide(&tgt.monitorDevicePath),
                p.targetInfo.targetAvailable.as_bool(),
                p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0,
            ));
        }
    }
    out
}

/// Paths from the display configuration (`QDC_ALL_PATHS` also lists
/// monitors that are connected but not part of the desktop).
fn display_paths(flags: QUERY_DISPLAY_CONFIG_FLAGS) -> Vec<PathInfo> {
    let mut out = Vec::new();
    unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(flags, &mut np, &mut nm).is_err() {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(flags, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return out;
        }
        for p in &paths[..np as usize] {
            if p.targetInfo.targetAvailable.as_bool() == false {
                continue;
            }
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            src.header.size = size_of_val(&src) as u32;
            src.header.adapterId = p.sourceInfo.adapterId;
            src.header.id = p.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            tgt.header.size = size_of_val(&tgt) as u32;
            tgt.header.adapterId = p.targetInfo.adapterId;
            tgt.header.id = p.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut tgt.header) != 0 {
                continue;
            }
            out.push(PathInfo {
                gdi: from_wide(&src.viewGdiDeviceName),
                friendly: from_wide(&tgt.monitorFriendlyDeviceName),
                device_path: from_wide(&tgt.monitorDevicePath),
                source: (p.sourceInfo.adapterId, p.sourceInfo.id),
            });
        }
    }
    out
}

pub fn monitors() -> Vec<Monitor> {
    unsafe extern "system" fn cb(h: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        unsafe {
            let list = &mut *(data.0 as *mut Vec<Monitor>);
            let mut mi = MONITORINFOEXW::default();
            mi.monitorInfo.cbSize = size_of::<MONITORINFOEXW>() as u32;
            if GetMonitorInfoW(h, &mut mi.monitorInfo).as_bool() {
                list.push(Monitor {
                    device_name: from_wide(&mi.szDevice),
                    rect: mi.monitorInfo.rcMonitor,
                    primary: mi.monitorInfo.dwFlags & 1 != 0,
                    friendly: String::new(),
                    device_path: String::new(),
                    dpi: {
                        let (mut x, mut y) = (96u32, 96u32);
                        let _ = GetDpiForMonitor(h, MDT_EFFECTIVE_DPI, &mut x, &mut y);
                        x
                    },
                });
            }
            true.into()
        }
    }
    let mut list: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut list as *mut _ as isize));
    }
    let paths = display_paths(QDC_ONLY_ACTIVE_PATHS);
    for m in &mut list {
        if let Some(p) = paths.iter().find(|p| p.gdi.eq_ignore_ascii_case(&m.device_name)) {
            m.friendly = p.friendly.clone();
            m.device_path = p.device_path.clone();
        }
    }
    list.sort_by_key(|m| m.device_name.clone());
    list
}

/// Select `mode` on `device_name`; with `right_of_primary` also place it to
/// the right of the primary monitor, top-aligned.
pub fn set_mode(device_name: &str, w: u32, h: u32, hz: u32, position: Option<(i32, i32)>) -> Result<()> {
    unsafe {
        let name = wide(device_name);
        let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
        dm.dmPelsWidth = w;
        dm.dmPelsHeight = h;
        dm.dmFields = DM_PELSWIDTH | DM_PELSHEIGHT;
        if hz > 0 {
            dm.dmDisplayFrequency = hz;
            dm.dmFields |= DM_DISPLAYFREQUENCY;
        }
        if let Some((x, y)) = position {
            dm.Anonymous1.Anonymous2.dmPosition = windows::Win32::Foundation::POINTL { x, y };
            dm.dmFields |= DM_POSITION;
        }
        let r = ChangeDisplaySettingsExW(
            PCWSTR(name.as_ptr()),
            Some(&dm),
            None,
            CDS_UPDATEREGISTRY | CDS_NORESET,
            None,
        );
        if r != DISP_CHANGE_SUCCESSFUL && hz > 0 {
            // The mode list may not carry this exact rate; take any.
            dm.dmFields &= !DM_DISPLAYFREQUENCY;
            let r2 = ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY | CDS_NORESET, None);
            if r2 != DISP_CHANGE_SUCCESSFUL {
                bail!("ChangeDisplaySettingsEx({device_name}, {w}x{h}) failed: {}", r2.0);
            }
        } else if r != DISP_CHANGE_SUCCESSFUL {
            bail!("ChangeDisplaySettingsEx({device_name}, {w}x{h}) failed: {}", r.0);
        }
        // Apply the batched change.
        let r = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        if r != DISP_CHANGE_SUCCESSFUL {
            bail!("applying display settings failed: {}", r.0);
        }
        info!("{device_name}: mode {w}x{h}@{hz}");
        Ok(())
    }
}

/// Supported modes of a display (width, height, hz), for diagnostics.
pub fn modes(device_name: &str) -> Vec<(u32, u32, u32)> {
    let name = wide(device_name);
    let mut out = Vec::new();
    unsafe {
        for i in 0.. {
            let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
            if !EnumDisplaySettingsW(PCWSTR(name.as_ptr()), ENUM_DISPLAY_SETTINGS_MODE(i), &mut dm).as_bool() {
                break;
            }
            let m = (dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency);
            if !out.contains(&m) {
                out.push(m);
            }
        }
    }
    out
}

/// Windows' scaling steps; the undocumented DPI-scale device info indexes
/// into this table relative to the monitor's recommended step.
const DPI_STEPS: [u32; 12] = [100, 125, 150, 175, 200, 225, 250, 300, 350, 400, 450, 500];

#[repr(C)]
struct DpiScaleGet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    min_rel: i32,
    cur_rel: i32,
    max_rel: i32,
}

#[repr(C)]
struct DpiScaleSet {
    header: DISPLAYCONFIG_DEVICE_INFO_HEADER,
    rel: i32,
}

/// Set a monitor's scaling (Settings > Display > Scale) to the nearest
/// supported step at or below `percent`. Uses the undocumented
/// DisplayConfig DPI-scale info (types -3/-4) that Settings itself uses.
pub fn set_scaling(device_name: &str, percent: u32) -> Result<u32> {
    let path = display_paths(QDC_ONLY_ACTIVE_PATHS)
        .into_iter()
        .find(|p| p.gdi.eq_ignore_ascii_case(device_name))
        .with_context(|| format!("{device_name} is not active"))?;
    unsafe {
        let mut get = DpiScaleGet {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-3),
                size: size_of::<DpiScaleGet>() as u32,
                adapterId: path.source.0,
                id: path.source.1,
            },
            min_rel: 0,
            cur_rel: 0,
            max_rel: 0,
        };
        if DisplayConfigGetDeviceInfo(&mut get.header) != 0 {
            bail!("reading the scaling of {device_name} failed");
        }
        let recommended = (-get.min_rel).max(0) as usize;
        let max = (recommended as i32 + get.max_rel).clamp(0, DPI_STEPS.len() as i32 - 1) as usize;
        let want = DPI_STEPS.iter().rposition(|&s| s <= percent).unwrap_or(0).min(max);
        let current = DPI_STEPS[(recommended as i32 + get.cur_rel).clamp(0, 11) as usize];
        if DPI_STEPS[want] == current {
            return Ok(current);
        }
        let mut set = DpiScaleSet {
            header: DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_TYPE(-4),
                size: size_of::<DpiScaleSet>() as u32,
                adapterId: path.source.0,
                id: path.source.1,
            },
            rel: want as i32 - recommended as i32,
        };
        if DisplayConfigSetDeviceInfo(&mut set.header) != 0 {
            bail!("setting the scaling of {device_name} failed");
        }
        info!("{device_name}: scaling {current}% -> {}%", DPI_STEPS[want]);
        Ok(DPI_STEPS[want])
    }
}

/// Whether a virtual display driver's monitor is connected, even if it is
/// not part of the desktop right now.
pub fn virtual_monitor_connected() -> bool {
    display_paths(QDC_ALL_PATHS)
        .iter()
        .any(|p| looks_virtual_path(&p.friendly, &p.device_path))
}

fn attached_virtual() -> Option<Monitor> {
    monitors().into_iter().find(|m| !m.primary && looks_virtual(m))
}

/// Take a monitor off the desktop (windows on it move to the others).
pub fn detach(device_name: &str) -> Result<()> {
    unsafe {
        let name = wide(device_name);
        let mut dm = DEVMODEW { dmSize: size_of::<DEVMODEW>() as u16, ..Default::default() };
        dm.dmFields = DM_POSITION | DM_PELSWIDTH | DM_PELSHEIGHT;
        let r = ChangeDisplaySettingsExW(PCWSTR(name.as_ptr()), Some(&dm), None, CDS_UPDATEREGISTRY | CDS_NORESET, None);
        if r != DISP_CHANGE_SUCCESSFUL {
            bail!("detaching {device_name} failed: {}", r.0);
        }
        let r = ChangeDisplaySettingsExW(PCWSTR::null(), None, None, CDS_TYPE(0), None);
        if r != DISP_CHANGE_SUCCESSFUL {
            bail!("applying display settings failed: {}", r.0);
        }
        info!("{device_name}: detached from the desktop");
        Ok(())
    }
}

fn target_path_of(p: &DISPLAYCONFIG_PATH_INFO) -> (String, String) {
    unsafe {
        let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
        tgt.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
        tgt.header.size = size_of_val(&tgt) as u32;
        tgt.header.adapterId = p.targetInfo.adapterId;
        tgt.header.id = p.targetInfo.id;
        if DisplayConfigGetDeviceInfo(&mut tgt.header) != 0 {
            return (String::new(), String::new());
        }
        (from_wide(&tgt.monitorFriendlyDeviceName), from_wide(&tgt.monitorDevicePath))
    }
}

fn source_name_of(p: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    unsafe {
        let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        src.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        src.header.size = size_of_val(&src) as u32;
        src.header.adapterId = p.sourceInfo.adapterId;
        src.header.id = p.sourceInfo.id;
        (DisplayConfigGetDeviceInfo(&mut src.header) == 0).then(|| from_wide(&src.viewGdiDeviceName))
    }
}

/// Add the virtual monitor to the desktop next to the current monitors.
/// Tries, in order: an explicit configuration (the current active paths plus
/// the virtual one on a free source, which also undoes a duplicate/clone),
/// the legacy attach (giving its source a size and position), and the
/// "Extend" topology from the database. Failures are logged (`loud`: as
/// warnings); the caller polls for the monitor.
fn attach_virtual(mode: &OutputMode, loud: bool) {
    let report = |what: &str, r: String| {
        if loud {
            tracing::warn!("{what}: {r}");
        } else {
            tracing::debug!("{what}: {r}");
        }
    };
    let (paths, modes) = unsafe {
        let (mut np, mut nm) = (0u32, 0u32);
        if GetDisplayConfigBufferSizes(QDC_ALL_PATHS, &mut np, &mut nm).is_err() {
            return;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); np as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); nm as usize];
        if QueryDisplayConfig(QDC_ALL_PATHS, &mut np, paths.as_mut_ptr(), &mut nm, modes.as_mut_ptr(), None).is_err() {
            return;
        }
        paths.truncate(np as usize);
        modes.truncate(nm as usize);
        (paths, modes)
    };
    let virt = |p: &DISPLAYCONFIG_PATH_INFO| {
        let (f, d) = target_path_of(p);
        looks_virtual_path(&f, &d)
    };
    let same_source = |a: &DISPLAYCONFIG_PATH_INFO, b: &DISPLAYCONFIG_PATH_INFO| {
        a.sourceInfo.adapterId.LowPart == b.sourceInfo.adapterId.LowPart
            && a.sourceInfo.adapterId.HighPart == b.sourceInfo.adapterId.HighPart
            && a.sourceInfo.id == b.sourceInfo.id
    };
    let active: Vec<DISPLAYCONFIG_PATH_INFO> = paths
        .iter()
        .filter(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 && !virt(p))
        .copied()
        .collect();
    let free = paths
        .iter()
        .find(|p| p.targetInfo.targetAvailable.as_bool() && virt(p) && !active.iter().any(|a| same_source(a, p)))
        .copied();
    // Duplicate mode: the virtual monitor shares another monitor's source and
    // Windows lists no other path for it. Move it to a free source of the same
    // adapter, which turns "duplicate" into "extend".
    let cloned = || {
        let p = *paths.iter().find(|p| p.flags & DISPLAYCONFIG_PATH_ACTIVE != 0 && virt(p))?;
        let used: Vec<u32> = active
            .iter()
            .filter(|a| a.sourceInfo.adapterId.LowPart == p.sourceInfo.adapterId.LowPart
                && a.sourceInfo.adapterId.HighPart == p.sourceInfo.adapterId.HighPart)
            .map(|a| a.sourceInfo.id)
            .collect();
        let mut ids: Vec<u32> = paths
            .iter()
            .filter(|q| q.sourceInfo.adapterId.LowPart == p.sourceInfo.adapterId.LowPart
                && q.sourceInfo.adapterId.HighPart == p.sourceInfo.adapterId.HighPart)
            .map(|q| q.sourceInfo.id)
            .filter(|id| !used.contains(id))
            .collect();
        ids.sort_unstable();
        let mut q = p;
        q.sourceInfo.id = *ids.first()?;
        info!("the virtual monitor duplicates another screen; switching it to extend");
        Some(q)
    };
    let Some(candidate) = free.or_else(cloned) else {
        report("attach", "Windows does not list the virtual monitor as available yet".into());
        return;
    };
    let candidate = &candidate;

    // 1. Explicit configuration.
    let mut v = *candidate;
    v.flags = DISPLAYCONFIG_PATH_ACTIVE;
    v.sourceInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    v.targetInfo.Anonymous.modeInfoIdx = DISPLAYCONFIG_PATH_MODE_IDX_INVALID;
    let mut config = active.clone();
    config.push(v);
    let r = unsafe {
        SetDisplayConfig(
            Some(&config),
            Some(&modes),
            SDC_APPLY | SDC_USE_SUPPLIED_DISPLAY_CONFIG | SDC_ALLOW_CHANGES | SDC_SAVE_TO_DATABASE,
        )
    };
    if r == 0 {
        info!("virtual monitor attached");
        return;
    }
    report("attach (supplied configuration)", format!("SetDisplayConfig error {r}"));

    // 2. Legacy attach: give the virtual source a size and a position.
    if let Some(gdi) = source_name_of(candidate) {
        let right = monitors().iter().map(|m| m.rect.right).max().unwrap_or(0);
        match set_mode(&gdi, mode.width, mode.height, 0, Some((right, 0))) {
            Ok(()) => return,
            Err(e) => report("attach (ChangeDisplaySettingsEx)", format!("{e:#}")),
        }
    }

    // 3. "Extend" from the topology database.
    let r = unsafe { SetDisplayConfig(None, None, SDC_APPLY | SDC_TOPOLOGY_EXTEND) };
    if r != 0 {
        report("attach (extend topology)", format!("SetDisplayConfig error {r}"));
    }
}

/// A virtual display driver's monitor (MTT Virtual Display Driver,
/// parsec-vdd, ...), attached to the desktop for the session: extended
/// (Win+P "Extend"), sized to the receiver's desktop and scaled like it, and
/// taken off the desktop again when the session ends.
pub struct VirtualMonitor {
    attached: Option<String>,
}

impl VirtualMonitor {
    pub fn new() -> Self {
        Self { attached: None }
    }
}

impl DisplayBackend for VirtualMonitor {
    fn ensure(&mut self, mode: &OutputMode) -> Result<Monitor> {
        let m = match attached_virtual() {
            Some(m) => m,
            None => {
                // Right after a driver (re)install the monitor can take a few
                // seconds to become available: keep trying.
                info!("attaching the virtual monitor");
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
                let mut round = 0;
                loop {
                    attach_virtual(mode, round == 0);
                    round += 1;
                    let settle = std::time::Instant::now() + std::time::Duration::from_secs(2);
                    let found = loop {
                        if let Some(m) = attached_virtual() {
                            break Some(m);
                        }
                        if std::time::Instant::now() > settle {
                            break None;
                        }
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    };
                    if let Some(m) = found {
                        break m;
                    }
                    if std::time::Instant::now() > deadline {
                        bail!(
                            "the virtual monitor did not join the desktop within 15 s. Try Win+P > Extend, \
                             then check Settings > Display; `opendisplay-win monitors` lists what Windows sees"
                        );
                    }
                }
            }
        };
        self.attached = Some(m.device_name.clone());
        let Some((w, h)) = best_mode(&modes(&m.device_name), mode.width, mode.height) else {
            bail!("{} reports no modes", m.device_name);
        };
        if (w, h) != (mode.width, mode.height) {
            tracing::warn!(
                "{} has no {}x{} mode; using {w}x{h}. Add <resolution> {}x{} to C:\\VirtualDisplayDriver\\vdd_settings.xml for a pixel-exact desktop",
                m.device_name, mode.width, mode.height, mode.width, mode.height
            );
        }
        if m.size() != (w, h) {
            set_mode(&m.device_name, w, h, mode.hz, None)?;
        }
        if let Err(e) = set_scaling(&m.device_name, (mode.scale * 100.0).round() as u32) {
            tracing::warn!("{e:#}");
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
        monitors()
            .into_iter()
            .find(|x| x.device_name == m.device_name)
            .context("the virtual monitor went away")
    }
    fn release(&mut self) {
        if let Some(name) = self.attached.take() {
            if let Err(e) = detach(&name) {
                tracing::warn!("{e:#}");
            }
        }
    }
    fn name(&self) -> &'static str {
        "virtual monitor (extended desktop)"
    }
    fn follows_plan(&self) -> bool {
        false
    }
}

/// `--display auto`: the virtual monitor whenever its driver is installed,
/// decided per connection; mirror the primary monitor when there is none or
/// it cannot be brought up (then retried on the next connection).
pub struct AutoDisplay {
    virt: VirtualMonitor,
    mirror: ExistingMonitor,
    driver_installed: fn() -> bool,
    using_virtual: bool,
}

impl AutoDisplay {
    pub fn new(driver_installed: fn() -> bool) -> Self {
        Self {
            virt: VirtualMonitor::new(),
            mirror: ExistingMonitor::new(None, false),
            driver_installed,
            using_virtual: false,
        }
    }
}

impl DisplayBackend for AutoDisplay {
    fn ensure(&mut self, mode: &OutputMode) -> Result<Monitor> {
        if (self.driver_installed)() || virtual_monitor_connected() {
            match self.virt.ensure(mode) {
                Ok(m) => {
                    self.using_virtual = true;
                    return Ok(m);
                }
                Err(e) => {
                    tracing::warn!("virtual monitor unavailable ({e:#}); mirroring the primary monitor this time");
                    if !virtual_monitor_connected() {
                        tracing::warn!(
                            "the virtual display driver is installed but provides no monitor; run \
                             `Install virtual display.cmd` (opendisplay-win install-driver) again to \
                             repair its settings and restart it"
                        );
                    }
                }
            }
        } else {
            tracing::warn!(
                "no virtual display driver installed; mirroring the primary monitor. \
                 Run `opendisplay-win install-driver` to use the device as a second screen"
            );
        }
        self.using_virtual = false;
        self.mirror.ensure(mode)
    }
    fn release(&mut self) {
        self.virt.release();
        self.mirror.release();
    }
    fn name(&self) -> &'static str {
        "virtual monitor when available, else mirror"
    }
    fn follows_plan(&self) -> bool {
        false
    }
}

/// Something that provides the monitor to stream.
pub trait DisplayBackend: Send {
    /// Make sure a monitor for `mode` exists and return it. Called on every
    /// (debounced) plan change; must be cheap when nothing changed.
    fn ensure(&mut self, mode: &OutputMode) -> Result<Monitor>;
    /// Remove whatever was created (session over, or exit).
    fn release(&mut self);
    fn name(&self) -> &'static str;
    /// Whether the stream raster follows `mode` (a virtual monitor) or the
    /// monitor's own size (an existing monitor).
    fn follows_plan(&self) -> bool;
}

/// Heuristic: monitors created by virtual display drivers (IddCx samples,
/// MTT Virtual Display Driver, parsec-vdd, ...) rather than real panels.
pub fn looks_virtual(m: &Monitor) -> bool {
    looks_virtual_path(&m.friendly, &m.device_path)
}

fn looks_virtual_path(friendly: &str, device_path: &str) -> bool {
    let hay = format!("{friendly} {device_path}").to_lowercase();
    ["virtual", "vdd", "mtt1337", "idd", "parsec", "psccdd"].iter().any(|k| hay.contains(k))
}

/// Stream an existing monitor. With `resize`, switch it to the planned
/// desktop size first (the way to use a virtual display driver's monitor as
/// an extended display) and put the original mode back on release.
pub struct ExistingMonitor {
    pub device_name: Option<String>,
    pub resize: bool,
    original: Option<(String, u32, u32)>,
}

impl ExistingMonitor {
    pub fn new(device_name: Option<String>, resize: bool) -> Self {
        Self { device_name, resize, original: None }
    }

    fn find(&self) -> Result<Monitor> {
        let all = monitors();
        match &self.device_name {
            Some(n) => all
                .into_iter()
                .find(|m| m.device_name.eq_ignore_ascii_case(n))
                .with_context(|| format!("no monitor {n}")),
            None => all.into_iter().find(|m| m.primary).context("no primary monitor"),
        }
    }
}

/// The supported mode closest to `w`x`h`: exact if offered, else the largest
/// mode with the same orientation that fits inside it, else the largest.
fn best_mode(modes: &[(u32, u32, u32)], w: u32, h: u32) -> Option<(u32, u32)> {
    if modes.iter().any(|&(mw, mh, _)| (mw, mh) == (w, h)) {
        return Some((w, h));
    }
    let landscape = w >= h;
    modes
        .iter()
        .filter(|&&(mw, mh, _)| (mw >= mh) == landscape)
        .map(|&(mw, mh, _)| (mw, mh))
        .max_by_key(|&(mw, mh)| {
            let fits = mw <= w && mh <= h;
            let aspect_err = ((mw as f64 / mh as f64) - (w as f64 / h as f64)).abs();
            (fits, -(aspect_err * 1000.0) as i64, mw as u64 * mh as u64)
        })
}

impl DisplayBackend for ExistingMonitor {
    fn ensure(&mut self, mode: &OutputMode) -> Result<Monitor> {
        let m = self.find()?;
        if !self.resize {
            return Ok(m);
        }
        let Some((w, h)) = best_mode(&modes(&m.device_name), mode.width, mode.height) else {
            return Ok(m);
        };
        if m.size() != (w, h) {
            if self.original.is_none() {
                let (ow, oh) = m.size();
                self.original = Some((m.device_name.clone(), ow, oh));
            }
            if (w, h) != (mode.width, mode.height) {
                tracing::warn!(
                    "{} has no {}x{} mode; using {w}x{h} (add the mode to the virtual display driver's settings for a pixel-exact desktop)",
                    m.device_name, mode.width, mode.height
                );
            }
            set_mode(&m.device_name, w, h, mode.hz, None)?;
            std::thread::sleep(std::time::Duration::from_millis(500));
            return self.find();
        }
        Ok(m)
    }
    fn release(&mut self) {
        if let Some((name, w, h)) = self.original.take() {
            if let Err(e) = set_mode(&name, w, h, 0, None) {
                tracing::warn!("restoring {name}: {e:#}");
            }
        }
    }
    fn name(&self) -> &'static str {
        if self.resize { "monitor resized to the receiver" } else { "existing monitor (mirror)" }
    }
    fn follows_plan(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::best_mode;

    #[test]
    fn picks_exact_then_same_aspect_that_fits() {
        let modes = [(1920, 1080, 60), (1600, 1200, 60), (2224, 1668, 60), (1024, 768, 60)];
        assert_eq!(best_mode(&modes, 2224, 1668), Some((2224, 1668)));
        assert_eq!(best_mode(&modes[..2], 2224, 1668), Some((1600, 1200)));
        assert_eq!(best_mode(&[(1920, 1080, 60), (1080, 1920, 60)], 1668, 2224), Some((1080, 1920)));
    }
}
