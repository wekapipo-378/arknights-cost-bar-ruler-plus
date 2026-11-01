//! Native window-frame helpers for the layered overlay windows.
//!
//! The overlay windows are per-pixel-alpha layered windows painted by Slint's
//! software renderer. They keep `WS_POPUP` for the frameless look but add
//! `WS_THICKFRAME` so Windows provides the normal sizing border: native resize
//! loop, edge/corner cursors and snapping. A thick frame makes the client area
//! smaller than the window rect, so the frame buffer is allocated at window
//! size and the rendered client image is blitted at the frame offset.

use windows::Win32::{
    Foundation::RECT,
    UI::WindowsAndMessaging::{AdjustWindowRectEx, WINDOW_EX_STYLE, WINDOW_STYLE},
};


/// `(window_w, window_h, offset_x, offset_y)` for a requested client size.
pub(super) fn window_metrics(
    style: WINDOW_STYLE,
    ex_style: WINDOW_EX_STYLE,
    client_w: i32,
    client_h: i32,
) -> (i32, i32, i32, i32) {
    let mut rect = RECT {
        left: 0,
        top: 0,
        right: client_w,
        bottom: client_h,
    };
    unsafe {
        let _ = AdjustWindowRectEx(&mut rect, style, false, ex_style);
    }
    let offset_x = -rect.left;
    let offset_y = -rect.top;
    let window_w = (rect.right - rect.left).max(client_w);
    let window_h = (rect.bottom - rect.top).max(client_h);
    (window_w, window_h, offset_x, offset_y)
}

