use crate::activity_pipeline::ActivityHandle;
use crate::config::AppConfig;
use crate::injector;
use crate::llm_client;
use crate::logger;
use crate::overlay_view;
use crate::records::RecordsHandle;
use crate::win32_service::{self, Win32Command, Win32Event};
use crossbeam_channel::{Receiver, Sender};
use iced::{Element, Point, Subscription, Task, window};
use std::thread::JoinHandle;
use std::time::Duration;

const REVISION: &str = env!("ASHE_WORKER_REVISION");
const OVERLAY_TICK_MS: u64 = 16;
const OVERLAY_SMOOTHING: f32 = 0.28;
const OVERLAY_SNAP_DISTANCE: f32 = 1.0;
const IDLE_TOOLTIP: &str = "Ashe Worker - Win+Shift+G grammar, Win+Shift+Q question";
type ActionResult = std::result::Result<String, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum TextActionKind {
    FixGrammar,
    AnswerQuestion,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// The request runs; Escape cancels it.
    Working(TextActionKind),
    /// The result is being injected; it can no longer be cancelled.
    Inserting,
}

pub struct UiApp {
    config: AppConfig,
    state: State,
    target_hwnd: isize,
    win32_tx: Sender<Win32Command>,
    win32_rx: Receiver<Win32Event>,
    _win32_thread: JoinHandle<()>,
    window_id: Option<window::Id>,
    position: Option<Point>,
    target_position: Option<Point>,
    activity: ActivityHandle,
    _records: RecordsHandle,
    last_activity_status: String,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    WindowReady(Option<window::Id>),
    WindowCloseRequested,
    TextActionCompleted(ActionResult),
}

impl UiApp {
    pub fn new(config: AppConfig) -> (Self, Task<Message>) {
        let (event_tx, win32_rx) = crossbeam_channel::unbounded();
        let (win32_tx, command_rx) = crossbeam_channel::unbounded();
        let win32_thread = win32_service::spawn(event_tx, command_rx);
        let activity = ActivityHandle::spawn(config.clone());
        let records = RecordsHandle::spawn(config.clone());
        let app = Self {
            config,
            state: State::Idle,
            target_hwnd: 0,
            win32_tx,
            win32_rx,
            _win32_thread: win32_thread,
            window_id: None,
            position: None,
            target_position: None,
            activity,
            _records: records,
            last_activity_status: String::new(),
        };
        app.send_win32(Win32Command::SetTooltip(IDLE_TOOLTIP.to_string()));
        (app, window::latest().map(Message::WindowReady))
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(OVERLAY_TICK_MS)).map(|_| Message::Tick),
            window::close_requests().map(|_| Message::WindowCloseRequested),
        ])
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowReady(id) => {
                self.window_id = id;
                self.apply_window_state()
            }
            Message::WindowCloseRequested => Task::none(),
            Message::Tick => self.pump(),
            Message::TextActionCompleted(result) => self.finish_text_action(result),
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        overlay_view::view()
    }

    fn pump(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        if self.window_id.is_none() {
            tasks.push(window::latest().map(Message::WindowReady));
        }
        while let Ok(event) = self.win32_rx.try_recv() {
            tasks.push(self.handle_win32_event(event));
        }
        let activity = self.activity.status();
        let activity_key = format!(
            "{}:{}:{}",
            activity.running, activity.current_frames, activity.summary
        );
        if activity_key != self.last_activity_status {
            self.last_activity_status = activity_key;
            self.send_win32(Win32Command::SetActivityStatus {
                running: activity.running,
                status: activity.summary,
            });
        }
        if self.overlay_visible() {
            self.advance_overlay_position();
            self.sync_overlay();
        }
        Task::batch(tasks)
    }

    fn handle_win32_event(&mut self, event: Win32Event) -> Task<Message> {
        match event {
            Win32Event::CancelRequested => self.cancel_text_action(),
            Win32Event::FixGrammarRequested { target_hwnd, x, y } => self.begin_text_action(
                TextActionKind::FixGrammar,
                target_hwnd,
                Point::new(x as f32, y as f32),
            ),
            Win32Event::AnswerQuestionRequested { target_hwnd, x, y } => self.begin_text_action(
                TextActionKind::AnswerQuestion,
                target_hwnd,
                Point::new(x as f32, y as f32),
            ),
            Win32Event::PositionChanged { x, y } => {
                self.target_position = Some(Point::new(x as f32, y as f32));
                Task::none()
            }
            Win32Event::ReloadConfigRequested => {
                self.reload_config();
                Task::none()
            }
            Win32Event::ToggleActivityRequested => {
                self.activity.toggle();
                Task::none()
            }
            Win32Event::OpenRecordsRequested => {
                self.send_win32(Win32Command::OpenPath(
                    self.activity.artifacts_dir().display().to_string(),
                ));
                Task::none()
            }
            Win32Event::OpenJournalRequested => {
                self.send_win32(Win32Command::OpenPath(
                    self.activity.today_journal().display().to_string(),
                ));
                Task::none()
            }
            Win32Event::AboutRequested => {
                self.show_about();
                Task::none()
            }
            Win32Event::QuitRequested => self.quit(),
            Win32Event::InjectionCompleted(result) => {
                match result {
                    Ok(()) => self.send_win32(Win32Command::SetTooltip(IDLE_TOOLTIP.to_string())),
                    Err(error) => {
                        logger::info(format!("Text injection failed: {error}"));
                        self.send_win32(Win32Command::SetTooltip(
                            "Ashe Worker - Paste failed".to_string(),
                        ));
                    }
                }
                self.finish()
            }
            Win32Event::ServiceStopped => {
                logger::info("Win32 service stopped event received");
                Task::none()
            }
        }
    }

    fn begin_text_action(
        &mut self,
        kind: TextActionKind,
        target_hwnd: isize,
        position: Point,
    ) -> Task<Message> {
        if self.state != State::Idle {
            logger::info("Text action ignored; not idle");
            return Task::none();
        }
        let selected = match injector::capture_selected_text() {
            Ok(Some(text)) => text,
            Ok(None) => {
                logger::info("Text action: no text selected");
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - No text selected".to_string(),
                ));
                return Task::none();
            }
            Err(err) => {
                logger::info(format!("Text action selection capture failed: {err:#}"));
                self.send_win32(Win32Command::ShowMessageBox {
                    title: "Ashe Worker".to_string(),
                    text: format!("Could not capture selected text: {err}"),
                });
                return Task::none();
            }
        };
        logger::info("Text action started");
        self.state = State::Working(kind);
        self.target_hwnd = target_hwnd;
        self.position = Some(position);
        self.target_position = Some(position);
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Working...".to_string(),
        ));
        self.send_win32(Win32Command::SetFollowCursor(true));
        // Only Escape is captured, so typing keeps reaching the foreground app.
        self.send_win32(Win32Command::SetEscapeCapture(true));
        let config = self.config.clone();
        let request = Task::perform(
            async move {
                match kind {
                    TextActionKind::FixGrammar => llm_client::fix_grammar(config, selected).await,
                    TextActionKind::AnswerQuestion => {
                        llm_client::answer_question(config, selected).await
                    }
                }
                .map_err(|err| format!("{err:#}"))
            },
            Message::TextActionCompleted,
        );
        Task::batch([self.apply_window_state(), request])
    }

    fn finish_text_action(&mut self, result: ActionResult) -> Task<Message> {
        // A cancelled request completes after the state moved on; drop its result.
        let State::Working(kind) = self.state else {
            return Task::none();
        };
        let text = match result {
            Ok(text) if !text.trim().is_empty() => text,
            Ok(_) => {
                logger::info("Text action returned an empty result");
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - Empty result".to_string(),
                ));
                return self.finish();
            }
            Err(error) => {
                logger::info(format!("Text action failed: {error}"));
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - LLM request failed".to_string(),
                ));
                return self.finish();
            }
        };
        logger::info("Text action completed");
        let append = kind == TextActionKind::AnswerQuestion;
        self.state = State::Inserting;
        self.send_win32(Win32Command::SetEscapeCapture(false));
        self.send_win32(Win32Command::InjectText {
            target_hwnd: self.target_hwnd,
            text: if append { format!("\n\n{text}") } else { text },
            append_after_selection: append,
        });
        Task::none()
    }

    fn cancel_text_action(&mut self) -> Task<Message> {
        if !matches!(self.state, State::Working(_)) {
            return Task::none();
        }
        logger::info("Text action cancelled");
        self.send_win32(Win32Command::SetTooltip(IDLE_TOOLTIP.to_string()));
        self.finish()
    }

    fn finish(&mut self) -> Task<Message> {
        self.state = State::Idle;
        self.target_hwnd = 0;
        self.send_win32(Win32Command::SetEscapeCapture(false));
        self.send_win32(Win32Command::SetFollowCursor(false));
        self.apply_window_state()
    }

    fn reload_config(&mut self) {
        if self.state != State::Idle {
            self.send_win32(Win32Command::ShowMessageBox {
                title: "Ashe Worker".to_string(),
                text: "Wait for the text action to finish before reloading the config.".to_string(),
            });
            return;
        }
        match AppConfig::load() {
            Ok(config) => {
                self.activity.shutdown();
                self.config = config;
                self.activity = ActivityHandle::spawn(self.config.clone());
                self._records = RecordsHandle::spawn(self.config.clone());
                logger::info("Config reloaded");
                self.send_win32(Win32Command::SetTooltip(IDLE_TOOLTIP.to_string()));
            }
            Err(error) => {
                logger::info("Config reload failed: invalid inputs");
                self.send_win32(Win32Command::ShowMessageBox {
                    title: "Ashe Worker".to_string(),
                    text: format!(
                        "The config was not reloaded.\r\n\r\n{}",
                        error.problems.join("\r\n")
                    ),
                });
            }
        }
    }

    fn show_about(&self) {
        self.send_win32(Win32Command::ShowMessageBox {
            title: "About Ashe Worker".to_string(),
            text: format!(
                "Ashe Worker\r\nRevision: {REVISION}\r\n\r\nGrammar: Win+Shift+G\r\nQuestion: Win+Shift+Q\r\nCancel: Esc\r\n\r\nActivity: {}\r\n{}",
                self.activity.status().summary,
                self.config.summary(),
            ),
        });
    }

    fn quit(&mut self) -> Task<Message> {
        logger::info("Quit requested");
        self.activity.shutdown();
        self.send_win32(Win32Command::Shutdown);
        match self.window_id {
            Some(id) => window::close(id),
            None => Task::none(),
        }
    }

    fn overlay_visible(&self) -> bool {
        matches!(self.state, State::Working(_))
    }

    fn advance_overlay_position(&mut self) {
        let Some(target) = self.target_position else {
            return;
        };
        let Some(current) = self.position else {
            self.position = Some(target);
            return;
        };
        let dx = target.x - current.x;
        let dy = target.y - current.y;
        self.position = Some(if dx.hypot(dy) <= OVERLAY_SNAP_DISTANCE {
            target
        } else {
            Point::new(
                current.x + dx * OVERLAY_SMOOTHING,
                current.y + dy * OVERLAY_SMOOTHING,
            )
        });
    }

    fn apply_window_state(&self) -> Task<Message> {
        self.sync_overlay();
        match self.window_id {
            Some(id) => overlay_view::keep_helper_hidden(id),
            None => Task::none(),
        }
    }

    fn sync_overlay(&self) {
        let position = self.position.unwrap_or(Point::new(120.0, 120.0));
        self.send_win32(Win32Command::UpdateOverlay {
            x: position.x,
            y: position.y,
            visible: self.overlay_visible(),
            text: Some("processing".to_string()),
        });
    }

    fn send_win32(&self, command: Win32Command) {
        if self.win32_tx.send(command).is_err() {
            logger::info("Win32 command send failed");
        }
    }
}

impl Drop for UiApp {
    fn drop(&mut self) {
        let _ = self.win32_tx.send(Win32Command::Shutdown);
    }
}
