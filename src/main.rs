#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

#[cfg(windows)]
mod activity_pipeline;
#[cfg(windows)]
mod activity_telemetry;
#[cfg(windows)]
mod injector;
#[cfg(windows)]
mod llm_client;
#[cfg(windows)]
mod native_overlay;
#[cfg(windows)]
mod observability;
#[cfg(windows)]
mod overlay_text;
#[cfg(windows)]
mod overlay_view;
#[cfg(windows)]
mod pill_renderer;
#[cfg(windows)]
mod screen_capture;
#[cfg(windows)]
mod ui_app;
#[cfg(windows)]
mod util;
#[cfg(windows)]
mod win32_service;

pub use ashe_worker::{artifact_store, block_artifact, config, daily_report, logger, records};
use std::process::ExitCode;

#[cfg(windows)]
const REVISION: &str = env!("ASHE_WORKER_REVISION");

#[cfg(not(windows))]
fn main() -> ExitCode {
    eprintln!("ashe-worker runs on Windows only");
    ExitCode::FAILURE
}

/// Command-line modes besides running the tray application:
/// `--version` prints the revision, `--check-inputs` validates the inputs and
/// prints the result, and `--qualify-observability` emits one qualification
/// trace, metric, and log record.
#[cfg(windows)]
fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let command_line_mode = arguments.iter().any(|argument| {
        matches!(
            argument.as_str(),
            "--version" | "--check-inputs" | "--qualify-observability"
        )
    });
    if command_line_mode {
        attach_parent_console();
    }
    if arguments.iter().any(|argument| argument == "--version") {
        println!("ashe-worker {REVISION}");
        return ExitCode::SUCCESS;
    }
    let check_only = arguments
        .iter()
        .any(|argument| argument == "--check-inputs");
    let config = match config::AppConfig::load() {
        Ok(config) => config,
        Err(error) => {
            if check_only {
                println!("{error}");
            } else {
                startup_failure(&format!(
                    "Ashe Worker cannot start.\r\n\r\n{}",
                    error.problems.join("\r\n")
                ));
            }
            return ExitCode::from(2);
        }
    };
    if check_only {
        println!("inputs: ok");
        return ExitCode::SUCCESS;
    }
    let telemetry = match observability::Telemetry::initialize(&config.telemetry, REVISION) {
        Ok(telemetry) => telemetry,
        Err(error) => {
            startup_failure(&format!("Ashe Worker cannot start telemetry: {error}"));
            return ExitCode::FAILURE;
        }
    };
    if arguments
        .iter()
        .any(|argument| argument == "--qualify-observability")
    {
        telemetry.emit_qualification();
        telemetry.shutdown();
        return ExitCode::SUCCESS;
    }
    std::panic::set_hook(Box::new(|_| logger::info("Unhandled panic")));
    init_process_dpi_awareness();
    let result = iced::application(
        move || ui_app::UiApp::new(config.clone()),
        ui_app::UiApp::update,
        ui_app::UiApp::view,
    )
    .title(|_: &ui_app::UiApp| overlay_view::TITLE.to_string())
    .subscription(ui_app::UiApp::subscription)
    .window(overlay_view::window_settings())
    .run();
    let code = match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(_) => {
            logger::info("Ashe Worker runtime failed");
            ExitCode::FAILURE
        }
    };
    telemetry.shutdown();
    code
}

#[cfg(windows)]
fn startup_failure(text: &str) {
    win32_service::message_box(
        windows::Win32::Foundation::HWND::default(),
        text,
        "Ashe Worker",
    );
}

/// Give a command-line mode the parent's console unless its output is
/// already redirected, since the release binary has no console of its own.
#[cfg(windows)]
fn attach_parent_console() {
    use windows::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_OUTPUT_HANDLE,
    };
    unsafe {
        let redirected = GetStdHandle(STD_OUTPUT_HANDLE)
            .is_ok_and(|handle| !handle.is_invalid() && !handle.0.is_null());
        if !redirected {
            let _ = AttachConsole(ATTACH_PARENT_PROCESS);
        }
    }
}

#[cfg(windows)]
fn init_process_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}
