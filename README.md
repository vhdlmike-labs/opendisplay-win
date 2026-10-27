# opendisplay-win

A Windows sender for [OpenDisplay](https://opendisplay.app): use an iPad or
iPhone running the stock OpenDisplay app as a display for a Windows PC, over
Wi-Fi or USB. It speaks the OpenDisplay wire protocol (`PROTOCOL.md`, pv 3),
so the receiver app needs no changes.

## Quick start

**Tray app:** `OpenDisplay.exe` (`src/bin/OpenDisplay.rs`, GUI subsystem) runs
the sender in the background with a notification-area icon showing the state
(grey waiting, blue streaming, amber mirroring/problem). Its menu covers
Extend/Mirror, connection (auto/USB/Wi-Fi), start with Windows, an optional
log file, driver install, and Quit, which detaches the virtual monitor first.
Settings live in `%APPDATA%\OpenDisplay\settings.json`. Both executables share
the library (`src/lib.rs`), and a named mutex allows only one sender per
session. The exes embed `assets/opendisplay.rc`: icons from
`packaging/make_icons.py`, version info, and a manifest requesting Common
Controls v6 (needed for `LoadIconMetric`) and PerMonitorV2. `build.rs`
compiles it with llvm-mingw's `windres`.

**Console:** `opendisplay-win.exe`:

Double-click `opendisplay-win.exe`: with no command it runs silently. It waits
for the receiver, streams to it, reconnects as needed, and stops when its
console window is closed, taking the virtual monitor down first. Pass
`--log [filter]` for a console log or `--log-file <path>` for a file; the
commands below are quiet without them, too.

The end-user package (`scripts/package.sh` → `dist/OpenDisplay-Windows-<version>.zip`)
contains the exe, the signed virtual display driver, `Install virtual display.cmd`
(runs `opendisplay-win install-driver`, which elevates itself and creates the
`Root\MttVDD` device via SetupAPI, with no devcon needed), and a plain-text README
(`packaging/README.txt`).

```
opendisplay-win list                      # receivers on Wi-Fi + USB, local monitors
opendisplay-win run                       # the receiver: over USB when cabled, else Wi-Fi
opendisplay-win run --to iPad             # by Bonjour name (or id fragment)
opendisplay-win run --to 192.168.1.162    # by address
opendisplay-win run --to usb              # over the cable (needs Apple Mobile Device Service)
```

When the device is both cabled and on Wi-Fi, the cable wins: lower latency
(about 3 ms vs 6 ms round trip) and a higher bitrate ceiling. It is checked on
every (re)dial, so pulling the cable continues over Wi-Fi. With a Bonjour name
(`--to iPad`), a USB device whose `hello` id doesn't match is skipped.
`--to <ip>` forces Wi-Fi and `--to usb` forces the cable.

Only one sender can drive a receiver: it always switches to the newest
connection. Two senders running at once take the device from each other
every second or two, and the log then warns about it.

Open OpenDisplay on the device first. Touch drives the mouse, two-finger
scroll drives the wheel, and Apple Pencil is injected as a Windows pen with
pressure and tilt. `Ctrl+C` stops. By default the sender exits when the app is
closed; `--stay` keeps waiting for it to come back.

### Extended desktop (recommended) vs. mirror

Windows only creates a monitor for a real display, so to *extend* the desktop
you need a virtual display driver. Without one, `opendisplay-win` mirrors the
primary monitor.

One-time setup (admin), tested with the signed MTT
[Virtual Display Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) 25.7.23:

1. Put `MttVDD.inf`, `MttVDD.dll`, `mttvdd.cat` (the "Driver Only" zip) and
   `vdd_settings.xml` in `C:\VirtualDisplayDriver\`. Add your device's panel
   resolution and its portrait version to `vdd_settings.xml`, e.g. 2224x1668
   and 1668x2224 for an iPad Pro 10.5". `run` logs the receiver's panel as
   `plan: panel WxH`.
2. `opendisplay-win install-driver` (asks for admin rights) copies
   `driver\*` next to the exe to `C:\VirtualDisplayDriver` and creates the
   `Root\MttVDD` device. `uninstall-driver` removes it. The package's
   `driver\vdd_settings.xml` (from `packaging/make_vdd_settings.py`) already
   lists current iPad/iPhone panels.

After that, plain `opendisplay-win run` (`--display auto`) does the rest on every connection:

- attaches the virtual monitor to the desktop (Win+P "Extend") if it is off or duplicated
- switches it to the receiver's desktop size (e.g. 2224x1668)
- sets its Windows scaling to the receiver's scale (200% for a Retina iPad)
- takes it off the desktop again when the session ends, so no windows are
  stranded on an invisible screen; Windows moves them back next time

The monitor stays where you arranged it in Settings > Display.

`--display DISPLAYn` streams a specific monitor as it is. Add `--resize true`
to switch it to the receiver's size for the session.
`--display primary` mirrors the main screen.

### Options

| Option | Meaning |
|---|---|
| `--scale mac\|passthrough\|<n>` | desktop size policy, as in the Linux sender (default `mac`: a 2x desktop for Retina panels) |
| `--max-fps <n>` | cap the frame rate below the receiver's panel rate |
| `--bitrate <Mbit/s>` | override the bitrate (default from resolution: 6–40 Mbit/s on Wi-Fi, 8–80 on USB) |
| `--software-encoder` | use Microsoft's CPU H.264 encoder instead of the GPU |
| `--log debug` | verbose logs; receiver telemetry is logged as `PHONE-STATS` |
| `--for <seconds>` | stop after a while (scripted tests) |

`opendisplay-win probe --display primary --out probe.h264` captures and encodes
60 frames to a file without a receiver, to check the capture and encoder.

## How it works

| Layer | Implementation |
|---|---|
| Protocol | the upstream sans-I/O crates `opendisplay-proto` + `opendisplay-session` (git dependency, pinned): framing, demux, `hello`/`welcome`, ping/pong, liveness, `kf`, cursor sequencing |
| Desktop/stream sizing | `src/desktop.rs`, vendored verbatim from the Linux sender (panel facts, H.264 L5.2 limits, `videoCaps`, 300 ms rotation debounce) |
| Discovery | mDNS `_opensidecar._tcp` (`mdns-sd`); USB via usbmuxd on `127.0.0.1:27015` (Apple Mobile Device Service) |
| Capture | DXGI Desktop Duplication (pointer excluded, sent as `cursor`/`cursorImg`) |
| Encode | D3D11 video processor (scale, BGRA→NV12, BT.709 limited) → Media Foundation H.264 hardware MFT (async, D3D11), Microsoft software MFT as fallback; High profile, low-latency, no B-frames, IDR only on demand, SPS/PPS on every IDR, 4-byte start codes |
| Flow control | at most 2 encoded frames queued; a slow link lowers the capture rate instead of dropping encoded frames |
| Virtual monitor | any IddCx virtual display driver; attach via `SetDisplayConfig(SDC_TOPOLOGY_EXTEND)`, mode via `ChangeDisplaySettingsEx`, scaling via the (undocumented) DisplayConfig DPI-scale info that Settings uses, detach on session end |
| Input | `SetCursorPos` + `SendInput` (touch, wheel), `InjectSyntheticPointerInput` `PT_PEN` (Pencil) |

## Building

Cross-compiled from Linux/WSL, no Visual Studio needed:

```
rustup target add x86_64-pc-windows-gnullvm
# llvm-mingw (https://github.com/mstorsjo/llvm-mingw) on PATH, for x86_64-w64-mingw32-clang
cargo build --release      # target set in .cargo/config.toml
cargo test                 # runs the Windows test exe (works from WSL)
```

The result is a single static `target/x86_64-pc-windows-gnullvm/release/opendisplay-win.exe`.

## Known limitations

- The UDP cursor side channel (§6.3) has not been acknowledged by the iPad in
  testing, so cursor positions fall back to TCP, as the spec requires. Over
  Wi-Fi the pointer can then lag behind large frames.
- No HEVC yet (`videoCaps` HEVC is ignored; H.264 is always offered by receivers).
- No virtual display driver is bundled; extended mode needs the one-time setup above.
- Per-monitor scaling uses an undocumented Windows interface. If a future
  Windows changes it, a warning is logged and the monitor keeps its scaling.
- The link is unencrypted, as with all OpenDisplay senders at pv 3. Use trusted networks.

## License

GPL-3.0-only, like OpenDisplay, from which the protocol crates and `desktop.rs`/`latest.rs` come.
