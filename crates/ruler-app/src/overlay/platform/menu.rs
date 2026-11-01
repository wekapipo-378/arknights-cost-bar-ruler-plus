//! Settings window: a normal framed Win32 window whose client area is built
//! from native common controls (no Slint surface, no software framebuffer).
//!
//! Layout follows the classic WinRAR options dialog: a page list on the left
//! (通用 / 校准 / 外观 / 关于), a group-boxed form per page on the right and a
//! footer with 保存 / 确定 / 取消.
//!
//! Settings edited here take effect immediately (the HUD reacts on the next
//! frame), so the footer behaves the way users expect from other settings
//! dialogs: 保存 applies and keeps the window open, 确定 applies and closes and
//! 取消 rolls the editable values (display mode / scale / pause lead) back to the
//! snapshot captured when the window opened.
//!
//! The Slint component `ui/menu.slint` is intentionally left untouched: the
//! generated `RulerMenu` stays available for other tooling even though this
//! module no longer renders it.

use std::{
    cell::Cell,
    ffi::c_void,
    path::PathBuf,
    sync::{mpsc::Sender, Arc},
};

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{
            CreateFontW, CreateSolidBrush, DeleteObject, RedrawWindow, HFONT, HGDIOBJ,
            RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
        },
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                GetKeyState, SetFocus, VK_BACK, VK_CONTROL, VK_MENU, VK_SHIFT,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetSystemMetrics,
                GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, LoadCursorW,
                IsWindowVisible, HWND_BOTTOM, RegisterClassW, SendMessageW, SetForegroundWindow,
                SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow, CREATESTRUCTW,
                CS_HREDRAW, CS_VREDRAW, GetDlgCtrlID, GWL_STYLE, GWLP_USERDATA, HMENU, IDC_ARROW,
                SM_CXSCREEN, SM_CYSCREEN, SW_HIDE, SW_SHOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOREDRAW,
                SWP_NOSIZE, SWP_NOZORDER, WINDOW_EX_STYLE, WINDOW_STYLE, WM_CLOSE, WM_COMMAND,
                WM_CREATE, WM_CTLCOLORSTATIC, WM_DESTROY, WM_KEYDOWN, WM_NCCREATE, WM_SIZE,
                WM_TIMER, WNDCLASSW,
            },
        },
    },
};

use super::hud::{window_state, window_state_mut};
use crate::{
    commands::UiCommand,
    i18n::I18n,
    resources::ResourceLocator,
    slint_win::wide,
    ui_state::{parse_timer_input_frames, FrameDisplayMode, OverlayMode, ProfileMenuItem, VERSION},
    worker::SharedAppState,
};

const MENU_TIMER_ID: usize = 2;
const MENU_TIMER_MS: u32 = 33;
// WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_VISIBLE (+ WS_CLIPCHILDREN).
// No WS_THICKFRAME / WS_MAXIMIZEBOX: the settings window has a fixed size and
// must not be resizable by the user.
const MENU_STYLE: u32 = 0x00C0_0000 | 0x0008_0000 | 0x0002_0000
    | 0x1000_0000
    // WS_CLIPCHILDREN: keep the class brush from painting over the controls.
    | 0x0200_0000;
// WS_EX_WINDOWEDGE only: the settings window is an ordinary window (no
// always-on-top, no exotic composition). `WS_EX_TOPMOST` used to fight with the
// HUD and `WS_EX_COMPOSITED` kept late-created child controls from painting at
// all, which is why the right-hand pane looked empty.
const MENU_EX_STYLE: u32 = 0x0000_0100;

const MENU_MIN_W: f32 = 560.0;
const MENU_MIN_H: f32 = 400.0;
// Default client area of the settings window: 620 x 440 physical pixels at 100%
// scale (logical units, multiplied by the layout scale).
const DEFAULT_CLIENT_W: f32 = 620.0;
const DEFAULT_CLIENT_H: f32 = 440.0;
// The dialog may follow the overlay scale, but never so much that it no longer
// fits on screen — that is what pushed the right-hand pane out of view.
const MENU_SCREEN_FILL: f32 = 0.90;

// ---------------------------------------------------------------------------
// Win32 raw constants. The `windows` crate wraps the WS_* values in newtypes
// and mixes `i32`/`u32` for the control styles, so the numeric values are kept
// here and combined as plain `u32`.
// ---------------------------------------------------------------------------

const STYLE_CHILD: u32 = 0x4000_0000;
const STYLE_VISIBLE: u32 = 0x1000_0000;
const STYLE_BORDER: u32 = 0x0080_0000;
const STYLE_VSCROLL: u32 = 0x0020_0000;
const STYLE_TABSTOP: u32 = 0x0001_0000;

const BS_PUSHBUTTON: u32 = 0x0000_0000;
// WS_EX_STATICEDGE: sunken border on a plain static.
const EX_STATICEDGE: u32 = 0x0000_0200;
const CBS_DROPDOWNLIST: u32 = 0x0000_0003;
// Editable dropdown: the user can pick a preset or type an arbitrary percentage.
const CBS_DROPDOWN: u32 = 0x0000_0002;
const ES_AUTOHSCROLL: u32 = 0x0000_0080;
const LBS_NOTIFY: u32 = 0x0000_0001;
const LBS_NOINTEGRALHEIGHT: u32 = 0x0000_0100;

const MSG_CB_ADDSTRING: u32 = 0x0143;
const MSG_CB_GETCURSEL: u32 = 0x0147;
const MSG_CB_SETCURSEL: u32 = 0x014E;
const MSG_LB_ADDSTRING: u32 = 0x0180;
const MSG_LB_GETCURSEL: u32 = 0x0188;
const MSG_LB_RESETCONTENT: u32 = 0x0184;
const MSG_LB_SETCURSEL: u32 = 0x0186;
const MSG_WM_SETFONT: u32 = 0x0030;

// Notification codes delivered through WM_COMMAND.
const LBN_SELCHANGE: u16 = 1;
const LBN_DBLCLK: u16 = 2;
const EN_KILLFOCUS: u16 = 0x0200;
const EN_SETFOCUS: u16 = 0x0100;

// Control ids.
const ID_PAGES: i32 = 1000;
const ID_COMBO_DISPLAY: i32 = 1010;
const ID_COMBO_SCALE: i32 = 1011;
const ID_TIMER_MINUS_CYCLE: i32 = 1020;
const ID_TIMER_MINUS_ONE: i32 = 1021;
const ID_TIMER_RESET: i32 = 1022;
const ID_TIMER_PLUS_ONE: i32 = 1023;
const ID_TIMER_PLUS_CYCLE: i32 = 1024;
const ID_TIMER_UNDO: i32 = 1025;
const ID_LEAD_EDIT: i32 = 1030;
const ID_PROFILE_LIST: i32 = 1040;
const ID_PROFILE_SELECT: i32 = 1041;
const ID_PROFILE_RENAME: i32 = 1042;
const ID_PROFILE_DELETE: i32 = 1043;
const ID_PROFILE_NEW: i32 = 1044;
const ID_PROFILE_RENAME_EDIT: i32 = 1045;
const ID_PROFILE_RENAME_OK: i32 = 1046;
const ID_PROFILE_RENAME_CANCEL: i32 = 1047;
const ID_PROFILE_HINT: i32 = 1048;
const ID_TIMER_LABEL: i32 = 1050;
const ID_TIMER_EDIT: i32 = 1051;
const ID_TIMER_SET: i32 = 1052;
const ID_TIMER_RESET_TIMER: i32 = 1053;
const ID_HOTKEY_RECORD: i32 = 1065;
const ID_HOTKEY_PAUSE: i32 = 1066;
const ID_HOTKEY_LAP: i32 = 1067;
const ID_ABOUT_OPEN: i32 = 1071;
const ID_ABOUT_BILIBILI: i32 = 1072;
const ID_BTN_SAVE: i32 = 1100;
const ID_BTN_OK: i32 = 1101;
const ID_BTN_CANCEL: i32 = 1102;
/// `IsDialogMessageW` reports Escape as a WM_COMMAND with IDCANCEL.
const ID_CANCEL_COMMAND: i32 = 2;
/// Combo-box notification: the list was closed by choosing an item / Enter.
const CBN_SELENDOK: u16 = 9;

// ---------------------------------------------------------------------------
// Logical layout metrics (multiplied by the window DPI scale at layout time).
// ---------------------------------------------------------------------------

const M: f32 = 12.0;
const SIDE_W: f32 = 132.0;
const GAP: f32 = 10.0;
const CX: f32 = M + SIDE_W + GAP;
const PAD: f32 = 12.0;
const ROW_H: f32 = 26.0;
const FOOTER_H: f32 = 30.0;
const BTN_W: f32 = 70.0;
const BTN_GAP: f32 = 6.0;

const PAGES: [Page; 4] = [
    Page::Calibration,
    Page::Appearance,
    Page::Hotkeys,
    Page::About,
];

thread_local! {
    /// Background brush handed to the window class and to WM_CTLCOLORSTATIC.
    /// The class is registered once per process, so the brush has to outlive
    /// every settings window (deleting it would leave the class with a dangling
    /// handle on the next open).
    static CLASS_BRUSH: Cell<isize> = const { Cell::new(0) };
    static CLASS_READY: Cell<bool> = const { Cell::new(false) };
}

#[derive(serde::Deserialize, serde::Serialize)]
struct MenuBounds {
    x: i32,
    y: i32,
    width: f32,
    height: f32,
}

fn debug_note(text: &str) {
    let Some(dir) = ResourceLocator::new().config_path().parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let path = dir.join("menu_debug.txt");
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{text}");
    }
}

fn menu_bounds_path() -> PathBuf {
    let resources = ResourceLocator::new();
    let config = resources.config_path();
    match config.parent() {
        Some(parent) => parent.join("menu_window.json"),
        None => PathBuf::from("menu_window.json"),
    }
}

fn load_menu_bounds() -> Option<MenuBounds> {
    let text = std::fs::read_to_string(menu_bounds_path()).ok()?;
    serde_json::from_str(&text).ok()
}

unsafe fn save_menu_bounds(hwnd: HWND, scale: f32) {
    let mut window_rect = RECT::default();
    let mut client_rect = RECT::default();
    if GetWindowRect(hwnd, &mut window_rect).is_err() || GetClientRect(hwnd, &mut client_rect).is_err()
    {
        return;
    }
    let divisor = if scale > 0.1 { scale } else { 1.0 };
    // Sizes are stored as *client* sizes, so reopening the window with the saved
    // values does not creep outwards by the frame on every open.
    let bounds = MenuBounds {
        x: window_rect.left,
        y: window_rect.top,
        width: ((client_rect.right - client_rect.left) as f32 / divisor).max(MENU_MIN_W),
        height: ((client_rect.bottom - client_rect.top) as f32 / divisor).max(MENU_MIN_H),
    };
    if let Ok(text) = serde_json::to_string_pretty(&bounds) {
        let _ = std::fs::write(menu_bounds_path(), text);
    }
}

// ---------------------------------------------------------------------------
// Small Win32 helpers
// ---------------------------------------------------------------------------

fn sp(value: f32, scale: f32) -> i32 {
    (value * scale).round() as i32
}

fn screen_size() -> (f32, f32) {
    let width = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(480) as f32;
    let height = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(360) as f32;
    (width, height)
}

/// Layout scale of the settings dialog: monitor DPI scale × overlay percentage.
/// At 100% overlay scale on a 100% DPI display the client area is exactly
/// `DEFAULT_CLIENT_W x DEFAULT_CLIENT_H` pixels. `first_open` additionally caps
/// it so the very first window always fits on screen; afterwards scaling stays
/// proportional.
fn resolve_layout_scale(base: f32, pct: u16, first_open: bool) -> f32 {
    let base = if base > 0.1 { base } else { 1.0 };
    let wanted = base * (f32::from(pct.max(1)) / 100.0);
    if !first_open {
        return wanted.clamp(0.3, 6.0);
    }
    let (screen_w, screen_h) = screen_size();
    let max_scale = (screen_w * MENU_SCREEN_FILL / DEFAULT_CLIENT_W)
        .min(screen_h * MENU_SCREEN_FILL / DEFAULT_CLIENT_H);
    wanted.min(max_scale).max(0.3)
}

unsafe fn make_child(
    parent: HWND,
    class: &str,
    text: &str,
    style: u32,
    id: i32,
    font: HFONT,
) -> HWND {
    let class_name = wide(class);
    let window_name = wide(text);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(window_name.as_ptr()),
        WINDOW_STYLE(style),
        0,
        0,
        10,
        10,
        parent,
        HMENU(id as isize as *mut c_void),
        HINSTANCE::default(),
        None,
    )
    .unwrap_or_default();
    if !hwnd.0.is_null() {
        SendMessageW(
            hwnd,
            MSG_WM_SETFONT,
            WPARAM(font.0 as isize as usize),
            LPARAM(1),
        );
    }
    hwnd
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    if hwnd.0.is_null() {
        return;
    }
    let value = wide(text);
    let _ = SetWindowTextW(hwnd, PCWSTR(value.as_ptr()));
}

/// Section container: a static with a sunken edge. A static always fills its
/// own background with the brush returned by `WM_CTLCOLORSTATIC`, which is what
/// makes resizing ghost-free — a `BS_GROUPBOX` paints only its border and is
/// clipped out of the parent's erase, so anything a control left behind over it
/// could never be cleared.
unsafe fn make_panel(parent: HWND, font: HFONT) -> HWND {
    let class_name = wide("STATIC");
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(EX_STATICEDGE),
        PCWSTR(class_name.as_ptr()),
        PCWSTR::null(),
        WINDOW_STYLE(STYLE_CHILD | STYLE_VISIBLE),
        0,
        0,
        10,
        10,
        parent,
        HMENU::default(),
        HINSTANCE::default(),
        None,
    )
    .unwrap_or_default();
    if !hwnd.0.is_null() {
        SendMessageW(
            hwnd,
            MSG_WM_SETFONT,
            WPARAM(font.0 as isize as usize),
            LPARAM(1),
        );
        // Keep every panel at the very bottom of the sibling z-order: the labels,
        // edits and buttons of a section live *inside* the panel rectangle and
        // must never be clipped/covered by it.
        let _ = SetWindowPos(
            hwnd,
            HWND_BOTTOM,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
    hwnd
}

unsafe fn get_text(hwnd: HWND) -> String {
    if hwnd.0.is_null() {
        return String::new();
    }
    let length = GetWindowTextLengthW(hwnd).max(0) as usize;
    let mut buffer = vec![0u16; length + 2];
    let written = GetWindowTextW(hwnd, &mut buffer).max(0) as usize;
    String::from_utf16_lossy(&buffer[..written.min(buffer.len())])
}

unsafe fn combo_add(hwnd: HWND, text: &str) {
    let value = wide(text);
    SendMessageW(
        hwnd,
        MSG_CB_ADDSTRING,
        WPARAM(0),
        LPARAM(value.as_ptr() as isize),
    );
}

unsafe fn combo_cur(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, MSG_CB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32
}

unsafe fn combo_set(hwnd: HWND, index: i32) {
    SendMessageW(hwnd, MSG_CB_SETCURSEL, WPARAM(index as usize), LPARAM(0));
}

unsafe fn list_add(hwnd: HWND, text: &str) {
    let value = wide(text);
    SendMessageW(
        hwnd,
        MSG_LB_ADDSTRING,
        WPARAM(0),
        LPARAM(value.as_ptr() as isize),
    );
}

unsafe fn list_cur(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, MSG_LB_GETCURSEL, WPARAM(0), LPARAM(0)).0 as i32
}

unsafe fn list_clear(hwnd: HWND) {
    SendMessageW(hwnd, MSG_LB_RESETCONTENT, WPARAM(0), LPARAM(0));
}

unsafe fn list_set(hwnd: HWND, index: i32) {
    SendMessageW(hwnd, MSG_LB_SETCURSEL, WPARAM(index as usize), LPARAM(0));
}

// ---------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Page {
    Calibration,
    Appearance,
    Hotkeys,
    About,
}

impl Page {
    fn index(self) -> usize {
        match self {
            Self::Calibration => 0,
            Self::Appearance => 1,
            Self::Hotkeys => 2,
            Self::About => 3,
        }
    }

    fn from_index(index: usize) -> Self {
        PAGES.get(index).copied().unwrap_or(Self::Calibration)
    }

    fn from_ipc(index: i32) -> Self {
        if index < 0 {
            Self::Calibration
        } else {
            Self::from_index(index as usize)
        }
    }
}

/// A child control plus its *logical* placement. `page` is the owning page
/// index, or [`usize::MAX`] for chrome (page list, footer, inline edit rows).
struct Placed {
    hwnd: HWND,
    page: usize,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    /// Width stretches until this inset from the client right edge.
    right: Option<f32>,
    /// Height stretches until this inset from the client bottom edge.
    bottom: Option<f32>,
    /// Instead of `y`, anchor the bottom edge this far above the client bottom.
    anchor_bottom: Option<f32>,
    /// Instead of `x`, anchor the right edge this far left of the client right.
    right_anchor: Option<f32>,
}

impl Placed {
    fn new(hwnd: HWND, page: usize, x: f32, y: f32, w: f32, h: f32) -> Self {
        Self {
            hwnd,
            page,
            x,
            y,
            w,
            h,
            right: None,
            bottom: None,
            anchor_bottom: None,
            right_anchor: None,
        }
    }

    fn stretch_right(mut self, inset: f32) -> Self {
        self.right = Some(inset);
        self
    }

    fn stretch_bottom(mut self, inset: f32) -> Self {
        self.bottom = Some(inset);
        self
    }

    fn anchored_bottom(mut self, inset: f32) -> Self {
        self.anchor_bottom = Some(inset);
        self
    }

    fn anchored_right(mut self, inset: f32) -> Self {
        self.right_anchor = Some(inset);
        self
    }
}

#[derive(Default)]
struct Controls {
    page_list: HWND,
    display_combo: HWND,
    scale_combo: HWND,
    lead_edit: HWND,
    profile_list: HWND,
    profile_hint: HWND,
    profile_rename_edit: HWND,
    profile_rename_ok: HWND,
    profile_rename_cancel: HWND,
    timer_label: HWND,
    timer_edit: HWND,
    hotkey_record: HWND,
    hotkey_pause: HWND,
    hotkey_lap: HWND,
}

struct SettingsState {
    parent: HWND,
    hwnd: HWND,
    state: Arc<SharedAppState>,
    command_tx: Sender<UiCommand>,
    i18n: Arc<I18n>,
    font: HFONT,
    scale: f32,
    page: Page,
    controls: Controls,
    placed: Vec<Placed>,
    profiles: Vec<ProfileMenuItem>,
    profiles_sig: String,
    profile_renaming: bool,
    calibrating: bool,
    /// Control id whose hotkey is being captured ("press a key" mode).
    capture_hotkey: Option<i32>,
    last_timer_label: String,
    /// Set by 新建校准: focus the game window once the dialog is destroyed.
    focus_game_after_close: bool,
    /// Monitor DPI scale (font/controls base of the dialog).
    base_scale: f32,
    /// Overlay scale percentage currently baked into `scale`.
    pct: u16,
    /// Values captured when the window opened, restored by 取消.
    open_display_mode: FrameDisplayMode,
    open_scale_pct: u16,
    open_pause_lead: i32,
}

impl Drop for SettingsState {
    fn drop(&mut self) {
        unsafe {
            if !self.font.is_invalid() {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
    }
}

impl SettingsState {
    fn tr(&self, key: &str) -> String {
        self.i18n.tr(key)
    }

    fn tr_with(&self, key: &str, replacements: &[(&str, String)]) -> String {
        self.i18n.tr_with(key, replacements)
    }

    fn cycles(&self) -> i32 {
        self.state.snapshot().ui.total_frames_in_cycle
    }

    fn selected_profile(&self) -> Option<ProfileMenuItem> {
        let index = unsafe { list_cur(self.controls.profile_list) };
        if index < 0 {
            return None;
        }
        self.profiles.get(index as usize).cloned()
    }
}

fn display_mode_index(mode: FrameDisplayMode) -> i32 {
    match mode {
        FrameDisplayMode::ZeroToNMinusOne => 0,
        FrameDisplayMode::ZeroToN => 1,
        FrameDisplayMode::OneToN => 2,
    }
}

fn index_to_display_mode(index: i32) -> FrameDisplayMode {
    match index {
        1 => FrameDisplayMode::ZeroToN,
        2 => FrameDisplayMode::OneToN,
        _ => FrameDisplayMode::ZeroToNMinusOne,
    }
}

fn profiles_signature(profiles: &[ProfileMenuItem]) -> String {
    let mut sig = String::new();
    for profile in profiles {
        sig.push_str(&profile.filename);
        sig.push('\u{1}');
        sig.push_str(&profile.basename);
        sig.push('\u{1}');
        sig.push_str(&profile.total_frames_str);
        sig.push(if profile.is_active { '1' } else { '0' });
        sig.push('\u{2}');
    }
    sig
}

// ---------------------------------------------------------------------------
// Window entry point
// ---------------------------------------------------------------------------

pub(super) unsafe fn show_menu(parent: HWND) {
    // Re-entrancy guard: the tray callback can arrive on the HUD window even
    // while the settings window is open (Shell_NotifyIcon ignores our mouse
    // capture), which would otherwise stack a second window.
    let previous = match window_state_mut(parent) {
        Some(state) => {
            if state.menu_alive && state.menu_hwnd != 0 {
                let still_open = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::IsWindow(HWND(
                        state.menu_hwnd as *mut _,
                    ))
                    .as_bool()
                };
                if still_open {
                    let _ = SetForegroundWindow(HWND(state.menu_hwnd as *mut _));
                    return;
                }
                state.menu_alive = false;
                state.menu_hwnd = 0;
            }
            let previous = state.menu_hwnd;
            state.menu_hwnd = 0;
            state.menu_open = true;
            previous
        }
        None => return,
    };
    if previous != 0 {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            HWND(previous as *mut _),
            WM_CLOSE,
            WPARAM(0),
            LPARAM(0),
        );
    }

    let Some(parent_state) = window_state(parent) else {
        return;
    };
    let state = Arc::clone(&parent_state.state);
    let command_tx = parent_state.command_tx.clone();
    let i18n = Arc::clone(&parent_state.i18n);
    let base_scale = if parent_state.base_scale > 0.1 {
        parent_state.base_scale
    } else {
        1.0
    };
    let snapshot = state.snapshot();
    let calibrating = snapshot.ui.mode == OverlayMode::Calibrating;
    let pct = snapshot.ui.overlay_scale_pct.max(1);
    // DPI-aware: 620x440 logical units, so the dialog matches the system scaling
    // like any other window. Only the first open is capped to the screen.
    let scale = resolve_layout_scale(base_scale, pct, true);

    let saved = load_menu_bounds();
    let (screen_w, screen_h) = screen_size();
    let max_client_w = (screen_w * MENU_SCREEN_FILL) as i32;
    let max_client_h = (screen_h * MENU_SCREEN_FILL) as i32;
    let client_w = saved
        .as_ref()
        .map(|bounds| sp(bounds.width.max(MENU_MIN_W), scale))
        .unwrap_or_else(|| sp(DEFAULT_CLIENT_W, scale))
        // A stale `menu_window.json` written by an older, bigger default must not
        // re-inflate the window: cap the restored size at the current default.
        .min(sp(DEFAULT_CLIENT_W, scale))
        .min(max_client_w);
    let client_h = saved
        .as_ref()
        .map(|bounds| sp(bounds.height.max(MENU_MIN_H), scale))
        .unwrap_or_else(|| sp(DEFAULT_CLIENT_H, scale))
        .min(sp(DEFAULT_CLIENT_H, scale))
        .min(max_client_h);

    let (window_w, window_h, _, _) = super::frame::window_metrics(
        WINDOW_STYLE(MENU_STYLE),
        WINDOW_EX_STYLE(MENU_EX_STYLE),
        client_w,
        client_h,
    );

    let font = create_ui_font(scale);
    let initial_page = Page::Calibration;
    let settings = Box::new(SettingsState {
        parent: HWND::default(),
        hwnd: HWND::default(),
        state: Arc::clone(&state),
        command_tx: command_tx.clone(),
        i18n: Arc::clone(&i18n),
        font,
        scale,
        page: initial_page,
        focus_game_after_close: false,
        controls: Controls::default(),
        placed: Vec::new(),
        profiles: Vec::new(),
        profiles_sig: String::new(),
        profile_renaming: false,
        calibrating,
        capture_hotkey: None,
        last_timer_label: String::new(),
        base_scale,
        pct,
        open_display_mode: snapshot.ui.display_mode,
        open_scale_pct: snapshot.ui.overlay_scale_pct,
        open_pause_lead: snapshot.ui.pause_lead_frames,
    });
    let state_ptr = Box::into_raw(settings);
    if let Some(settings) = state_ptr.as_mut() {
        settings.parent = parent;
    }

    let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
        let _ = Box::from_raw(state_ptr);
        release_menu_guard(parent);
        return;
    };
    ensure_window_class(instance);

    let class_name = wide("RulerSettingsWindowClass");
    let title = i18n.tr("settings.title");
    let title = wide(&title);
    // Centre the window on first open (never at the cursor) and keep the whole
    // frame on screen, also when the saved position is stale or the monitor
    // layout changed.
    let screen_w = GetSystemMetrics(SM_CXSCREEN).max(1);
    let screen_h = GetSystemMetrics(SM_CYSCREEN).max(1);
    let (mut left, mut top) = match saved.as_ref() {
        Some(bounds) => (bounds.x, bounds.y),
        None => ((screen_w - window_w) / 2, (screen_h - window_h) / 2),
    };
    left = left.clamp(0, (screen_w - window_w).max(0));
    top = top.clamp(0, (screen_h - window_h).max(0));

    let hwnd = match CreateWindowExW(
        WINDOW_EX_STYLE(MENU_EX_STYLE),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WINDOW_STYLE(MENU_STYLE),
        left,
        top,
        window_w,
        window_h,
        HWND::default(),
        HMENU::default(),
        HINSTANCE(instance.0),
        Some(state_ptr.cast()),
    ) {
        Ok(hwnd) => hwnd,
        Err(error) => {
            debug_note(&format!("create_failed: {error:?}"));
            log::error!("settings: CreateWindowExW failed: {error:?}");
            let _ = Box::from_raw(state_ptr);
            release_menu_guard(parent);
            return;
        }
    };

    // Align the outer window with the requested client size (AdjustWindowRect
    // already did it; re-apply in case of DPI rounding differences).
    {
        let mut client = RECT::default();
        let mut window_rect = RECT::default();
        if GetClientRect(hwnd, &mut client).is_ok() && GetWindowRect(hwnd, &mut window_rect).is_ok()
        {
            let dx = client_w - (client.right - client.left);
            let dy = client_h - (client.bottom - client.top);
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                0,
                0,
                (window_rect.right - window_rect.left) + dx,
                (window_rect.bottom - window_rect.top) + dy,
                SWP_NOZORDER | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE,
            );
        }
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    // Force a first full paint (window + all children) so the panes are never
    // shown blank on the very first frame.
    repaint_client(hwnd);
    debug_note(&format!(
        "settings shown: hwnd={:?} client={client_w}x{client_h}",
        hwnd.0
    ));
    if let Some(state) = window_state_mut(parent) {
        state.menu_hwnd = hwnd.0 as isize;
        state.menu_alive = true;
    }
    let _ = SetForegroundWindow(hwnd);
    let _ = SetTimer(hwnd, MENU_TIMER_ID, MENU_TIMER_MS, None);
    tick(hwnd);
    if let Some(state) = settings_state_mut(hwnd) {
        debug_dump_layout(state);
    }

    // Modeless from here on: the window lives on the HUD's message loop and is
    // torn down by `WM_DESTROY`. Running a nested `GetMessage` loop here used to
    // wedge every other window on the thread (a second window that decided to
    // close could not finish while this loop owned the thread).
}

/// Clears the "a settings window is open" guard when creation fails.
unsafe fn release_menu_guard(parent: HWND) {
    if let Some(state) = window_state_mut(parent) {
        state.menu_open = false;
        state.menu_hwnd = 0;
        state.menu_alive = false;
    }
}

unsafe fn create_ui_font(scale: f32) -> HFONT {
    let face = wide("Microsoft YaHei UI");
    let height = -(sp(12.0, scale).max(11));
    CreateFontW(
        height,
        0,
        0,
        0,
        400,
        0,
        0,
        0,
        1, // DEFAULT_CHARSET
        0,
        0,
        5, // CLEARTYPE_QUALITY
        0,
        PCWSTR(face.as_ptr()),
    )
}

unsafe fn ensure_window_class(instance: windows::Win32::Foundation::HMODULE) {
    if CLASS_READY.with(|flag| flag.get()) {
        return;
    }
    let brush = CreateSolidBrush(COLORREF(0x00F0_F0F0));
    CLASS_BRUSH.with(|value| value.set(brush.0 as isize));
    let class_name = wide("RulerSettingsWindowClass");
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(settings_window_proc),
        hInstance: HINSTANCE(instance.0),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
        hbrBackground: brush,
        ..Default::default()
    };
    RegisterClassW(&class);
    CLASS_READY.with(|flag| flag.set(true));
}

// ---------------------------------------------------------------------------
// Child construction
// ---------------------------------------------------------------------------

unsafe fn build_children(state: &mut SettingsState) {
    let hwnd = state.hwnd;
    let font = state.font;
    let i18n = Arc::clone(&state.i18n);
    let tr = |key: &str| i18n.tr(key);
    let calibration = Page::Calibration.index();
    let appearance = Page::Appearance.index();
    let hotkeys_page = Page::Hotkeys.index();
    let about_page = Page::About.index();
    let mut placed: Vec<Placed> = Vec::new();
    let mut controls = Controls::default();

    let list_style = STYLE_CHILD
        | STYLE_VISIBLE
        | STYLE_BORDER
        | STYLE_VSCROLL
        | STYLE_TABSTOP
        | LBS_NOTIFY
        | LBS_NOINTEGRALHEIGHT;
    let button_style = STYLE_CHILD | STYLE_VISIBLE | STYLE_TABSTOP | BS_PUSHBUTTON;
    let static_style = STYLE_CHILD | STYLE_VISIBLE;
    let edit_style = STYLE_CHILD | STYLE_VISIBLE | STYLE_TABSTOP | STYLE_BORDER | ES_AUTOHSCROLL;
    let combo_style = STYLE_CHILD
        | STYLE_VISIBLE
        | STYLE_TABSTOP
        | STYLE_VSCROLL
        | CBS_DROPDOWNLIST;
    let scale_combo_style = STYLE_CHILD
        | STYLE_VISIBLE
        | STYLE_TABSTOP
        | STYLE_VSCROLL
        | CBS_DROPDOWN | ES_AUTOHSCROLL;

    // ---- page list -------------------------------------------------------
    controls.page_list = make_child(hwnd, "LISTBOX", "", list_style, ID_PAGES, font);
    for key in [
        "settings.page.calibration",
        "settings.page.appearance",
        "settings.page.hotkeys",
        "settings.page.about",
    ] {
        list_add(controls.page_list, &tr(key));
    }
    list_set(controls.page_list, state.page.index() as i32);
    placed.push(
        Placed::new(controls.page_list, usize::MAX, M, M, SIDE_W, 0.0)
            .stretch_bottom(M + FOOTER_H + 10.0),
    );

    // ---- footer: 保存 / 确定 / 取消 (right-aligned, 取消 outermost) ---------
    let save = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.btn.save"),
        button_style,
        ID_BTN_SAVE,
        font,
    );
    placed.push(
        Placed::new(save, usize::MAX, 0.0, 0.0, 88.0, ROW_H)
            .anchored_right(M + 2.0 * (88.0 + GAP))
            .anchored_bottom(M),
    );
    let confirm = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.btn.ok"),
        button_style,
        ID_BTN_OK,
        font,
    );
    placed.push(
        Placed::new(confirm, usize::MAX, 0.0, 0.0, 88.0, ROW_H)
            .anchored_right(M + 88.0 + GAP)
            .anchored_bottom(M),
    );
    let cancel = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.btn.cancel"),
        button_style,
        ID_BTN_CANCEL,
        font,
    );
    placed.push(
        Placed::new(cancel, usize::MAX, 0.0, 0.0, 88.0, ROW_H)
            .anchored_right(M)
            .anchored_bottom(M),
    );

    // ===================== page: 通用 =====================================
    placed.push(
        Placed::new(
            make_panel(hwnd, font),
            appearance,
            CX,
            M,
            0.0,
            104.0,
        )
        .stretch_right(M),
    );
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.section.display"),
            static_style,
            0,
            font,
        ),
        appearance,
        CX + PAD,
        M + 6.0,
        200.0,
        18.0,
    ));
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.display.mode"),
            static_style,
            0,
            font,
        ),
        appearance,
        CX + PAD,
        M + 28.0,
        84.0,
        22.0,
    ));
    controls.display_combo = make_child(
        hwnd,
        "COMBOBOX",
        "",
        combo_style,
        ID_COMBO_DISPLAY,
        font,
    );
    for key in [
        "settings.display.mode_0",
        "settings.display.mode_1",
        "settings.display.mode_2",
    ] {
        combo_add(controls.display_combo, &tr(key));
    }
    placed.push(Placed::new(
        controls.display_combo,
        appearance,
        CX + PAD + 88.0,
        M + 24.0,
        220.0,
        200.0,
    ));
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.display.scale"),
            static_style,
            0,
            font,
        ),
        appearance,
        CX + PAD,
        M + 64.0,
        84.0,
        22.0,
    ));
    controls.scale_combo = make_child(
        hwnd,
        "COMBOBOX",
        "",
        scale_combo_style,
        ID_COMBO_SCALE,
        font,
    );
    for label in ["75%", "100%", "125%", "150%", "200%"] {
        combo_add(controls.scale_combo, label);
    }
    placed.push(Placed::new(
        controls.scale_combo,
        appearance,
        CX + PAD + 88.0,
        M + 60.0,
        140.0,
        200.0,
    ));

    placed.push(
        Placed::new(
            make_panel(hwnd, font),
            calibration,
            CX,
            170.0,
            0.0,
            64.0,
        )
        .stretch_right(M),
    );
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.section.timer"),
            static_style,
            0,
            font,
        ),
        calibration,
        CX + PAD,
        174.0,
        200.0,
        18.0,
    ));
    let timer_buttons = [
        (ID_TIMER_MINUS_CYCLE, "settings.timer.minus_cycle"),
        (ID_TIMER_MINUS_ONE, "settings.timer.minus_one"),
        (ID_TIMER_RESET, "settings.timer.reset"),
        (ID_TIMER_PLUS_ONE, "settings.timer.plus_one"),
        (ID_TIMER_PLUS_CYCLE, "settings.timer.plus_cycle"),
    ];
    for (index, (id, key)) in timer_buttons.iter().enumerate() {
        let button = make_child(hwnd, "BUTTON", &tr(key), button_style, *id, font);
        placed.push(Placed::new(
            button,
            calibration,
            CX + PAD + index as f32 * (BTN_W + BTN_GAP),
            190.0,
            BTN_W,
            26.0,
        ));
    }
    let undo = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.timer.undo"),
        button_style,
        ID_TIMER_UNDO,
        font,
    );
    placed.push(Placed::new(
        undo,
        calibration,
        CX + PAD + 5.0 * (BTN_W + BTN_GAP),
        190.0,
        56.0,
        26.0,
    ));

    placed.push(
        Placed::new(
            make_panel(hwnd, font),
            calibration,
            CX,
            326.0,
            0.0,
            58.0,
        )
        .stretch_right(M),
    );
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.section.pause"),
            static_style,
            0,
            font,
        ),
        calibration,
        CX + PAD,
        330.0,
        200.0,
        18.0,
    ));
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.pause.lead"),
            static_style,
            0,
            font,
        ),
        calibration,
        CX + PAD,
        350.0,
        136.0,
        22.0,
    ));
    controls.lead_edit = make_child(hwnd, "EDIT", "", edit_style, ID_LEAD_EDIT, font);
    placed.push(Placed::new(
        controls.lead_edit,
        calibration,
        CX + PAD + 142.0,
        348.0,
        70.0,
        24.0,
    ));
    placed.push(Placed::new(
        make_child(hwnd, "STATIC", &tr("settings.pause.unit"), static_style, 0, font),
        calibration,
        CX + PAD + 218.0,
        350.0,
        40.0,
        22.0,
    ));

    // ===================== page: 校准 =====================================
    placed.push(
        Placed::new(
            make_panel(hwnd, font),
            calibration,
            CX,
            M,
            0.0,
            150.0,
        )
        .stretch_right(M),
    );
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.section.profiles"),
            static_style,
            0,
            font,
        ),
        calibration,
        CX + PAD,
        M + 5.0,
        120.0,
        18.0,
    ));
    controls.profile_list = make_child(hwnd, "LISTBOX", "", list_style, ID_PROFILE_LIST, font);
    placed.push(
        Placed::new(controls.profile_list, calibration, CX + PAD, M + 26.0, 0.0, 62.0)
            .stretch_right(M + PAD),
    );
    controls.profile_hint = make_child(hwnd, "STATIC", "", static_style, ID_PROFILE_HINT, font);
    placed.push(Placed::new(
        controls.profile_hint,
        calibration,
        CX + PAD + 130.0,
        M + 8.0,
        200.0,
        18.0,
    ));
    let profile_buttons = [
        (ID_PROFILE_SELECT, "settings.profile.select", 0.0, 72.0),
        (ID_PROFILE_RENAME, "settings.profile.rename", 78.0, 78.0),
        (ID_PROFILE_DELETE, "settings.profile.delete", 162.0, 72.0),
        (ID_PROFILE_NEW, "settings.profile.new", 240.0, 72.0),
    ];
    for (id, key, offset, width) in profile_buttons {
        let button = make_child(hwnd, "BUTTON", &tr(key), button_style, id, font);
        placed.push(
            Placed::new(button, calibration, CX + PAD + offset, M + 94.0, width, 26.0),
        );
    }
    controls.profile_rename_edit =
        make_child(hwnd, "EDIT", "", edit_style, ID_PROFILE_RENAME_EDIT, font);
    placed.push(
        Placed::new(
            controls.profile_rename_edit,
            usize::MAX,
            CX + PAD,
            M + 124.0,
            200.0,
            24.0,
        ),
    );
    controls.profile_rename_ok = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.rename.ok"),
        button_style,
        ID_PROFILE_RENAME_OK,
        font,
    );
    placed.push(
        Placed::new(
            controls.profile_rename_ok,
            usize::MAX,
            CX + PAD + 208.0,
            M + 124.0,
            64.0,
            24.0,
        ),
    );
    controls.profile_rename_cancel = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.rename.cancel"),
        button_style,
        ID_PROFILE_RENAME_CANCEL,
        font,
    );
    placed.push(
        Placed::new(
            controls.profile_rename_cancel,
            usize::MAX,
            CX + PAD + 278.0,
            M + 124.0,
            64.0,
            24.0,
        ),
    );

    placed.push(
        Placed::new(
            make_panel(hwnd, font),
            calibration,
            CX,
            242.0,
            0.0,
            76.0,
        )
        .stretch_right(M),
    );
    placed.push(
        Placed::new(
            make_child(
                hwnd,
                "STATIC",
                &tr("settings.section.timer_edit"),
                static_style,
                0,
                font,
            ),
            calibration,
            CX + PAD,
            246.0,
            200.0,
            18.0,
        ),
    );
    controls.timer_label = make_child(hwnd, "STATIC", "", static_style, ID_TIMER_LABEL, font);
    placed.push(
        Placed::new(controls.timer_label, calibration, CX + PAD, 266.0, 240.0, 22.0),
    );
    controls.timer_edit = make_child(hwnd, "EDIT", "", edit_style, ID_TIMER_EDIT, font);
    placed.push(
        Placed::new(controls.timer_edit, calibration, CX + PAD, 292.0, 150.0, 24.0),
    );
    let timer_set = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.timer.set"),
        button_style,
        ID_TIMER_SET,
        font,
    );
    placed.push(
        Placed::new(timer_set, calibration, CX + PAD + 158.0, 292.0, 64.0, 24.0),
    );
    let timer_reset = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.timer.reset"),
        button_style,
        ID_TIMER_RESET_TIMER,
        font,
    );
    placed.push(
        Placed::new(timer_reset, calibration, CX + PAD + 228.0, 292.0, 64.0, 24.0),
    );
    placed.push(
        Placed::new(
            make_child(
                hwnd,
                "STATIC",
                &tr("settings.timer.hint"),
                static_style,
                0,
                font,
            ),
            calibration,
            CX + PAD + 298.0,
            293.0,
            120.0,
            22.0,
        ),
    );

    // ===================== page: 外观 =====================================
    // The appearance page only holds the 显示 group above (frame display +
    // scale); colours and opacity were dropped on purpose.


    // ===================== page: 快捷键 ==================================
    placed.push(
        Placed::new(
            make_panel(hwnd, font),
            hotkeys_page,
            CX,
            M,
            0.0,
            150.0,
        )
        .stretch_right(M),
    );
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.section.hotkeys"),
            static_style,
            0,
            font,
        ),
        hotkeys_page,
        CX + PAD,
        M + 6.0,
        200.0,
        18.0,
    ));
    // Order top-down: 计时 / 记录帧标记 / 切换暂停模式. Each row is a button that
    // captures the next key combination (Backspace clears the binding).
    let hotkey_rows = [
        ("settings.hotkey.lap", ID_HOTKEY_LAP),
        ("settings.hotkey.record_frame", ID_HOTKEY_RECORD),
        ("settings.hotkey.pause_mode", ID_HOTKEY_PAUSE),
    ];
    for (index, (key, id)) in hotkey_rows.iter().enumerate() {
        let row_y = M + 34.0 + index as f32 * 34.0;
        placed.push(Placed::new(
            make_child(hwnd, "STATIC", &tr(key), static_style, 0, font),
            hotkeys_page,
            CX + PAD,
            row_y,
            120.0,
            22.0,
        ));
        let field = make_child(hwnd, "BUTTON", "", button_style, *id, font);
        match *id {
            ID_HOTKEY_RECORD => controls.hotkey_record = field,
            ID_HOTKEY_PAUSE => controls.hotkey_pause = field,
            _ => controls.hotkey_lap = field,
        }
        placed.push(Placed::new(
            field,
            hotkeys_page,
            CX + PAD + 128.0,
            row_y,
            180.0,
            24.0,
        ));
    }

    // ===================== page: 关于 =====================================
    placed.push(
        Placed::new(
            make_panel(hwnd, font),
            about_page,
            CX,
            M,
            0.0,
            236.0,
        )
        .stretch_right(M),
    );
    placed.push(Placed::new(
        make_child(
            hwnd,
            "STATIC",
            &tr("settings.section.about"),
            static_style,
            0,
            font,
        ),
        about_page,
        CX + PAD,
        M + 6.0,
        200.0,
        18.0,
    ));
    // Title/version/author statics are created with their final text; the
    // update badge is refreshed by `sync_controls` (it changes when the update
    // check finishes while the window is open).
    placed.push(
        Placed::new(
            make_child(
                hwnd,
                "STATIC",
                &tr("settings.about.title"),
                static_style,
                0,
                font,
            ),
            about_page,
            CX + PAD,
            M + 30.0,
            0.0,
            24.0,
        )
        .stretch_right(M + PAD),
    );
    let version_text = i18n.tr_with(
        "settings.about.version",
        &[("version", VERSION.to_string())],
    );
    placed.push(
        Placed::new(
            make_child(hwnd, "STATIC", &version_text, static_style, 0, font),
            about_page,
            CX + PAD,
            M + 58.0,
            0.0,
            22.0,
        )
        .stretch_right(M + PAD),
    );
    placed.push(
        Placed::new(
            make_child(
                hwnd,
                "STATIC",
                &tr("settings.about.author"),
                static_style,
                0,
                font,
            ),
            about_page,
            CX + PAD,
            M + 86.0,
            0.0,
            22.0,
        )
        .stretch_right(M + PAD),
    );
    let config_text = i18n.tr_with(
        "settings.about.config",
        &[(
            "path",
            ResourceLocator::new().config_path().display().to_string(),
        )],
    );
    placed.push(
        Placed::new(
            make_child(hwnd, "STATIC", &config_text, static_style, 0, font),
            about_page,
            CX + PAD,
            M + 114.0,
            0.0,
            // Two lines: a long config path used to be clipped in a
            // single-line static.
            38.0,
        )
        .stretch_right(M + PAD),
    );
    let about_open = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.about.homepage"),
        button_style,
        ID_ABOUT_OPEN,
        font,
    );
    placed.push(Placed::new(
        about_open,
        about_page,
        CX + PAD,
        M + 170.0,
        148.0,
        26.0,
    ));
    // Bilibili entry on its own row below the project page button.
    let about_bilibili = make_child(
        hwnd,
        "BUTTON",
        &tr("settings.about.bilibili_button"),
        button_style,
        ID_ABOUT_BILIBILI,
        font,
    );
    placed.push(Placed::new(
        about_bilibili,
        about_page,
        CX + PAD,
        M + 204.0,
        148.0,
        26.0,
    ));

    state.controls = controls;
    state.placed = placed;
    state.profile_renaming = false;
    sync_controls(state);
    update_rename_rows(state);
}

// ---------------------------------------------------------------------------
// Layout / page switching
// ---------------------------------------------------------------------------

unsafe fn layout_controls(state: &mut SettingsState, client_w: i32, client_h: i32) {
    let scale = state.scale;
    for item in &state.placed {
        let width = match item.right {
            Some(inset) => (client_w - sp(item.x, scale) - sp(inset, scale)).max(sp(60.0, scale)),
            None => sp(item.w, scale),
        };
        let height = match item.bottom {
            Some(inset) => (client_h - sp(item.y, scale) - sp(inset, scale)).max(sp(40.0, scale)),
            None => sp(item.h, scale),
        };
        let y = match item.anchor_bottom {
            Some(inset) => client_h - sp(inset, scale) - height,
            None => sp(item.y, scale),
        };
        let x = match item.right_anchor {
            Some(inset) => client_w - sp(inset, scale) - width,
            None => sp(item.x, scale),
        };
        let _ = SetWindowPos(
            item.hwnd,
            HWND::default(),
            x,
            y,
            width,
            height,
            SWP_NOZORDER | SWP_NOREDRAW | SWP_NOACTIVATE,
        );
    }
    // Children are siblings that overlap (group boxes sit behind the labels and
    // edits), so moving them silently and then repainting the whole client —
    // children included — is the only reliable way to avoid trails/ghosting.
    repaint_client(state.hwnd);
}

/// One-shot layout dump written to `menu_debug.txt` (next to `config.json`), so
/// a mis-laid-out dialog can be diagnosed from the log instead of screenshots.
unsafe fn debug_dump_layout(state: &SettingsState) {
    let mut out = String::new();
    let mut client = RECT::default();
    let mut window = RECT::default();
    if GetClientRect(state.hwnd, &mut client).is_ok() && GetWindowRect(state.hwnd, &mut window).is_ok()
    {
        let style = GetWindowLongPtrW(state.hwnd, GWL_STYLE) as u32;
        out.push_str(&format!(
            "settings layout: style=0x{style:08X} thick-frame={} scale={:.3} pct={} client={}x{} window={}x{}@{},{}\n",
            style & 0x0004_0000 != 0,
            state.scale,
            state.pct,
            client.right - client.left,
            client.bottom - client.top,
            window.right - window.left,
            window.bottom - window.top,
            window.left,
            window.top
        ));
    }
    out.push_str(&format!("  placed={} children\n", state.placed.len()));
    for item in &state.placed {
        let mut rect = RECT::default();
        let _ = GetWindowRect(item.hwnd, &mut rect);
        out.push_str(&format!(
            "  page={:>2} id={:>5} vis={} rect={},{},{},{}\n",
            item.page,
            GetDlgCtrlID(item.hwnd),
            IsWindowVisible(item.hwnd).as_bool(),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top
        ));
    }
    debug_note(&out);
}

/// Repaint the window and every child after a layout change.
unsafe fn repaint_client(hwnd: HWND) {
    let _ = RedrawWindow(
        hwnd,
        None,
        None,
        RDW_INVALIDATE | RDW_ERASE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}

unsafe fn show_page(state: &mut SettingsState, page: usize) {
    state.page = Page::from_index(page);
    for item in &state.placed {
        let visible = item.page == usize::MAX || item.page == page;
        let _ = ShowWindow(item.hwnd, if visible { SW_SHOW } else { SW_HIDE });
    }
    update_rename_rows(state);
    repaint_client(state.hwnd);
}

/// Re-scale the whole dialog when the overlay scale changes: new font for every
/// child, a proportionally resized window (which triggers `WM_SIZE` →
/// [`layout_controls`]) and the cached metrics updated.
unsafe fn apply_layout_scale(state: &mut SettingsState, pct: u16) {
    // Uncapped on purpose: only the *first* open has to fit on screen, afterwards
    // the dialog scales proportionally with the overlay scale.
    let new_scale = resolve_layout_scale(state.base_scale, pct, false);
    if (new_scale - state.scale).abs() < 0.001 {
        state.pct = pct;
        return;
    }

    let mut client = RECT::default();
    let (mut client_w, mut client_h) = (
        sp(DEFAULT_CLIENT_W, state.scale),
        sp(DEFAULT_CLIENT_H, state.scale),
    );
    if GetClientRect(state.hwnd, &mut client).is_ok() {
        client_w = client.right - client.left;
        client_h = client.bottom - client.top;
    }
    let ratio = if state.scale > 0.01 {
        new_scale / state.scale
    } else {
        1.0
    };

    let font = create_ui_font(new_scale);
    for item in &state.placed {
        SendMessageW(
            item.hwnd,
            MSG_WM_SETFONT,
            WPARAM(font.0 as isize as usize),
            LPARAM(1),
        );
    }
    let previous_font = std::mem::replace(&mut state.font, font);
    if !previous_font.is_invalid() {
        let _ = DeleteObject(HGDIOBJ(previous_font.0));
    }

    state.scale = new_scale;
    state.pct = pct;
    set_scale_text(state.controls.scale_combo, pct);

    let width = ((client_w as f32) * ratio).round().max(1.0) as i32;
    let height = ((client_h as f32) * ratio).round().max(1.0) as i32;
    let (window_w, window_h, _, _) = super::frame::window_metrics(
        WINDOW_STYLE(MENU_STYLE),
        WINDOW_EX_STYLE(MENU_EX_STYLE),
        width,
        height,
    );
    let _ = SetWindowPos(
        state.hwnd,
        HWND::default(),
        0,
        0,
        window_w,
        window_h,
        SWP_NOMOVE | SWP_NOZORDER,
    );
}

unsafe fn update_rename_rows(state: &mut SettingsState) {
    let visible = state.profile_renaming && state.page == Page::Calibration;
    for hwnd in [
        state.controls.profile_rename_edit,
        state.controls.profile_rename_ok,
        state.controls.profile_rename_cancel,
    ] {
        if !hwnd.0.is_null() {
            let _ = ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
    }
    repaint_client(state.hwnd);
}

// ---------------------------------------------------------------------------
// Model ↔ control synchronisation
// ---------------------------------------------------------------------------

unsafe fn sync_controls(state: &mut SettingsState) {
    let snapshot = state.state.snapshot();
    set_display_combo(state, snapshot.ui.display_mode);
    set_text(
        state.controls.lead_edit,
        &snapshot.ui.pause_lead_frames.to_string(),
    );
    set_scale_text(state.controls.scale_combo, snapshot.ui.overlay_scale_pct);
    set_text(
        state.controls.hotkey_record,
        &snapshot.ui.hotkey_record_frame,
    );
    set_text(state.controls.hotkey_pause, &snapshot.ui.hotkey_pause_mode);
    set_text(state.controls.hotkey_lap, &snapshot.ui.hotkey_lap);
    set_text(state.controls.timer_edit, &snapshot.ui.time_str);
    let label = state.tr_with(
        "settings.timer.current",
        &[("time", snapshot.ui.time_str.clone())],
    );
    set_text(state.controls.timer_label, &label);
    state.last_timer_label = label;

    state.calibrating = snapshot.ui.mode == OverlayMode::Calibrating;
    let hint = if state.calibrating {
        state.tr("settings.calibrating")
    } else {
        String::new()
    };
    set_text(state.controls.profile_hint, &hint);

    state.profiles = snapshot.ui.profiles.clone();
    state.profiles_sig = profiles_signature(&state.profiles);
    rebuild_profile_list(state);
}

unsafe fn rebuild_profile_list(state: &mut SettingsState) {
    let list = state.controls.profile_list;
    if list.0.is_null() {
        return;
    }
    let previous = list_cur(list);
    // Keep the user's selection when there is one; on the first build (or after
    // the list was cleared) fall back to the profile that is currently in use, so
    // the active entry is highlighted by default.
    let prev_name = if previous >= 0 {
        state
            .profiles
            .get(previous as usize)
            .map(|profile| profile.filename.clone())
    } else {
        None
    };
    let labels: Vec<String> = state
        .profiles
        .iter()
        .map(|profile| {
            let marker = if profile.is_active { "* " } else { "  " };
            format!(
                "{marker}{}  ({})",
                profile.basename, profile.total_frames_str
            )
        })
        .collect();
    list_clear(list);
    for label in &labels {
        list_add(list, label);
    }
    if labels.is_empty() {
        return;
    }
    let restored = match prev_name {
        Some(name) => state
            .profiles
            .iter()
            .position(|profile| profile.filename == name)
            // The entry is gone (deleted): keep the same index so the *next*
            // profile becomes selected instead of jumping back to the top.
            .unwrap_or_else(|| (previous.max(0) as usize).min(labels.len() - 1)),
        None => state
            .profiles
            .iter()
            .position(|profile| profile.is_active)
            .unwrap_or(0),
    }
    .min(labels.len() - 1);
    list_set(list, restored as i32);
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

unsafe fn on_command(state: &mut SettingsState, id: i32, code: u16) -> bool {
    match id {
        ID_PAGES if code == LBN_SELCHANGE => {
            let index = list_cur(state.controls.page_list);
            show_page(state, Page::from_ipc(index).index());
        }
        // Commit only when the user actually confirms a list item. (For a dropped
        // CBS_DROPDOWNLIST the *selection* already follows the mouse; reacting to
        // that notification and writing it back pinned the highlight.)
        ID_COMBO_DISPLAY if code == CBN_SELENDOK => {
            let mode = index_to_display_mode(combo_cur(state.controls.display_combo));
            apply_display_mode(state, mode);
        }
        ID_COMBO_DISPLAY => {
            // Hover feedback only: nothing to do until the selection is confirmed.
        }
        ID_COMBO_SCALE => {
            // Staged: the scale is only applied by 保存 / 确定 (or Enter).
        }
        ID_TIMER_MINUS_CYCLE => timer_action(state, 0),
        ID_TIMER_MINUS_ONE => timer_action(state, 1),
        ID_TIMER_RESET => timer_action(state, 2),
        ID_TIMER_PLUS_ONE => timer_action(state, 3),
        ID_TIMER_PLUS_CYCLE => timer_action(state, 4),
        ID_TIMER_UNDO => timer_action(state, 5),
        ID_LEAD_EDIT if code == EN_KILLFOCUS => commit_lead(state),
        ID_TIMER_EDIT if code == EN_SETFOCUS => {
            // Adjusting the timer must not be interrupted by the automatic pause;
            // turning pause mode off also prevents it from firing on release.
            state.state.set_auto_pause_suspended(true);
            if state.state.snapshot().ui.pause_mode {
                let _ = state.command_tx.send(UiCommand::TogglePauseMode { enabled: false });
            }
        }
        ID_TIMER_EDIT if code == EN_KILLFOCUS => {
            state.state.set_auto_pause_suspended(false);
            commit_timer_edit(state);
        }
        ID_TIMER_SET => {
            state.state.set_auto_pause_suspended(false);
            commit_timer_edit(state);
        }
        ID_TIMER_RESET_TIMER => {
            disable_pause(state);
            let _ = state.command_tx.send(UiCommand::ResetTimer);
        }
        ID_PROFILE_LIST if code == LBN_DBLCLK => select_profile(state),
        ID_PROFILE_SELECT => select_profile(state),
        ID_PROFILE_RENAME => start_profile_rename(state),
        ID_PROFILE_RENAME_OK => apply_profile_rename(state),
        ID_PROFILE_RENAME_CANCEL => cancel_profile_rename(state),
        ID_PROFILE_DELETE => delete_profile(state),
        ID_PROFILE_NEW => return new_profile(state),
        ID_ABOUT_OPEN => open_about(state),
        ID_ABOUT_BILIBILI => open_bilibili(state),
        ID_HOTKEY_RECORD | ID_HOTKEY_PAUSE | ID_HOTKEY_LAP => start_hotkey_capture(state, id),
        ID_BTN_SAVE => commit_settings(state),
        ID_BTN_OK => {
            commit_settings(state);
            return true;
        }
        ID_BTN_CANCEL | ID_CANCEL_COMMAND => {
            revert_settings(state);
            return true;
        }
        _ => {}
    }
    false
}

/// Clicking a hotkey button puts that row into "press the combination" mode.
fn start_hotkey_capture(state: &mut SettingsState, id: i32) {
    state.capture_hotkey = Some(id);
    let field = hotkey_field(state, id);
    unsafe {
        set_text(field, "…");
        // The settings window itself must own the keyboard for the next keystroke.
        let _ = SetFocus(state.hwnd);
    }
}

fn hotkey_field(state: &SettingsState, id: i32) -> HWND {
    match id {
        ID_HOTKEY_RECORD => state.controls.hotkey_record,
        ID_HOTKEY_PAUSE => state.controls.hotkey_pause,
        _ => state.controls.hotkey_lap,
    }
}

/// Builds "Ctrl+Shift+M" from the modifiers currently held plus the pressed key.
unsafe fn finish_hotkey_capture(state: &mut SettingsState, vk: u16) {
    let Some(id) = state.capture_hotkey.take() else {
        return;
    };
    let field = hotkey_field(state, id);
    if vk == VK_BACK.0 {
        // Backspace unbinds, like every other hotkey editor.
        set_text(field, "");
        return;
    }
    if is_modifier_key(vk) {
        // Wait for the actual key; keep the prompt visible.
        state.capture_hotkey = Some(id);
        return;
    }
    let mut parts: Vec<String> = Vec::new();
    if held(VK_CONTROL.0) {
        parts.push("Ctrl".to_string());
    }
    if held(VK_SHIFT.0) {
        parts.push("Shift".to_string());
    }
    if held(VK_MENU.0) {
        parts.push("Alt".to_string());
    }
    let Some(name) = key_name(vk) else {
        state.capture_hotkey = Some(id);
        return;
    };
    parts.push(name);
    set_text(field, &parts.join("+"));
}

fn held(vk: u16) -> bool {
    unsafe { (GetKeyState(i32::from(vk)) as u16) & 0x8000 != 0 }
}

fn is_modifier_key(vk: u16) -> bool {
    matches!(vk, 0x10 | 0x11 | 0x12 | 0x5B | 0x5C | 0xA0..=0xA5)
}

/// Virtual-key code -> the token understood by the HUD side (`parse_hotkey`).
fn key_name(vk: u16) -> Option<String> {
    let letter = match vk {
        0x30..=0x39 | 0x41..=0x5A => Some((vk as u8) as char),
        _ => None,
    };
    if let Some(ch) = letter {
        return Some(ch.to_string());
    }
    if (0x70..=0x87).contains(&vk) {
        return Some(format!("F{}", vk - 0x70 + 1));
    }
    let name = match vk {
        0x20 => "Space",
        0x09 => "Tab",
        0x0D => "Enter",
        0x1B => "Esc",
        0x24 => "Home",
        0x23 => "End",
        0x25 => "Left",
        0x26 => "Up",
        0x27 => "Right",
        0x28 => "Down",
        _ => return None,
    };
    Some(name.to_string())
}

fn apply_display_mode(state: &mut SettingsState, mode: FrameDisplayMode) {
    // Deliberately no `combo_set` here: this runs from the combo's own
    // notification, and writing the selection back makes a dropped list snap its
    // highlight to that entry. Programmatic changes update the control through
    // `set_display_combo` at the few call sites that need it.
    state.state.update_ui(|ui, _| ui.display_mode = mode);
    let _ = state.command_tx.send(UiCommand::SetDisplayMode(mode));
}

/// Syncs the combo box with a value that was *not* picked from it (open, 取消).
fn set_display_combo(state: &mut SettingsState, mode: FrameDisplayMode) {
    unsafe { combo_set(state.controls.display_combo, display_mode_index(mode)) };
}

fn apply_scale_multiplier(state: &mut SettingsState, multiplier: f32) {
    let pct = (multiplier * 100.0).round().clamp(50.0, 200.0) as u16;
    let multiplier = f32::from(pct) / 100.0;
    state.state.update_ui(|ui, _| ui.overlay_scale_pct = pct);
    let _ = state.command_tx.send(UiCommand::SetOverlayScale(multiplier));
}

/// 保存 / 确定: flush in-flight edits and re-assert the current control values.
unsafe fn commit_settings(state: &mut SettingsState) {
    commit_lead(state);
    // NOTE: no `commit_timer_edit` here. That field holds a *time* ("MM:SS:FF"),
    // and re-applying it on 保存/确定 rewound the running timer to the value the
    // dialog was opened with. It is committed only by its 设定 button / focus loss.
    let mode = index_to_display_mode(combo_cur(state.controls.display_combo));
    apply_display_mode(state, mode);
    commit_scale(state);
    commit_hotkeys(state);
}

/// 快捷键: buttons show the binding; empty means unbound.
unsafe fn commit_hotkeys(state: &mut SettingsState) {
    let record_frame = get_text(state.controls.hotkey_record);
    let pause_mode = get_text(state.controls.hotkey_pause);
    let lap = get_text(state.controls.hotkey_lap);
    let _ = state.command_tx.send(UiCommand::SetHotkeys {
        record_frame: normalize_hotkey_text(record_frame.as_str()),
        pause_mode: normalize_hotkey_text(pause_mode.as_str()),
        lap: normalize_hotkey_text(lap.as_str()),
    });
}

/// Empty / placeholder text means "unbound".
fn normalize_hotkey_text(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        String::new()
    } else {
        trimmed.to_string()
    }
}

/// Reads the (possibly typed) scale field and applies it. Invalid input falls
/// back to the value that is currently in effect, and the text is normalised so
/// the user sees what was accepted.
unsafe fn commit_scale(state: &mut SettingsState) {
    let current = state.state.snapshot().ui.overlay_scale_pct.max(1);
    let text = get_text(state.controls.scale_combo);
    let pct = parse_scale_text(text.as_str())
        .map(|value| value.round().clamp(50.0, 200.0) as u16)
        .unwrap_or(current);
    set_scale_text(state.controls.scale_combo, pct);
    if pct == current {
        return;
    }
    apply_scale_multiplier(state, f32::from(pct) / 100.0);
    // The dialog itself follows the newly committed scale right away.
    apply_layout_scale(state, pct);
}

/// Accepts `110`, `110%`, ` 110 % ` and rejects anything that is not a number.
fn parse_scale_text(text: &str) -> Option<f32> {
    let cleaned = text.trim().trim_end_matches('%').trim();
    let value: f32 = cleaned.parse().ok()?;
    if !value.is_finite() || value <= 0.0 {
        return None;
    }
    Some(value)
}

unsafe fn set_scale_text(hwnd: HWND, pct: u16) {
    set_text(hwnd, &format!("{}%", pct.max(1)));
}

/// 取消: restore the values captured when the window opened (the edits are
/// live, so "discard" means rolling them back) and let the caller close.
fn revert_settings(state: &mut SettingsState) {
    apply_display_mode(state, state.open_display_mode);
    set_display_combo(state, state.open_display_mode);
    let pct = state.open_scale_pct.max(1);
    apply_scale_multiplier(state, f32::from(pct) / 100.0);
    unsafe { set_scale_text(state.controls.scale_combo, pct) };
    if state.open_pause_lead != state.state.snapshot().ui.pause_lead_frames {
        let _ = state
            .command_tx
            .send(UiCommand::SetPauseLead {
                frames: state.open_pause_lead,
            });
    }
}

fn timer_action(state: &mut SettingsState, action: i32) {
    let cycle = state.cycles();
    let command = match action {
        0 => UiCommand::AdjustTimer { frames: -cycle },
        1 => UiCommand::AdjustTimer { frames: -1 },
        2 => UiCommand::ResetTimer,
        3 => UiCommand::AdjustTimer { frames: 1 },
        4 => UiCommand::AdjustTimer { frames: cycle },
        5 => UiCommand::UndoResetTimer,
        _ => return,
    };
    // Adjusting the timer always turns the automatic pause off.
    disable_pause(state);
    let _ = state.command_tx.send(command);
}

/// Turns pause mode off from the dialog side (the worker does the same for every
/// timer command, this makes the UI state change immediately).
fn disable_pause(state: &mut SettingsState) {
    if state.state.snapshot().ui.pause_mode {
        state
            .state
            .update_ui(|ui, _| ui.pause_mode = false);
        let _ = state
            .command_tx
            .send(UiCommand::TogglePauseMode { enabled: false });
    }
}

unsafe fn commit_lead(state: &mut SettingsState) {
    let text = get_text(state.controls.lead_edit);
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    let Ok(frames) = text.parse::<i32>() else {
        return;
    };
    if frames == state.state.snapshot().ui.pause_lead_frames {
        return;
    }
    let _ = state.command_tx.send(UiCommand::SetPauseLead { frames });
}

unsafe fn commit_timer_edit(state: &mut SettingsState) {
    let text = get_text(state.controls.timer_edit);
    let Some(frames) = parse_timer_input_frames(text.as_str()) else {
        return;
    };
    disable_pause(state);
    let _ = state.command_tx.send(UiCommand::SetTimer { frames });
}

fn select_profile(state: &mut SettingsState) {
    if state.calibrating {
        return;
    }
    let Some(profile) = state.selected_profile() else {
        return;
    };
    let _ = state.command_tx.send(UiCommand::UseProfile {
        filename: profile.filename,
    });
}

fn delete_profile(state: &mut SettingsState) {
    if state.calibrating {
        return;
    }
    let Some(profile) = state.selected_profile() else {
        return;
    };
    let _ = state.command_tx.send(UiCommand::DeleteProfile {
        filename: profile.filename,
    });
}

/// 新建校准 hands the thread over to the calibration wizard, so the settings
/// window closes itself (returns `true`).
fn new_profile(state: &mut SettingsState) -> bool {
    let _ = state.command_tx.send(UiCommand::PrepareCalibration);
    // Bring the game window forward for the calibration wizard / capture loop.
    state.focus_game_after_close = true;
    true
}

fn start_profile_rename(state: &mut SettingsState) {
    if state.calibrating {
        return;
    }
    let Some(profile) = state.selected_profile() else {
        return;
    };
    unsafe { set_text(state.controls.profile_rename_edit, &profile.basename) };
    state.profile_renaming = true;
    unsafe { update_rename_rows(state) };
}

fn cancel_profile_rename(state: &mut SettingsState) {
    state.profile_renaming = false;
    unsafe { update_rename_rows(state) };
}

fn apply_profile_rename(state: &mut SettingsState) {
    if state.profile_renaming {
        let name = unsafe { get_text(state.controls.profile_rename_edit) }
            .trim()
            .to_string();
        if let Some(profile) = state.selected_profile() {
            if !name.is_empty() && name != profile.basename {
                let _ = state.command_tx.send(UiCommand::RenameProfile {
                    old: profile.filename,
                    new_base: name,
                });
            }
        }
    }
    state.profile_renaming = false;
    unsafe { update_rename_rows(state) };
}

fn open_about(_state: &mut SettingsState) {
    unsafe { crate::menu::win32::open_about_page() }
}

fn open_bilibili(_state: &mut SettingsState) {
    unsafe { crate::menu::win32::open_bilibili_page() }
}

// ---------------------------------------------------------------------------
// Timer tick: keep the read-only pieces in sync with the worker
// ---------------------------------------------------------------------------

unsafe fn tick(hwnd: HWND) {
    let Some(state) = settings_state_mut(hwnd) else {
        return;
    };
    let snapshot = state.state.snapshot();
    let calibrating = snapshot.ui.mode == OverlayMode::Calibrating;
    if calibrating != state.calibrating {
        state.calibrating = calibrating;
        let hint = if calibrating {
            state.tr("settings.calibrating")
        } else {
            String::new()
        };
        set_text(state.controls.profile_hint, &hint);
        if calibrating && state.profile_renaming {
            state.profile_renaming = false;
            update_rename_rows(state);
        }
    }

    // Follow a scale change made here (or pushed by the worker / a config
    // reload): the whole dialog is re-scaled and resized, not just the HUD.
    if snapshot.ui.overlay_scale_pct != state.pct {
        apply_layout_scale(state, snapshot.ui.overlay_scale_pct);
    }

    // The display-mode combo is user-driven: no periodic re-sync here. The worker
    // echoes its own (possibly one-frame stale) snapshot, and writing the combo
    // back would yank the highlight off the item under the mouse. Programmatic
    // changes go through `apply_display_mode`, which updates the control directly.

    let label = state.tr_with(
        "settings.timer.current",
        &[("time", snapshot.ui.time_str.clone())],
    );
    if label != state.last_timer_label {
        state.last_timer_label = label.clone();
        set_text(state.controls.timer_label, &label);
    }

    let sig = profiles_signature(&snapshot.ui.profiles);
    if sig != state.profiles_sig {
        state.profiles_sig = sig;
        state.profiles = snapshot.ui.profiles.clone();
        rebuild_profile_list(state);
    }
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe extern "system" fn settings_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create_struct = lparam.0 as *const CREATESTRUCTW;
            let state_ptr = (*create_struct).lpCreateParams as *mut SettingsState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
            LRESULT(1)
        }
        WM_CREATE => {
            if let Some(state) = settings_state_mut(hwnd) {
                state.hwnd = hwnd;
                build_children(state);
                let mut client = RECT::default();
                let (mut width, mut height) = (0, 0);
                if GetClientRect(hwnd, &mut client).is_ok() {
                    width = client.right - client.left;
                    height = client.bottom - client.top;
                }
                layout_controls(state, width, height);
                let page = state.page;
                show_page(state, page.index());
            }
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(state) = settings_state_mut(hwnd) {
                let width = (lparam.0 & 0xffff) as i32;
                let height = ((lparam.0 >> 16) & 0xffff) as i32;
                if width > 0 && height > 0 {
                    layout_controls(state, width, height);
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == MENU_TIMER_ID {
                tick(hwnd);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as i32;
            let code = ((wparam.0 >> 16) & 0xffff) as u16;
            let mut close = false;
            let mut focus_game = false;
            let mut parent = HWND::default();
            if let Some(state) = settings_state_mut(hwnd) {
                close = on_command(state, id, code);
                focus_game = state.focus_game_after_close;
                parent = state.parent;
            }
            if close {
                let _ = DestroyWindow(hwnd);
            }
            // Done after the dialog is gone, so it cannot take the foreground back.
            if focus_game && !parent.0.is_null() {
                if let Some(parent_state) = window_state(parent) {
                    super::hud::focus_game_window(&parent_state.state);
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            // Hotkey capture owns the keyboard: the next stroke becomes the binding.
            if let Some(state) = settings_state_mut(hwnd) {
                if state.capture_hotkey.is_some() {
                    finish_hotkey_capture(state, wparam.0 as u16);
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let brush = CLASS_BRUSH.with(|value| value.get());
            LRESULT(brush)
        }
        WM_CLOSE => {
            // The title-bar X behaves like 取消: roll the editables back, then close.
            if let Some(state) = settings_state_mut(hwnd) {
                revert_settings(state);
            }
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
            if !state_ptr.is_null() {
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                let parent = (*state_ptr).parent;
                let scale = (*state_ptr).scale;
                // Never leave the HUD's auto-pause suspended after the dialog goes away.
                (*state_ptr).state.set_auto_pause_suspended(false);
                save_menu_bounds(hwnd, scale);
                let _ = Box::from_raw(state_ptr);
                if let Some(state) = window_state_mut(parent) {
                    state.menu_open = false;
                    if state.menu_hwnd == hwnd.0 as isize {
                        state.menu_hwnd = 0;
                    }
                    state.menu_alive = false;
                }
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn settings_state_mut<'a>(hwnd: HWND) -> Option<&'a mut SettingsState> {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsState;
    if state_ptr.is_null() {
        None
    } else {
        Some(&mut *state_ptr)
    }
}
