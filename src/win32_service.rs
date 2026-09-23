#![allow(unsafe_op_in_unsafe_fn)]

use crate::injector;
use crate::logger;
use crate::native_overlay::{NativeOverlay, OverlayFrame};
use crate::pill_renderer::{self, TopBarContent};
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
    GetAsyncKeyState, GetKeyboardLayout, GetKeyboardState, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, RegisterHotKey, ToUnicodeEx, UnregisterHotKey, VK_BACK, VK_CONTROL,
    VK_ESCAPE, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU, VK_RCONTROL, VK_RETURN,
    VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows::Win32::UI::WindowsAndMessaging::*;

const TOGGLE_HOTKEY_ID: i32 = 1001;
const FIX_GRAMMAR_HOTKEY_ID: i32 = 1006;
const ANSWER_QUESTION_HOTKEY_ID: i32 = 1007;
const PASTE_IMAGE_HOTKEY_ID: i32 = 1008;
const VOICE_HOTKEY_ID: i32 = 1009;
const TIMER_SERVICE: usize = 2001;
const TIMER_INTERVAL_MS: u32 = 16;
const CURSOR_OVERLAY_GAP: i32 = 8;
const WM_TRAY: u32 = WM_APP + 1;
const MENU_TOGGLE: usize = 3001;
const MENU_RELOAD_CONFIG: usize = 3002;
const MENU_OPEN_LOG: usize = 3003;
const MENU_COPY_LOG_PATH: usize = 3004;
const MENU_ABOUT: usize = 3005;
const MENU_QUIT: usize = 3006;
const MENU_TOGGLE_ACTIVITY: usize = 3007;
const MENU_OPEN_ARTIFACTS: usize = 3008;
const MENU_OPEN_JOURNAL: usize = 3009;
const ICON_FILE_NAME: &str = "ashe-worker.ico";
const ICON_DATA_PATH: &str = "assets/ashe-worker.ico";
const APP_ICON_RESOURCE_ID: u16 = 1;

#[derive(Debug, Clone)]
pub enum Win32Event {
    ToggleRequested { target_hwnd: isize, x: i32, y: i32 },
    CancelRequested,
    SubmitRequested,
    LineBreakRequested,
    TypingStarted,
    TextInput(String),
    BackspaceRequested,
    PasteTextRequested,
    KeyboardCaptureFailed(String),
    FixGrammarRequested { target_hwnd: isize, x: i32, y: i32 },
    AnswerQuestionRequested { target_hwnd: isize, x: i32, y: i32 },
    PasteImageRequested { target_hwnd: isize },
    ToggleVoiceRequested,
    PositionChanged { x: i32, y: i32 },
    ReloadConfigRequested,
    OpenLogRequested,
    CopyLogPathRequested,
    ToggleActivityRequested,
    OpenArtifactsRequested,
    OpenJournalRequested,
    AboutRequested,
    QuitRequested,
    PasteCompleted(Result<(), String>),
    PathPasteCompleted(Result<(), String>),
    ServiceStopped,
}

#[derive(Debug, Clone)]
pub enum Win32Command {
    SetActive(bool),
    SetKeyboardCapture(bool),
    SetEscapeCapture(bool),
    EndTyping,
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
    OpenLog(String),
    OpenPath(String),
    CopyText(String),
    PasteText {
        target_hwnd: isize,
        text: String,
    },
    PastePath {
        target_hwnd: isize,
        text: String,
    },
    InjectText {
        target_hwnd: isize,
        text: String,
        append_after_selection: bool,
    },
    UpdateOverlay {
        x: f32,
        y: f32,
        visible: bool,
        bars: Vec<f32>,
        state: pill_renderer::PillState,
        main_text: Option<String>,
        top_bar: Option<TopBarContent>,
        mini: bool,
    },
    Shutdown,
}

struct ServiceState {
    event_tx: Sender<Win32Event>,
    command_rx: Receiver<Win32Command>,
    active: bool,
    follow_cursor: bool,
    activity_running: bool,
    activity_status: String,
    last_artifacts_open: Option<Instant>,
    instance: HMODULE,
    overlay: Option<NativeOverlay>,
    overlay_error_logged: bool,
    keyboard_hook: Option<HHOOK>,
}

struct OverlayUpdate {
    x: f32,
    y: f32,
    visible: bool,
    bars: Vec<f32>,
    state: pill_renderer::PillState,
    main_text: Option<String>,
    top_bar: Option<TopBarContent>,
    mini: bool,
}

thread_local! {
    static KEYBOARD_HOOK_STATE: RefCell<Option<KeyboardHookState>> = const { RefCell::new(None) };
}

struct KeyboardHookState {
    event_tx: Sender<Win32Event>,
    keyboard_state: [u8; 256],
    physical_down: [bool; 256],
    captured: [bool; 256],
    accepting: bool,
    /// Forward everything except a bare Escape. Text actions (grammar,
    /// question) want a cancel key without stealing typing from the
    /// foreground app while the request is in flight.
    cancel_only: bool,
    typing: bool,
    dead_key_pending: bool,
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
    let app_icon = load_app_icon(0, 0);
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        hInstance: instance.into(),
        lpfnWndProc: Some(window_proc),
        lpszClassName: pcwstr(&class),
        hIcon: app_icon,
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
    set_window_icons(hwnd);
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
        active: false,
        follow_cursor: false,
        activity_running: false,
        activity_status: "activity tracking starting".to_string(),
        last_artifacts_open: None,
        instance,
        overlay,
        overlay_error_logged: false,
        keyboard_hook: None,
    });
    SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(state) as isize);
    register_hotkey(hwnd);
    let timer_id = SetTimer(Some(hwnd), TIMER_SERVICE, TIMER_INTERVAL_MS, None);
    if timer_id == 0 {
        logger::info("Win32 service SetTimer failed");
    }
    add_tray(hwnd, "Ashe Worker - Idle - Win+Shift+H");
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
        WM_HOTKEY => match wparam.0 as i32 {
            TOGGLE_HOTKEY_ID => {
                logger::info("Toggle hotkey pressed");
                if let Some(state) = state {
                    let (x, y) = active_input_position();
                    let target_hwnd = target_window(hwnd);
                    let _ = state
                        .event_tx
                        .send(Win32Event::ToggleRequested { target_hwnd, x, y });
                }
                return LRESULT(0);
            }
            FIX_GRAMMAR_HOTKEY_ID => {
                logger::info("Fix grammar hotkey pressed");
                if let Some(state) = state {
                    let (x, y) = active_input_position();
                    let target_hwnd = target_window(hwnd);
                    let _ =
                        state
                            .event_tx
                            .send(Win32Event::FixGrammarRequested { target_hwnd, x, y });
                }
                return LRESULT(0);
            }
            ANSWER_QUESTION_HOTKEY_ID => {
                logger::info("Answer question hotkey pressed");
                if let Some(state) = state {
                    let (x, y) = active_input_position();
                    let target_hwnd = target_window(hwnd);
                    let _ = state.event_tx.send(Win32Event::AnswerQuestionRequested {
                        target_hwnd,
                        x,
                        y,
                    });
                }
                return LRESULT(0);
            }
            PASTE_IMAGE_HOTKEY_ID => {
                logger::info("Paste image hotkey pressed");
                if let Some(state) = state {
                    let _ = state.event_tx.send(Win32Event::PasteImageRequested {
                        target_hwnd: target_window(hwnd),
                    });
                }
                return LRESULT(0);
            }
            VOICE_HOTKEY_ID => {
                if let Some(state) = state {
                    let _ = state.event_tx.send(Win32Event::ToggleVoiceRequested);
                }
                return LRESULT(0);
            }
            _ => {}
        },
        WM_TIMER => {
            if let Some(state) = state {
                drain_commands(hwnd, state);
                finish_keyboard_capture_if_drained(state);
                if state.active || state.follow_cursor {
                    let (x, y) = active_input_position();
                    let _ = state.event_tx.send(Win32Event::PositionChanged { x, y });
                }
            }
            return LRESULT(0);
        }
        WM_TRAY => {
            if lparam.0 as u32 == WM_LBUTTONUP {
                if let Some(state) = state {
                    let now = Instant::now();
                    let should_open = state
                        .last_artifacts_open
                        .is_none_or(|last| now.duration_since(last) >= Duration::from_millis(750));
                    if should_open {
                        state.last_artifacts_open = Some(now);
                        let _ = state.event_tx.send(Win32Event::OpenArtifactsRequested);
                    }
                }
                return LRESULT(0);
            }
            if lparam.0 as u32 == WM_RBUTTONUP {
                if let Some(state) = state {
                    show_tray_menu(
                        hwnd,
                        state.active,
                        state.activity_running,
                        &state.activity_status,
                    );
                }
                return LRESULT(0);
            }
        }
        WM_COMMAND => {
            if let Some(state) = state {
                match wparam.0 & 0xffff {
                    MENU_TOGGLE => {
                        let (x, y) = active_input_position();
                        let _ = state.event_tx.send(Win32Event::ToggleRequested {
                            target_hwnd: target_window(hwnd),
                            x,
                            y,
                        });
                        return LRESULT(0);
                    }
                    MENU_RELOAD_CONFIG => {
                        let _ = state.event_tx.send(Win32Event::ReloadConfigRequested);
                        return LRESULT(0);
                    }
                    MENU_OPEN_LOG => {
                        let _ = state.event_tx.send(Win32Event::OpenLogRequested);
                        return LRESULT(0);
                    }
                    MENU_COPY_LOG_PATH => {
                        let _ = state.event_tx.send(Win32Event::CopyLogPathRequested);
                        return LRESULT(0);
                    }
                    MENU_TOGGLE_ACTIVITY => {
                        let _ = state.event_tx.send(Win32Event::ToggleActivityRequested);
                        return LRESULT(0);
                    }
                    MENU_OPEN_ARTIFACTS => {
                        let _ = state.event_tx.send(Win32Event::OpenArtifactsRequested);
                        return LRESULT(0);
                    }
                    MENU_OPEN_JOURNAL => {
                        let _ = state.event_tx.send(Win32Event::OpenJournalRequested);
                        return LRESULT(0);
                    }
                    MENU_ABOUT => {
                        let _ = state.event_tx.send(Win32Event::AboutRequested);
                        return LRESULT(0);
                    }
                    MENU_QUIT => {
                        let _ = state.event_tx.send(Win32Event::QuitRequested);
                        return LRESULT(0);
                    }
                    _ => {}
                }
            }
        }
        WM_DESTROY => {
            remove_tray(hwnd);
            if let Some(state) = state {
                force_remove_keyboard_capture(state);
            }
            let _ = UnregisterHotKey(Some(hwnd), TOGGLE_HOTKEY_ID);
            let _ = UnregisterHotKey(Some(hwnd), FIX_GRAMMAR_HOTKEY_ID);
            let _ = UnregisterHotKey(Some(hwnd), ANSWER_QUESTION_HOTKEY_ID);
            let _ = UnregisterHotKey(Some(hwnd), PASTE_IMAGE_HOTKEY_ID);
            let _ = UnregisterHotKey(Some(hwnd), VOICE_HOTKEY_ID);
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
    // Visual frames arrive at 60 Hz. Coalesce any backlog so a temporarily
    // busy service thread always presents the newest frame instead of
    // replaying stale animation.
    let mut pending_overlay = None;
    while let Ok(command) = state.command_rx.try_recv() {
        match command {
            Win32Command::SetActive(active) => {
                state.active = active;
            }
            Win32Command::SetKeyboardCapture(active) => {
                set_keyboard_capture(state, active, false);
            }
            Win32Command::SetEscapeCapture(active) => {
                set_keyboard_capture(state, active, true);
            }
            Win32Command::EndTyping => {
                KEYBOARD_HOOK_STATE.with(|hook_state| {
                    if let Some(hook_state) = hook_state.borrow_mut().as_mut() {
                        unsafe { hook_state.end_typing() };
                    }
                });
            }
            Win32Command::SetFollowCursor(follow) => {
                state.follow_cursor = follow;
            }
            Win32Command::SetTooltip(tooltip) => set_tray_tooltip(hwnd, &tooltip),
            Win32Command::SetActivityStatus { running, status } => {
                state.activity_running = running;
                state.activity_status = status;
            }
            Win32Command::ShowMessageBox { title, text } => message_box(hwnd, &text, &title),
            Win32Command::OpenLog(path) => {
                if let Err(err) = Command::new("notepad.exe").arg(path).spawn() {
                    logger::info(format!("Open log failed: {err:#}"));
                    message_box(
                        hwnd,
                        &format!("Could not open log file: {err}"),
                        "Ashe Worker",
                    );
                }
            }
            Win32Command::OpenPath(path) => {
                let mut path = PathBuf::from(&path);
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
            Win32Command::CopyText(text) => {
                if let Err(err) = injector::copy_text(&text) {
                    logger::info(format!("Copy text failed: {err:#}"));
                    message_box(hwnd, &format!("Could not copy text: {err}"), "Ashe Worker");
                }
            }
            Win32Command::PasteText { target_hwnd, text } => {
                let hwnd = HWND(target_hwnd as *mut c_void);
                let result = injector::paste_text_to(hwnd, &text).map_err(|err| format!("{err:#}"));
                let _ = state.event_tx.send(Win32Event::PasteCompleted(result));
            }
            Win32Command::PastePath { target_hwnd, text } => {
                let hwnd = HWND(target_hwnd as *mut c_void);
                let result = injector::paste_text_to(hwnd, &text).map_err(|err| format!("{err:#}"));
                let _ = state.event_tx.send(Win32Event::PathPasteCompleted(result));
            }
            Win32Command::InjectText {
                target_hwnd,
                text,
                append_after_selection,
            } => {
                let hwnd = HWND(target_hwnd as *mut c_void);
                let result = injector::inject_text_to(hwnd, &text, append_after_selection)
                    .map_err(|err| format!("{err:#}"));
                let _ = state.event_tx.send(Win32Event::PasteCompleted(result));
            }
            Win32Command::UpdateOverlay {
                x,
                y,
                visible,
                bars,
                state,
                main_text,
                top_bar,
                mini,
            } => {
                pending_overlay = Some(OverlayUpdate {
                    x,
                    y,
                    visible,
                    bars,
                    state,
                    main_text,
                    top_bar,
                    mini,
                });
            }
            Win32Command::Shutdown => {
                // Defer destruction until this borrowed service state is no
                // longer in use by the current timer callback.
                let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
                return;
            }
        }
    }
    // Recover from a failed overlay creation without hiding the failure.
    // Dictation keeps working through audio/transcription/injection even
    // when the pill never appears, so retry once per visible run and keep
    // the throttled log so a missing pill is never silent.
    if state.overlay.is_none()
        && let Some(update) = pending_overlay.as_ref()
        && update.visible
        && !state.overlay_error_logged
    {
        match NativeOverlay::new(state.instance) {
            Ok(overlay) => {
                state.overlay = Some(overlay);
                state.overlay_error_logged = false;
                logger::info("Native layered overlay recreated after missing");
            }
            Err(error) => {
                logger::info(format!(
                    "Native layered overlay recreation failed: {error:#} \
                     state={:?} mini={} pos=({},{})",
                    update.state, update.mini, update.x, update.y,
                ));
                state.overlay_error_logged = true;
            }
        }
    }
    if let (Some(update), Some(overlay)) =
        (pending_overlay.as_ref(), state.overlay.as_mut())
    {
        match overlay.update(OverlayFrame {
            x: update.x,
            y: update.y,
            visible: update.visible,
            bars: &update.bars,
            state: update.state,
            main_text: update.main_text.as_deref(),
            top_bar: update.top_bar.as_ref(),
            mini: update.mini,
        }) {
            Ok(()) => state.overlay_error_logged = false,
            Err(error) if !state.overlay_error_logged => {
                // Include the requested frame so a single log line explains
                // why dictation kept working while no pill was visible.
                logger::info(format!(
                    "Native layered overlay update failed: {error:#} \
                     visible={} state={:?} bars={} top_bar={} mini={} pos=({},{})",
                    update.visible,
                    update.state,
                    update.bars.len(),
                    update.top_bar.is_some(),
                    update.mini,
                    update.x,
                    update.y,
                ));
                state.overlay_error_logged = true;
            }
            Err(_) => {}
        }
    }
    // A failed overlay creation must not permanently hide dictation UI
    // while the feature itself keeps working. Surface the gap loudly so a
    // missing pill is never mistaken for an idle worker.
    // If creation still failed (or was throttled after a previous failure),
    // keep the gap visible in logs without spamming every 16 ms frame.
    if let Some(update) = pending_overlay.as_ref()
        && state.overlay.is_none()
    {
        if update.visible {
            if !state.overlay_error_logged {
                logger::info(format!(
                    "Native layered overlay missing while visible requested \
                     state={:?} pos=({},{})",
                    update.state, update.x, update.y,
                ));
                state.overlay_error_logged = true;
            }
        } else {
            // Nothing visible requested: reset so the next visible gap
            // logs again instead of staying silent.
            state.overlay_error_logged = false;
        }
    }
}

unsafe fn set_keyboard_capture(state: &mut ServiceState, active: bool, cancel_only: bool) {
    if active && state.keyboard_hook.is_some() {
        KEYBOARD_HOOK_STATE.with(|hook_state| {
            if let Some(hook_state) = hook_state.borrow_mut().as_mut() {
                hook_state.accepting = true;
                hook_state.cancel_only = cancel_only;
            }
        });
        return;
    }
    if !active {
        KEYBOARD_HOOK_STATE.with(|hook_state| {
            let mut hook_state = hook_state.borrow_mut();
            if let Some(state) = hook_state.as_mut() {
                state.accepting = false;
                state.typing = false;
                unsafe { state.clear_dead_key_state() };
            }
        });
        finish_keyboard_capture_if_drained(state);
        return;
    }

    let mut keyboard_state = [0_u8; 256];
    let _ = GetKeyboardState(&mut keyboard_state);
    let mut physical_down = [false; 256];
    for (index, value) in keyboard_state.iter_mut().enumerate() {
        let down = GetAsyncKeyState(index as i32) < 0;
        physical_down[index] = down;
        if down {
            *value |= 0x80;
        } else {
            *value &= 0x7f;
        }
    }
    KEYBOARD_HOOK_STATE.with(|hook_state| {
        *hook_state.borrow_mut() = Some(KeyboardHookState {
            event_tx: state.event_tx.clone(),
            keyboard_state,
            physical_down,
            captured: [false; 256],
            accepting: true,
            cancel_only,
            typing: false,
            dead_key_pending: false,
        });
    });

    let module = match GetModuleHandleW(None) {
        Ok(module) => module,
        Err(error) => {
            KEYBOARD_HOOK_STATE.with(|hook_state| *hook_state.borrow_mut() = None);
            let message = format!("Could not initialize dictation keyboard capture: {error}");
            logger::info(&message);
            let _ = state
                .event_tx
                .send(Win32Event::KeyboardCaptureFailed(message));
            return;
        }
    };
    match SetWindowsHookExW(
        WH_KEYBOARD_LL,
        Some(dictation_keyboard_proc),
        Some(module.into()),
        0,
    ) {
        Ok(hook) => {
            state.keyboard_hook = Some(hook);
            logger::info("Dictation keyboard capture enabled");
        }
        Err(error) => {
            KEYBOARD_HOOK_STATE.with(|hook_state| *hook_state.borrow_mut() = None);
            let message = format!("Could not install dictation keyboard capture: {error}");
            logger::info(&message);
            let _ = state
                .event_tx
                .send(Win32Event::KeyboardCaptureFailed(message));
        }
    }
}

unsafe fn finish_keyboard_capture_if_drained(state: &mut ServiceState) {
    if state.keyboard_hook.is_none() {
        return;
    }
    let drained = KEYBOARD_HOOK_STATE.with(|hook_state| {
        hook_state.borrow().as_ref().is_none_or(|hook_state| {
            !hook_state.accepting && hook_state.captured.iter().all(|captured| !captured)
        })
    });
    if !drained {
        return;
    }
    force_remove_keyboard_capture(state);
}

unsafe fn force_remove_keyboard_capture(state: &mut ServiceState) {
    KEYBOARD_HOOK_STATE.with(|hook_state| {
        if let Some(hook_state) = hook_state.borrow_mut().as_mut() {
            unsafe { hook_state.clear_dead_key_state() };
        }
    });
    if let Some(hook) = state.keyboard_hook.take()
        && let Err(error) = UnhookWindowsHookEx(hook)
    {
        logger::info(format!("Keyboard capture hook removal failed: {error:#}"));
    }
    KEYBOARD_HOOK_STATE.with(|hook_state| *hook_state.borrow_mut() = None);
    logger::info("Dictation keyboard capture disabled");
}

unsafe extern "system" fn dictation_keyboard_proc(
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
    if !is_down && !is_up {
        return CallNextHookEx(None, code, wparam, lparam);
    }
    let Some(key) = (lparam.0 as *const KBDLLHOOKSTRUCT).as_ref() else {
        return CallNextHookEx(None, code, wparam, lparam);
    };
    if key.flags.contains(LLKHF_INJECTED) || key.dwExtraInfo == injector::ASHE_INJECTED_EXTRA_INFO {
        return CallNextHookEx(None, code, wparam, lparam);
    }

    let captured = KEYBOARD_HOOK_STATE.with(|hook_state| {
        hook_state
            .borrow_mut()
            .as_mut()
            .is_some_and(|state| unsafe { state.handle_key(key, is_down) })
    });
    if captured {
        LRESULT(1)
    } else {
        CallNextHookEx(None, code, wparam, lparam)
    }
}

impl KeyboardHookState {
    unsafe fn handle_key(&mut self, key: &KBDLLHOOKSTRUCT, is_down: bool) -> bool {
        let Some(index) = usize::try_from(key.vkCode)
            .ok()
            .filter(|index| *index < self.keyboard_state.len())
        else {
            return false;
        };
        let was_down = self.physical_down[index];
        self.update_key_state(key.vkCode, is_down, was_down);

        if !is_down {
            let captured = self.captured[index];
            self.captured[index] = false;
            return captured;
        }
        if !self.accepting {
            return self.captured[index];
        }

        let modifiers = ModifierState::from_keyboard_state(&self.keyboard_state);
        let action = classify_key(key.vkCode, modifiers, self.typing);
        let Some(action) = effective_action(action, self.cancel_only) else {
            return false;
        };
        match action {
            CaptureAction::Pass => false,
            CaptureAction::Backspace => {
                self.captured[index] = true;
                if self.dead_key_pending {
                    self.clear_dead_key_state();
                } else {
                    let _ = self.event_tx.send(Win32Event::BackspaceRequested);
                }
                true
            }
            CaptureAction::Paste => {
                self.captured[index] = true;
                if !was_down {
                    self.clear_dead_key_state();
                    self.start_typing();
                    let _ = self.event_tx.send(Win32Event::PasteTextRequested);
                }
                true
            }
            CaptureAction::Submit => {
                self.captured[index] = true;
                if !was_down {
                    self.clear_dead_key_state();
                    self.typing = false;
                    let _ = self.event_tx.send(Win32Event::SubmitRequested);
                }
                true
            }
            CaptureAction::LineBreak => {
                self.captured[index] = true;
                if !was_down {
                    self.clear_dead_key_state();
                    let _ = self.event_tx.send(Win32Event::LineBreakRequested);
                }
                true
            }
            CaptureAction::Cancel => {
                self.captured[index] = true;
                if !was_down {
                    self.clear_dead_key_state();
                    self.typing = false;
                    let _ = self.event_tx.send(Win32Event::CancelRequested);
                }
                true
            }
            CaptureAction::Text => match self.translate_text(key) {
                Translation::None => false,
                Translation::DeadKey => {
                    self.captured[index] = true;
                    self.dead_key_pending = true;
                    self.start_typing();
                    true
                }
                Translation::Text(text) => {
                    self.captured[index] = true;
                    self.dead_key_pending = false;
                    self.start_typing();
                    if !text.is_empty() {
                        let _ = self.event_tx.send(Win32Event::TextInput(text));
                    }
                    true
                }
            },
        }
    }

    fn start_typing(&mut self) {
        if !self.typing {
            self.typing = true;
            let _ = self.event_tx.send(Win32Event::TypingStarted);
        }
    }

    /// Drop hook-local typing residue after the app resumes listening
    /// without a submit, so further keys classify as listening input.
    unsafe fn end_typing(&mut self) {
        self.typing = false;
        unsafe { self.clear_dead_key_state() };
    }

    fn update_key_state(&mut self, vk: u32, is_down: bool, was_down: bool) {
        let index = vk as usize;
        if is_down {
            self.keyboard_state[index] |= 0x80;
            if !was_down && matches!(vk, 0x14 | 0x90 | 0x91) {
                self.keyboard_state[index] ^= 0x01;
            }
        } else {
            self.keyboard_state[index] &= 0x7f;
        }
        self.physical_down[index] = is_down;

        let sync_generic = |state: &mut [u8; 256], generic: u16, down: bool| {
            if down {
                state[generic as usize] |= 0x80;
            } else {
                state[generic as usize] &= 0x7f;
            }
        };
        match vk as u16 {
            value if value == VK_LSHIFT.0 || value == VK_RSHIFT.0 => sync_generic(
                &mut self.keyboard_state,
                VK_SHIFT.0,
                self.physical_down[VK_LSHIFT.0 as usize]
                    || self.physical_down[VK_RSHIFT.0 as usize],
            ),
            value if value == VK_LCONTROL.0 || value == VK_RCONTROL.0 => sync_generic(
                &mut self.keyboard_state,
                VK_CONTROL.0,
                self.physical_down[VK_LCONTROL.0 as usize]
                    || self.physical_down[VK_RCONTROL.0 as usize],
            ),
            value if value == VK_LMENU.0 || value == VK_RMENU.0 => sync_generic(
                &mut self.keyboard_state,
                VK_MENU.0,
                self.physical_down[VK_LMENU.0 as usize] || self.physical_down[VK_RMENU.0 as usize],
            ),
            _ => {}
        }
    }

    unsafe fn translate_text(&self, key: &KBDLLHOOKSTRUCT) -> Translation {
        let foreground = GetForegroundWindow();
        let thread_id = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let layout = GetKeyboardLayout(thread_id);
        let mut output = [0_u16; 8];
        let count = ToUnicodeEx(
            key.vkCode,
            key.scanCode,
            &self.keyboard_state,
            &mut output,
            0,
            Some(layout),
        );
        if count < 0 {
            return Translation::DeadKey;
        }
        if count == 0 {
            return Translation::None;
        }
        let text: String = String::from_utf16_lossy(&output[..count as usize])
            .chars()
            .filter(|character| !character.is_control())
            .collect();
        if text.is_empty() {
            Translation::None
        } else {
            Translation::Text(text)
        }
    }

    unsafe fn clear_dead_key_state(&mut self) {
        if !self.dead_key_pending {
            return;
        }
        let foreground = GetForegroundWindow();
        let thread_id = if foreground.0.is_null() {
            0
        } else {
            GetWindowThreadProcessId(foreground, None)
        };
        let layout = GetKeyboardLayout(thread_id);
        let mut output = [0_u16; 8];
        let neutral_state = [0_u8; 256];
        for _ in 0..4 {
            if ToUnicodeEx(0x20, 0, &neutral_state, &mut output, 0, Some(layout)) >= 0 {
                break;
            }
        }
        self.dead_key_pending = false;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ModifierState {
    shift: bool,
    control: bool,
    alt: bool,
    right_alt: bool,
    windows: bool,
}

impl ModifierState {
    fn from_keyboard_state(state: &[u8; 256]) -> Self {
        let down = |key: u16| state[key as usize] & 0x80 != 0;
        Self {
            shift: down(VK_SHIFT.0),
            control: down(VK_CONTROL.0),
            alt: down(VK_MENU.0),
            right_alt: down(VK_RMENU.0),
            windows: down(VK_LWIN.0) || down(VK_RWIN.0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureAction {
    Pass,
    Text,
    Paste,
    Backspace,
    Submit,
    LineBreak,
    Cancel,
}

fn classify_key(vk: u32, modifiers: ModifierState, typing: bool) -> CaptureAction {
    if vk == VK_ESCAPE.0 as u32 && !modifiers.control && !modifiers.alt && !modifiers.windows {
        return CaptureAction::Cancel;
    }
    if vk == VK_RETURN.0 as u32 && !modifiers.control && !modifiers.alt && !modifiers.windows {
        return if modifiers.shift {
            CaptureAction::LineBreak
        } else {
            CaptureAction::Submit
        };
    }
    if vk == VK_BACK.0 as u32
        && typing
        && !modifiers.control
        && !modifiers.alt
        && !modifiers.windows
    {
        return CaptureAction::Backspace;
    }
    if vk == 'V' as u32 && modifiers.control && !modifiers.alt && !modifiers.windows {
        return CaptureAction::Paste;
    }

    let alt_gr = modifiers.right_alt && !modifiers.windows;
    if is_text_candidate(vk)
        && ((!modifiers.control && !modifiers.alt && !modifiers.windows) || alt_gr)
    {
        CaptureAction::Text
    } else {
        CaptureAction::Pass
    }
}

/// Narrow a classified key for escape-only capture. Text actions run while
/// the user keeps typing in the foreground app, so only the cancel key may
/// be intercepted; everything else passes through untouched.
fn effective_action(action: CaptureAction, cancel_only: bool) -> Option<CaptureAction> {
    if cancel_only && !matches!(action, CaptureAction::Cancel) {
        None
    } else {
        Some(action)
    }
}

fn is_text_candidate(vk: u32) -> bool {
    vk == 0x20
        || (0x30..=0x5a).contains(&vk)
        || (0x60..=0x6f).contains(&vk)
        || (0xba..=0xc0).contains(&vk)
        || (0xdb..=0xdf).contains(&vk)
        || vk == 0xe2
        || vk == 0xe7
}

enum Translation {
    None,
    DeadKey,
    Text(String),
}

unsafe fn register_hotkey(hwnd: HWND) {
    if let Err(err) = RegisterHotKey(
        Some(hwnd),
        TOGGLE_HOTKEY_ID,
        MOD_WIN | MOD_SHIFT | MOD_NOREPEAT,
        'H' as u32,
    ) {
        logger::info(format!("RegisterHotKey failed: {err:#}"));
        message_box(
            hwnd,
            "Win+Shift+H could not be registered. Another app may already be using it.",
            "Ashe Worker",
        );
    }
    if let Err(err) = RegisterHotKey(
        Some(hwnd),
        FIX_GRAMMAR_HOTKEY_ID,
        MOD_WIN | MOD_SHIFT | MOD_NOREPEAT,
        'G' as u32,
    ) {
        logger::info(format!("RegisterHotKey (fix grammar) failed: {err:#}"));
        message_box(
            hwnd,
            "Win+Shift+G could not be registered. Another app may already be using it.",
            "Ashe Worker",
        );
    }
    if let Err(err) = RegisterHotKey(
        Some(hwnd),
        ANSWER_QUESTION_HOTKEY_ID,
        MOD_WIN | MOD_SHIFT | MOD_NOREPEAT,
        'Q' as u32,
    ) {
        logger::info(format!("RegisterHotKey (answer question) failed: {err:#}"));
        message_box(
            hwnd,
            "Win+Shift+Q could not be registered. Another app may already be using it.",
            "Ashe Worker",
        );
    }
    if let Err(error) = RegisterHotKey(
        Some(hwnd),
        PASTE_IMAGE_HOTKEY_ID,
        MOD_CONTROL | MOD_ALT | MOD_NOREPEAT,
        'V' as u32,
    ) {
        logger::info(format!(
            "RegisterHotKey (paste image Ctrl+Alt+V) failed: {error:#}"
        ));
    } else {
        logger::info("Clipboard image hotkey registered as Ctrl+Alt+V");
    }
    if let Err(error) = RegisterHotKey(
        Some(hwnd),
        VOICE_HOTKEY_ID,
        MOD_WIN | MOD_SHIFT | MOD_NOREPEAT,
        'A' as u32,
    ) {
        logger::info(format!("RegisterHotKey (voice Win+Shift+A) failed: {error:#}"));
    }
}

unsafe fn active_input_position() -> (i32, i32) {
    let mut cursor = POINT::default();
    if GetCursorPos(&mut cursor).is_err() {
        return (120, 120);
    }

    let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST);
    let scale = monitor_scale_factor(monitor);
    let overlay_width = pill_renderer::WIDTH * scale;
    let pill_height = pill_renderer::PILL_HEIGHT * scale;
    let top_extent = pill_renderer::MAIN_TOP * scale;
    let gap = CURSOR_OVERLAY_GAP as f32 * scale;
    let cursor_x = cursor.x as f32;
    let cursor_y = cursor.y as f32;
    let mut x = cursor_x + gap;
    let mut y = cursor_y + gap;

    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if GetMonitorInfoW(monitor, &mut info).as_bool() {
        let work_left = info.rcWork.left as f32;
        let work_top = info.rcWork.top as f32;
        let work_right = info.rcWork.right as f32;
        let work_bottom = info.rcWork.bottom as f32;
        if x + overlay_width > work_right {
            x = cursor_x - overlay_width - gap;
        }
        if y + pill_height > work_bottom {
            y = cursor_y - pill_height - gap;
        }
        x = clamp_to_work_area(x, work_left, work_right - overlay_width);
        y = clamp_to_work_area(y, work_top + top_extent, work_bottom - pill_height);
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

unsafe fn set_window_icons(hwnd: HWND) {
    let big_icon = load_app_icon(GetSystemMetrics(SM_CXICON), GetSystemMetrics(SM_CYICON));
    let small_icon = load_app_icon(GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON));
    let _ = SendMessageW(
        hwnd,
        WM_SETICON,
        Some(WPARAM(ICON_BIG as usize)),
        Some(LPARAM(big_icon.0 as isize)),
    );
    let _ = SendMessageW(
        hwnd,
        WM_SETICON,
        Some(WPARAM(ICON_SMALL as usize)),
        Some(LPARAM(small_icon.0 as isize)),
    );
}

unsafe fn load_app_icon(width: i32, height: i32) -> HICON {
    if let Some(icon) = load_app_icon_from_resource(width, height) {
        return icon;
    }

    if let Some(path) = find_app_icon_path() {
        let wide_path = wide(&path.to_string_lossy());
        match LoadImageW(
            None,
            pcwstr(&wide_path),
            IMAGE_ICON,
            width,
            height,
            LR_LOADFROMFILE,
        ) {
            Ok(handle) => {
                logger::info(format!(
                    "Loaded app icon path={} width={width} height={height}",
                    path.display()
                ));
                return HICON(handle.0);
            }
            Err(err) => logger::info(format!(
                "Load app icon failed path={} width={width} height={height}: {err:#}",
                path.display()
            )),
        }
    } else {
        logger::info("App icon file not found; falling back to default Windows icon");
    }

    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
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
    let group_size = SizeofResource(Some(module), group) as usize;
    if group_ptr.is_null() || group_size == 0 {
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
    match CreateIconFromResourceEx(bits, true, 0x0003_0000, width, height, LR_DEFAULTCOLOR) {
        Ok(icon) => {
            logger::info(format!(
                "Loaded embedded app icon resource id={APP_ICON_RESOURCE_ID} width={width} height={height}"
            ));
            Some(icon)
        }
        Err(err) => {
            logger::info(format!(
                "Load embedded app icon failed id={APP_ICON_RESOURCE_ID} width={width} height={height}: {err:#}"
            ));
            None
        }
    }
}

fn int_resource(id: u16) -> windows::core::PCWSTR {
    windows::core::PCWSTR(id as usize as *const u16)
}

fn find_app_icon_path() -> Option<PathBuf> {
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(Path::to_path_buf));
    let current_dir = std::env::current_dir().ok();
    let manifest_dir = option_env!("CARGO_MANIFEST_DIR").map(PathBuf::from);

    let candidates = [
        exe_dir.as_ref().map(|dir| dir.join(ICON_FILE_NAME)),
        exe_dir.as_ref().map(|dir| dir.join(ICON_DATA_PATH)),
        current_dir.as_ref().map(|dir| dir.join(ICON_DATA_PATH)),
        manifest_dir.as_ref().map(|dir| dir.join(ICON_DATA_PATH)),
    ];

    candidates.into_iter().flatten().find(|path| path.exists())
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
    for (idx, value) in wide(tooltip)
        .iter()
        .copied()
        .take(data.szTip.len())
        .enumerate()
    {
        data.szTip[idx] = value;
    }
    data
}

fn show_tray_menu(hwnd: HWND, active: bool, activity_running: bool, activity_status: &str) {
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        let label = if active {
            "Stop dictation"
        } else {
            "Start dictation"
        };
        let _ = AppendMenuW(menu, MF_STRING, MENU_TOGGLE, pcwstr(&wide(label)));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let activity_label = if activity_running {
            "Pause activity tracking"
        } else {
            "Resume activity tracking"
        };
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_TOGGLE_ACTIVITY,
            pcwstr(&wide(activity_label)),
        );
        let status = format!("Activity: {activity_status}");
        let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, pcwstr(&wide(&status)));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN_ARTIFACTS,
            pcwstr(&wide("Open artifacts folder")),
        );
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_OPEN_JOURNAL,
            pcwstr(&wide("Open today's journal")),
        );
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_RELOAD_CONFIG,
            pcwstr(&wide("Reload config")),
        );
        let _ = AppendMenuW(menu, MF_STRING, MENU_OPEN_LOG, pcwstr(&wide("Open log")));
        let _ = AppendMenuW(
            menu,
            MF_STRING,
            MENU_COPY_LOG_PATH,
            pcwstr(&wide("Copy log path")),
        );
        let _ = AppendMenuW(menu, MF_STRING, MENU_ABOUT, pcwstr(&wide("About")));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
        let _ = AppendMenuW(menu, MF_STRING, MENU_QUIT, pcwstr(&wide("Quit")));
        let mut point = POINT::default();
        let _ = GetCursorPos(&mut point);
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
    }
}

fn message_box(hwnd: HWND, text: &str, title: &str) {
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
mod tests {
    use super::{
        CaptureAction, KeyboardHookState, ModifierState, Win32Event, classify_key,
        effective_action,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_BACK, VK_ESCAPE, VK_LSHIFT, VK_RETURN};
    use windows::Win32::UI::WindowsAndMessaging::KBDLLHOOKSTRUCT;

    fn modifiers() -> ModifierState {
        ModifierState {
            shift: false,
            control: false,
            alt: false,
            right_alt: false,
            windows: false,
        }
    }

    #[test]
    fn printable_keys_and_paste_are_captured_without_blocking_shortcuts() {
        assert_eq!(
            classify_key('A' as u32, modifiers(), false),
            CaptureAction::Text
        );
        assert_eq!(
            classify_key(
                'V' as u32,
                ModifierState {
                    control: true,
                    ..modifiers()
                },
                false,
            ),
            CaptureAction::Paste
        );
        for (vk, state) in [
            (
                'C' as u32,
                ModifierState {
                    control: true,
                    ..modifiers()
                },
            ),
            (
                'A' as u32,
                ModifierState {
                    alt: true,
                    ..modifiers()
                },
            ),
            (
                'D' as u32,
                ModifierState {
                    windows: true,
                    ..modifiers()
                },
            ),
        ] {
            assert_eq!(classify_key(vk, state, true), CaptureAction::Pass);
        }
    }

    #[test]
    fn editing_and_session_keys_are_scoped_to_dictation() {
        assert_eq!(
            classify_key(VK_BACK.0 as u32, modifiers(), false),
            CaptureAction::Pass
        );
        assert_eq!(
            classify_key(VK_BACK.0 as u32, modifiers(), true),
            CaptureAction::Backspace
        );
        assert_eq!(
            classify_key(VK_RETURN.0 as u32, modifiers(), false),
            CaptureAction::Submit
        );
        assert_eq!(
            classify_key(
                VK_RETURN.0 as u32,
                ModifierState {
                    shift: true,
                    ..modifiers()
                },
                false,
            ),
            CaptureAction::LineBreak
        );
        assert_eq!(
            classify_key(VK_ESCAPE.0 as u32, modifiers(), false),
            CaptureAction::Cancel
        );
    }

    #[test]
    fn escape_only_capture_forwards_everything_but_cancel() {
        assert_eq!(
            effective_action(CaptureAction::Cancel, true),
            Some(CaptureAction::Cancel)
        );
        for action in [
            CaptureAction::Text,
            CaptureAction::Paste,
            CaptureAction::Backspace,
            CaptureAction::Submit,
            CaptureAction::LineBreak,
            CaptureAction::Pass,
        ] {
            assert_eq!(effective_action(action, true), None);
        }
        for action in [
            CaptureAction::Text,
            CaptureAction::Paste,
            CaptureAction::Backspace,
            CaptureAction::Submit,
            CaptureAction::LineBreak,
            CaptureAction::Pass,
            CaptureAction::Cancel,
        ] {
            assert_eq!(effective_action(action, false), Some(action));
        }
    }

    #[test]
    fn enter_auto_repeat_emits_only_one_submit() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut state = KeyboardHookState {
            event_tx,
            keyboard_state: [0; 256],
            physical_down: [false; 256],
            captured: [false; 256],
            accepting: true,
            cancel_only: false,
            typing: true,
            dead_key_pending: false,
        };
        let key = KBDLLHOOKSTRUCT {
            vkCode: VK_RETURN.0 as u32,
            ..Default::default()
        };
        assert!(unsafe { state.handle_key(&key, true) });
        state.accepting = false;
        assert!(unsafe { state.handle_key(&key, true) });
        assert!(unsafe { state.handle_key(&key, false) });
        assert!(state.captured.iter().all(|captured| !captured));
        assert!(matches!(
            event_rx.try_recv(),
            Ok(Win32Event::SubmitRequested)
        ));
        assert!(event_rx.try_recv().is_err());
    }

    #[test]
    fn end_typing_releases_backspace_to_listening() {
        let (event_tx, _event_rx) = crossbeam_channel::unbounded();
        let mut state = KeyboardHookState {
            event_tx,
            keyboard_state: [0; 256],
            physical_down: [false; 256],
            captured: [false; 256],
            accepting: true,
            cancel_only: false,
            typing: true,
            dead_key_pending: false,
        };
        unsafe { state.end_typing() };
        assert!(!state.typing);
        assert!(state.accepting);
        let backspace = KBDLLHOOKSTRUCT {
            vkCode: VK_BACK.0 as u32,
            ..Default::default()
        };
        assert!(!unsafe { state.handle_key(&backspace, true) });
    }

    #[test]
    fn shift_enter_auto_repeat_emits_only_one_line_break() {
        let (event_tx, event_rx) = crossbeam_channel::unbounded();
        let mut state = KeyboardHookState {
            event_tx,
            keyboard_state: [0; 256],
            physical_down: [false; 256],
            captured: [false; 256],
            accepting: true,
            cancel_only: false,
            typing: false,
            dead_key_pending: false,
        };
        let shift = KBDLLHOOKSTRUCT {
            vkCode: VK_LSHIFT.0 as u32,
            ..Default::default()
        };
        let enter = KBDLLHOOKSTRUCT {
            vkCode: VK_RETURN.0 as u32,
            ..Default::default()
        };
        assert!(!unsafe { state.handle_key(&shift, true) });
        assert!(unsafe { state.handle_key(&enter, true) });
        assert!(unsafe { state.handle_key(&enter, true) });
        assert!(unsafe { state.handle_key(&enter, false) });
        assert!(matches!(
            event_rx.try_recv(),
            Ok(Win32Event::LineBreakRequested)
        ));
        assert!(event_rx.try_recv().is_err());
    }
}
