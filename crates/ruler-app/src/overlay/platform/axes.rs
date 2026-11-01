//! Axes window: the frame-mark document list, opened from the HUD's
//! right-click. Second layered window software-rendering `axes.slint` in its
//! own nested message loop, resizable through `WS_THICKFRAME`.

use std::{
    cell::Cell,
    path::{Path, PathBuf},
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use slint::{
    platform::{
        software_renderer::{MinimalSoftwareWindow, SoftwareRenderer},
        Key, PointerEventButton, WindowAdapter, WindowEvent,
    },
    ComponentHandle, ModelRc, PhysicalSize, VecModel,
};

use super::hud::{window_state, window_state_mut};
use super::OVERLAY_TIMER_INTERVAL_MS;
use crate::{
    commands::UiCommand,
    resources::ResourceLocator,
    slint_win::{
        client_xy, create_dib, decode_wm_char, dispatch_key, logical_pos, present_layered, wide,
        PreBgra,
    },
    ui::{AxesPanel, AxisRow, FrameRow},
    ui_state::PauseMark,
    worker::SharedAppState,
};

use super::hud::focus_game_window;
use ruler_core::RulerConfig;
use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM},
        Graphics::Gdi::{DeleteDC, DeleteObject, HBITMAP, HDC, HGDIOBJ},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                GetKeyState, ReleaseCapture, SetCapture, VK_BACK, VK_C, VK_CONTROL, VK_DELETE,
                VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RETURN, VK_RIGHT, VK_SHIFT, VK_V,
            },
            WindowsAndMessaging::{
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetCursorPos,
                GetMessageW, GetWindowLongPtrW, GetWindowRect, LoadCursorW, PostQuitMessage,
                RegisterClassW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
                ShowWindow, TranslateMessage,                CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HMENU, HWND_NOTOPMOST,
                HWND_TOPMOST, IDC_ARROW, IDC_SIZENESW, IDC_SIZENS, IDC_SIZENWSE, IDC_SIZEWE, MSG,
                SW_SHOW, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
                WINDOW_EX_STYLE,
                WINDOW_STYLE, WM_ACTIVATE, WM_APP, WM_CHAR, WM_DESTROY, WM_KEYDOWN, WM_KEYUP,
                WM_LBUTTONDOWN,
                WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE,
                WM_RBUTTONDOWN, WM_SETCURSOR, WM_SIZE,
                WM_TIMER, WNDCLASSW,
                WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_VISIBLE,
            },
        },
    },
};

pub(super) const WM_CLOSE_AXES: u32 = WM_APP + 9;
const AXES_TIMER_ID: usize = 3;
const AXES_LOGICAL_W: f32 = 300.0;
const AXES_LOGICAL_H: f32 = 420.0;
pub(super) const AXES_NEW_BASE: &str = "轴";
const AXES_COPY_SUFFIX: &str = "_副本";
const AXES_ACTIVATION_GRACE_MS: u128 = 400;
const AXES_MIN_W: i32 = 220;
const AXES_MIN_H: i32 = 200;
const AXES_DRAG_THRESHOLD: i32 = 3;
const AXES_EDGE_MARGIN: f32 = 14.0;

#[derive(serde::Deserialize, serde::Serialize)]
struct AxesBounds {
    x: i32,
    y: i32,
    width: f32,
    height: f32,
}

struct AxesData {
    dir: PathBuf,
    active: Option<String>,
    rows: Vec<(String, usize)>,
    file: Option<String>,
    marks: Vec<PauseMark>,
    follow: bool,
    scroll: f32,
    current: u64,
    /// Last time the open axis file was re-read, so marks recorded elsewhere
    /// (HUD button / hotkey) show up in this window without reopening it.
    last_mark_reload: Instant,
    /// Marks count at the last reload, to detect external appends cheaply.
    last_mark_count: usize,
}

struct AxesState {
    panel: Rc<AxesPanel>,
    window: Rc<MinimalSoftwareWindow>,
    data: Rc<std::cell::RefCell<AxesData>>,
    state: Arc<SharedAppState>,
    command_tx: std::sync::mpsc::Sender<UiCommand>,
    mem_dc: HDC,
    dib: HBITMAP,
    bits: *mut PreBgra,
    buf_w: usize,
    buf_h: usize,
    scale: f32,
    /// Monitor DPI scale; `scale` is this times the overlay scale percentage.
    base_scale: f32,
    closing: Rc<Cell<bool>>,
    #[allow(dead_code)]
    reopen: Rc<Cell<bool>>,
    opened_at: Instant,
    pending_high: Cell<u16>,
    scale_pct: u16,
    mouse_down: bool,
    moved: bool,
    drag_blocked: bool,
    drag_origin: POINT,
    window_origin: POINT,
    resize_zone: u8,
    pending_resize: u8,
    resize_origin: POINT,
    resize_rect: RECT,
}

impl Drop for AxesState {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteDC(self.mem_dc);
            let _ = DeleteObject(HGDIOBJ(self.dib.0));
        }
    }
}

pub(super) unsafe fn show_axes(parent: HWND) {
    match window_state_mut(parent) {
        Some(state) => {
            if state.axes_hwnd != 0 {
                let still_open = unsafe {
                    windows::Win32::UI::WindowsAndMessaging::IsWindow(HWND(
                        state.axes_hwnd as *mut _,
                    ))
                    .as_bool()
                };
                if still_open {
                    return;
                }
            }
            state.axes_hwnd = 0;
            state.axes_open = true;
        }
        None => return,
    }

    let Some(parent_state) = window_state(parent) else {
        return;
    };
    let command_tx = parent_state.command_tx.clone();
    let slot = Rc::clone(&parent_state.window_slot);
    let snapshot = parent_state.state.snapshot();
    let scale = parent_state.base_scale * (f32::from(snapshot.ui.overlay_scale_pct.max(1)) / 100.0);

    let resources = ResourceLocator::new();
    let dir = resources.axes_dir();
    let bounds_dir = dir.clone();
    let _ = std::fs::create_dir_all(&dir);
    let active = RulerConfig::load_from_path(resources.config_path())
        .ok()
        .and_then(|config| config.active_axis);

    let Ok(panel) = AxesPanel::new() else {
        return;
    };
    let Some(window) = slot.borrow_mut().take() else {
        return;
    };
    let panel = Rc::new(panel);

    let data = Rc::new(std::cell::RefCell::new(AxesData {
        dir,
        active,
        rows: Vec::new(),
        file: None,
        marks: Vec::new(),
        follow: true,
        scroll: 0.0,
        current: 0,
        last_mark_reload: Instant::now(),
        last_mark_count: 0,
    }));
    {
        let borrowed = data.borrow();
        refresh_axes(&borrowed, &panel);
    }
    {
        let mut borrowed = data.borrow_mut();
        let active = borrowed.active.clone();
        if let Some(name) = active {
            if axis_path(&borrowed, &name).exists() {
                open_axis(&panel, &mut borrowed, &name);
            }
        }
    }
    let closing = Rc::new(Cell::new(false));
    let reopen = Rc::new(Cell::new(false));
    let _ = &reopen;
    let hwnd_cell = Rc::new(Cell::new(0isize));
    wire_axes_callbacks(
        &panel,
        &data,
        &command_tx,
        &closing,
        &hwnd_cell,
        &parent_state.state,
    );

    let bounds = load_axes_bounds(&bounds_dir);
    let default_width = (AXES_LOGICAL_W * scale).round() as i32;
    let default_height = (AXES_LOGICAL_H * scale).round() as i32;
    let width = bounds
        .as_ref()
        .map(|bounds| (bounds.width * scale).round() as i32)
        .filter(|width| *width >= AXES_MIN_W)
        .unwrap_or(default_width);
    let height = bounds
        .as_ref()
        .map(|bounds| (bounds.height * scale).round() as i32)
        .filter(|height| *height >= AXES_MIN_H)
        .unwrap_or(default_height);
    let left = bounds.as_ref().map(|bounds| bounds.x).unwrap_or(60);
    let top = bounds.as_ref().map(|bounds| bounds.y).unwrap_or(60);
    let _ = window
        .window()
        .try_dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: scale,
        });
    window.set_size(PhysicalSize::new(width as u32, height as u32));
    let _ = panel.show();
    panel.set_pinned(true);
    let _ = window
        .window()
        .try_dispatch_event(WindowEvent::WindowActiveChanged(true));

    let Some((mem_dc, dib, bits)) = create_dib(width, height) else {
        return;
    };

    let state_box = Box::new(AxesState {
        panel: Rc::clone(&panel),
        window,
        data,
        state: Arc::clone(&parent_state.state),
        command_tx: command_tx.clone(),
        mem_dc,
        dib,
        bits,
        buf_w: width as usize,
        buf_h: height as usize,
        scale,
        base_scale: parent_state.base_scale,
        closing: Rc::clone(&closing),
        reopen: Rc::clone(&reopen),
        opened_at: Instant::now(),
        pending_high: Cell::new(0),
        scale_pct: parent_state.state.snapshot().ui.overlay_scale_pct,
        mouse_down: false,
        moved: false,
        drag_blocked: false,
        drag_origin: POINT::default(),
        window_origin: POINT::default(),
        resize_zone: 0,
        pending_resize: 0,
        resize_origin: POINT::default(),
        resize_rect: RECT::default(),
    });
    let state_ptr = Box::into_raw(state_box);

    let Ok(instance) = GetModuleHandleW(PCWSTR::null()) else {
        let _ = Box::from_raw(state_ptr);
        return;
    };
    let class_name = wide("RulerAxesWindowClass");
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(axes_window_proc),
        hInstance: HINSTANCE(instance.0),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hCursor: LoadCursorW(HINSTANCE::default(), IDC_ARROW).unwrap_or_default(),
        ..Default::default()
    };
    RegisterClassW(&class);

    let title = wide("Cost Bar Ruler Plus - Axes");
    let Ok(hwnd) = CreateWindowExW(
        WINDOW_EX_STYLE(WS_EX_TOPMOST.0 | WS_EX_LAYERED.0 | WS_EX_TOOLWINDOW.0),
        PCWSTR(class_name.as_ptr()),
        PCWSTR(title.as_ptr()),
        WINDOW_STYLE(WS_POPUP.0 | WS_VISIBLE.0),
        left,
        top,
        width,
        height,
        parent,
        HMENU::default(),
        HINSTANCE(instance.0),
        Some(state_ptr.cast()),
    ) else {
        let _ = Box::from_raw(state_ptr);
        return;
    };
    if hwnd.0.is_null() {
        let _ = Box::from_raw(state_ptr);
        return;
    }

    hwnd_cell.set(hwnd.0 as isize);
    if let Some(parent_state) = window_state_mut(parent) {
        parent_state.axes_hwnd = hwnd.0 as isize;
    }

    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
    let _ = SetTimer(hwnd, AXES_TIMER_ID, OVERLAY_TIMER_INTERVAL_MS, None);
    axes_tick(hwnd);

    let mut message = MSG::default();
    while !closing.get() {
        let result = GetMessageW(&mut message, HWND::default(), 0, 0).0;
        if result == 0 {
            PostQuitMessage(0);
            break;
        }
        if result == -1 {
            break;
        }
        // This loop owns the thread while the axis window is open, so dialog
        // keys for a modeless settings window have to be routed from here too.
        let settings_hwnd = window_state(parent)
            .map(|state| state.menu_hwnd)
            .unwrap_or(0);
        if settings_hwnd != 0
            && windows::Win32::UI::WindowsAndMessaging::IsDialogMessageW(
                HWND(settings_hwnd as *mut _),
                &message,
            )
            .as_bool()
        {
            continue;
        }
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }

    if let Some(state) = axes_state(hwnd) {
        if let Ok(mut data) = state.data.try_borrow_mut() {
            flush_edits(&state.panel, &mut data, &state.command_tx);
        }
    }

    // Bounds are stored in logical units, so divide by the *current* scale
    // (it changes when the overlay scale changes while the window is open).
    let scale = axes_state(hwnd).map(|state| state.scale).unwrap_or(scale);
    save_axes_bounds(hwnd, &bounds_dir, scale);
    if let Ok(cursor) = LoadCursorW(HINSTANCE::default(), IDC_ARROW) {
        let _ = windows::Win32::UI::WindowsAndMessaging::SetCursor(cursor);
    }
    let _ = ShowWindow(hwnd, SW_HIDE);
    let _ = DestroyWindow(hwnd);

    if let Some(state) = window_state_mut(parent) {
        state.axes_open = false;
        if state.axes_hwnd == hwnd.0 as isize {
            state.axes_hwnd = 0;
        }
    }
}

fn wire_axes_callbacks(
    panel: &Rc<AxesPanel>,
    data: &Rc<std::cell::RefCell<AxesData>>,
    command_tx: &std::sync::mpsc::Sender<UiCommand>,
    closing: &Rc<Cell<bool>>,
    hwnd_cell: &Rc<Cell<isize>>,
    state: &Arc<SharedAppState>,
) {
    panel.on_close_window({
        let closing = Rc::clone(closing);
        move || closing.set(true)
    });

    panel.on_toggle_pin({
        let panel = Rc::downgrade(panel);
        let hwnd_cell = Rc::clone(hwnd_cell);
        move || {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let pinned = !panel.get_pinned();
            panel.set_pinned(pinned);
            let raw = hwnd_cell.get();
            if raw == 0 {
                return;
            }
            unsafe {
                let insert_after = if pinned {
                    HWND_TOPMOST
                } else {
                    HWND_NOTOPMOST
                };
                let _ = SetWindowPos(
                    HWND(raw as *mut _),
                    insert_after,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE,
                );
            }
        }
    });

    panel.on_new_axis({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        move || {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            let name = unique_axes_name(&data.dir, AXES_NEW_BASE);
            let path = data.dir.join(format!("{name}.txt"));
            if std::fs::write(&path, "").is_err() {
                return;
            }
            data.active = Some(name.clone());
            refresh_axes(&data, &panel);
            open_axis(&panel, &mut data, &name);
        }
    });

    panel.on_select_axis({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        let command_tx = command_tx.clone();
        move |index| {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            let Some(name) = data
                .rows
                .get(index as usize)
                .map(|(name, _)| name.clone())
            else {
                return;
            };
            data.active = Some(name.clone());
            let _ = command_tx.send(UiCommand::SelectAxis {
                name: name.clone(),
            });
            refresh_axes(&data, &panel);
            open_axis(&panel, &mut data, &name);
        }
    });

    panel.on_commit_rename({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        let command_tx = command_tx.clone();
        move |index, new_name| {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            apply_rename(&panel, &mut data, &command_tx, index, new_name.as_str());
        }
    });

    // Clicking the axis title renames the axis that is currently open.
    panel.on_commit_heading_rename({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        let command_tx = command_tx.clone();
        move |new_name| {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            let Some(old) = data.file.clone() else {
                return;
            };
            let new = sanitize_axes_name(new_name.as_str());
            if new.is_empty() || new == old {
                panel.set_heading(old.into());
                return;
            }
            let from = data.dir.join(format!("{old}.txt"));
            let to = data.dir.join(format!("{new}.txt"));
            if to.exists() || std::fs::rename(&from, &to).is_err() {
                panel.set_heading(old.into());
                return;
            }
            if data.active.as_deref() == Some(old.as_str()) {
                data.active = Some(new.clone());
                let _ = command_tx.send(UiCommand::SelectAxis { name: new.clone() });
            }
            open_axis(&panel, &mut data, &new);
            refresh_axes(&data, &panel);
        }
    });
    panel.on_cancel_heading_rename({
        let panel = Rc::downgrade(panel);
        move || {
            if let Some(panel) = panel.upgrade() {
                panel.set_heading_editing(false);
            }
        }
    });

    panel.on_copy_axis({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        move |index| {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let data = data.borrow_mut();
            let Some(name) = data
                .rows
                .get(index as usize)
                .map(|(name, _)| name.clone())
            else {
                return;
            };
            let base = copy_root_name(&name);
            let copy_name = next_copy_name(&data.dir, &base);
            let from = data.dir.join(format!("{name}.txt"));
            let to = data.dir.join(format!("{copy_name}.txt"));
            if std::fs::copy(&from, &to).is_err() {
                return;
            }
            refresh_axes(&data, &panel);
        }
    });

    panel.on_back({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        move || {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            close_axis(&panel, &mut data);
            refresh_axes(&data, &panel);
        }
    });

    panel.on_toggle_follow({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        move || {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            data.follow = !data.follow;
            panel.set_follow(data.follow);
        }
    });

    panel.on_new_frame({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        let state = Arc::clone(state);
        move || {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            let Some(file) = data.file.clone() else {
                return;
            };
            let mut frame = u32::try_from(state.snapshot().ui.global_frame).unwrap_or(1);
            let mut attempts = 0;
            while data.marks.iter().any(|mark| mark.frame == frame) && attempts < 100_000 {
                frame = frame.saturating_add(1);
                attempts += 1;
            }
            if data.marks.iter().any(|mark| mark.frame == frame) {
                return;
            }
            data.marks.push(PauseMark {
                frame,
                text: String::new(),
            });
            if save_marks_file(&axis_path(&data, &file), &data.marks).is_err() {
                data.marks.retain(|mark| mark.frame != frame);
                return;
            }
            data.marks = load_marks_file(&axis_path(&data, &file));
            data.current = u64::from(frame);
            data.follow = true;
            panel.set_follow(true);
            refresh_frames(&panel, &data, u64::from(frame));
            refresh_axes(&data, &panel);
        }
    });

    panel.on_save_frame_edit({
        let data = Rc::clone(data);
        move |index, frame_text, note_text| {
            let mut data = data.borrow_mut();
            let Some(file) = data.file.clone() else {
                return;
            };
            let Ok(frame) = frame_text.trim().parse::<u32>() else {
                return;
            };
            let note = note_text.trim().to_string();
            if data
                .marks
                .iter()
                .enumerate()
                .any(|(position, mark)| position != index as usize && mark.frame == frame)
            {
                return;
            }
            let Some(mark) = data.marks.get_mut(index as usize) else {
                return;
            };
            if mark.frame == frame && mark.text == note {
                return;
            }
            mark.frame = frame;
            mark.text = note;
            let path = axis_path(&data, &file);
            let _ = save_marks_file(&path, &data.marks);
        }
    });

    panel.on_finish_frame_edit({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        move || {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            let Some(file) = data.file.clone() else {
                return;
            };
            let path = axis_path(&data, &file);
            data.marks = load_marks_file(&path);
            let current = data.current;
            refresh_frames(&panel, &data, current);
            refresh_axes(&data, &panel);
        }
    });

    panel.on_cancel_frame_edit({
        let panel = Rc::downgrade(panel);
        move || {
            if let Some(panel) = panel.upgrade() {
                panel.set_frame_editing(-1);
            }
        }
    });

    panel.on_delete_frame({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        move |index| {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            let Some(file) = data.file.clone() else {
                return;
            };
            if index < 0 || index as usize >= data.marks.len() {
                return;
            }
            let removed = data.marks.remove(index as usize);
            if save_marks_file(&axis_path(&data, &file), &data.marks).is_err() {
                data.marks.insert(index as usize, removed);
                return;
            }
            panel.set_frame_editing(-1);
            refresh_frames(&panel, &data, 0);
            refresh_axes(&data, &panel);
        }
    });

    panel.on_delete_axis({
        let panel = Rc::downgrade(panel);
        let data = Rc::clone(data);
        let command_tx = command_tx.clone();
        move |index| {
            let Some(panel) = panel.upgrade() else {
                return;
            };
            let mut data = data.borrow_mut();
            let Some(name) = data
                .rows
                .get(index as usize)
                .map(|(name, _)| name.clone())
            else {
                return;
            };
            let path = data.dir.join(format!("{name}.txt"));
            if std::fs::remove_file(&path).is_err() {
                return;
            }
            if data.active.as_deref() == Some(name.as_str()) {
                data.active = None;
                let _ = command_tx.send(UiCommand::SelectAxis {
                    name: String::new(),
                });
            }
            refresh_axes(&data, &panel);
        }
    });
}

fn bounds_path(dir: &Path) -> PathBuf {
    dir.join("window.json")
}

fn load_axes_bounds(dir: &Path) -> Option<AxesBounds> {
    let text = std::fs::read_to_string(bounds_path(dir)).ok()?;
    serde_json::from_str(&text).ok()
}

unsafe fn save_axes_bounds(hwnd: HWND, dir: &Path, scale: f32) {
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return;
    }
    let width = rect.right - rect.left;
    let height = rect.bottom - rect.top;
    if width < AXES_MIN_W || height < AXES_MIN_H {
        return;
    }
    let _ = std::fs::create_dir_all(dir);
    let divisor = if scale > 0.1 { scale } else { 1.0 };
    let bounds = AxesBounds {
        x: rect.left,
        y: rect.top,
        width: width as f32 / divisor,
        height: height as f32 / divisor,
    };
    if let Ok(text) = serde_json::to_string_pretty(&bounds) {
        let _ = std::fs::write(bounds_path(dir), text);
    }
}

pub(super) fn resize_zone(scale: f32, width: i32, height: i32, x: i32, y: i32) -> u8 {
    resize_zone_with(AXES_EDGE_MARGIN, scale, width, height, x, y)
}

pub(super) fn resize_zone_with(
    margin: f32,
    scale: f32,
    width: i32,
    height: i32,
    x: i32,
    y: i32,
) -> u8 {
    let _ = (margin, scale);
    let edge = 8;
    let corner = 3;

    let corner_x = if x <= corner {
        1
    } else if x >= width - corner - 1 {
        4
    } else {
        0
    };
    let corner_y = if y <= corner {
        2
    } else if y >= height - corner - 1 {
        8
    } else {
        0
    };
    if corner_x != 0 && corner_y != 0 {
        return corner_x | corner_y;
    }

    let edge_x = if x <= edge {
        1
    } else if x >= width - edge - 1 {
        4
    } else {
        0
    };
    let edge_y = if y <= edge {
        2
    } else if y >= height - edge - 1 {
        8
    } else {
        0
    };
    edge_x | edge_y
}

fn parse_mark_line(line: &str) -> Option<PauseMark> {
    let line = line.trim();
    let rest = line.strip_prefix('[')?;
    let close = rest.find(']')?;
    let frame = rest[..close].trim().parse::<u32>().ok()?;
    Some(PauseMark {
        frame,
        text: rest[close + 1..].trim().to_string(),
    })
}

fn load_marks_file(path: &Path) -> Vec<PauseMark> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut marks: Vec<PauseMark> = content.lines().filter_map(parse_mark_line).collect();
    marks.sort_by_key(|mark| mark.frame);
    marks
}

fn save_marks_file(path: &Path, marks: &[PauseMark]) -> std::io::Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut others: Vec<String> = Vec::new();
    for line in existing.lines() {
        let trimmed = line.trim();
        if !trimmed.is_empty() && parse_mark_line(trimmed).is_none() {
            others.push(trimmed.to_string());
        }
    }

    let mut sorted: Vec<PauseMark> = marks.to_vec();
    sorted.sort_by_key(|mark| mark.frame);

    let mut output = String::new();
    for mark in &sorted {
        output.push_str(&format!("[{}]{}\n", mark.frame, mark.text));
    }
    for line in others {
        output.push_str(&line);
        output.push('\n');
    }
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, output)
}

fn axis_path(data: &AxesData, name: &str) -> PathBuf {
    data.dir.join(format!("{name}.txt"))
}

fn open_axis(panel: &AxesPanel, data: &mut AxesData, name: &str) {
    let path = axis_path(data, name);
    data.marks = load_marks_file(&path);
    data.last_mark_count = data.marks.len();
    data.last_mark_reload = Instant::now();
    data.file = Some(name.to_string());
    data.scroll = 0.0;
    data.follow = true;
    panel.set_scroll_y(0.0);
    panel.set_follow(true);
    panel.set_heading(name.into());
    panel.set_detail(true);
    panel.set_frame_editing(-1);
    refresh_frames(panel, data, 0);
}

fn close_axis(panel: &AxesPanel, data: &mut AxesData) {
    data.file = None;
    data.marks.clear();
    data.scroll = 0.0;
    panel.set_scroll_y(0.0);
    panel.set_detail(false);
    panel.set_frame_editing(-1);
    panel.set_heading("轴列表".into());
    panel.set_frames(ModelRc::new(VecModel::from(Vec::<FrameRow>::new())));
}

fn refresh_frames(panel: &AxesPanel, data: &AxesData, current_frame: u64) {
    let rows: Vec<FrameRow> = data
        .marks
        .iter()
        .map(|mark| FrameRow {
            frame: mark.frame.to_string().into(),
            note: mark.text.as_str().into(),
            current: u64::from(mark.frame) == current_frame,
        })
        .collect();
    panel.set_frames(ModelRc::new(VecModel::from(rows)));
}

fn follow_scroll(data: &AxesData, list_height: f32, current_frame: u64) -> f32 {
    const ROW_H: f32 = 30.0;
    if list_height <= ROW_H {
        return data.scroll;
    }
    let index = match data
        .marks
        .iter()
        .rposition(|mark| u64::from(mark.frame) <= current_frame)
    {
        Some(index) => index,
        None => 0,
    };
    let row_top = index as f32 * ROW_H;
    let row_bottom = row_top + ROW_H;
    if row_top >= data.scroll && row_bottom <= data.scroll + list_height {
        return data.scroll;
    }
    let centered = row_top - (list_height - ROW_H) / 2.0;
    centered.max(0.0)
}

fn apply_rename(
    panel: &AxesPanel,
    data: &mut AxesData,
    command_tx: &std::sync::mpsc::Sender<UiCommand>,
    index: i32,
    new_name: &str,
) {
    let Some(old) = data.rows.get(index as usize).map(|(name, _)| name.clone()) else {
        return;
    };
    let new = sanitize_axes_name(new_name);
    if new.is_empty() || new == old {
        return;
    }
    let from = data.dir.join(format!("{old}.txt"));
    let to = data.dir.join(format!("{new}.txt"));
    if to.exists() || std::fs::rename(&from, &to).is_err() {
        return;
    }
    if data.active.as_deref() == Some(old.as_str()) {
        data.active = Some(new.clone());
        let _ = command_tx.send(UiCommand::SelectAxis { name: new });
    }
    refresh_axes(data, panel);
}

fn flush_edits(
    panel: &AxesPanel,
    data: &mut AxesData,
    command_tx: &std::sync::mpsc::Sender<UiCommand>,
) {
    // Renaming the axis title is committed by clicking anywhere else, like a note.
    if panel.get_heading_editing() {
        let name = panel.get_heading_edit_text().to_string();
        panel.set_heading_editing(false);
        if let Some(old) = data.file.clone() {
            let new = sanitize_axes_name(name.as_str());
            if !new.is_empty() && new != old {
                let from = data.dir.join(format!("{old}.txt"));
                let to = data.dir.join(format!("{new}.txt"));
                if !to.exists() && std::fs::rename(&from, &to).is_ok() {
                    if data.active.as_deref() == Some(old.as_str()) {
                        data.active = Some(new.clone());
                        let _ = command_tx.send(UiCommand::SelectAxis { name: new.clone() });
                    }
                    open_axis(panel, data, &new);
                    refresh_axes(data, panel);
                } else {
                    panel.set_heading(old.into());
                }
            } else if new == old {
                panel.set_heading(old.into());
            }
        }
    }
    if panel.get_frame_editing() >= 0 {
        panel.set_frame_editing(-1);
        if let Some(file) = data.file.clone() {
            data.marks = load_marks_file(&axis_path(data, &file));
            let current = data.current;
            refresh_frames(panel, data, current);
            refresh_axes(data, panel);
        }
    }
    let index = panel.get_editing_index();
    if index >= 0 {
        let name = panel.get_axis_edit_text().to_string();
        panel.set_editing_index(-1);
        apply_rename(panel, data, command_tx, index, name.as_str());
    }
}

fn refresh_axes(data: &AxesData, panel: &AxesPanel) {
    let rows: Vec<AxisRow> = data
        .rows
        .iter()
        .map(|(name, count)| AxisRow {
            name: name.as_str().into(),
            count: count.to_string().into(),
            active: data.active.as_deref() == Some(name.as_str()),
        })
        .collect();
    panel.set_rows(ModelRc::new(VecModel::from(rows)));
}

pub(super) fn load_axes_rows(dir: &Path) -> Vec<(String, usize)> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows: Vec<(String, usize)> = entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("txt") {
                return None;
            }
            let name = path.file_stem()?.to_str()?.to_string();
            let count = count_frame_marks(&path);
            Some((name, count))
        })
        .collect();
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows
}

fn count_frame_marks(path: &Path) -> usize {
    let Ok(content) = std::fs::read_to_string(path) else {
        return 0;
    };
    content
        .lines()
        .filter(|line| {
            let line = line.trim();
            let Some(rest) = line.strip_prefix('[') else {
                return false;
            };
            let Some(close) = rest.find(']') else {
                return false;
            };
            rest[..close].trim().parse::<u32>().is_ok()
        })
        .count()
}

pub(super) fn unique_axes_name(dir: &Path, base: &str) -> String {
    for index in 1..10_000u32 {
        let candidate = format!("{base}{index}");
        if !dir.join(format!("{candidate}.txt")).exists() {
            return candidate;
        }
    }
    format!("{base}{}", std::process::id())
}

pub(super) fn copy_root_name(name: &str) -> String {
    let mut root = name.to_string();
    if let Some(position) = root.rfind(AXES_COPY_SUFFIX) {
        let tail = &root[position + AXES_COPY_SUFFIX.len()..];
        if !tail.is_empty() && tail.chars().all(|ch| ch.is_ascii_digit()) {
            root.truncate(position);
        }
    }
    root
}

pub(super) fn next_copy_name(dir: &Path, root: &str) -> String {
    for index in 1..10_000u32 {
        let candidate = format!("{root}{AXES_COPY_SUFFIX}{index}");
        if !dir.join(format!("{candidate}.txt")).exists() {
            return candidate;
        }
    }
    format!("{root}{AXES_COPY_SUFFIX}{}", std::process::id())
}

pub(super) fn sanitize_axes_name(raw: &str) -> String {
    let cleaned: String = raw
        .trim()
        .chars()
        .filter(|ch| !matches!(ch, '\\' | '/' | ':' | '*' | '?' | '"' | '<' | '>' | '|'))
        .collect();
    cleaned.trim().trim_end_matches('.').to_string()
}

unsafe extern "system" fn axes_window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCCREATE => {
            let create_struct = lparam.0 as *const CREATESTRUCTW;
            let state_ptr = (*create_struct).lpCreateParams as *mut AxesState;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, state_ptr as isize);
            LRESULT(1)
        }
        WM_TIMER => {
            axes_tick(hwnd);
            LRESULT(0)
        }
        WM_SIZE => {
            if let Some(state) = axes_state_mut(hwnd) {
                let width = (lparam.0 & 0xffff) as i32;
                let height = ((lparam.0 >> 16) & 0xffff) as i32;
                if width > 0
                    && height > 0
                    && (width as usize != state.buf_w || height as usize != state.buf_h)
                {
                    resize_axes(state, width, height);
                }
            }
            axes_tick(hwnd);
            LRESULT(0)
        }
        WM_ACTIVATE => {
            let active = (wparam.0 & 0xffff) as u32;
            if active == 0 {
                if let Some(state) = axes_state_mut(hwnd) {
                    if let Ok(mut data) = state.data.try_borrow_mut() {
                        flush_edits(&state.panel, &mut data, &state.command_tx);
                    }
                    if !state.panel.get_pinned()
                        && state.opened_at.elapsed().as_millis() > AXES_ACTIVATION_GRACE_MS
                    {
                        state.closing.set(true);
                    }
                }
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            if let Some(state) = axes_state(hwnd) {
                let mut point = POINT::default();
                if GetCursorPos(&mut point).is_ok() {
                    let _ = windows::Win32::Graphics::Gdi::ScreenToClient(hwnd, &mut point);
                    let zone = resize_zone(
                        state.scale,
                        state.buf_w as i32,
                        state.buf_h as i32,
                        point.x,
                        point.y,
                    );
                    let id = match zone {
                        3 | 12 => Some(IDC_SIZENWSE),
                        6 | 9 => Some(IDC_SIZENESW),
                        1 | 4 => Some(IDC_SIZEWE),
                        2 | 8 => Some(IDC_SIZENS),
                        _ => None,
                    };
                    if let Some(id) = id {
                        if let Ok(cursor) = LoadCursorW(HINSTANCE::default(), id) {
                            let _ = windows::Win32::UI::WindowsAndMessaging::SetCursor(cursor);
                        }
                    } else if let Ok(cursor) = LoadCursorW(HINSTANCE::default(), IDC_ARROW) {
                        let _ = windows::Win32::UI::WindowsAndMessaging::SetCursor(cursor);
                    }
                }
            }
            LRESULT(1)
        }
        WM_MOUSEWHEEL => {
            if let Some(state) = axes_state_mut(hwnd) {
                if let Ok(mut data) = state.data.try_borrow_mut() {
                    if data.file.is_some() {
                        let delta = ((wparam.0 >> 16) & 0xffff) as i16 as f32;
                        let list_height = (state.buf_h as f32 / state.scale) - 46.0;
                        let max_scroll =
                            ((data.marks.len() as f32) * 30.0 - list_height).max(0.0);
                        let next = (data.scroll - delta / 120.0 * 90.0).clamp(0.0, max_scroll);
                        data.scroll = next;
                        data.follow = false;
                        state.panel.set_scroll_y(next);
                        state.panel.set_follow(false);
                    }
                }
            }
            LRESULT(0)
        }
        WM_RBUTTONDOWN | WM_MBUTTONDOWN => {
            if let Some(state) = axes_state(hwnd) {
                focus_game_window(&state.state);
            }
            LRESULT(0)
        }
        WM_CLOSE_AXES => {
            if let Some(state) = axes_state(hwnd) {
                state.closing.set(true);
            }
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            if let Some(state) = axes_state_mut(hwnd) {
                if state.resize_zone != 0 {
                    let mut cursor = POINT::default();
                    let _ = GetCursorPos(&mut cursor);
                    let dx = cursor.x - state.resize_origin.x;
                    let dy = cursor.y - state.resize_origin.y;
                    let mut left = state.resize_rect.left;
                    let mut top = state.resize_rect.top;
                    let mut right = state.resize_rect.right;
                    let mut bottom = state.resize_rect.bottom;
                    if state.resize_zone & 1 != 0 {
                        left += dx;
                    }
                    if state.resize_zone & 2 != 0 {
                        top += dy;
                    }
                    if state.resize_zone & 4 != 0 {
                        right += dx;
                    }
                    if state.resize_zone & 8 != 0 {
                        bottom += dy;
                    }
                    let mut width = right - left;
                    let mut height = bottom - top;
                    if width < AXES_MIN_W {
                        width = AXES_MIN_W;
                        if state.resize_zone & 1 != 0 {
                            left = right - AXES_MIN_W;
                        }
                    }
                    if height < AXES_MIN_H {
                        height = AXES_MIN_H;
                        if state.resize_zone & 2 != 0 {
                            top = bottom - AXES_MIN_H;
                        }
                    }
                    let _ = SetWindowPos(
                        hwnd,
                        HWND::default(),
                        left,
                        top,
                        width,
                        height,
                        SWP_NOZORDER,
                    );
                    return LRESULT(0);
                }

                if state.mouse_down && !state.drag_blocked {
                    let mut cursor = POINT::default();
                    let _ = GetCursorPos(&mut cursor);
                    let dx = cursor.x - state.drag_origin.x;
                    let dy = cursor.y - state.drag_origin.y;
                    if dx.abs() > AXES_DRAG_THRESHOLD || dy.abs() > AXES_DRAG_THRESHOLD {
                        state.moved = true;
                    }
                    if state.moved {
                        if state.pending_resize != 0 {
                            state.resize_zone = state.pending_resize;
                            state.pending_resize = 0;
                            state.mouse_down = false;
                            state.resize_origin = state.drag_origin;
                            let mut rect = RECT::default();
                            if GetWindowRect(hwnd, &mut rect).is_ok() {
                                state.resize_rect = rect;
                            }
                            return LRESULT(0);
                        }
                        let _ = SetWindowPos(
                            hwnd,
                            HWND::default(),
                            state.window_origin.x + dx,
                            state.window_origin.y + dy,
                            0,
                            0,
                            SWP_NOZORDER | SWP_NOSIZE,
                        );
                        return LRESULT(0);
                    }
                }

                let position = logical_pos(lparam, state.scale);
                let _ = state
                    .window
                    .window()
                    .try_dispatch_event(WindowEvent::PointerMoved { position });
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            if let Some(state) = axes_state_mut(hwnd) {
                let (x, y) = client_xy(lparam);
                if x < 0 || y < 0 || x >= state.buf_w as i32 || y >= state.buf_h as i32 {
                    focus_game_window(&state.state);
                    if let Ok(mut data) = state.data.try_borrow_mut() {
                        flush_edits(&state.panel, &mut data, &state.command_tx);
                    }
                    return LRESULT(0);
                }

                let logical_y = y as f32 / state.scale;
                let row_h = 30.0;
                let rows_top = 38.0;
                let heading_editing = state.panel.get_heading_editing();
                let mut band: Option<(f32, f32)> = None;
                if heading_editing {
                    // The axis title sits in the header strip above the rows.
                    band = Some((0.0, rows_top));
                } else if let Ok(data) = state.data.try_borrow() {
                    if let Some(index) = Some(state.panel.get_frame_editing()).filter(|i| *i >= 0) {
                        let top = rows_top + index as f32 * row_h - data.scroll;
                        band = Some((top, top + row_h));
                    } else if state.panel.get_editing_index() >= 0 {
                        let index = state.panel.get_editing_index();
                        let top = rows_top + index as f32 * row_h;
                        band = Some((top, top + row_h));
                    }
                }
                let inside_band = band
                    .map(|(top, bottom)| logical_y >= top && logical_y < bottom)
                    .unwrap_or(false);
                if !inside_band {
                    if let Ok(mut data) = state.data.try_borrow_mut() {
                        flush_edits(&state.panel, &mut data, &state.command_tx);
                    }
                }

                let rows_count = if let Ok(data) = state.data.try_borrow() {
                    if data.file.is_some() {
                        data.marks.len()
                    } else {
                        data.rows.len()
                    }
                } else {
                    0
                };
                let scroll = if let Ok(data) = state.data.try_borrow() {
                    if data.file.is_some() {
                        data.scroll
                    } else {
                        0.0
                    }
                } else {
                    0.0
                };
                let rows_bottom =
                    rows_top + (rows_count as f32 * row_h - scroll).max(0.0);
                let logical_x = x as f32 / state.scale;
                let in_header_left = logical_y < rows_top && logical_x < 34.0;
                let in_rows = logical_y >= rows_top && logical_y < rows_bottom;
                let editing_frame = state.panel.get_detail() && in_rows;
                // Never steal the foreground while an inline editor is open: clicking
                // inside the TextInput (e.g. the axis title, which starts right of the
                // back chevron) must keep the focus in the editor so text can be
                // selected with the mouse.
                let any_editor_open = heading_editing
                    || state.panel.get_editing_index() >= 0
                    || state.panel.get_frame_editing() >= 0;
                if !any_editor_open && !editing_frame && !in_header_left {
                    focus_game_window(&state.state);
                }

                let zone = resize_zone(state.scale, state.buf_w as i32, state.buf_h as i32, x, y);
                // While an inline editor is open the pointer belongs to it: the axis
                // title sits inside the top/left resize band, so without this guard a
                // press there started a window resize and never reached Slint's
                // TextInput (text could not be selected with the mouse).
                if zone != 0 && !any_editor_open {
                    state.resize_zone = zone;
                    let mut cursor = POINT::default();
                    let _ = GetCursorPos(&mut cursor);
                    state.resize_origin = cursor;
                    let mut rect = RECT::default();
                    if GetWindowRect(hwnd, &mut rect).is_ok() {
                        state.resize_rect = rect;
                    }
                    let _ = SetCapture(hwnd);
                    return LRESULT(0);
                }

                state.mouse_down = true;
                state.moved = false;
                // Any open inline editor (row rename, frame note, axis title) keeps
                // the pointer for text selection instead of dragging the window.
                state.drag_blocked = inside_band
                    || heading_editing
                    || state.panel.get_editing_index() >= 0
                    || state.panel.get_frame_editing() >= 0;
                state.pending_resize = resize_zone_with(
                    40.0,
                    state.scale,
                    state.buf_w as i32,
                    state.buf_h as i32,
                    x,
                    y,
                );
                let mut cursor = POINT::default();
                let _ = GetCursorPos(&mut cursor);
                state.drag_origin = cursor;
                let _ = SetCapture(hwnd);
                let mut rect = RECT::default();
                if GetWindowRect(hwnd, &mut rect).is_ok() {
                    state.window_origin = POINT {
                        x: rect.left,
                        y: rect.top,
                    };
                }

                let position = logical_pos(lparam, state.scale);
                let _ = state
                    .window
                    .window()
                    .try_dispatch_event(WindowEvent::PointerPressed {
                        position,
                        button: PointerEventButton::Left,
                    });
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if let Some(state) = axes_state_mut(hwnd) {
                if state.resize_zone != 0 {
                    state.resize_zone = 0;
                    let _ = ReleaseCapture();
                    return LRESULT(0);
                }

                let moved = state.moved;
                state.mouse_down = false;
                state.moved = false;
                state.drag_blocked = false;
                state.pending_resize = 0;
                let _ = ReleaseCapture();
                if moved {
                    let _ = state
                        .window
                        .window()
                        .try_dispatch_event(WindowEvent::PointerExited);
                } else {
                    let position = logical_pos(lparam, state.scale);
                    let _ = state.window.window().try_dispatch_event(
                        WindowEvent::PointerReleased {
                            position,
                            button: PointerEventButton::Left,
                        },
                    );
                }
            }
            LRESULT(0)
        }
        WM_CHAR => {
            if let Some(state) = axes_state(hwnd) {
                let editing = state.panel.get_editing_index() >= 0
                    || state.panel.get_frame_editing() >= 0
                    || state.panel.get_heading_editing();
                if editing {
                    if let Some(text) = decode_wm_char(&state.pending_high, wparam.0 as u16) {
                        dispatch_key(&state.window, text);
                    }
                }
            }
            LRESULT(0)
        }
        WM_KEYUP => {
            if let Some(state) = axes_state(hwnd) {
                let editing = state.panel.get_editing_index() >= 0
                    || state.panel.get_frame_editing() >= 0
                    || state.panel.get_heading_editing();
                if editing && wparam.0 as u16 == VK_SHIFT.0 {
                    // Release the modifier so Slint's TextInput stops extending
                    // the selection with the arrow keys.
                    let _ = state.window.window().try_dispatch_event(
                        WindowEvent::KeyReleased {
                            text: Key::Shift.into(),
                        },
                    );
                }
            }
            LRESULT(0)
        }
        WM_KEYDOWN => {
            if let Some(state) = axes_state(hwnd) {
                let editing = state.panel.get_editing_index() >= 0
                    || state.panel.get_frame_editing() >= 0
                    || state.panel.get_heading_editing();
                let vk = wparam.0 as u16;
                // Forward Shift so text selection (Shift+Arrows) works while
                // renaming an axis, exactly like in a normal edit control.
                if editing && vk == VK_SHIFT.0 {
                    dispatch_key(&state.window, Key::Shift.into());
                    return LRESULT(0);
                }
                let ctrl = (GetKeyState(VK_CONTROL.0 as i32) as u16) & 0x8000 != 0;
                if editing && ctrl {
                    if vk == VK_C.0 {
                        let text = state.panel.get_focus_text().to_string();
                        if !text.is_empty() {
                            let _ = crate::clipboard::set_text(&text);
                        }
                    } else if vk == VK_V.0 {
                        if let Some(text) = crate::clipboard::get_text() {
                            for ch in text.chars() {
                                if !ch.is_control() {
                                    dispatch_key(&state.window, ch.to_string().into());
                                }
                            }
                        }
                    }
                    return LRESULT(0);
                }
                if vk == VK_ESCAPE.0 {
                    if state.panel.get_heading_editing() {
                        state.panel.set_heading_editing(false);
                    } else if state.panel.get_frame_editing() >= 0 {
                        state.panel.set_frame_editing(-1);
                    } else if state.panel.get_editing_index() >= 0 {
                        state.panel.set_editing_index(-1);
                    } else {
                        state.closing.set(true);
                    }
                } else if editing {
                    if vk == VK_RETURN.0 {
                        dispatch_key(&state.window, Key::Return.into());
                        focus_game_window(&state.state);
                    } else {
                        let key = match vk {
                            v if v == VK_BACK.0 => Some(Key::Backspace),
                            v if v == VK_DELETE.0 => Some(Key::Delete),
                            v if v == VK_LEFT.0 => Some(Key::LeftArrow),
                            v if v == VK_RIGHT.0 => Some(Key::RightArrow),
                            v if v == VK_HOME.0 => Some(Key::Home),
                            v if v == VK_END.0 => Some(Key::End),
                            _ => None,
                        };
                        if let Some(key) = key {
                            dispatch_key(&state.window, key.into());
                        }
                    }
                }
            }
            LRESULT(0)
        }
        WM_DESTROY => {
            let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AxesState;
            if !state_ptr.is_null() {
                let _ = Box::from_raw(state_ptr);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn resize_axes(state: &mut AxesState, width: i32, height: i32) {
    let _ = DeleteDC(state.mem_dc);
    let _ = DeleteObject(HGDIOBJ(state.dib.0));
    if let Some((mem_dc, dib, bits)) = create_dib(width, height) {
        state.mem_dc = mem_dc;
        state.dib = dib;
        state.bits = bits;
        state.buf_w = width as usize;
        state.buf_h = height as usize;
    }
    state
        .window
        .set_size(PhysicalSize::new(width as u32, height as u32));
}

/// Re-scale the axis window in place when the overlay scale changes: new font
/// metrics for Slint, a fresh framebuffer DIB and a resized window.
unsafe fn apply_axes_scale(hwnd: HWND, state: &mut AxesState, pct: u16) {
    let base = if state.base_scale > 0.1 {
        state.base_scale
    } else {
        1.0
    };
    let new_scale = base * (f32::from(pct.max(1)) / 100.0);
    if (new_scale - state.scale).abs() < 0.001 {
        state.scale_pct = pct;
        return;
    }

    let ratio = if state.scale > 0.01 {
        new_scale / state.scale
    } else {
        1.0
    };
    let width = ((state.buf_w as f32) * ratio).round().max(1.0) as i32;
    let height = ((state.buf_h as f32) * ratio).round().max(1.0) as i32;

    state.scale = new_scale;
    state.scale_pct = pct;
    let _ = state
        .window
        .window()
        .try_dispatch_event(WindowEvent::ScaleFactorChanged {
            scale_factor: new_scale,
        });
    // Rebuilds the DIB and resizes the Slint window to match.
    resize_axes(state, width, height);
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

unsafe fn axes_tick(hwnd: HWND) {
    slint::platform::update_timers_and_animations();
    let Some(state) = axes_state_mut(hwnd) else {
        return;
    };

    if state.panel.get_editing_index() >= 0
        || state.panel.get_frame_editing() >= 0
        || state.panel.get_heading_editing()
    {
        let foreground = unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() };
        if foreground != hwnd {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow(hwnd);
        }
    }

    let snapshot = state.state.snapshot();
    // Follow the overlay scale instead of closing: the old code closed the
    // window here, which could wedge it (the flag was set while another message
    // loop owned the thread, and `axes_tick` stopped drawing in the meantime).
    if snapshot.ui.overlay_scale_pct != state.scale_pct {
        apply_axes_scale(hwnd, state, snapshot.ui.overlay_scale_pct);
    }

    let current_frame = snapshot.ui.global_frame;

    if let Ok(mut data) = state.data.try_borrow_mut() {
        if data.file.is_some() {
            // Re-read the open axis file periodically: frames recorded from the
            // HUD (button or hotkey) are appended to that file by the worker, and
            // this window used to keep showing the list it loaded when opened.
            if state.panel.get_frame_editing() < 0
                && data.last_mark_reload.elapsed() >= Duration::from_millis(250)
            {
                data.last_mark_reload = Instant::now();
                if let Some(file) = data.file.clone() {
                    let path = axis_path(&data, &file);
                    let reloaded = load_marks_file(&path);
                    if reloaded.len() != data.last_mark_count || reloaded != data.marks {
                        data.last_mark_count = reloaded.len();
                        data.marks = reloaded;
                        refresh_frames(&state.panel, &data, current_frame);
                        refresh_axes(&data, &state.panel);
                    }
                }
            }
            if data.current != current_frame {
                data.current = current_frame;
                refresh_frames(&state.panel, &data, current_frame);
            }
            if data.follow {
                let logical_height = state.buf_h as f32 / state.scale;
                let list_height = logical_height - 16.0 - 30.0;
                let target = follow_scroll(&data, list_height.max(0.0), current_frame);
                if (target - data.scroll).abs() > 0.5 {
                    data.scroll = target;
                    state.panel.set_scroll_y(target);
                }
            }
        } else {
            let rows = load_axes_rows(&data.dir);
            if rows != data.rows {
                data.rows = rows;
                refresh_axes(&data, &state.panel);
            }
        }
    }

    let w = state.buf_w;
    let h = state.buf_h;
    let bits = state.bits;
    let drawn = state.window.draw_if_needed(|renderer: &SoftwareRenderer| {
        let buffer = unsafe { std::slice::from_raw_parts_mut(bits, w * h) };
        renderer.render(buffer, w);
    });
    if drawn {
        present_layered(hwnd, state.mem_dc, w as i32, h as i32);
    }
}

unsafe fn axes_state<'a>(hwnd: HWND) -> Option<&'a AxesState> {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AxesState;
    if state_ptr.is_null() {
        None
    } else {
        Some(&*state_ptr)
    }
}

unsafe fn axes_state_mut<'a>(hwnd: HWND) -> Option<&'a mut AxesState> {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut AxesState;
    if state_ptr.is_null() {
        None
    } else {
        Some(&mut *state_ptr)
    }
}
