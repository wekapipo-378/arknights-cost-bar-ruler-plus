//! The HUD overlay window: a layered, top-most, click-through-where-empty
//! window that software-renders `hud.slint`, owns the message loop and the
//! 16 ms repaint timer, mirrors `SharedAppState` into HUD properties, and
//! handles pointer drag / tray plumbing. Its right-click opens the [`super::menu`].

use std::{
    cell::Cell,
    rc::Rc,
    sync::{
        atomic::{AtomicIsize, Ordering},
        mpsc::Sender,
        Arc,
    },
    time::Instant,
};

use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, SoftwareRenderer},
        PointerEventButton, WindowAdapter, WindowEvent,
    },
    ComponentHandle, LogicalPosition, PhysicalSize,
};

use super::geometry::{
    advance_displayed_progress, compute_reset_phase, initial_geometry,
    outer_area_should_pass_through_at,
};
use super::{
    axes, menu, LOGICAL_H, LOGICAL_PANEL_H, LOGICAL_TOOLBAR_H, LOGICAL_TOOLBAR_RIGHT_PAD,
    LOGICAL_TOOLBAR_TOP_GAP, LOGICAL_TOOLBAR_W, LOGICAL_W, OVERLAY_TIMER_INTERVAL_MS,
};
use crate::overlay::OverlayError;
use crate::{
    commands::UiCommand,
    i18n::I18n,
    icons::IconSet,
    slint_win::{
        create_dib, ensure_platform, logical_pos, present_layered, wide, PreBgra, WindowSlot,
    },
    tray,
    ui::{Hud, HudMode},
    ui_state::{OverlayMode, ResetKind},
    worker::SharedAppState,
};
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Controls::WM_MOUSELEAVE,
            Input::KeyboardAndMouse::{
                keybd_event, RegisterHotKey, ReleaseCapture, SendInput, SetCapture, TrackMouseEvent,
                UnregisterHotKey, HOT_KEY_MODIFIERS, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
                KEYBD_EVENT_FLAGS, KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOD_ALT, MOD_CONTROL,
                MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, TME_LEAVE, TRACKMOUSEEVENT, VIRTUAL_KEY, VK_MENU,
            },
            Shell::NOTIFYICONDATAW,
            WindowsAndMessaging::{
                CreateWindowExW, BringWindowToTop, DefWindowProcW, DestroyWindow, DispatchMessageW,
                GetCursorPos, GetForegroundWindow, GetMessageW, GetWindowLongPtrW, GetWindowRect,
                IsDialogMessageW, IsIconic, LoadCursorW, PostMessageW, PostQuitMessage,
                RegisterClassW, SetForegroundWindow, SetTimer,
                SetWindowLongPtrW, SetWindowPos,
                ShowWindow, TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA,
                HICON, HMENU, HWND_TOPMOST, IDC_ARROW, MSG, SWP_NOMOVE, SWP_NOSIZE, SWP_NOACTIVATE,
                SWP_NOZORDER, SW_SHOW,
                SW_RESTORE,
                WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_DESTROY,
                WM_LBUTTONDOWN, WM_LBUTTONUP,
                WM_MOUSEMOVE, WM_NCCREATE, WM_NCHITTEST, WM_PAINT, WM_RBUTTONUP, WM_TIMER,
                WM_HOTKEY,
                WNDCLASSW, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
            },
        },
    },
};

const OVERLAY_TIMER_ID: usize = 1;
const WM_OVERLAY_WAKE: u32 = WM_APP + 2;
const WM_TRAYICON: u32 = WM_APP + 1;
const HTTRANSPARENT_RESULT: isize = -1;
const PAUSE_MIN_SHOW_DURATION: std::time::Duration = std::time::Duration::from_millis(50);
const TOPMOST_REASSERT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(1000);

/// In-flight reset cover animation. `kind` selects the label text.
#[derive(Clone, Copy)]
struct ResetAnim {
    start: Instant,
    kind: ResetKind,
}

pub(super) struct WindowState {
    hud: Hud,
    window: Rc<MinimalSoftwareWindow>,
    pub(super) state: Arc<SharedAppState>,
    pub(super) command_tx: Sender<UiCommand>,
    pub(super) i18n: Arc<I18n>,
    // software framebuffer backed by a DIB section (no copy on present)
    mem_dc: HDC,
    dib: HBITMAP,
    bits: *mut PreBgra,
    buf_w: usize,
    buf_h: usize,
    pub(super) scale: f32,
    pub(super) base_scale: f32,
    scale_mult: f32,
    displayed_progress: f32,
    last_mode: OverlayMode,
    // Reset cover animation: detects new resets via `reset_pulse` and plays
    // the cover sweep. `None` when idle.
    reset_anim: Option<ResetAnim>,
    last_reset_pulse: u32,
    // factory slot, used to claim windows for menu/dialog popups
    pub(super) window_slot: WindowSlot,
    // tray icon bound to this window (Shell_NotifyIcon)
    nid: NOTIFYICONDATAW,
    tray_icon: Option<HICON>,
    // guards against a re-entrant popup (e.g. tray click while menu is open)
    pub(super) menu_open: bool,
    pub(super) axes_open: bool,
    pub(super) menu_hwnd: isize,
    pub(super) menu_alive: bool,
    pub(super) axes_hwnd: isize,
    // pointer / drag bookkeeping
    mouse_down: bool,
    moved: bool,
    drag_on_bg: Rc<Cell<bool>>,
    drag_origin: POINT,
    window_origin: POINT,
    tracking_leave: bool,
    last_topmost_at: Instant,
    pause_was_enabled: bool,
    pause_shown_at: Instant,
    seen_marks_generation: u32,
    last_global_frame: u64,
    /// Currently registered global hotkeys (same order as the config fields).
    hotkeys: [String; 3],
    hotkeys_generation: u32,
}

impl Drop for WindowState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(HGDIOBJ(self.dib.0));
        }
    }
}

pub(crate) fn run(
    shared_state: Arc<SharedAppState>,
    command_tx: Sender<UiCommand>,
    i18n: Arc<I18n>,
    icons: Arc<IconSet>,
    placement: crate::overlay::OverlayPlacement,
) -> Result<(), OverlayError> {
    // Shared, idempotent platform init: the config wizard may have already
    // installed it (set_platform is once-per-process). NewBuffer repaint and
    // bundled fonts are configured there.
    let slot: WindowSlot = ensure_platform();

    let hud = Hud::new()
        .map_err(|error| OverlayError::new(format!("failed to build HUD component: {error}")))?;
    let window = slot
        .borrow_mut()
        .take()
        .ok_or_else(|| OverlayError::new("platform did not produce a HUD window"))?;

    let scale_mult = placement.scale_or_default();
    let (left, top, width, height, base_scale) = initial_geometry(placement.pos, scale_mult);
    let scale = base_scale * scale_mult;

    // Drive layout at the fixed logical design size via the scale factor.
    window
        .window()
        .try_dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        })
        .map_err(|error| OverlayError::new(format!("scale dispatch failed: {error}")))?;
    window.set_size(PhysicalSize::new(width as u32, height as u32));

    let drag_on_bg = Rc::new(Cell::new(false));
    let hwnd_cell = Rc::new(Cell::new(0isize));
    wire_callbacks(&hud, &shared_state, &command_tx, &drag_on_bg, &hwnd_cell);

    hud.show()
        .map_err(|error| OverlayError::new(format!("failed to show HUD: {error}")))?;

    unsafe {
        let instance = GetModuleHandleW(PCWSTR::null())
            .map_err(|error| OverlayError::new(format!("GetModuleHandleW failed: {error}")))?;
        let class_name = wide("RulerOverlayWindowClass");
        let title = wide("Cost Bar Ruler Plus");

        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW)
                .map_err(|error| OverlayError::new(format!("LoadCursorW failed: {error}")))?,
            ..Default::default()
        };
        let atom = RegisterClassW(&class);
        if atom == 0 {
            return Err(OverlayError::new("RegisterClassW failed"));
        }

        let (mem_dc, dib, bits) = create_dib(width, height)
            .ok_or_else(|| OverlayError::new("failed to create framebuffer DIB"))?;

        let state_box = Box::new(WindowState {
            hud,
            window: window.clone(),
            state: Arc::clone(&shared_state),
            command_tx,
            i18n,
            mem_dc,
            dib,
            bits,
            buf_w: width as usize,
            buf_h: height as usize,
            scale,
            base_scale,
            scale_mult,
            displayed_progress: 0.0,
            last_mode: OverlayMode::Booting,
            reset_anim: None,
            last_reset_pulse: 0,
            window_slot: Rc::clone(&slot),
            nid: NOTIFYICONDATAW::default(),
            tray_icon: None,
            menu_open: false,
            axes_open: false,
            menu_hwnd: 0,
            menu_alive: false,
            axes_hwnd: 0,
            mouse_down: false,
            moved: false,
            drag_on_bg,
            drag_origin: POINT::default(),
            window_origin: POINT::default(),
            tracking_leave: false,
            last_topmost_at: Instant::now(),
            pause_was_enabled: false,
            pause_shown_at: Instant::now(),
            seen_marks_generation: 0,
            last_global_frame: 0,
            hotkeys: [String::new(), String::new(), String::new()],
            hotkeys_generation: u32::MAX,
        });
        let state_ptr = Box::into_raw(state_box);

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_LAYERED.0 | WS_EX_TOOLWINDOW.0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WINDOW_STYLE(WS_POPUP.0 | WS_VISIBLE.0),
            left,
            top,
            width,
            height,
            HWND::default(),
            HMENU::default(),
            HINSTANCE(instance.0),
            Some(state_ptr.cast()),
        )
        .map_err(|error| OverlayError::new(format!("CreateWindowExW failed: {error}")))?;

        if hwnd.0.is_null() {
            let _ = Box::from_raw(state_ptr);
            return Err(OverlayError::new("CreateWindowExW returned null"));
        }
        hwnd_cell.set(hwnd.0 as isize);

        let overlay_hwnd = Arc::new(AtomicIsize::new(hwnd.0 as isize));
        let overlay_waker_hwnd = Arc::clone(&overlay_hwnd);
        shared_state.set_overlay_waker(Some(Arc::new(move || {
            let raw_hwnd = overlay_waker_hwnd.load(Ordering::Relaxed);
            if raw_hwnd != 0 {
                let _ = PostMessageW(
                    HWND(raw_hwnd as *mut _),
                    WM_OVERLAY_WAKE,
                    WPARAM(0),
                    LPARAM(0),
                );
            }
        })));

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetTimer(hwnd, OVERLAY_TIMER_ID, OVERLAY_TIMER_INTERVAL_MS, None);
        let _ = PostMessageW(hwnd, WM_OVERLAY_WAKE, WPARAM(0), LPARAM(0));

        // Tray icon now lives on the UI thread alongside the HUD, so its
        // right-click can open the same Slint menu.
        if let Some((nid, tray_icon)) = tray::win32::install(hwnd, &icons, WM_TRAYICON) {
            if let Some(state) = window_state_mut(hwnd) {
                state.nid = nid;
                state.tray_icon = tray_icon;
            }
        }

        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, HWND::default(), 0, 0).0;
            if result == -1 {
                return Err(OverlayError::new("GetMessageW failed"));
            }
            if result == 0 {
                break;
            }
            // The settings window is modeless (it lives on this loop), so its
            // dialog keyboard handling — Tab between controls, Enter on the
            // focused button, Esc as 取消 — has to run here.
            let settings_hwnd = window_state(hwnd)
                .map(|state| state.menu_hwnd)
                .unwrap_or(0);
            if settings_hwnd != 0
                && IsDialogMessageW(HWND(settings_hwnd as *mut _), &message).as_bool()
            {
                continue;
            }
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }

        overlay_hwnd.store(0, Ordering::Relaxed);
        shared_state.set_overlay_waker(None);
        Ok(())
    }
}

fn wire_callbacks(
    hud: &Hud,
    state: &Arc<SharedAppState>,
    command_tx: &Sender<UiCommand>,
    drag_on_bg: &Rc<Cell<bool>>,
    hwnd_cell: &Rc<Cell<isize>>,
) {
    hud.on_start_clicked({
        let tx = command_tx.clone();
        move || {
            let _ = tx.send(UiCommand::StartCalibration);
        }
    });
    hud.on_close_clicked({
        let tx = command_tx.clone();
        let state = Arc::clone(state);
        move || {
            state.request_cancel_calibration();
            let _ = tx.send(UiCommand::Exit);
        }
    });
    hud.on_settings_clicked({
        let hwnd_cell = Rc::clone(hwnd_cell);
        move || {
            let raw = hwnd_cell.get();
            if raw != 0 {
                unsafe { menu::show_menu(HWND(raw as *mut _)) };
            }
        }
    });
    hud.on_lap_clicked({
        let tx = command_tx.clone();
        let state = Arc::clone(state);
        move || {
            focus_game_window(&state);
            let _ = tx.send(UiCommand::ToggleLapTimer);
        }
    });
    hud.on_record_frame_clicked({
        let tx = command_tx.clone();
        let state = Arc::clone(state);
        move || {
            let frame = state.snapshot().ui.global_frame;
            focus_game_window(&state);
            let _ = tx.send(UiCommand::RecordFrameMark { frame });
        }
    });
    hud.on_pause_mode_clicked({
        let tx = command_tx.clone();
        let state = Arc::clone(state);
        move || {
            let ui = state.snapshot().ui;
            let enabled = if ui.pause_active {
                true
            } else {
                !ui.pause_mode
            };
            focus_game_window(&state);
            let _ = tx.send(UiCommand::TogglePauseMode { enabled });
        }
    });
    hud.on_open_update({
        let state = Arc::clone(state);
        move || {
            let url = state
                .snapshot()
                .ui
                .update_notice
                .map(|notice| notice.html_url);
            if let Some(url) = url {
                unsafe { crate::menu::win32::open_url(&url) };
            }
        }
    });
    hud.on_bg_pressed({
        let drag_on_bg = Rc::clone(drag_on_bg);
        move || drag_on_bg.set(true)
    });
    hud.on_bg_released({
        let drag_on_bg = Rc::clone(drag_on_bg);
        move || drag_on_bg.set(false)
    });
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create_struct = lparam.0 as *const CREATESTRUCTW;
            let state_ptr = (*create_struct).lpCreateParams as *mut WindowState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
            LRESULT(1)
        }
        WM_PAINT => {
            // Painting is driven by the timer via UpdateLayeredWindow; just validate.
            let mut paint = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
            let hdc = windows::Win32::Graphics::Gdi::BeginPaint(hwnd, &mut paint);
            let _ = hdc;
            let _ = windows::Win32::Graphics::Gdi::EndPaint(hwnd, &paint);
            LRESULT(0)
        }
        WM_NCHITTEST => {
            if let Some(state) = window_state_mut(hwnd) {
                if outer_area_should_pass_through(hwnd, state) {
                    return LRESULT(HTTRANSPARENT_RESULT);
                }
            }
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_TIMER => {
            if should_exit(hwnd) {
                let _ = DestroyWindow(hwnd);
            } else {
                tick(hwnd);
            }
            LRESULT(0)
        }
        WM_OVERLAY_WAKE => {
            if should_exit(hwnd) {
                let _ = DestroyWindow(hwnd);
            } else {
                pause_check(hwnd);
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            handle_left_down(hwnd, lparam);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            handle_mouse_move(hwnd, lparam);
            LRESULT(0)
        }
        WM_MOUSELEAVE => {
            if let Some(state) = window_state_mut(hwnd) {
                state.tracking_leave = false;
                let _ = state
                    .window
                    .window()
                    .try_dispatch_event(WindowEvent::PointerExited);
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            handle_left_up(hwnd);
            LRESULT(0)
        }
        WM_HOTKEY => {
            let id = wparam.0 as i32;
            if let Some(state) = window_state(hwnd) {
                let snapshot = state.state.snapshot();
                let command = match id {
                    1 => Some(UiCommand::RecordFrameMark {
                        frame: snapshot.ui.global_frame,
                    }),
                    2 => Some(UiCommand::TogglePauseMode {
                        enabled: !snapshot.ui.pause_mode,
                    }),
                    3 => Some(UiCommand::ToggleLapTimer),
                    _ => None,
                };
                if let Some(command) = command {
                    // Frame/lap actions must land in the game, like the buttons do.
                    if id == 1 || id == 3 {
                        focus_game_window(&state.state);
                    }
                    let _ = state.command_tx.send(command);
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let axes_open = window_state(hwnd)
                .map(|state| {
                    state.axes_hwnd != 0
                        && unsafe {
                            windows::Win32::UI::WindowsAndMessaging::IsWindow(HWND(
                                state.axes_hwnd as *mut _,
                            ))
                            .as_bool()
                        }
                })
                .unwrap_or(false);
            let running = window_state(hwnd)
                .map(|state| state.state.snapshot().ui.mode == OverlayMode::Running)
                .unwrap_or(false);
            // Without an active calibration profile there is nothing the axis
            // window could show, so a right click always goes to the settings
            // window (which opens on the calibration page) — even when the axis
            // window happens to be open.
            let no_active_profile = window_state(hwnd)
                .map(|state| state.state.snapshot().ui.active_profile.is_none())
                .unwrap_or(true);
            if no_active_profile {
                menu::show_menu(hwnd);
            } else if axes_open {
                if let Some(state) = window_state_mut(hwnd) {
                    let target = state.axes_hwnd;
                    state.axes_hwnd = 0;
                    state.axes_open = false;
                    // Close through the window message only; the window's own
                    // tick loop reacts to `WM_CLOSE_AXES` and exits cleanly.
                    if target != 0 {
                        let _ = PostMessageW(
                            HWND(target as *mut _),
                            axes::WM_CLOSE_AXES,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
            } else if running {
                axes::show_axes(hwnd);
            } else {
                menu::show_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_TRAYICON => {
            // Shell forwards the mouse action in the low word of lparam.
            if lparam.0 as u32 == WM_RBUTTONUP {
                let command = unsafe { tray::win32::show_context_menu(hwnd) };
                if command != 0 {
                    let mut exit = false;
                    let mut open_menu = false;
                    let mut open_axes = false;
                    match command {
                        1 => open_menu = true,
                        2 => open_axes = true,
                        3 => exit = true,
                        _ => {}
                    }
                    if let Some(state) = window_state(hwnd) {
                        if exit {
                            state.state.request_cancel_calibration();
                            let _ = state.command_tx.send(UiCommand::Exit);
                        }
                    }
                    if open_menu {
                        menu::show_menu(hwnd);
                    } else if open_axes {
                        axes::show_axes(hwnd);
                    }
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
            if !state_ptr.is_null() {
                (*state_ptr).state.set_overlay_waker(None);
                tray::win32::remove(&(*state_ptr).nid, (*state_ptr).tray_icon);
                let _ = Box::from_raw(state_ptr);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn pause_check(hwnd: HWND) {
    let Some(state) = window_state_mut(hwnd) else {
        return;
    };
    let snapshot = state.state.snapshot();
    if !snapshot.ui.pause_mode && !snapshot.ui.pause_active {
        return;
    }
    advance_pause_mode(state, &snapshot.ui, snapshot.api.battle_state.as_deref());
}

unsafe fn tick(hwnd: HWND) {
    slint::platform::update_timers_and_animations();
    let Some(state) = window_state_mut(hwnd) else {
        return;
    };
    let snapshot = state.state.snapshot();

    if !state.menu_open
        && !state.axes_open
        && state.last_topmost_at.elapsed() >= TOPMOST_REASSERT_INTERVAL
    {
        state.last_topmost_at = Instant::now();
        let _ = SetWindowPos(
            hwnd,
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }

    let want_mult = f32::from(snapshot.ui.overlay_scale_pct) / 100.0;
    if (want_mult - state.scale_mult).abs() > 0.001 {
        apply_scale(hwnd, state, want_mult);
    }

    sync_properties(state, &snapshot.ui);
    apply_hotkeys(hwnd, state, &snapshot.ui);
    advance_reset_anim(state, &snapshot.ui);
    advance_pause_mode(state, &snapshot.ui, snapshot.api.battle_state.as_deref());

    let w = state.buf_w;
    let h = state.buf_h;
    let bits = state.bits;
    let drawn = state.window.draw_if_needed(|renderer: &SoftwareRenderer| {
        let buffer = unsafe { std::slice::from_raw_parts_mut(bits, w * h) };
        renderer.render(buffer, w);
    });
    if drawn {
        present_layered(hwnd, state.mem_dc, w as i32, h as i32);
        state
            .state
            .record_overlay_paint(snapshot.worker_timing.sample_index, Instant::now());
    }
}

unsafe fn apply_scale(hwnd: HWND, state: &mut WindowState, new_mult: f32) {
    let effective = state.base_scale * new_mult;
    let width = (LOGICAL_W * effective).round() as i32;
    let height = (LOGICAL_H * effective).round() as i32;

    // Rebuild the framebuffer DIB at the new physical size.
    let _ = DeleteDC(state.mem_dc);
    let _ = DeleteObject(HGDIOBJ(state.dib.0));
    if let Some((mem_dc, dib, bits)) = create_dib(width, height) {
        state.mem_dc = mem_dc;
        state.dib = dib;
        state.bits = bits;
        state.buf_w = width as usize;
        state.buf_h = height as usize;
    }

    state.scale = effective;
    state.scale_mult = new_mult;
    let _ = state
        .window
        .window()
        .try_dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: effective,
        });
    state
        .window
        .set_size(PhysicalSize::new(width as u32, height as u32));
    let _ = SetWindowPos(
        hwnd,
        HWND::default(),
        0,
        0,
        width,
        height,
        SWP_NOMOVE | SWP_NOZORDER,
    );
}

fn sync_properties(state: &mut WindowState, ui: &crate::ui_state::UiSnapshot) {
    let displayed_progress = displayed_calibration_progress(state, ui);
    let progress_text = displayed_progress.round().clamp(0.0, 100.0) as u8;

    {
        let hud = &state.hud;

        hud.set_mode(map_mode(&ui.mode));
        hud.set_time_str(ui.time_str.as_str().into());
        hud.set_frame_str(ui.display_frame.as_str().into());
        hud.set_undo_reset_enabled(ui.can_undo_reset);

        let negative = ui.display_total.ends_with('*');
        let total_clean = ui.display_total.trim_end_matches('*');
        hud.set_total_str(total_clean.into());
        hud.set_cost_negative(negative);

        let lap = ui
            .lap_frames
            .map(|frames| frames.to_string())
            .unwrap_or_default();
        hud.set_lap_str(lap.into());

        hud.set_progress(displayed_progress);
        hud.set_progress_str(format!("{progress_text}%").into());

        hud.set_cursor_blocked(ui.cursor_blocked);
        hud.set_cursor_warning_text(state.i18n.tr("overlay.cursor.blocked").into());
        hud.set_update_available(ui.update_notice.is_some());
        hud.set_update_text(state.i18n.tr("update.badge.short").into());
        hud.set_mark_str(mark_text(ui).into());
        hud.set_next_mark_str(next_mark_text(ui).into());
        hud.set_axis_str(
            ui.active_axis_name
                .as_deref()
                .unwrap_or_default()
                .into(),
        );
        hud.set_pause_mode(ui.pause_mode);
        hud.set_pause_lead_str(
            if ui.pause_mode {
                // A positive lead pauses *before* the mark (shown as "-N"), a
                // negative one after it ("+N"); 0 shows plainly as "0".
                let frames = ui.pause_lead_frames as i64;
                let text = match frames {
                    0 => "0".to_string(),
                    n if n > 0 => format!("-{n}"),
                    n => format!("+{}", -n),
                };
                text
            } else {
                String::new()
            }
            .into(),
        );

        let message = match ui.mode {
            OverlayMode::Idle => state.i18n.tr("overlay.msg.idle"),
            OverlayMode::PreCalibration => state.i18n.tr("overlay.msg.pre_cal"),
            OverlayMode::Error | OverlayMode::Booting => ui.message.clone(),
            _ => String::new(),
        };
        hud.set_message(message.into());
    }

    state.last_mode = ui.mode.clone();
}

/// Amber line at the bottom-left of the HUD: the current frame plus the note of
/// the mark that sits exactly on this frame.
///
/// The note only needs the *current* frame to match a mark; pause mode is
/// irrelevant, so it also shows while the timer is running (it used to be gated
/// by the mark list only being reloaded while pause mode was armed).
fn mark_text(ui: &crate::ui_state::UiSnapshot) -> String {
    let frame = ui.global_frame;
    let note = ui
        .pause_marks
        .iter()
        .find(|mark| u64::from(mark.frame) == frame)
        .map(|mark| mark.text.trim())
        .filter(|text| !text.is_empty());
    match note {
        Some(text) => format!("{frame}  {text}"),
        None => frame.to_string(),
    }
}

/// Amber label at the bottom-right: frame number of the next marked frame.
fn next_mark_text(ui: &crate::ui_state::UiSnapshot) -> String {
    let frame = ui.global_frame;
    ui.pause_marks
        .iter()
        .map(|mark| u64::from(mark.frame))
        .filter(|mark_frame| *mark_frame > frame)
        .min()
        .map(|next| next.to_string())
        .unwrap_or_default()
}

/// "Ctrl+Shift+M" -> hotkey modifiers + virtual-key code.
fn parse_hotkey(spec: &str) -> Option<(HOT_KEY_MODIFIERS, u32)> {
    let text = spec.trim();
    if text.is_empty() {
        return None;
    }
    let mut modifiers = 0u32;
    let mut vk = None;
    for part in text.split('+') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => modifiers |= MOD_CONTROL.0,
            "shift" => modifiers |= MOD_SHIFT.0,
            "alt" => modifiers |= MOD_ALT.0,
            "win" | "meta" => modifiers |= MOD_WIN.0,
            other => vk = vk_code(other),
        }
    }
    vk.map(|vk| (HOT_KEY_MODIFIERS(modifiers | MOD_NOREPEAT.0), vk))
}

fn vk_code(name: &str) -> Option<u32> {
    let upper = name.to_ascii_uppercase();
    let bytes = upper.as_bytes();
    if bytes.len() == 1 {
        let ch = bytes[0];
        if ch.is_ascii_uppercase() || ch.is_ascii_digit() {
            return Some(ch as u32);
        }
    }
    if let Some(rest) = upper.strip_prefix('F') {
        if let Ok(number) = rest.parse::<u32>() {
            if (1..=24).contains(&number) {
                return Some(0x70 + number - 1);
            }
        }
    }
    match upper.as_str() {
        "SPACE" => Some(0x20),
        "TAB" => Some(0x09),
        "ENTER" | "RETURN" => Some(0x0D),
        "ESC" | "ESCAPE" => Some(0x1B),
        "HOME" => Some(0x24),
        "END" => Some(0x23),
        "LEFT" => Some(0x25),
        "UP" => Some(0x26),
        "RIGHT" => Some(0x27),
        "DOWN" => Some(0x28),
        _ => None,
    }
}

/// (Re-)register the three global hotkeys whenever the bindings change.
unsafe fn apply_hotkeys(hwnd: HWND, state: &mut WindowState, ui: &crate::ui_state::UiSnapshot) {
    if state.hotkeys_generation == ui.appearance_generation {
        return;
    }
    state.hotkeys_generation = ui.appearance_generation;
    let wanted = [
        ui.hotkey_record_frame.clone(),
        ui.hotkey_pause_mode.clone(),
        ui.hotkey_lap.clone(),
    ];
    if wanted == state.hotkeys {
        return;
    }
    for id in 1..=3 {
        let _ = UnregisterHotKey(hwnd, id);
    }
    state.hotkeys = wanted.clone();
    for (index, spec) in wanted.iter().enumerate() {
        let Some((modifiers, vk)) = parse_hotkey(spec) else {
            continue;
        };
        let id = index as i32 + 1;
        match RegisterHotKey(hwnd, id, modifiers, vk) {
            Ok(()) => log::info!("hotkey {id} registered: {spec}"),
            Err(error) => log::warn!("hotkey '{spec}' could not be registered: {error:?}"),
        }
    }
}

/// Detect new resets via `reset_pulse` and advance the cover animation.
/// A fresh pulse mid-animation restarts it (overriding the current kind),
/// so a manual reset right after an auto one re-arms cleanly.
fn advance_reset_anim(state: &mut WindowState, ui: &crate::ui_state::UiSnapshot) {
    if ui.reset_pulse != state.last_reset_pulse {
        state.last_reset_pulse = ui.reset_pulse;
        // Always (re)start from the top so the sweep reads as a new event.
        state.reset_anim = Some(ResetAnim {
            start: Instant::now(),
            kind: ui.reset_kind,
        });
    }

    let Some(anim) = state.reset_anim else {
        state.hud.set_reset_active(false);
        return;
    };

    let elapsed = anim.start.elapsed().as_millis();
    let phase = compute_reset_phase(elapsed);
    let hud = &state.hud;
    if phase.done {
        state.reset_anim = None;
        hud.set_reset_active(false);
        return;
    }

    let label_key = match anim.kind {
        ResetKind::Manual => "overlay.reset.manual",
        ResetKind::Auto => "overlay.reset.auto",
    };
    hud.set_reset_active(true);
    hud.set_reset_cover(phase.cover);
    hud.set_reset_top(phase.top);
    hud.set_reset_text(state.i18n.tr(label_key).into());
    hud.set_reset_text_opacity(phase.text_opacity);
}

fn advance_pause_mode(
    state: &mut WindowState,
    ui: &crate::ui_state::UiSnapshot,
    battle_state: Option<&str>,
) {
    let frame = ui.global_frame;
    let previous = state.last_global_frame;
    state.last_global_frame = frame;

    // While the user is adjusting the timer in the settings dialog, no automatic
    // pause may fire (it would steal focus and send keys mid-edit).
    if state.state.auto_pause_suspended() {
        return;
    }

    if !ui.pause_mode {
        state.pause_was_enabled = false;
        if ui.pause_active {
            clear_pause_display(state);
        }
        return;
    }

    if !state.pause_was_enabled {
        state.pause_was_enabled = true;
        state.last_global_frame = frame;
        return;
    }

    if ui.pause_marks_generation != state.seen_marks_generation {
        state.seen_marks_generation = ui.pause_marks_generation;
        state.last_global_frame = frame;
        return;
    }

    if ui.pause_active {
        release_pause_display(state, battle_state);
        return;
    }

    let crossed = ui
        .pause_marks
        .iter()
        .filter(|mark| {
            // Positive lead pauses before the mark, negative after it.
            let target = i64::from(mark.frame) - i64::from(ui.pause_lead_frames);
            let target = target.max(0) as u64;
            target > previous && target <= frame
        })
        .min_by_key(|mark| mark.frame);

    let Some(mark) = crossed else {
        return;
    };

    let text = if mark.text.is_empty() {
        format!("[{}]", mark.frame)
    } else {
        mark.text.clone()
    };
    state.pause_shown_at = Instant::now();
    log::info!(
        "pause mode triggered at frame {frame} (mark {}), previous frame {previous}",
        mark.frame
    );
    send_pause_key(state.state.target_hwnd());
    state.state.update_ui(|ui, _| {
        ui.pause_active = true;
        ui.pause_text = text;
    });
}

fn release_pause_display(state: &mut WindowState, battle_state: Option<&str>) {
    if state.pause_shown_at.elapsed() < PAUSE_MIN_SHOW_DURATION {
        return;
    }

    if let Some(battle_state) = battle_state {
        if is_battle_paused(battle_state) {
            return;
        }
    }

    clear_pause_display(state);
}

fn is_battle_paused(battle_state: &str) -> bool {
    matches!(battle_state, "0.2x_paused" | "1x_paused" | "2x_paused")
}

fn clear_pause_display(state: &mut WindowState) {
    state.state.update_ui(|ui, _| {
        ui.pause_active = false;
        ui.pause_text.clear();
    });
}

pub(super) fn focus_game_window(state: &SharedAppState) {
    let target = state.target_hwnd();
    if target == 0 {
        return;
    }
    unsafe {
        let hwnd = HWND(target as *mut _);
        if GetForegroundWindow() == hwnd {
            return;
        }
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        keybd_event(VK_MENU.0 as u8, 0, KEYBD_EVENT_FLAGS(0), 0);
        let _ = BringWindowToTop(hwnd);
        let _ = SetForegroundWindow(hwnd);
        keybd_event(VK_MENU.0 as u8, 0, KEYEVENTF_KEYUP, 0);
    }
}

fn send_pause_key(target: isize) {
    const ESC_SCAN: u16 = 0x01;
    if target == 0 {
        log::warn!("pause key not sent: capture target window is unknown");
        return;
    }

    let key_event = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: ESC_SCAN,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let inputs_down = [key_event(KEYEVENTF_SCANCODE)];
    let inputs_up = [key_event(KEYBD_EVENT_FLAGS(
        KEYEVENTF_SCANCODE.0 | KEYEVENTF_KEYUP.0,
    ))];
    let input_size = std::mem::size_of::<INPUT>() as i32;

    unsafe {
        let hwnd = HWND(target as *mut _);
        if GetForegroundWindow() != hwnd {
            let _ = SetForegroundWindow(hwnd);
            std::thread::sleep(std::time::Duration::from_millis(80));
            let foreground = GetForegroundWindow();
            if foreground != hwnd {
                log::warn!(
                    "pause key may have gone to the wrong window: target={target} foreground={}",
                    foreground.0 as isize
                );
            }
        }
        let sent_down = SendInput(&inputs_down, input_size);
        std::thread::sleep(std::time::Duration::from_millis(40));
        let sent_up = SendInput(&inputs_up, input_size);
        if sent_down != 1 || sent_up != 1 {
            log::warn!("failed to send pause key: down={sent_down} up={sent_up}");
        }
    }
}

fn displayed_calibration_progress(
    state: &mut WindowState,
    ui: &crate::ui_state::UiSnapshot,
) -> f32 {
    if ui.mode != OverlayMode::Calibrating {
        state.displayed_progress = 0.0;
        return 0.0;
    }

    let target = ui.progress_percent.clamp(0.0, 100.0);
    if state.last_mode != OverlayMode::Calibrating || target <= 0.0 {
        state.displayed_progress = 0.0;
    }
    if target >= 99.9 {
        state.displayed_progress = 100.0;
        return state.displayed_progress;
    }

    state.displayed_progress = advance_displayed_progress(state.displayed_progress, target);
    state.displayed_progress
}

fn map_mode(mode: &OverlayMode) -> HudMode {
    match mode {
        OverlayMode::Booting => HudMode::Booting,
        OverlayMode::Idle => HudMode::Idle,
        OverlayMode::PreCalibration => HudMode::Precal,
        OverlayMode::Calibrating => HudMode::Calibrating,
        OverlayMode::Running => HudMode::Running,
        OverlayMode::Error => HudMode::Error,
    }
}

unsafe fn handle_left_down(hwnd: HWND, lparam: LPARAM) {
    let Some(state) = window_state_mut(hwnd) else {
        return;
    };
    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let mut window_rect = RECT::default();
    let _ = GetWindowRect(hwnd, &mut window_rect);

    state.mouse_down = true;
    state.moved = false;
    state.drag_on_bg.set(false);
    state.drag_origin = cursor;
    state.window_origin = POINT {
        x: window_rect.left,
        y: window_rect.top,
    };
    let _ = SetCapture(hwnd);

    let position = logical_pos(lparam, state.scale);
    let _ = state
        .window
        .window()
        .try_dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
}

unsafe fn handle_mouse_move(hwnd: HWND, lparam: LPARAM) {
    let Some(state) = window_state_mut(hwnd) else {
        return;
    };

    if !state.tracking_leave {
        let mut tme = TRACKMOUSEEVENT {
            cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
            dwFlags: TME_LEAVE,
            hwndTrack: hwnd,
            dwHoverTime: 0,
        };
        if TrackMouseEvent(&mut tme).is_ok() {
            state.tracking_leave = true;
        }
    }

    if state.mouse_down {
        let mut cursor = POINT::default();
        let _ = GetCursorPos(&mut cursor);
        let dx = cursor.x - state.drag_origin.x;
        let dy = cursor.y - state.drag_origin.y;
        if dx.abs() > 2 || dy.abs() > 2 {
            state.moved = true;
        }
        if state.moved && state.drag_on_bg.get() {
            let _ = SetWindowPos(
                hwnd,
                HWND::default(),
                state.window_origin.x + dx,
                state.window_origin.y + dy,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER,
            );
            return;
        }
    }

    let position = logical_pos(lparam, state.scale);
    let _ = state
        .window
        .window()
        .try_dispatch_event(WindowEvent::PointerMoved { position });
}

unsafe fn handle_left_up(hwnd: HWND) {
    let Some(state) = window_state_mut(hwnd) else {
        return;
    };
    let moved = state.moved;
    let dragged_bg = state.drag_on_bg.get();
    state.mouse_down = false;
    state.moved = false;
    state.drag_on_bg.set(false);
    let _ = ReleaseCapture();

    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);
    let mut client = cursor;
    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut client);
    let position =
        LogicalPosition::new(client.x as f32 / state.scale, client.y as f32 / state.scale);

    if moved && dragged_bg {
        // Drag finished — cancel Slint's press grab so it is not read as a click.
        let _ = state
            .window
            .window()
            .try_dispatch_event(WindowEvent::PointerExited);
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_ok() {
            let _ = state.command_tx.send(UiCommand::SaveOverlayPlacement {
                x: rect.left,
                y: rect.top,
            });
        }
    } else {
        let in_toolbar = position.y >= LOGICAL_PANEL_H
            && position.y < LOGICAL_PANEL_H + LOGICAL_TOOLBAR_TOP_GAP + LOGICAL_TOOLBAR_H
            && position.x >= LOGICAL_W - LOGICAL_TOOLBAR_W - LOGICAL_TOOLBAR_RIGHT_PAD
            && position.x < LOGICAL_W - LOGICAL_TOOLBAR_RIGHT_PAD;
        // Focus the game window for every click on the overlay, including the one
        // that starts a calibration: the capture loop needs the game in front.
        if !in_toolbar {
            focus_game_window(&state.state);
        }
        let _ = state
            .window
            .window()
            .try_dispatch_event(WindowEvent::PointerReleased {
                position,
                button: PointerEventButton::Left,
            });
    }
}

unsafe fn should_exit(hwnd: HWND) -> bool {
    window_state(hwnd)
        .map(|state| state.state.snapshot().ui.should_exit)
        .unwrap_or(false)
}

pub(super) unsafe fn window_state<'a>(hwnd: HWND) -> Option<&'a WindowState> {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    if state_ptr.is_null() {
        None
    } else {
        Some(&*state_ptr)
    }
}

pub(super) unsafe fn window_state_mut<'a>(hwnd: HWND) -> Option<&'a mut WindowState> {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut WindowState;
    if state_ptr.is_null() {
        None
    } else {
        Some(&mut *state_ptr)
    }
}

unsafe fn outer_area_should_pass_through(hwnd: HWND, state: &mut WindowState) -> bool {
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_err() {
        return false;
    }
    let mut window_rect = RECT::default();
    if GetWindowRect(hwnd, &mut window_rect).is_err() {
        return false;
    }

    let x = cursor.x - window_rect.left;
    let y = cursor.y - window_rect.top;
    let ui = state.state.snapshot().ui;
    let running = matches!(ui.mode, OverlayMode::Running);
    outer_area_should_pass_through_at(x, y, state.scale, running, ui.update_notice.is_some())
}
