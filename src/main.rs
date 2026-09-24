#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
mod activity_pipeline;
mod activity_telemetry;
mod audio;
mod composition;
mod injector;
mod llm_client;
mod native_overlay;
mod overlay_text;
mod overlay_view;
mod paste_upload;
mod pill_renderer;
mod screen_capture;
mod spectrum;
mod spoken_punctuation;
mod stt;
mod observability;
mod ui_app;
mod util;
mod voice;
mod voice_audio;
mod win32_service;

use anyhow::Result;
pub use ashe_worker::{archive, artifact_store, block_artifact, config, daily_report, logger};
use std::process::ExitCode;
use ui_app::UiApp;
use windows::Win32::UI::HiDpi::{
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_ID: &str = env!("ASHE_BUILD_ID");

fn main() -> ExitCode {
    logger::init();
    config::load_env_file_near_exe();
    let telemetry = observability::Telemetry::initialize(APP_VERSION, BUILD_ID);
    install_panic_logger();
    let result = match run() {
        Ok(()) => {
            logger::info("Ashe Worker runtime exited cleanly");
            ExitCode::SUCCESS
        }
        Err(error) => {
            logger::info(format!("Ashe Worker runtime failed: {error:#}"));
            ExitCode::FAILURE
        }
    };
    telemetry.shutdown();
    result
}

fn run() -> Result<()> {
    init_process_dpi_awareness();
    init_rustls_crypto_provider();
    logger::info(format!("Build version={APP_VERSION} build_id={BUILD_ID}"));
    logger::info(format!("Log path: {}", logger::log_path().display()));
    logger::info("Launching Iced UI on main thread");
    iced::application(UiApp::new, UiApp::update, UiApp::view)
        .title(app_title)
        .subscription(UiApp::subscription)
        .window(overlay_view::window_settings())
        .run()
        .map_err(|err| anyhow::anyhow!("Iced runtime failed: {err:#}"))
}

fn install_panic_logger() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic| {
        logger::info(format!("Unhandled panic: {panic}"));
        previous(panic);
    }));
}

fn app_title(_app: &UiApp) -> String {
    overlay_view::TITLE.to_string()
}

fn init_process_dpi_awareness() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

fn init_rustls_crypto_provider() {
    match rustls::crypto::aws_lc_rs::default_provider().install_default() {
        Ok(()) => logger::info("Rustls crypto provider initialized: aws-lc-rs"),
        Err(_) => logger::info("Rustls crypto provider was already initialized"),
    }
}
