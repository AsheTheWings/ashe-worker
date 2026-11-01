use crate::logger;
use crate::util::wide;
use anyhow::{Context, Result, anyhow};
use std::mem::size_of;
use std::thread;
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{HANDLE, HGLOBAL, HWND};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    SetClipboardData,
};
use windows::Win32::System::Memory::{
    GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock,
};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput,
    VK_CONTROL, VK_LWIN, VK_MENU, VK_RIGHT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

const CLIPBOARD_RETRIES: usize = 12;
const CLIPBOARD_RETRY_DELAY: Duration = Duration::from_millis(25);
const COPY_SETTLE_DELAY: Duration = Duration::from_millis(120);
const PASTE_SETTLE_DELAY: Duration = Duration::from_millis(180);
const MODIFIER_RELEASE_TIMEOUT: Duration = Duration::from_millis(1000);
const MODIFIER_POLL_INTERVAL: Duration = Duration::from_millis(10);
const MAX_CLIPBOARD_TEXT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const ASHE_INJECTED_EXTRA_INFO: usize = 0x4153_4845_574b_5252;

pub fn capture_selected_text() -> Result<Option<String>> {
    // Selection capture is triggered by a global hotkey (e.g. Win+Shift+G) that fires on
    // key-down while the user is still physically holding Win+Shift. If we synthesize
    // Ctrl+C now, the OS sees a polluted chord (Win+Shift+Ctrl+C) that does not copy, so
    // the capture silently fails. Wait for the modifiers to be released first.
    wait_for_modifiers_released();
    let previous_text = read_clipboard_text().ok().flatten();
    set_clipboard_text("").context("failed to clear clipboard before selection capture")?;
    send_ctrl_c().context("failed to send Ctrl+C")?;
    thread::sleep(COPY_SETTLE_DELAY);
    let selected_text = read_clipboard_text()
        .context("failed to read clipboard after selection capture")?
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());

    match previous_text {
        Some(previous_text) => {
            if let Err(err) = set_clipboard_text(&previous_text) {
                logger::info(format!(
                    "Clipboard restore failed after selection capture: {err:#}"
                ));
            }
        }
        None => {
            if let Err(err) = set_clipboard_text("") {
                logger::info(format!(
                    "Clipboard clear failed after selection capture: {err:#}"
                ));
            }
        }
    }

    if let Some(text) = selected_text.as_ref() {
        logger::info(format!("Captured selected context chars={}", text.len()));
    }
    Ok(selected_text)
}

fn paste_text(text: &str) -> Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    set_clipboard_text(text).context("failed to set clipboard text")?;
    logger::info(format!(
        "Clipboard injection chars={}",
        text.chars().count()
    ));
    wait_for_modifiers_released();
    send_ctrl_v().context("failed to send Ctrl+V")?;
    thread::sleep(PASTE_SETTLE_DELAY);
    Ok(())
}

/// Refocus `hwnd` and inject `text`. When `append_after_selection` is set, the current
/// selection is first collapsed to its right edge (via the Right arrow key) so the text
/// lands *after* the selection instead of replacing it; otherwise the paste overwrites
/// the active selection.
pub fn inject_text_to(hwnd: HWND, text: &str, append_after_selection: bool) -> Result<()> {
    unsafe {
        if !hwnd.0.is_null() {
            let _ = SetForegroundWindow(hwnd);
            thread::sleep(Duration::from_millis(90));
        }
    }
    if append_after_selection {
        send_key(VK_RIGHT.0).context("failed to collapse selection")?;
        thread::sleep(Duration::from_millis(30));
    }
    paste_text(text)
}

fn read_clipboard_text() -> Result<Option<String>> {
    let _guard = ClipboardGuard::open()?;
    unsafe {
        if IsClipboardFormatAvailable(CF_UNICODETEXT.0 as u32).is_err() {
            return Ok(None);
        }

        let handle = GetClipboardData(CF_UNICODETEXT.0 as u32)?;
        let global = HGLOBAL(handle.0);
        let byte_size = GlobalSize(global);
        if byte_size == 0 {
            return Ok(None);
        }
        if byte_size > MAX_CLIPBOARD_TEXT_BYTES {
            return Err(anyhow!("clipboard text exceeds the capture limit"));
        }
        let size = byte_size / size_of::<u16>();

        let ptr = GlobalLock(global) as *const u16;
        if ptr.is_null() {
            return Err(anyhow!("GlobalLock failed while reading clipboard"));
        }

        let slice = std::slice::from_raw_parts(ptr, size);
        let end = slice
            .iter()
            .position(|value| *value == 0)
            .unwrap_or(slice.len());
        let text = String::from_utf16_lossy(&slice[..end]);
        let _ = GlobalUnlock(global);
        Ok(Some(text))
    }
}

fn set_clipboard_text(text: &str) -> Result<()> {
    let _guard = ClipboardGuard::open()?;
    unsafe {
        EmptyClipboard()?;
        let wide_text = wide(text);
        let bytes = wide_text.len() * size_of::<u16>();
        let memory = GlobalAlloc(GMEM_MOVEABLE, bytes)?;
        let ptr = GlobalLock(memory) as *mut u16;
        if ptr.is_null() {
            return Err(anyhow!("GlobalLock failed while setting clipboard"));
        }

        ptr.copy_from_nonoverlapping(wide_text.as_ptr(), wide_text.len());
        let _ = GlobalUnlock(memory);
        SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.0)))?;
        Ok(())
    }
}

struct ClipboardGuard;

impl ClipboardGuard {
    fn open() -> Result<Self> {
        for attempt in 1..=CLIPBOARD_RETRIES {
            unsafe {
                if OpenClipboard(None).is_ok() {
                    return Ok(Self);
                }
            }
            logger::info(format!(
                "Clipboard open retry {attempt}/{CLIPBOARD_RETRIES}"
            ));
            thread::sleep(CLIPBOARD_RETRY_DELAY);
        }
        Err(anyhow!("clipboard is busy"))
    }
}

impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}

fn send_ctrl_c() -> Result<()> {
    send_ctrl_key('C' as u16)
}

/// Poll the async key state until the hotkey modifier keys (Win/Shift/Ctrl/Alt) are
/// physically released, or a short timeout elapses. This prevents synthesized keystrokes
/// from combining with still-held modifiers into an unintended chord.
fn wait_for_modifiers_released() {
    let modifiers = [VK_LWIN, VK_RWIN, VK_SHIFT, VK_CONTROL, VK_MENU];
    let start = Instant::now();
    loop {
        let any_down = modifiers
            .iter()
            .any(|vk| unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 });
        if !any_down {
            return;
        }
        if start.elapsed() >= MODIFIER_RELEASE_TIMEOUT {
            logger::info("Modifier keys still held after timeout; proceeding with capture");
            return;
        }
        thread::sleep(MODIFIER_POLL_INTERVAL);
    }
}
fn send_ctrl_v() -> Result<()> {
    send_ctrl_key('V' as u16)
}

fn send_key(vk: u16) -> Result<()> {
    unsafe {
        let inputs = [key_input(vk, false), key_input(vk, true)];
        let sent = SendInput(&inputs, size_of::<INPUT>() as i32);
        if sent != inputs.len() as u32 {
            return Err(anyhow!("SendInput sent {sent}/{} events", inputs.len()));
        }
    }
    Ok(())
}

fn send_ctrl_key(key: u16) -> Result<()> {
    unsafe {
        let inputs = [
            key_input(VK_CONTROL.0, false),
            key_input(key, false),
            key_input(key, true),
            key_input(VK_CONTROL.0, true),
        ];
        let sent = SendInput(&inputs, size_of::<INPUT>() as i32);
        if sent != inputs.len() as u32 {
            return Err(anyhow!("SendInput sent {sent}/{} events", inputs.len()));
        }
    }
    Ok(())
}

fn key_input(vk: u16, up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: windows::Win32::UI::Input::KeyboardAndMouse::VIRTUAL_KEY(vk),
                wScan: 0,
                dwFlags: if up {
                    KEYEVENTF_KEYUP
                } else {
                    Default::default()
                },
                time: 0,
                dwExtraInfo: ASHE_INJECTED_EXTRA_INFO,
            },
        },
    }
}
