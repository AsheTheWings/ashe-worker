use crate::logger;
use iced::widget::Space;
use iced::window;
use iced::{Element, Length, Point, Size, Task};

pub const TITLE: &str = "Ashe Worker";
const HELPER_WINDOW_SIZE: f32 = 1.0;

pub fn window_settings() -> window::Settings {
    logger::info(format!(
        "Iced helper window settings title={TITLE} visible=false"
    ));
    window::Settings {
        size: Size::new(HELPER_WINDOW_SIZE, HELPER_WINDOW_SIZE),
        position: window::Position::Specific(Point::new(120.0, 120.0)),
        visible: false,
        resizable: false,
        decorations: false,
        transparent: false,
        // The native per-pixel-alpha overlay is owned by the Win32 service
        // thread. Iced remains a hidden event-loop host.
        exit_on_close_request: false,
        platform_specific: platform_specific_settings(),
        ..Default::default()
    }
}

#[cfg(target_os = "windows")]
fn platform_specific_settings() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific {
        skip_taskbar: true,
        ..Default::default()
    }
}

#[cfg(not(target_os = "windows"))]
fn platform_specific_settings() -> window::settings::PlatformSpecific {
    window::settings::PlatformSpecific::default()
}

/// The visible overlay is native on Windows. This empty Iced view only keeps
/// the application event loop alive without presenting a second surface.
pub fn view<'a, Message: 'a>() -> Element<'a, Message> {
    Space::new().width(Length::Fill).height(Length::Fill).into()
}

pub fn keep_helper_hidden<Message: 'static>(id: window::Id) -> Task<Message> {
    window::set_mode(id, window::Mode::Hidden)
}

#[cfg(test)]
mod unit {
    #[test]
    fn overlay_close_request_does_not_exit_tray_application() {
        assert!(!super::window_settings().exit_on_close_request);
    }
}
