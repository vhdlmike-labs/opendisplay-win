//! Capture one monitor with DXGI Desktop Duplication. The pointer is never
//! part of the duplicated image; it travels as `cursor`/`cursorImg` (§6.2).

use anyhow::{Context, Result, bail};
use windows::Win32::Foundation::{HMODULE, LUID};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::StationsAndDesktops::{
    DESKTOP_ACCESS_FLAGS, DESKTOP_CONTROL_FLAGS, OpenInputDesktop, SetThreadDesktop,
};
use windows::core::Interface;

pub struct Gpu {
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    /// The adapter's name, e.g. "AMD Radeon(TM) Graphics".
    pub adapter: String,
    /// The adapter's LUID, to pick encoders that run on it.
    pub luid: LUID,
}

fn adapter_desc(adapter: &IDXGIAdapter1) -> (String, LUID, u32) {
    match unsafe { adapter.GetDesc1() } {
        Ok(d) => {
            let name = String::from_utf16_lossy(&d.Description);
            (name.trim_end_matches('\0').trim().to_string(), d.AdapterLuid, d.Flags)
        }
        Err(_) => ("unknown adapter".into(), LUID::default(), 0),
    }
}

pub fn same_luid(a: LUID, b: LUID) -> bool {
    a.LowPart == b.LowPart && a.HighPart == b.HighPart
}

/// Real GPUs (no software rasterizer), least dedicated video memory first,
/// so an integrated GPU comes before a discrete one.
fn hardware_adapters() -> Vec<(IDXGIAdapter1, String, LUID)> {
    let mut out: Vec<(usize, IDXGIAdapter1, String, LUID)> = Vec::new();
    unsafe {
        let Ok(factory) = CreateDXGIFactory1::<IDXGIFactory1>() else { return Vec::new() };
        for a in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(a) else { break };
            let Ok(d) = adapter.GetDesc1() else { continue };
            // 0x1414: Microsoft (Basic Render Driver and other virtual adapters).
            if d.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 || d.VendorId == 0x1414 {
                continue;
            }
            let (name, luid, _) = adapter_desc(&adapter);
            out.push((d.DedicatedVideoMemory, adapter, name, luid));
        }
    }
    out.sort_by_key(|(mem, ..)| *mem);
    out.into_iter().map(|(_, a, n, l)| (a, n, l)).collect()
}

/// Hardware adapters other than `except`, integrated first (hybrid laptops).
pub fn other_adapters(except: LUID) -> Vec<IDXGIAdapter1> {
    hardware_adapters()
        .into_iter()
        .filter(|(_, _, l)| !same_luid(*l, except))
        .map(|(a, ..)| a)
        .collect()
}

/// On a PC with more than one GPU, the integrated (power-saving) one, as
/// (name, LUID): the virtual monitor should render there so the discrete
/// GPU can stay asleep. `None` with a single GPU.
pub fn power_saving_adapter() -> Option<(String, LUID)> {
    let all = hardware_adapters();
    if all.len() < 2 {
        return None;
    }
    all.into_iter().next().map(|(_, n, l)| (n, l))
}

/// The adapter and output that scan out `device_name` (`\\.\DISPLAYn`).
fn find_output(device_name: &str) -> Result<(IDXGIAdapter1, IDXGIOutput)> {
    unsafe {
        let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
        let mut seen = Vec::new();
        for a in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(a) else { break };
            for o in 0.. {
                let Ok(output) = adapter.EnumOutputs(o) else { break };
                let desc = output.GetDesc()?;
                let name = String::from_utf16_lossy(&desc.DeviceName);
                let name = name.trim_end_matches('\0').to_string();
                if name.eq_ignore_ascii_case(device_name) {
                    return Ok((adapter, output));
                }
                seen.push(name);
            }
        }
        bail!("no DXGI output {device_name} (outputs: {})", seen.join(", "))
    }
}

pub fn create_device(adapter: &IDXGIAdapter1) -> Result<Gpu> {
    unsafe {
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )
        .context("D3D11CreateDevice")?;
        let device: ID3D11Device = device.context("no device")?;
        // Media Foundation uses the device from its own threads.
        if let Ok(mt) = device.cast::<ID3D11Multithread>() {
            let _ = mt.SetMultithreadProtected(true);
        }
        let (name, luid, _) = adapter_desc(adapter);
        Ok(Gpu {
            device,
            context: context.context("no context")?,
            adapter: name,
            luid,
        })
    }
}

pub enum Grab {
    /// The desktop image changed; `Capturer::frame` holds it.
    New,
    /// Nothing changed within the timeout.
    Idle,
}

pub struct Capturer {
    pub gpu: Gpu,
    dup: IDXGIOutputDuplication,
    /// Our copy of the latest desktop image (BGRA), owned so the duplication
    /// frame can be released right away.
    pub frame: ID3D11Texture2D,
    pub width: u32,
    pub height: u32,
    pub have_frame: bool,
}

/// Follow the input desktop (Winlogon/UAC secure desktop vs Default) so a
/// recreated duplication attaches to whatever is on screen when allowed.
fn attach_input_desktop() {
    unsafe {
        if let Ok(desk) = OpenInputDesktop(DESKTOP_CONTROL_FLAGS(0), false, DESKTOP_ACCESS_FLAGS(0x10000000)) {
            let _ = SetThreadDesktop(desk);
        }
    }
}

impl Capturer {
    pub fn new(device_name: &str) -> Result<Self> {
        attach_input_desktop();
        let (adapter, output) = find_output(device_name)?;
        let gpu = create_device(&adapter)?;
        unsafe {
            let dup = match output.cast::<IDXGIOutput5>() {
                Ok(o5) => o5
                    .DuplicateOutput1(&gpu.device, 0, &[DXGI_FORMAT_B8G8R8A8_UNORM])
                    .context("DuplicateOutput1")?,
                Err(_) => output
                    .cast::<IDXGIOutput1>()?
                    .DuplicateOutput(&gpu.device)
                    .context("DuplicateOutput")?,
            };
            let dd = dup.GetDesc();
            let (width, height) = (dd.ModeDesc.Width, dd.ModeDesc.Height);
            let desc = D3D11_TEXTURE2D_DESC {
                Width: width,
                Height: height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: 0,
            };
            let mut frame = None;
            gpu.device.CreateTexture2D(&desc, None, Some(&mut frame))?;
            Ok(Self {
                gpu,
                dup,
                frame: frame.context("no texture")?,
                width,
                height,
                have_frame: false,
            })
        }
    }

    /// Wait up to `timeout_ms` for a new desktop image. An error means the
    /// duplication is gone (mode change, secure desktop, display removed):
    /// drop this capturer and build a new one.
    pub fn grab(&mut self, timeout_ms: u32) -> Result<Grab> {
        unsafe {
            let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;
            match self.dup.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(Grab::Idle),
                Err(e) => return Err(e).context("AcquireNextFrame"),
            }
            let result = (|| {
                // LastPresentTime 0: only the pointer moved.
                if info.LastPresentTime == 0 && self.have_frame {
                    return Ok(Grab::Idle);
                }
                let tex: ID3D11Texture2D = resource.context("no frame resource")?.cast()?;
                self.gpu.context.CopyResource(&self.frame, &tex);
                self.have_frame = true;
                Ok(Grab::New)
            })();
            let _ = self.dup.ReleaseFrame();
            result
        }
    }
}
