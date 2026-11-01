//! Minimal Unicode-text clipboard access for the inline editors.
//!
//! Slint's synthetic key events carry no modifier state in this version, so
//! Ctrl+C / Ctrl+V are implemented here instead of being forwarded.

#[cfg(windows)]
pub(crate) fn set_text(text: &str) -> bool {
    use windows::Win32::{
        Foundation::HANDLE,
        System::{
            DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
            Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE},
        },
    };

    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        if EmptyClipboard().is_err() {
            let _ = CloseClipboard();
            return false;
        }

        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = wide.len() * std::mem::size_of::<u16>();
        let Ok(handle) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            let _ = CloseClipboard();
            return false;
        };
        let target = GlobalLock(handle) as *mut u16;
        if target.is_null() {
            let _ = CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), target, wide.len());
        let _ = GlobalUnlock(handle);

        let ok = SetClipboardData(13, HANDLE(handle.0)).is_ok();
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(windows)]
pub(crate) fn get_text() -> Option<String> {
    use windows::Win32::{
        System::{
            DataExchange::{CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard},
            Memory::GlobalLock,
        },
    };

    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        if IsClipboardFormatAvailable(13).is_err() {
            let _ = CloseClipboard();
            return None;
        }
        let Ok(handle) = GetClipboardData(13) else {
            let _ = CloseClipboard();
            return None;
        };
        let pointer = GlobalLock(windows::Win32::Foundation::HGLOBAL(handle.0)) as *const u16;
        if pointer.is_null() {
            let _ = CloseClipboard();
            return None;
        }

        let mut length = 0usize;
        while *pointer.add(length) != 0 {
            length += 1;
        }
        let slice = std::slice::from_raw_parts(pointer, length);
        let text = String::from_utf16_lossy(slice);
        let _ = CloseClipboard();
        Some(text)
    }
}

#[cfg(not(windows))]
pub(crate) fn set_text(_text: &str) -> bool {
    false
}

#[cfg(not(windows))]
pub(crate) fn get_text() -> Option<String> {
    None
}
