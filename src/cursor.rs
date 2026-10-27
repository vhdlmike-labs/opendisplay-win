//! Cursor forwarding (§6.2): position at input rate and the sprite as a PNG,
//! separate from the video so the pointer moves at input latency.

use base64::Engine;
use opendisplay_proto::control::CursorImg;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Base64 must stay under the §4 control-message limit.
const MAX_PNG: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CursorPos {
    pub x: f64,
    pub y: f64,
    pub visible: bool,
}

/// Where the pointer is relative to `rect`, and the current cursor handle.
pub fn poll(rect: &RECT) -> (CursorPos, Option<HCURSOR>) {
    unsafe {
        let mut ci = CURSORINFO { cbSize: size_of::<CURSORINFO>() as u32, ..Default::default() };
        if GetCursorInfo(&mut ci).is_err() {
            return (CursorPos { x: 0.0, y: 0.0, visible: false }, None);
        }
        let POINT { x, y } = ci.ptScreenPos;
        let inside = x >= rect.left && x < rect.right && y >= rect.top && y < rect.bottom;
        let showing = ci.flags.0 & CURSOR_SHOWING.0 != 0;
        let w = (rect.right - rect.left).max(1) as f64;
        let h = (rect.bottom - rect.top).max(1) as f64;
        (
            CursorPos {
                x: (x - rect.left) as f64 / w,
                y: (y - rect.top) as f64 / h,
                visible: inside && showing,
            },
            (!ci.hCursor.is_invalid()).then_some(ci.hCursor),
        )
    }
}

/// Read a bitmap as top-down BGRA rows.
unsafe fn bitmap_bgra(bmp: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        let mut bm = BITMAP::default();
        if GetObjectW(bmp.into(), size_of::<BITMAP>() as i32, Some(&mut bm as *mut _ as _)) == 0 {
            return None;
        }
        let (w, h) = (bm.bmWidth as u32, bm.bmHeight as u32);
        let mut info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w as i32,
                biHeight: -(h as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let dc = GetDC(None);
        let lines = GetDIBits(dc, bmp, 0, h, Some(buf.as_mut_ptr() as _), &mut info, DIB_RGB_COLORS);
        ReleaseDC(None, dc);
        (lines > 0).then_some((w, h, buf))
    }
}

/// The sprite of `cursor` as a `cursorImg` message for a display of `rect`.
pub fn sprite(cursor: HCURSOR, rect: &RECT) -> Option<CursorImg> {
    unsafe {
        let mut ii = ICONINFO::default();
        GetIconInfo(cursor.into(), &mut ii).ok()?;
        let color = (!ii.hbmColor.is_invalid()).then(|| bitmap_bgra(ii.hbmColor)).flatten();
        let mask = (!ii.hbmMask.is_invalid()).then(|| bitmap_bgra(ii.hbmMask)).flatten();
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor.into());
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(ii.hbmMask.into());
        }
        let (w, h, rgba) = match (color, mask) {
            (Some((w, h, bgra)), mask) => {
                let has_alpha = bgra.chunks(4).any(|p| p[3] != 0);
                let mut rgba = Vec::with_capacity(bgra.len());
                for (i, p) in bgra.chunks(4).enumerate() {
                    let a = if has_alpha {
                        p[3]
                    } else {
                        // Alpha from the AND mask: set bit = transparent.
                        match &mask {
                            Some((_, _, m)) if m[i * 4] != 0 => 0,
                            _ => 255,
                        }
                    };
                    rgba.extend_from_slice(&[p[2], p[1], p[0], a]);
                }
                (w, h, rgba)
            }
            (None, Some((w, h2, m))) => {
                // Monochrome: AND mask on top, XOR mask below.
                let h = h2 / 2;
                let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                for i in 0..(w * h) as usize {
                    let and = m[i * 4] != 0;
                    let xor = m[(i + (w * h) as usize) * 4] != 0;
                    rgba.extend_from_slice(match (and, xor) {
                        (true, false) => &[0, 0, 0, 0],
                        (false, false) => &[0, 0, 0, 255],
                        (false, true) => &[255, 255, 255, 255],
                        // Screen inversion has no PNG equivalent: draw it dark.
                        (true, true) => &[0, 0, 0, 200],
                    });
                }
                (w, h, rgba)
            }
            _ => return None,
        };
        let mut png_bytes = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut png_bytes, w, h);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let mut writer = enc.write_header().ok()?;
            writer.write_image_data(&rgba).ok()?;
        }
        if png_bytes.len() > MAX_PNG {
            return None;
        }
        let dw = (rect.right - rect.left).max(1) as f64;
        let dh = (rect.bottom - rect.top).max(1) as f64;
        Some(CursorImg {
            nw: w as f64 / dw,
            nh: h as f64 / dh,
            ax: ii.xHotspot as f64 / w as f64,
            ay: ii.yHotspot as f64 / h as f64,
            png: base64::engine::general_purpose::STANDARD.encode(&png_bytes),
        })
    }
}
