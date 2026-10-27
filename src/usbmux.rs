//! USB binding (PROTOCOL.md §2.2): dial the receiver's TCP port through
//! usbmuxd. On Windows usbmuxd is Apple Mobile Device Service (installed with
//! iTunes, "Apple Devices", or third-party apps such as Duet); it listens on
//! 127.0.0.1:27015 and speaks the same plist protocol as on macOS.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use plist::{Dictionary, Value};

const USBMUXD_ADDR: &str = "127.0.0.1:27015";
/// Header: length (incl. header), version 1 (plist), message 8 (plist), tag.
const HEADER_LEN: usize = 16;
const VERSION_PLIST: u32 = 1;
const MESSAGE_PLIST: u32 = 8;

#[derive(Debug, Clone)]
pub struct UsbDevice {
    pub device_id: u64,
    pub serial: String,
    pub product_id: Option<u64>,
}

fn request(stream: &mut TcpStream, tag: u32, mut dict: Dictionary) -> Result<Dictionary> {
    dict.insert("ClientVersionString".into(), "opendisplay-win".into());
    dict.insert("ProgName".into(), "opendisplay-win".into());
    dict.insert("kLibUSBMuxVersion".into(), Value::Integer(3.into()));
    let mut body = Vec::new();
    plist::to_writer_xml(&mut body, &Value::Dictionary(dict))?;
    let mut msg = Vec::with_capacity(HEADER_LEN + body.len());
    for v in [
        (HEADER_LEN + body.len()) as u32,
        VERSION_PLIST,
        MESSAGE_PLIST,
        tag,
    ] {
        msg.extend_from_slice(&v.to_le_bytes());
    }
    msg.extend_from_slice(&body);
    stream.write_all(&msg)?;

    let mut header = [0u8; HEADER_LEN];
    stream.read_exact(&mut header)?;
    let len = u32::from_le_bytes(header[0..4].try_into().unwrap()) as usize;
    if !(HEADER_LEN..=(1 << 24)).contains(&len) {
        bail!("usbmuxd: bad reply length {len}");
    }
    let mut reply = vec![0u8; len - HEADER_LEN];
    stream.read_exact(&mut reply)?;
    match plist::from_bytes::<Value>(&reply)? {
        Value::Dictionary(d) => Ok(d),
        other => bail!("usbmuxd: unexpected reply {other:?}"),
    }
}

fn open() -> Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&USBMUXD_ADDR.parse()?, Duration::from_secs(2))
        .context(
            "cannot reach usbmuxd (Apple Mobile Device Service) on 127.0.0.1:27015; \
             install Apple Devices or iTunes for USB support",
        )?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    Ok(stream)
}

/// iPhones/iPads attached over USB (network-paired entries are skipped).
pub fn list_devices() -> Result<Vec<UsbDevice>> {
    let mut stream = open()?;
    let mut d = Dictionary::new();
    d.insert("MessageType".into(), "ListDevices".into());
    let reply = request(&mut stream, 1, d)?;
    let list = reply
        .get("DeviceList")
        .and_then(Value::as_array)
        .context("usbmuxd: ListDevices reply without DeviceList")?;
    let mut out = Vec::new();
    for entry in list {
        let Some(props) = entry
            .as_dictionary()
            .and_then(|e| e.get("Properties"))
            .and_then(Value::as_dictionary)
        else {
            continue;
        };
        if props.get("ConnectionType").and_then(Value::as_string) != Some("USB") {
            continue;
        }
        let Some(device_id) = props.get("DeviceID").and_then(Value::as_unsigned_integer) else {
            continue;
        };
        out.push(UsbDevice {
            device_id,
            serial: props
                .get("SerialNumber")
                .and_then(Value::as_string)
                .unwrap_or("?")
                .to_string(),
            product_id: props.get("ProductID").and_then(Value::as_unsigned_integer),
        });
    }
    Ok(out)
}

/// Open a tunnel to `port` on the device. After `Result 0` the socket is a
/// transparent byte pipe to the receiver.
pub fn connect(device_id: u64, port: u16) -> Result<TcpStream> {
    let mut stream = open()?;
    let mut d = Dictionary::new();
    d.insert("MessageType".into(), "Connect".into());
    d.insert("DeviceID".into(), Value::Integer(device_id.into()));
    // usbmuxd wants the port in network byte order, read as a host integer.
    d.insert("PortNumber".into(), Value::Integer(u64::from(port.to_be()).into()));
    let reply = request(&mut stream, 2, d)?;
    match reply.get("Number").and_then(Value::as_unsigned_integer) {
        Some(0) => {
            stream.set_read_timeout(None)?;
            Ok(stream)
        }
        // 3 = connection refused: the app is not listening (not open, or in the background).
        Some(3) => bail!("device refused port {port}: is the OpenDisplay app open on it?"),
        n => bail!("usbmuxd Connect failed: {n:?}"),
    }
}
