//! Install/remove the bundled virtual display driver (MTT Virtual Display
//! Driver, MIT, SignPath-signed) without devcon: create the root-enumerated
//! `Root\MttVDD` device and bind the INF to it, as `devcon install` does.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use windows::Win32::Devices::DeviceAndDriverInstallation::*;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{GetExitCodeProcess, INFINITE, WaitForSingleObject};
use windows::Win32::UI::Shell::{IsUserAnAdmin, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{GUID, PCWSTR};

pub const HARDWARE_ID: &str = r"Root\MttVDD";
/// The driver reads `vdd_settings.xml` (modes, monitor count) from here.
pub const INSTALL_DIR: &str = r"C:\VirtualDisplayDriver";
const FILES: [&str; 4] = ["MttVDD.inf", "MttVDD.dll", "mttvdd.cat", "vdd_settings.xml"];

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `driver\` next to the executable.
fn bundled_dir() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    let dir = exe.parent().context("no exe directory")?.join("driver");
    if !dir.join("MttVDD.inf").exists() {
        bail!("{} not found; keep the `driver` folder next to opendisplay-win.exe", dir.display());
    }
    Ok(dir)
}

/// Device instances whose hardware ids include `Root\MttVDD`, present or not.
fn for_each_device(mut f: impl FnMut(HDEVINFO, &SP_DEVINFO_DATA)) -> Result<usize> {
    unsafe {
        let set = SetupDiGetClassDevsW(None, PCWSTR::null(), None, DIGCF_ALLCLASSES)?;
        let mut found = 0;
        for i in 0.. {
            let mut did = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            if SetupDiEnumDeviceInfo(set, i, &mut did).is_err() {
                break;
            }
            let mut buf = [0u8; 1024];
            if SetupDiGetDeviceRegistryPropertyW(set, &did, SPDRP_HARDWAREID, None, Some(&mut buf), None).is_err() {
                continue;
            }
            let ids: Vec<u16> = buf.chunks(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
            let ids = String::from_utf16_lossy(&ids);
            if ids.split('\0').any(|id| id.eq_ignore_ascii_case(HARDWARE_ID)) {
                found += 1;
                f(set, &did);
            }
        }
        let _ = SetupDiDestroyDeviceInfoList(set);
        Ok(found)
    }
}

pub fn installed() -> bool {
    for_each_device(|_, _| {}).unwrap_or(0) > 0
}

fn is_admin() -> bool {
    unsafe { IsUserAnAdmin().as_bool() }
}

/// Re-run this executable elevated with `args` (UAC prompt) and wait for it.
fn run_elevated(args: &str) -> Result<u32> {
    let exe = wide(&std::env::current_exe()?.to_string_lossy());
    let verb = wide("runas");
    let params = wide(args);
    let mut info = SHELLEXECUTEINFOW {
        cbSize: size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(exe.as_ptr()),
        lpParameters: PCWSTR(params.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };
    unsafe {
        ShellExecuteExW(&mut info).context("administrator rights were not granted")?;
        let mut code = 1u32;
        if !info.hProcess.is_invalid() {
            WaitForSingleObject(info.hProcess, INFINITE);
            let _ = GetExitCodeProcess(info.hProcess, &mut code);
            let _ = CloseHandle(info.hProcess);
        }
        Ok(code)
    }
}

fn copy_files(from: &Path) -> Result<PathBuf> {
    let to = PathBuf::from(INSTALL_DIR);
    std::fs::create_dir_all(&to).with_context(|| format!("creating {INSTALL_DIR}"))?;
    for f in FILES {
        let dst = to.join(f);
        // The bundled settings are known to work (the driver creates no
        // monitor with too many modes); keep a differing old file as .bak.
        if f == "vdd_settings.xml" && dst.exists() {
            let old = std::fs::read(&dst).unwrap_or_default();
            if old != std::fs::read(from.join(f)).unwrap_or_default() {
                let bak = to.join("vdd_settings.xml.bak");
                let _ = std::fs::write(&bak, old);
                println!("replaced vdd_settings.xml (previous one saved as {})", bak.display());
            }
        }
        std::fs::copy(from.join(f), &dst).with_context(|| format!("copying {f}"))?;
    }
    Ok(to.join("MttVDD.inf"))
}

fn install_elevated() -> Result<()> {
    let inf = copy_files(&bundled_dir()?)?;
    let inf_w = wide(&inf.to_string_lossy());
    let hwid_w = wide(HARDWARE_ID);
    unsafe {
        if !installed() {
            let mut guid = GUID::zeroed();
            let mut class = [0u16; 64];
            SetupDiGetINFClassW(PCWSTR(inf_w.as_ptr()), &mut guid, &mut class, None).context("reading the INF class")?;
            let set = SetupDiCreateDeviceInfoList(Some(&guid), None)?;
            let mut did = SP_DEVINFO_DATA { cbSize: size_of::<SP_DEVINFO_DATA>() as u32, ..Default::default() };
            let r = (|| -> Result<()> {
                SetupDiCreateDeviceInfoW(set, PCWSTR(class.as_ptr()), &guid, PCWSTR::null(), None, DICD_GENERATE_ID, Some(&mut did))?;
                // REG_MULTI_SZ: the id, then an empty string.
                let ids: Vec<u8> = HARDWARE_ID
                    .encode_utf16()
                    .chain([0, 0])
                    .flat_map(|c| c.to_le_bytes())
                    .collect();
                SetupDiSetDeviceRegistryPropertyW(set, &mut did, SPDRP_HARDWAREID, Some(&ids))?;
                SetupDiCallClassInstaller(DIF_REGISTERDEVICE, set, Some(&did)).context("registering the device")?;
                Ok(())
            })();
            let _ = SetupDiDestroyDeviceInfoList(set);
            r?;
            println!("created device {HARDWARE_ID}");
        }
        let mut reboot = windows::core::BOOL(0);
        UpdateDriverForPlugAndPlayDevicesW(
            None::<HWND>,
            PCWSTR(hwid_w.as_ptr()),
            PCWSTR(inf_w.as_ptr()),
            INSTALLFLAG_FORCE,
            Some(&mut reboot),
        )
        .context("installing the driver (was the driver installation prompt declined?)")?;
        // Restart the device so it reads the new settings (and recovers from
        // a failed state such as Code 43).
        restart_devices();
        println!("virtual display driver installed{}", if reboot.as_bool() { "; restart Windows to finish" } else { "" });
    }
    Ok(())
}

/// DIF_PROPERTYCHANGE / DICS_PROPCHANGE: stop and start each device.
fn restart_devices() {
    let _ = for_each_device(|set, did| unsafe {
        let params = SP_PROPCHANGE_PARAMS {
            ClassInstallHeader: SP_CLASSINSTALL_HEADER {
                cbSize: size_of::<SP_CLASSINSTALL_HEADER>() as u32,
                InstallFunction: DIF_PROPERTYCHANGE,
            },
            StateChange: DICS_PROPCHANGE,
            Scope: DICS_FLAG_CONFIGSPECIFIC,
            HwProfile: 0,
        };
        let r = SetupDiSetClassInstallParamsW(
            set,
            Some(did),
            Some(&params.ClassInstallHeader),
            size_of::<SP_PROPCHANGE_PARAMS>() as u32,
        )
        .and_then(|()| SetupDiCallClassInstaller(DIF_PROPERTYCHANGE, set, Some(did)));
        match r {
            Ok(()) => println!("restarted the virtual display device"),
            Err(e) => println!("restarting the virtual display device failed: {e}"),
        }
    });
}

fn uninstall_elevated() -> Result<()> {
    let removed = for_each_device(|set, did| unsafe {
        if let Err(e) = SetupDiCallClassInstaller(DIF_REMOVE, set, Some(did)) {
            println!("removing a device failed: {e}");
        }
    })?;
    println!("removed {removed} device(s)");
    let inf = Path::new(INSTALL_DIR).join("MttVDD.inf");
    if inf.exists() {
        let w = wide(&inf.to_string_lossy());
        match unsafe { DiUninstallDriverW(None, PCWSTR(w.as_ptr()), DIUNINSTALLDRIVER_FLAGS(0), None) } {
            Ok(()) => println!("removed the driver package"),
            Err(e) => println!("removing the driver package: {e}"),
        }
    }
    Ok(())
}

fn elevate_or(args: &str, f: fn() -> Result<()>) -> Result<()> {
    if is_admin() {
        let r = f();
        if let Err(e) = &r {
            // The elevated window closes on exit: keep the error readable.
            eprintln!("error: {e:#}");
            pause();
        }
        return r;
    }
    println!("asking for administrator rights...");
    let code = run_elevated(args)?;
    if code != 0 {
        bail!("the elevated installer failed (exit code {code})");
    }
    Ok(())
}

pub fn install() -> Result<()> {
    elevate_or("install-driver", install_elevated)?;
    if installed() {
        println!("Virtual display driver is installed. Start opendisplay-win.exe to use the device as a second screen.");
        Ok(())
    } else {
        bail!("the virtual display device was not created")
    }
}

pub fn uninstall() -> Result<()> {
    elevate_or("uninstall-driver", uninstall_elevated)?;
    if installed() {
        bail!("the virtual display device is still present")
    }
    println!("Virtual display driver removed.");
    Ok(())
}

pub fn pause() {
    use std::io::{BufRead, Write};
    print!("Press Enter to close...");
    let _ = std::io::stdout().flush();
    let _ = std::io::stdin().lock().lines().next();
}
