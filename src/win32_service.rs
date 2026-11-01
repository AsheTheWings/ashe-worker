#![allow(unsafe_op_in_unsafe_fn)]

//! The Win32 side of the worker: hotkeys, the tray icon and menu, the native
//! overlay, text injection, and an Escape-only keyboard hook that cancels a
//! running text action without stealing other keys.

use crate::injector;
use crate::logger;
use crate::native_overlay::{NativeOverlay, OverlayFrame};
use crate::pill_renderer;
use crate::util::{pcwstr, wide};
use crossbeam_channel::{Receiver, Sender};
use std::cell::RefCell;
use std::ffi::c_void;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::ptr::null_mut;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HMODULE, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromPoint,
};
use windows::Win32::System::LibraryLoader::{
    FindResourceW, GetModuleHandleW, LoadResource, LockResource, SizeofResource,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, MOD_NOREPEAT, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
    VK_CONTROL, VK_ESCAPE, VK_LWIN, VK_MENU, VK_RWIN,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const FIX_GRAMMAR_HOTKEY_ID: i32 = 1006;
const ANSWER_QUESTION_HOTKEY_ID: i32 = 1007;
const TIMER_SERVICE: usize = 2001;
const TIMER_INTERVAL_MS: u32 = 16;
const CURSOR_OVERLAY_GAP: i32 = 8;
const WM_TRAY: u32 = WM_APP + 1;
const MENU_RELOAD_CONFIG: usize = 3002;
const MENU_ABOUT: usize = 3005;
const MENU_QUIT: usize = 3006;
const MENU_TOGGLE_ACTIVITY: usize = 3007;
const MENU_OPEN_RECORDS: usize = 3008;
const MENU_OPEN_JOURNAL: usize = 3009;
const APP_ICON_RESOURCE_ID: u16 = 1;

#[derive(Debug, Clone)]
pub enum Win32Event {
    CancelRequested,
    FixGrammarRequested { target_hwnd: isize, x: i32, y: i32 },
    AnswerQuestionRequested { target_hwnd: isize, x: i32, y: i32 },
    PositionChanged { x: i32, y: i32 },
    ReloadConfigRequested,
    ToggleActivityRequested,
    OpenRecordsRequested,
    OpenJournalRequested,
    AboutRequested,
    QuitRequested,
    InjectionCompleted(Result<(), String>),
    ServiceStopped,
}

#[derive(Debug, Clone)]
pub enum Win32Command {
    SetEscapeCapture(bool),
    SetFollowCursor(bool),
    SetTooltip(String),
    SetActivityStatus {
        running: bool,
        status: String,
    },
    ShowMessageBox {
        title: String,
        text: String,
    },
    OpenPath(String),
    InjectText {
        target_hwnd: isize,
        text: String,
        append_after_selection: bool,
    },
    UpdateOverlay {
        x: f32,
        y: f32,
        visible: bool,
        text: Option<String>,
    },
    Shutdown,
}

struct ServiceState {
    event_tx: Sender<Win32Event>,
    command_rx: Receiver<Win32Command>,
    follow_cursor: bool,
    activity_running: bool,
    activity_status: String,
    last_records_open: Option<Instant>,
    instance: HMODULE,
    overlay: Option<NativeOverlay>,
    overlay_error_logged: bool,
    keyboard_hook: Option<HHOOK>,
}

struct OverlayUpdate {
    x: f32,
    y: f32,
    visible: bool,
    text: Option<String>,
}

thread_local! {
    static ESCAPE_HOOK: RefCell<Option<EscapeHookState>> = const { RefCell::new(None) };
}

/// State of the Escape-only hook. Every key but a bare Escape passes through.
struct EscapeHookState {
    event_tx: Sender<Win32Event>,
    accepting: bool,
    /// Escape's key-down was swallowed, so its key-up must be swallowed too.
    swallowed: bool,
}

pub fn spawn(
    event_tx: Sender<Win32Event>,
    command_rx: Receiver<Win32Command>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || run(event_tx, command_rx))
}

fn run(event_tx: Sender<Win32Event>, command_rx: Receiver<Win32Command>) {
    logger::info("Win32 service thread starting");
    let result = unsafe { run_message_loop(event_tx.clone(), command_rx) };
    if let Err(err) = result {
        logger::info(format!("Win32 service failed: {err:#}"));
    }
    let _ = event_tx.send(Win32Event::ServiceStopped);
    logger::info("Win32 service thread stopped");
}

unsafe fn run_message_loop(
    event_tx: Sender<Win32Event>,
    command_rx: Receiver<Win32Command>,
) -> anyhow::Result<()> {
    let instance = GetModuleHandleW(None)?;
    let class = wide("AsheWorkerServiceWindow");
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        hInstance: instance.into(),
        lpfnWndProc: Some(window_proc),
        lpszClassName: pcwstr(&class),
        hIcon: load_app_icon(0, 0),
        hIconSm: load_app_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)),
        ..Default::default()
    };
    let _ = RegisterClassExW(&wc);
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        pcwstr(&class),
        pcwstr(&wide("Ashe Worker Service")),
        WS_OVERLAPPEDWINDOW,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        None,
        None,
        Some(instance.into()),
        Some(null_mut()),
    )?;
    let overlay = match NativeOverlay::new(instance) {
        Ok(overlay) => Some(overlay),
        Err(error) => {
            logger::info(format!("Native layered overlay creation failed: {error:#}"));
            None
        }
    };
    let state = Box::new(ServiceState {
        event_tx,
        command_rx,
        follow_cursor: false,
        activity_running: false,
        activity_status: "activity tracking starting".to_string(),
        last_records_open: None,
        instance,
        overlay,
        overlay_error_logged: false,
        keyboard_hook: None,
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    register_hotkeys(hwnd);
    if SetTimer(Some(hwnd), TIMER_SERVICE, TIMER_INTERVAL_MS, None) == 0 {
        logger::info("Win32 service SetTimer failed");
    }
    add_tray(hwnd, "Ashe Worker - Idle");
    let mut message = MSG::default();
    while GetMessageW(&mut message, None, 0, 0).into() {
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let state_ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut ServiceState;
    let state = state_ptr.as_mut();
    match message {
        WM_HOTKEY => {
            if let Some(state) = state {
                let (x, y) = active_input_position();
                let target_hwnd = target_window(hwnd);
                let event = match wparam.0 as i32 {
                    FIX_GRAMMAR_HOTKEY_ID => {
                        Some(Win32Event::FixGrammarRequested { target_hwnd, x, y })
                    }
                    ANSWER_QUESTION_HOTKEY_ID => {
                        Some(Win32Event::AnswerQuestionRequested { target_hwnd, x, y })
                    }
                    _ => None,
                };
                if let Some(event) = event {
                    logger::info("Text action hotkey pressed");
                    let _ = state.event_tx.send(event);
                }
            }
            return LRESULT(0);
        }
        WM_TIMER => {
            if let Some(state) = state {
                drain_commands(hwnd, state);
                remove_escape_hook_if_drained(state);
                if state.follow_cursor {
                    let (x, y) = active_input_position();
                    let _ = state.event_tx.send(Win32Event::PositionChanged { x, y });
                }
            }
            return LRESULT(0);
        }
        WM_TRAY => {
            if let Some(state) = state {
                if lparam.0 as u32 == WM_LBUTTONUP {
                    let now = Instant::now();
                    let should_open = state
                        .last_records_open
                        .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(750));
                    if should_open {
                        state.last_records_open = Some(now);
                        let _ = state.event_tx.send(Win32Event::OpenRecordsRequested);
                    }
                    return LRESULT(0);
                }
                if lparam.0 as u32 == WM_RBUTTONUP {
                    show_tray_menu(hwnd, state.activity_running, &state.activity_status);
                    return LRESULT(0);
                }
            }
        }
        WM_COMMAND => {
            if let Some(state) = state {
                let event = match wparam.0 & 0xffff {
                    MENU_RELOAD_CONFIG => Some(Win32Event::ReloadConfigRequested),
                    MENU_TOGGLE_ACTIVITY => Some(Win32Event::ToggleActivityRequested),
                    MENU_OPEN_RECORDS => Some(Win32Event::OpenRecordsRequested),
                    MENU_OPEN_JOURNAL => Some(Win32Event::OpenJournalRequested),
                    MENU_ABOUT => Some(Win32Event::AboutRequested),
                    MENU_QUIT => Some(Win32Event::QuitRequested),
                    _ => None,
                };
                if let Some(event) = event {
                    let _ = state.event_tx.send(event);
                    return LRESULT(0);
                }
            }
        }
        WM_DESTROY => {
            remove_tray(hwnd);
            if let Some(state) = state {
                remove_escape_hook(state);
            }
            let _ = UnregisterHotKey(Some(hwnd), FIX_GRAMMAR_HOTKEY_ID);
            let _ = UnregisterHotKey(Some(hwnd), ANSWER_QUESTION_HOTKEY_ID);
            if !state_ptr.is_null() {
                let _ = Box::from_raw(state_ptr);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            return LRESULT(0);
        }
        _ => {}
    }
    DefWindowProcW(hwnd, message, wparam, lparam)
}

unsafe fn drain_commands(hwnd: HWND, state: &mut ServiceState) {
    // Frames arrive at 60 Hz; present only the newest one of a backlog.
    let mut pending_overlay = None;
    while let Ok(command) = state.command_rx.try_recv() {
        match command {
            Win32Command::SetEscapeCapture(active) => set_escape_capture(state, active),
            Win32Command::SetFollowCursor(follow) => state.follow_cursor = follow,
            Win32Command::SetTooltip(tooltip) => set_tray_tooltip(hwnd, &tooltip),
            Win32Command::SetActivityStatus { running, status } => {
                state.activity_running = running;
                state.activity_status = status;
            }
            Win32Command::ShowMessageBox { title, text } => message_box(hwnd, &text, &title),
            Win32Command::OpenPath(path) => open_path(hwnd, &path),
            Win32Command::InjectText {
                target_hwnd,
                text,
                append_after_selection,
            } => {
                let target = HWND(target_hwnd as *mut c_void);
                let result = injector::inject_text_to(target, &text, append_after_selection)
                    .map_err(|err| format!("{err:#}"));
                let _ = state.event_tx.send(Win32Event::InjectionCompleted(result));
            }
            Win32Command::UpdateOverlay {
                x,
                y,
                visible,
                text,
            } => {
                pending_overlay = Some(OverlayUpdate {
                    x,
                    y,
                    visible,
                    text,
                });
            }
            Win32Command::Shutdown => {
                // Destroy the window after this timer callback releases the state.
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                return;
            }
        }
    }
    let Some(update) = pending_overlay else {
        return;
    };
    if state.overlay.is_none() && update.visible && !state.overlay_error_logged {
        match NativeOverlay::new(state.instance) {
            Ok(overlay) => state.overlay = Some(overlay),
            Err(error) => {
                logger::info(format!(
                    "Native layered overlay recreation failed: {error:#}"
                ));
                state.overlay_error_logged = true;
            }
        }
    }
    let Some(overlay) = state.overlay.as_mut() else {
        return;
    };
    match overlay.update(OverlayFrame {
        x: update.x,
        y: update.y,
        visible: update.visible,
        text: update.text.as_deref(),
    }) {
        Ok(()) => state.overlay_error_logged = false,
        Err(error) if !state.overlay_error_logged => {
            logger::info(format!("Native layered overlay update failed: {error:#}"));
            state.overlay_error_logged = true;
        }
        Err(_) => {}
    }
}

unsafe fn open_path(hwnd: HWND, path: &str) {
    let mut path = PathBuf::from(path);
    while !path.exists() {
        let Some(parent) = path.parent().map(Path::to_path_buf) else {
            break;
        };
        if parent == path {
            break;
        }
        path = parent;
    }
    if let Err(err) = Command::new("explorer.exe").arg(path).spawn() {
        logger::info(format!("Open path failed: {err:#}"));
        message_box(hwnd, &format!("Could not open path: {err}"), "Ashe Worker");
    }
}

unsafe fn set_escape_capture(state: &mut ServiceState, active: bool) {
    if !active {
        ESCAPE_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook.accepting = false;
            }
        });
        remove_escape_hook_if_drained(state);
        return;
    }
    if state.keyboard_hook.is_some() {
        ESCAPE_HOOK.with(|hook| {
            if let Some(hook) = hook.borrow_mut().as_mut() {
                hook.accepting = true;
            }
        });
        return;
    }
    ESCAPE_HOOK.with(|hook| {
        *hook.borrow_mut() = Some(EscapeHookState {
            event_tx: state.event_tx.clone(),
            accepting: true,
            swallowed: false,
        });
    });
    let installed = GetModuleHandleW(None).and_then(|module| {
        SetWindowsHookExW(
            WH_KEYBOARD_LL,
            Some(escape_keyboard_proc),
            Some(module.into()),
            0,
        )
    });
    match installed {
        Ok(hook) => state.keyboard_hook = Some(hook),
        Err(error) => {
            ESCAPE_HOOK.with(|hook| *hook.borrow_mut() = None);
            logger::info(format!("Escape keyboard hook failed: {error}"));
        }
    }
}

/// Remove the hook once it no longer accepts Escape and no swallowed Escape
/// still awaits its key-up.
unsafe fn remove_escape_hook_if_drained(state: &mut ServiceState) {
    if state.keyboard_hook.is_none() {
        return;
    }
    let drained = ESCAPE_HOOK.with(|hook| {
        hook.borrow()
            .as_ref()
            .is_none_or(|hook| !hook.accepting && !hook.swallowed)
    });
    if drained {
        remove_escape_hook(state);
    }
}

unsafe fn remove_escape_hook(state: &mut ServiceState) {
    if let Some(hook) = state.keyboard_hook.take()
        && let Err(error) = UnhookWindowsHookEx(hook)
    {
        logger::info(format!("Escape keyboard hook removal failed: {error:#}"));
    }
    ESCAPE_HOOK.with(|hook| *hook.borrow_mut() = None);
}

unsafe extern "system" fn escape_keyboard_proc(
    code: i32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if code != HC_ACTION as i32 {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let message = wparam.0 as u32;
    let is_down = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
    let is_up = matches!(message, WM_KEYUP | WM_SYSKEYUP);
    let Some(key) = (lparam.0 as *const KBDLLHOOKSTRUCT).as_ref() else {
        return CallNextHookEx(None, code, wparam, lparam);
    };
    if key.vkCode != VK_ESCAPE.0 as u32
        || (!is_down && !is_up)
        || key.flags.contains(LLKHF_INJECTED)
        || key.dwExtraInfo == injector::ASHE_INJECTED_EXTRA_INFO
    {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let modified = [VK_CONTROL, VK_MENU, VK_LWIN, VK_RWIN]
        .iter()
        .any(|key| GetAsyncKeyState(key.0 as i32) < 0);
    let swallow = ESCAPE_HOOK.with(|hook| {
        let mut hook = hook.borrow_mut();
        let Some(hook) = hook.as_mut() else {
            return false;
        };
        escape_decision(hook, is_down, modified)
    });
    if swallow {
        LRESULT(1)
    } else {
        CallNextHookEx(None, code, wparam, lparam)
    }
}

/// Decide whether one Escape key event is swallowed, sending a cancel on the
/// first key-down of a bare Escape while the hook accepts.
fn escape_decision(hook: &mut EscapeHookState, is_down: bool, modified: bool) -> bool {
    if !is_down {
        return std::mem::take(&mut hook.swallowed);
    }
    if hook.swallowed {
        return true;
    }
    if !hook.accepting || modified {
        return false;
    }
    hook.swallowed = true;
    let _ = hook.event_tx.send(Win32Event::CancelRequested);
    true
}

unsafe fn register_hotkeys(hwnd: HWND) {
    for (id, key, label) in [
        (FIX_GRAMMAR_HOTKEY_ID, 'G', "Win+Shift+G"),
        (ANSWER_QUESTION_HOTKEY_ID, 'Q', "Win+Shift+Q"),
    ] {
        if let Err(err) = RegisterHotKey(
            Some(hwnd),
            id,
            MOD_WIN | MOD_SHIFT | MOD_NOREPEAT,
            key as u32,
        ) {
            logger::info(format!("RegisterHotKey failed: {err:#}"));
            message_box(
                hwnd,
                &format!("{label} could not be registered. Another app may already be using it."),
                "Ashe Worker",
            );
        }
    }
}

/// Anchor of the overlay's bottom-left corner: just above and right of the
/// cursor, kept inside the monitor's work area.
unsafe fn active_input_position() -> (i32, i32) {
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_err() {
        return (120, 120);
    }
    let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
    let scale = monitor_scale_factor(monitor);
    let width = pill_renderer::MINI_WIDTH * scale;
    let height = pill_renderer::MINI_HEIGHT * scale;
    let gap = CURSOR_OVERLAY_GAP as f32 * scale;
    let cursor_x = cursor.x as f32;
    let cursor_y = cursor.y as f32;
    let mut x = cursor_x + gap;
    let mut y = cursor_y - gap;
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let left = info.rcWork.left as f32;
        let top = info.rcWork.top as f32;
        let right = info.rcWork.right as f32;
        let bottom = info.rcWork.bottom as f32;
        if x + width > right {
            x = cursor_x - width - gap;
        }
        if y - height < top {
            y = cursor_y + gap + height;
        }
        x = clamp_to_work_area(x, left, right - width);
        y = clamp_to_work_area(y, top + height, bottom);
    }
    (x.round() as i32, y.round() as i32)
}

unsafe fn monitor_scale_factor(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> f32 {
    let mut dpi_x = 96;
    let mut dpi_y = 96;
    if GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y).is_err() || dpi_x == 0 {
        return 1.0;
    }
    dpi_x as f32 / 96.0
}

fn clamp_to_work_area(value: f32, min: f32, max: f32) -> f32 {
    if min > max {
        min
    } else {
        value.clamp(min, max)
    }
}

unsafe fn target_window(service_hwnd: HWND) -> isize {
    let foreground = GetForegroundWindow();
    if foreground.0.is_null() || foreground == service_hwnd {
        0
    } else {
        foreground.0 as isize
    }
}

fn add_tray(hwnd: HWND, tooltip: &str) {
    unsafe {
        let mut data = tray_data(hwnd, tooltip);
        data.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
        data.uCallbackMessage = WM_TRAY;
        data.hIcon = load_app_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
        let _ = Shell_NotifyIconW(NIM_ADD, &data);
    }
}

/// The embedded icon at the requested size, or the default application icon.
unsafe fn load_app_icon(width: i32, height: i32) -> HICON {
    load_app_icon_from_resource(width, height)
        .unwrap_or_else(|| LoadIconW(None, IDI_APPLICATION).unwrap_or_default())
}

unsafe fn load_app_icon_from_resource(width: i32, height: i32) -> Option<HICON> {
    let module = GetModuleHandleW(None).ok()?;
    let group = FindResourceW(
        Some(module),
        int_resource(APP_ICON_RESOURCE_ID),
        RT_GROUP_ICON,
    );
    if group.is_invalid() {
        return None;
    }
    let group_data = LoadResource(Some(module), group).ok()?;
    let group_ptr = LockResource(group_data) as *const u8;
    if group_ptr.is_null() || SizeofResource(Some(module), group) == 0 {
        return None;
    }
    let icon_id = LookupIconIdFromDirectoryEx(group_ptr, true, width, height, LR_DEFAULTCOLOR);
    if icon_id == 0 {
        return None;
    }
    let icon = FindResourceW(Some(module), int_resource(icon_id as u16), RT_ICON);
    if icon.is_invalid() {
        return None;
    }
    let icon_data = LoadResource(Some(module), icon).ok()?;
    let icon_ptr = LockResource(icon_data) as *const u8;
    let icon_size = SizeofResource(Some(module), icon) as usize;
    if icon_ptr.is_null() || icon_size == 0 {
        return None;
    }
    let bits = std::slice::from_raw_parts(icon_ptr, icon_size);
    CreateIconFromResourceEx(bits, true, 0x0003_0000, width, height, LR_DEFAULTCOLOR).ok()
}

fn int_resource(id: u16) -> windows::core::PCWSTR {
    windows::core::PCWSTR(id as usize as *const u16)
}

fn set_tray_tooltip(hwnd: HWND, tooltip: &str) {
    unsafe {
        let mut data = tray_data(hwnd, tooltip);
        data.uFlags = NIF_TIP;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
    }
}

fn remove_tray(hwnd: HWND) {
    unsafe {
        let data = tray_data(hwnd, "");
        let _ = Shell_NotifyIconW(NIM_DELETE, &data);
    }
}

fn tray_data(hwnd: HWND, tooltip: &str) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: 1,
        ..Default::default()
    };
    for (index, value) in wide(tooltip)
        .iter()
        .copied()
        .take(data.szTip.len())
        .enumerate()
    {
        data.szTip[index] = value;
    }
    data
}

fn show_tray_menu(hwnd: HWND, activity_running: bool, activity_status: &str) {
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        let activity_label = if activity_running {
            "Pause activity tracking"
        } else {
            "Resume activity tracking"
        };
        let append = |id: usize, label: &str| {
            let _ = AppendMenuW(menu, MF_STRING, id, pcwstr(&wide(label)));
        };
        append(MENU_TOGGLE_ACTIVITY, activity_label);
        let status = format!("Activity: {activity_status}");
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, pcwstr(&wide(&status)));
        append(MENU_OPEN_RECORDS, "Open records folder");
        append(MENU_OPEN_JOURNAL, "Open today's journal");
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        append(MENU_RELOAD_CONFIG, "Reload config");
        append(MENU_ABOUT, "About");
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        append(MENU_QUIT, "Quit");
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

pub fn message_box(hwnd: HWND, text: &str, title: &str) {
    unsafe {
        let _ = MessageBoxW(
            Some(hwnd),
            pcwstr(&wide(text)),
            pcwstr(&wide(title)),
            MB_OK | MB_ICONWARNING,
        );
    }
}

#[cfg(test)]
mod unit {
    use super::{EscapeHookState, Win32Event, escape_decision};

    fn hook(accepting: bool) -> (EscapeHookState, crossbeam_channel::Receiver<Win32Event>) {
        let (event_tx, events) = crossbeam_channel::unbounded();
        (
            EscapeHookState {
                event_tx,
                accepting,
                swallowed: false,
            },
            events,
        )
    }

    #[test]
    fn a_bare_escape_cancels_once_and_swallows_its_key_up() {
        let (mut state, events) = hook(true);
        assert!(escape_decision(&mut state, true, false));
        assert!(escape_decision(&mut state, true, false));
        assert!(escape_decision(&mut state, false, false));
        assert!(!escape_decision(&mut state, false, false));
        assert_eq!(events.try_iter().count(), 1);
    }

    #[test]
    fn modified_or_unaccepted_escape_passes_through() {
        let (mut state, events) = hook(true);
        assert!(!escape_decision(&mut state, true, true));
        let (mut idle, idle_events) = hook(false);
        assert!(!escape_decision(&mut idle, true, false));
        assert_eq!(
            events.try_iter().count() + idle_events.try_iter().count(),
            0
        );
    }
}
