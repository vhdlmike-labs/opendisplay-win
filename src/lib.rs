//! opendisplay-win: a Windows sender for OpenDisplay (https://opendisplay.app).
//! Extends (or mirrors) the Windows desktop onto an iPad/iPhone running the
//! OpenDisplay receiver app, over Wi-Fi or USB. Wire protocol: PROTOCOL.md pv 3,
//! implemented by the upstream `opendisplay-proto`/`opendisplay-session` crates.
//!
//! Two front ends share this library: the `opendisplay-win` CLI
//! (`src/main.rs`) and the `OpenDisplay` tray app (`src/bin/OpenDisplay.rs`).

pub mod app;
pub mod capture;
pub mod cursor;
pub mod desktop;
pub mod discover;
pub mod display;
pub mod driver;
pub mod encoder;
pub mod input;
#[allow(dead_code)]
pub mod latest;
pub mod session;
pub mod usbmux;
