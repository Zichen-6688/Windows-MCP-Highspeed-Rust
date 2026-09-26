//! Highlight overlay: draws a colored, transparent, click-through border
//! rectangle over an element's bounding rect for `duration_ms`, on a dedicated
//! thread with its own message pump. Never steals focus, never crashes the
//! server on failure.

use std::thread::JoinHandle;

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, FrameRect, GetDC, ReleaseDC, HBRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassExW, SetLayeredWindowAttributes, SetTimer, SetWindowPos, ShowWindow,
    TranslateMessage, UnregisterClassW, LWA_COLORKEY, MSG, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SW_SHOWNOACTIVATE, WM_DESTROY, WM_TIMER, WNDCLASSEXW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, HWND_TOPMOST,
};

use crate::error::AppError;

/// An RGB color for the overlay border.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb(pub u8, pub u8, pub u8);

impl Rgb {
    fn colorref(self) -> COLORREF {
        COLORREF(self.0 as u32 | (self.1 as u32) << 8 | (self.2 as u32) << 16)
    }
}

/// Parse a color name or `#RRGGBB` hex string.
pub fn parse_color(input: &str) -> Result<Rgb, AppError> {
    let named = match input.to_ascii_lowercase().as_str() {
        "red" => Some(Rgb(255, 0, 0)),
        "green" => Some(Rgb(0, 180, 0)),
        "blue" => Some(Rgb(0, 120, 255)),
        "yellow" => Some(Rgb(255, 220, 0)),
        "cyan" => Some(Rgb(0, 220, 220)),
        "magenta" => Some(Rgb(255, 0, 255)),
        "white" => Some(Rgb(255, 255, 255)),
        "black" => Some(Rgb(0, 0, 0)),
        "orange" => Some(Rgb(255, 140, 0)),
        _ => None,
    };
    if let Some(rgb) = named {
        return Ok(rgb);
    }
    let hex = input
        .strip_prefix('#')
        .ok_or_else(|| AppError::InvalidParams(format!("unknown color '{input}'")))?;
    if hex.len() != 6 {
        return Err(AppError::InvalidParams(format!(
            "color '{input}' must be a name or #RRGGBB"
        )));
    }
    let r = u8::from_str_radix(&hex[0..2], 16)
        .map_err(|_| AppError::InvalidParams(format!("invalid color '{input}'")))?;
    let g = u8::from_str_radix(&hex[2..4], 16)
        .map_err(|_| AppError::InvalidParams(format!("invalid color '{input}'")))?;
    let b = u8::from_str_radix(&hex[4..6], 16)
        .map_err(|_| AppError::InvalidParams(format!("invalid color '{input}'")))?;
    Ok(Rgb(r, g, b))
}

/// Interior fill uses a transparent color key so only the border is visible.
const KEY_COLOR: COLORREF = COLORREF(0x000001);

const CLASS_NAME: &str = "windows-mcp-highspeed-highlight\0";
const BORDER_PX: i32 = 3;

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_TIMER => {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

struct HighlightParams {
    rect: [i32; 4],
    duration_ms: u32,
    color: Rgb,
}

/// Show the overlay. Spawns a short-lived thread with a message pump and
/// returns immediately; the window destroys itself after `duration_ms`.
pub fn show_overlay(rect: [i32; 4], duration_ms: u64, color: Rgb) -> Result<JoinHandle<()>, AppError> {
    if rect[2] <= 0 || rect[3] <= 0 {
        return Err(AppError::InvalidParams(format!(
            "cannot highlight an empty rectangle {rect:?}"
        )));
    }
    let params = HighlightParams {
        rect,
        duration_ms: duration_ms.clamp(1, 60_000) as u32,
        color,
    };
    std::thread::Builder::new()
        .name("highlight-overlay".to_string())
        .spawn(move || {
            if let Err(e) = run_overlay(&params) {
                tracing::warn!(error = %e, "highlight overlay failed");
            }
        })
        .map_err(|e| AppError::Internal(format!("failed to spawn highlight thread: {e}")))
}

fn run_overlay(params: &HighlightParams) -> Result<(), AppError> {
    let [x, y, w, h] = params.rect;
    unsafe {
        let hinstance = GetModuleHandleW(None)
            .map_err(|e| AppError::Internal(format!("GetModuleHandleW failed: {e}")))?;

        let class = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            hInstance: hinstance.into(),
            lpszClassName: windows::core::PCWSTR(CLASS_NAME.as_ptr() as *const u16),
            ..Default::default()
        };
        let atom = RegisterClassExW(&class);
        if atom == 0 {
            return Err(AppError::Internal(
                "RegisterClassExW failed for highlight overlay".into(),
            ));
        }
        let class_name_ptr = CLASS_NAME.as_ptr() as *const u16;

        let result = (|| -> Result<(), AppError> {
            let hwnd = CreateWindowExW(
                WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                windows::core::PCWSTR(class_name_ptr),
                windows::core::PCWSTR::null(),
                WS_POPUP,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(hinstance.into()),
                None,
            )
            .map_err(|e| AppError::Internal(format!("CreateWindowExW failed: {e}")))?;

            // Color key makes the interior fully transparent; only the FrameRect
            // border is visible.
            SetLayeredWindowAttributes(hwnd, KEY_COLOR, 0, LWA_COLORKEY)
                .map_err(|e| AppError::Internal(format!("SetLayeredWindowAttributes failed: {e}")))?;

            let hdc = GetDC(Some(hwnd));
            if hdc.is_invalid() {
                let _ = DestroyWindow(hwnd);
                return Err(AppError::Internal("GetDC failed for highlight overlay".into()));
            }
            let key_brush: HBRUSH = CreateSolidBrush(KEY_COLOR);
            let border_brush: HBRUSH = CreateSolidBrush(params.color.colorref());
            let fill = windows::Win32::Foundation::RECT { left: 0, top: 0, right: w, bottom: h };
            let _ = FillRect(hdc, &fill, key_brush);
            // FrameRect draws a border one logical unit thick; draw nested
            // frames for a thicker border.
            for i in 0..BORDER_PX {
                let frame = windows::Win32::Foundation::RECT {
                    left: i,
                    top: i,
                    right: w - i,
                    bottom: h - i,
                };
                let _ = FrameRect(hdc, &frame, border_brush);
            }
            let _ = ReleaseDC(Some(hwnd), hdc);
            let _ = DeleteObject(key_brush.into());
            let _ = DeleteObject(border_brush.into());

            let _ = SetTimer(Some(hwnd), 1, params.duration_ms, None);
            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );

            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&mut msg, None, 0, 0);
                if ret.0 <= 0 {
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            Ok(())
        })();

        let _ = UnregisterClassW(windows::core::PCWSTR(class_name_ptr), Some(hinstance.into()));
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_named_and_hex_colors() {
        assert_eq!(parse_color("red").unwrap(), Rgb(255, 0, 0));
        assert_eq!(parse_color("Red").unwrap(), Rgb(255, 0, 0));
        assert_eq!(parse_color("#00ff40").unwrap(), Rgb(0, 255, 64));
        assert!(parse_color("nope").is_err());
        assert!(parse_color("#00").is_err());
        assert!(parse_color("#zzzzzz").is_err());
    }
}
