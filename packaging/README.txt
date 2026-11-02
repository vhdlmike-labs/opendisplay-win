OpenDisplay for Windows
=======================

Use an iPad or iPhone as a second screen for this Windows PC, over a USB
cable or Wi-Fi, with the free OpenDisplay app (https://opendisplay.app).
Touch, scrolling and Apple Pencil work on the device.

Requirements
------------
- Windows 10 (1903 or later) or Windows 11, 64-bit.
- The OpenDisplay app on the iPad/iPhone (App Store).
- For USB: Apple's device support, which comes with the "Apple Devices" app
  (Microsoft Store) or iTunes. Not needed for Wi-Fi.
- For Wi-Fi: the PC and the device on the same network. When Windows asks
  whether opendisplay-win may use the network, allow it (private networks
  at least), otherwise the device cannot be found automatically.


Setup (once)
------------
1. Unzip this folder somewhere permanent, e.g. C:\Program Files\OpenDisplay
   or your Documents folder. Keep the "driver" folder next to the exe.

2. Double-click "Install virtual display.cmd" and confirm the Windows prompts
   (administrator rights, and "Install this device software? Publisher:
   SignPath Foundation"). This adds a virtual monitor so the device becomes an
   EXTRA screen. Without it, the device just mirrors your main screen.

   Windows may warn that opendisplay-win.exe is from an unknown publisher
   (it is not code-signed). Choose "More info" > "Run anyway".


Use
---
1. Open OpenDisplay on the iPad/iPhone and connect the cable (or be on Wi-Fi).
2. Double-click OpenDisplay.exe. It runs in the background with an icon in
   the notification area (bottom right; it may sit under the ^ arrow, and you
   can drag it onto the taskbar). The device becomes a second screen within a
   few seconds, and a notification says which device connected. With the
   cable plugged in, USB is used (lowest latency).

The icon shows the state:
   grey   waiting for a device (open OpenDisplay on it)
   blue   streaming: the device is your second screen
   amber  mirroring although "Extend" is selected, or a problem
Hover over it for details, click it (left or right) for the menu:
   Extend: use as a second screen / Mirror the main screen
   Connection: Automatic (USB, else Wi-Fi) / USB cable only / Wi-Fi only
   Battery saver: Off (smoothest, up to 60 fps) / On (30 fps, less GPU and
      battery use, picture stays sharp) / Automatic (on while on battery)
   Start with Windows
   Write log file / Open log file
   Install virtual display driver... (only shown when it is missing)
   Quit OpenDisplay: the virtual screen goes away and windows on it move
   back to your main screen.

Leave it running: if the device locks, the app is closed or the cable is
pulled, it reconnects by itself (falling back to Wi-Fi when possible).
Settings are kept in %APPDATA%\OpenDisplay, the log in %LOCALAPPDATA%\OpenDisplay.

Arrange the screen (left/right of your monitor) in Windows Settings >
System > Display, like any monitor. Windows remembers the position.

Touch drives the mouse (tap = click, drag = drag), two fingers scroll,
Apple Pencil works as a pen with pressure and tilt.

opendisplay-win.exe is the same sender as a console program, for
troubleshooting and scripting (`opendisplay-win.exe help`). Only one of the
two can run at a time.


Troubleshooting
---------------
- See what is happening: tick "Write log file" in the tray menu, then
  "Open log file". Or quit the tray app and run "OpenDisplay (with log).cmd",
  which shows a live log in a console window.
- List what the sender can see (devices on Wi-Fi/USB, monitors):
      opendisplay-win.exe list
- Device not found on Wi-Fi: make sure OpenDisplay is open in the foreground,
  allowed under iPad Settings > Privacy & Security > Local Network, and that
  Windows Firewall allows opendisplay-win. You can also connect directly:
      opendisplay-win.exe run --to 192.168.1.50      (the device's IP)
      opendisplay-win.exe run --to usb
- The device keeps disconnecting (every few seconds, or every 15 seconds or
  so), or the log says "another sender keeps taking this device over":
  another sender is connected to the same device, and only one can drive
  it. Check other Windows users signed in on this PC (Task Manager > Users;
  OpenDisplay there pauses itself from version 0.2.4 on, older versions do
  not), other PCs and Macs.
- The device keeps disconnecting every 15 seconds or so, or the log says
  "the screen being streamed renders on <NVIDIA ...>": on a laptop with two
  graphics chips, run "Install virtual display.cmd" again. It moves the
  virtual screen to the integrated (power-saving) chip, where the hardware
  video encoder works and the discrete one can stay asleep.
- The device only mirrors the main screen: test the virtual screen alone,
  without the device:
      opendisplay-win.exe virtual-display on --log
      opendisplay-win.exe virtual-display off
  "on" should add a second screen in Settings > Display. If it reports an
  error, send its output along with `opendisplay-win.exe monitors`.
- Text too small or big on the device: Windows Settings > Display > select
  the virtual screen > Scale. The sender sets 200% for Retina devices.
- All options:  opendisplay-win.exe help run


Uninstall
---------
Double-click "Uninstall virtual display.cmd", then delete this folder.
The driver's settings folder C:\VirtualDisplayDriver can be deleted afterwards.


Licenses
--------
opendisplay-win is free software under the GNU GPL v3 (LICENSE.txt). It uses
the OpenDisplay protocol code by the OpenDisplay project (GPL-3.0). The
virtual display driver is "Virtual Display Driver" by VirtualDrivers/MikeTheTech
(MIT, driver\LICENSE.txt), https://github.com/VirtualDrivers/Virtual-Display-Driver
