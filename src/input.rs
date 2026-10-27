//! Inject receiver input (§6.1) into Windows, mapped onto the streamed
//! monitor's rectangle. Touch drives the mouse (as the Mac sender does),
//! scroll drives the wheel, Pencil is a synthetic pen with pressure and tilt.

use opendisplay_proto::control::{Pencil, PencilPhase, Proximity, Scroll, Touch, TouchPhase};
use tracing::{debug, warn};
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Controls::{
    CreateSyntheticPointerDevice, HSYNTHETICPOINTERDEVICE,
    POINTER_FEEDBACK_DEFAULT, POINTER_TYPE_INFO, POINTER_TYPE_INFO_0,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::Pointer::*;
use windows::Win32::UI::WindowsAndMessaging::{PEN_MASK_PRESSURE, PEN_MASK_TILT_X, PEN_MASK_TILT_Y, PT_PEN, SetCursorPos};

/// Wheel units per video pixel of finger travel (120 = one notch).
const WHEEL_PER_PIXEL: f64 = 2.0;

pub struct Injector {
    rect: RECT,
    pen: Option<HSYNTHETICPOINTERDEVICE>,
    pen_tried: bool,
    pen_down: bool,
    pen_in_range: bool,
    mouse_down: bool,
    wheel_acc: (f64, f64),
}

impl Injector {
    pub fn new(rect: RECT) -> Self {
        Self {
            rect,
            pen: None,
            pen_tried: false,
            pen_down: false,
            pen_in_range: false,
            mouse_down: false,
            wheel_acc: (0.0, 0.0),
        }
    }

    pub fn set_rect(&mut self, rect: RECT) {
        self.rect = rect;
    }

    fn to_screen(&self, x: f64, y: f64) -> POINT {
        let w = (self.rect.right - self.rect.left) as f64;
        let h = (self.rect.bottom - self.rect.top) as f64;
        POINT {
            x: self.rect.left + (x.clamp(0.0, 1.0) * (w - 1.0)).round() as i32,
            y: self.rect.top + (y.clamp(0.0, 1.0) * (h - 1.0)).round() as i32,
        }
    }

    fn buttons(flags: MOUSE_EVENT_FLAGS, data: i32) {
        let input = INPUT {
            r#type: INPUT_MOUSE,
            Anonymous: INPUT_0 {
                mi: MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: data as u32,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                },
            },
        };
        unsafe { SendInput(&[input], size_of::<INPUT>() as i32) };
    }

    pub fn touch(&mut self, t: &Touch) {
        let p = self.to_screen(t.x, t.y);
        unsafe {
            let _ = SetCursorPos(p.x, p.y);
        }
        match t.phase {
            TouchPhase::Began if !self.mouse_down => {
                Self::buttons(MOUSEEVENTF_LEFTDOWN, 0);
                self.mouse_down = true;
            }
            TouchPhase::Ended | TouchPhase::Cancelled if self.mouse_down => {
                Self::buttons(MOUSEEVENTF_LEFTUP, 0);
                self.mouse_down = false;
            }
            _ => {}
        }
    }

    pub fn scroll(&mut self, s: &Scroll) {
        // Natural-scrolling sign (§6.1): fingers down => dy > 0 => content
        // moves down, which on Windows is a positive (up) wheel delta.
        self.wheel_acc.0 += -s.dx * WHEEL_PER_PIXEL;
        self.wheel_acc.1 += s.dy * WHEEL_PER_PIXEL;
        let v = self.wheel_acc.1.trunc();
        if v != 0.0 {
            self.wheel_acc.1 -= v;
            Self::buttons(MOUSEEVENTF_WHEEL, v as i32);
        }
        let h = self.wheel_acc.0.trunc();
        if h != 0.0 {
            self.wheel_acc.0 -= h;
            Self::buttons(MOUSEEVENTF_HWHEEL, h as i32);
        }
    }

    fn pen_device(&mut self) -> Option<HSYNTHETICPOINTERDEVICE> {
        if !self.pen_tried {
            self.pen_tried = true;
            match unsafe { CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT) } {
                Ok(d) => self.pen = Some(d),
                Err(e) => warn!("no synthetic pen ({e}); Pencil drives the mouse"),
            }
        }
        self.pen
    }

    fn inject_pen(&mut self, x: f64, y: f64, flags: POINTER_FLAGS, pressure: u32, tilt: (i32, i32)) -> bool {
        let Some(dev) = self.pen_device() else { return false };
        let p = self.to_screen(x, y);
        let info = POINTER_TYPE_INFO {
            r#type: PT_PEN,
            Anonymous: POINTER_TYPE_INFO_0 {
                penInfo: POINTER_PEN_INFO {
                    pointerInfo: POINTER_INFO {
                        pointerType: PT_PEN,
                        pointerId: 0,
                        pointerFlags: flags,
                        ptPixelLocation: p,
                        ..Default::default()
                    },
                    penFlags: 0,
                    penMask: PEN_MASK_PRESSURE | PEN_MASK_TILT_X | PEN_MASK_TILT_Y,
                    pressure,
                    rotation: 0,
                    tiltX: tilt.0,
                    tiltY: tilt.1,
                },
            },
        };
        match unsafe { InjectSyntheticPointerInput(dev, &[info]) } {
            Ok(()) => true,
            Err(e) => {
                debug!("pen injection failed: {e}");
                false
            }
        }
    }

    pub fn pencil(&mut self, p: &Pencil) {
        // Tilt from iOS altitude (pi/2 = perpendicular) and azimuth.
        let lean = (std::f64::consts::FRAC_PI_2 - p.altitude).clamp(0.0, 1.5);
        let tilt = (
            (lean.tan() * p.azimuth.cos()).atan().to_degrees().round() as i32,
            (lean.tan() * p.azimuth.sin()).atan().to_degrees().round() as i32,
        );
        let pressure = (p.pressure.clamp(0.0, 1.0) * 1024.0) as u32;
        let (flags, pressure) = match p.phase {
            PencilPhase::Down => {
                self.pen_down = true;
                (POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_DOWN, pressure.max(1))
            }
            PencilPhase::Move if self.pen_down => {
                (POINTER_FLAG_INRANGE | POINTER_FLAG_INCONTACT | POINTER_FLAG_UPDATE, pressure.max(1))
            }
            PencilPhase::Up => {
                self.pen_down = false;
                (POINTER_FLAG_INRANGE | POINTER_FLAG_UP, 0)
            }
            _ => (POINTER_FLAG_INRANGE | POINTER_FLAG_UPDATE, 0), // hover
        };
        self.pen_in_range = true;
        if !self.inject_pen(p.x, p.y, flags, pressure, tilt) {
            // No pen support: behave like a finger.
            let phase = match p.phase {
                PencilPhase::Down => TouchPhase::Began,
                PencilPhase::Up => TouchPhase::Ended,
                _ if self.pen_down => TouchPhase::Moved,
                _ => return,
            };
            self.touch(&Touch { phase, x: p.x, y: p.y, t: None });
        }
    }

    pub fn proximity(&mut self, p: &Proximity) {
        if !p.entering && self.pen_in_range {
            self.pen_in_range = false;
            // Leaving hover range: an update without INRANGE.
            self.inject_pen(p.x, p.y, POINTER_FLAG_UPDATE, 0, (0, 0));
        }
    }

    /// Release anything held when the session ends.
    pub fn release(&mut self) {
        if self.mouse_down {
            Self::buttons(MOUSEEVENTF_LEFTUP, 0);
            self.mouse_down = false;
        }
        if self.pen_down {
            self.pen_down = false;
            self.inject_pen(0.5, 0.5, POINTER_FLAG_UP, 0, (0, 0));
        }
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        self.release();
        if let Some(d) = self.pen.take() {
            unsafe { windows::Win32::UI::Controls::DestroySyntheticPointerDevice(d) };
        }
    }
}
