//! Find receivers: Bonjour on the LAN (§2.1) and usbmuxd on the cable (§2.2).
//! The mDNS browse is adapted from the Linux sender's `discover.rs`.

use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use mdns_sd::{ServiceDaemon, ServiceEvent};
use opendisplay_proto::{DEFAULT_PORT, SERVICE_TYPE};

use crate::usbmux;

#[derive(Debug, Clone)]
pub struct WifiReceiver {
    pub name: String,
    pub addr: SocketAddr,
    pub id: Option<String>,
    pub pv: u32,
}

/// Browse for `timeout`, returning every receiver that resolved.
pub fn browse(timeout: Duration) -> Result<Vec<WifiReceiver>> {
    let daemon = ServiceDaemon::new().context("starting mDNS browser")?;
    let ty = format!("{SERVICE_TYPE}.local.");
    let rx = daemon.browse(&ty).context("browsing")?;
    let deadline = Instant::now() + timeout;
    let mut out: Vec<WifiReceiver> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        let Ok(event) = rx.recv_timeout(left) else { break };
        let ServiceEvent::ServiceResolved(info) = event else { continue };
        let name = info
            .get_fullname()
            .trim_end_matches(&format!(".{ty}"))
            .to_string();
        // Prefer IPv4: link-local IPv6 needs a scope id.
        let Some(ip) = info
            .get_addresses()
            .iter()
            .map(|a| a.to_ip_addr())
            .min_by_key(|a| if a.is_ipv4() { 0 } else { 1 })
        else {
            continue;
        };
        if out.iter().any(|r| r.name == name) {
            continue;
        }
        out.push(WifiReceiver {
            name,
            addr: SocketAddr::new(ip, info.get_port()),
            id: info.get_property_val_str("id").map(str::to_owned),
            pv: info
                .get_property_val_str("pv")
                .and_then(|p| p.parse().ok())
                .unwrap_or(1),
        });
    }
    let _ = daemon.stop_browse(&ty);
    let _ = daemon.shutdown();
    Ok(out)
}

/// Where to dial, resolved once and redialed on every reconnect.
#[derive(Debug, Clone)]
pub enum Target {
    Tcp(SocketAddr),
    /// A usbmuxd device id; `None` means "the first USB device present".
    Usb(Option<u64>),
    /// The cable when a device answers on it, else Wi-Fi. `id` is the
    /// receiver's Bonjour/`hello` id: a USB session whose `hello.id` differs
    /// is a different device (§2.1: `id` is the device identity).
    Auto { wifi: Option<SocketAddr>, id: Option<String> },
}

impl Target {
    pub fn describe(&self) -> String {
        match self {
            Target::Tcp(a) => format!("{a} (Wi-Fi/LAN)"),
            Target::Usb(Some(id)) => format!("USB device {id}"),
            Target::Usb(None) => "first USB device".into(),
            Target::Auto { wifi: Some(a), .. } => format!("USB, else {a} (Wi-Fi/LAN)"),
            Target::Auto { wifi: None, .. } => "USB".into(),
        }
    }

    /// The receiver identity a USB session must present, if any.
    pub fn expected_id(&self) -> Option<&str> {
        match self {
            Target::Auto { id, .. } => id.as_deref(),
            _ => None,
        }
    }

    fn dial_usb(id: Option<u64>) -> Result<TcpStream> {
        let id = match id {
            Some(id) => id,
            None => match usbmux::list_devices()?.first() {
                Some(d) => d.device_id,
                None => bail!("no iPhone/iPad attached over USB"),
            },
        };
        usbmux::connect(id, DEFAULT_PORT)
    }

    fn dial_tcp(addr: &SocketAddr) -> Result<TcpStream> {
        TcpStream::connect_timeout(addr, Duration::from_secs(5)).with_context(|| format!("connecting to {addr}"))
    }

    /// Dial the receiver; returns the socket and whether it runs over USB.
    /// `allow_usb: false` skips the cable for `Auto` (it reached another device).
    pub fn dial(&self, allow_usb: bool) -> Result<(TcpStream, bool)> {
        let (stream, usb) = match self {
            Target::Tcp(addr) => (Self::dial_tcp(addr)?, false),
            Target::Usb(id) => (Self::dial_usb(*id)?, true),
            Target::Auto { wifi, .. } => {
                let usb = if allow_usb {
                    match Self::dial_usb(None) {
                        Ok(s) => Some(s),
                        Err(e) => {
                            tracing::debug!("USB: {e:#}");
                            None
                        }
                    }
                } else {
                    None
                };
                match (usb, wifi) {
                    (Some(s), _) => (s, true),
                    (None, Some(addr)) => (Self::dial_tcp(addr)?, false),
                    (None, None) => bail!("no receiver on USB"),
                }
            }
        };
        stream.set_nodelay(true)?;
        Ok((stream, usb))
    }
}

/// Turn `--to` into a dial target: `usb`, `usb:<id|serial>`, `ip[:port]`, or
/// a Bonjour name/id fragment.
pub fn resolve(to: Option<&str>, timeout: Duration) -> Result<Target> {
    match to {
        Some("usb") => return Ok(Target::Usb(None)),
        Some(t) if t.starts_with("usb:") => {
            let want = &t[4..];
            let dev = usbmux::list_devices()?
                .into_iter()
                .find(|d| d.device_id.to_string() == want || d.serial.eq_ignore_ascii_case(want))
                .with_context(|| format!("no USB device {want:?}"))?;
            return Ok(Target::Usb(Some(dev.device_id)));
        }
        Some("wifi") => {
            // Wi-Fi only: the only receiver on the network.
            let found = browse(timeout)?;
            return match found.as_slice() {
                [one] => Ok(Target::Tcp(one.addr)),
                [] => bail!("no receiver found on Wi-Fi within {timeout:?}"),
                _ => bail!("several receivers on Wi-Fi; pick one with --to <name>"),
            };
        }
        Some(t) => {
            if let Ok(a) = t.parse::<SocketAddr>() {
                return Ok(Target::Tcp(a));
            }
            if let Ok(ip) = t.parse::<std::net::IpAddr>() {
                return Ok(Target::Tcp(SocketAddr::new(ip, DEFAULT_PORT)));
            }
        }
        None => {}
    }
    let found = browse(timeout)?;
    let mut matching: Vec<_> = found
        .iter()
        .filter(|r| match to {
            Some(t) => {
                r.name.to_lowercase().contains(&t.to_lowercase())
                    || r.id.as_deref().is_some_and(|id| id.eq_ignore_ascii_case(t))
            }
            None => true,
        })
        .collect();
    match matching.len() {
        // Prefer the cable whenever this receiver is also plugged in.
        1 => {
            let r = matching.remove(0);
            Ok(Target::Auto { wifi: Some(r.addr), id: r.id.clone() })
        }
        0 if to.is_none() => {
            // Nothing on Wi-Fi: a cabled device is the next best guess.
            if usbmux::list_devices().is_ok_and(|d| !d.is_empty()) {
                return Ok(Target::Auto { wifi: None, id: None });
            }
            bail!(
                "no receiver found within {timeout:?}. Is OpenDisplay open in the foreground \
                 on the device, on the same network, with Local Network access allowed? \
                 Or connect with --to <ip> / --to usb."
            )
        }
        0 => {
            // A busy receiver can miss a browse window; the cable is still worth a try.
            if usbmux::list_devices().is_ok_and(|d| !d.is_empty()) {
                tracing::warn!("{:?} not seen on Wi-Fi; trying the USB device", to.unwrap_or_default());
                return Ok(Target::Auto { wifi: None, id: None });
            }
            bail!("no receiver matching {:?}", to.unwrap_or_default())
        }
        _ => {
            let names: Vec<_> = matching.iter().map(|r| format!("{} ({})", r.name, r.addr)).collect();
            bail!("several receivers found, pick one with --to: {}", names.join(", "))
        }
    }
}
