use crate::activity_pipeline::ActivityHandle;
use crate::audio::AudioCapture;
use crate::composition::{CompositionSession, MAX_SESSION_PCM_BYTES, SilenceCompactor};
use crate::config::AppConfig;
use crate::stt::transcribe_pcm;
use crate::injector;
use crate::llm_client;
use crate::logger;
use crate::overlay_view;
use crate::paste_upload::PasteUploader;
use crate::pill_renderer;
use crate::spectrum::{SpectrumAnalyzer, pcm_chunk_to_mono};
use crate::win32_service::{self, Win32Command, Win32Event};
use crate::voice::{VoiceEvent, VoiceHandle};
use crossbeam_channel::{Receiver, Sender};
use iced::{Element, Point, Subscription, Task, window};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const APP_VERSION: &str = env!("CARGO_PKG_VERSION");
const BUILD_ID: &str = env!("ASHE_BUILD_ID");
const OVERLAY_TICK_MS: u64 = 16;
const OVERLAY_SMOOTHING: f32 = 0.28;
const OVERLAY_SNAP_DISTANCE: f32 = 1.0;
const TYPING_PREVIEW_CHARACTERS: usize = 256;
const LINE_BREAK_SYMBOL: &str = "↵";
const LINE_BREAK_FEEDBACK_DURATION: Duration = Duration::from_millis(1_000);
type PolishResult = std::result::Result<String, String>;

#[derive(Clone, Copy, PartialEq, Eq)]
enum DictationState {
    Idle,
    Starting,
    Listening,
    Typing,
    Transcribing,
    Inserting,
    FixingGrammar,
    AnsweringQuestion,
}

#[derive(Clone, Copy)]
enum TextActionKind {
    FixGrammar,
    AnswerQuestion,
}

struct TextAction {
    kind: TextActionKind,
    target_hwnd: isize,
}

pub struct UiApp {
    config: AppConfig,
    state: DictationState,
    win32_tx: Sender<Win32Command>,
    win32_rx: Receiver<Win32Event>,
    _win32_thread: JoinHandle<()>,
    audio: Option<AudioCapture>,
    audio_rx: Option<tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>>,
    current_audio: Option<SilenceCompactor>,
    spectrum: SpectrumAnalyzer,
    session: Option<CompositionSession>,
    dictation_bar_active: bool,
    dictation_insertion_count: usize,
    recording_elapsed: Duration,
    recording_started_at: Option<Instant>,
    line_break_feedback_until: Option<Instant>,
    text_action: Option<TextAction>,
    window_id: Option<window::Id>,
    position: Option<Point>,
    target_position: Option<Point>,
    visible: bool,
    status: String,
    transcript: String,
    polished: Option<String>,
    error: Option<String>,
    activity: ActivityHandle,
    last_activity_status: String,
    paste_in_flight: bool,
    paste_uploader: PasteUploader,
    voice: Option<VoiceHandle>,
    voice_stopping: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    Tick,
    WindowReady(Option<window::Id>),
    WindowCloseRequested(window::Id),
    TranscriptionCompleted {
        target_hwnd: isize,
        result: Result<String, String>,
    },
    TextActionCompleted(PolishResult),
    PasteImageUploaded {
        target_hwnd: isize,
        result: PolishResult,
    },
}

impl UiApp {
    pub fn new() -> (Self, Task<Message>) {
        let config = AppConfig::load();
        logger::info(format!("Config loaded: {}", config.log_summary()));
        let (event_tx, win32_rx) = crossbeam_channel::unbounded();
        let (win32_tx, command_rx) = crossbeam_channel::unbounded();
        let win32_thread = win32_service::spawn(event_tx, command_rx);
        let activity = ActivityHandle::spawn(config.clone());
        let output_sample_rate = config.output_sample_rate;
        let app = Self {
            config,
            state: DictationState::Idle,
            win32_tx,
            win32_rx,
            _win32_thread: win32_thread,
            audio: None,
            audio_rx: None,
            current_audio: None,
            spectrum: SpectrumAnalyzer::new(output_sample_rate),
            session: None,
            dictation_bar_active: false,
            dictation_insertion_count: 0,
            recording_elapsed: Duration::ZERO,
            recording_started_at: None,
            line_break_feedback_until: None,
            text_action: None,
            window_id: None,
            position: None,
            target_position: None,
            visible: false,
            status: "Ready".to_string(),
            transcript: String::new(),
            polished: None,
            error: None,
            activity,
            last_activity_status: String::new(),
            paste_in_flight: false,
            paste_uploader: PasteUploader::default(),
            voice: None,
            voice_stopping: false,
        };
        app.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Idle - Win+Shift+H".to_string(),
        ));
        (app, window::latest().map(Message::WindowReady))
    }

    pub fn subscription(&self) -> Subscription<Message> {
        Subscription::batch([
            iced::time::every(Duration::from_millis(OVERLAY_TICK_MS)).map(|_| Message::Tick),
            window::close_requests().map(Message::WindowCloseRequested),
        ])
    }

    pub fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::WindowReady(id) => {
                logger::info(format!("Iced main WindowReady id={id:?}"));
                self.window_id = id;
                self.apply_window_state()
            }
            Message::WindowCloseRequested(id) => {
                logger::info(format!(
                    "Ignored overlay close request id={id:?}; use the tray Quit action to exit"
                ));
                Task::none()
            }
            Message::Tick => self.pump(),
            Message::TranscriptionCompleted {
                target_hwnd,
                result,
            } => self.finish_transcription(target_hwnd, result),
            Message::TextActionCompleted(result) => self.finish_text_action(result),
            Message::PasteImageUploaded {
                target_hwnd,
                result,
            } => self.finish_image_upload(target_hwnd, result),
        }
    }

    pub fn view(&self) -> Element<'_, Message> {
        overlay_view::view()
    }

    fn pill_state(&self) -> pill_renderer::PillState {
        if self.error.is_some() {
            pill_renderer::PillState::Error
        } else {
            match self.state {
                DictationState::Starting | DictationState::Listening | DictationState::Typing => {
                    pill_renderer::PillState::Listening
                }
                DictationState::Transcribing
                | DictationState::Inserting
                | DictationState::FixingGrammar
                | DictationState::AnsweringQuestion => {
                    pill_renderer::PillState::Working
                }
                DictationState::Idle => pill_renderer::PillState::Idle,
            }
        }
    }

    fn pump(&mut self) -> Task<Message> {
        let mut tasks = Vec::new();
        if self.window_id.is_none() {
            tasks.push(window::latest().map(Message::WindowReady));
        }
        while let Ok(event) = self.win32_rx.try_recv() {
            tasks.push(self.handle_win32_event(event));
        }
        self.pump_voice();
        if self.pump_audio_capture() {
            logger::info("Dictation memory safety threshold reached; finalizing session");
            tasks.push(self.request_stop());
        }
        if self.visible {
            self.advance_overlay_position();
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
        if self.visible {
            self.sync_overlay();
        }
        Task::batch(tasks)
    }

    /// Drain captured PCM into the recording buffer while feeding the live
    /// voice spectrum. Transcription happens once on stop, not streaming.
    fn pump_audio_capture(&mut self) -> bool {
        if !matches!(
            self.state,
            DictationState::Starting | DictationState::Listening
        ) {
            return false;
        }
        let mut chunks = Vec::new();
        if let Some(rx) = self.audio_rx.as_mut() {
            while let Ok(chunk) = rx.try_recv() {
                chunks.push(chunk);
            }
        }
        for chunk in chunks {
            self.process_audio_chunk(&chunk);
        }
        self.spectrum.update();
        self.retained_audio_bytes() >= MAX_SESSION_PCM_BYTES
    }

    fn process_audio_chunk(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        let samples = pcm_chunk_to_mono(chunk);
        let signal_active = self.spectrum.push_samples(&samples);
        if let Some(compactor) = self.current_audio.as_mut() {
            compactor.push_pcm(chunk, signal_active);
        }
    }

    fn retained_audio_bytes(&self) -> usize {
        self.session
            .as_ref()
            .map_or(0, CompositionSession::audio_bytes)
            .saturating_add(
                self.current_audio
                    .as_ref()
                    .map_or(0, SilenceCompactor::estimated_bytes),
            )
    }

    fn handle_win32_event(&mut self, event: Win32Event) -> Task<Message> {
        match event {
            Win32Event::ToggleRequested { target_hwnd, x, y } => {
                self.toggle(target_hwnd, Point::new(x as f32, y as f32))
            }
            Win32Event::CancelRequested => self.cancel_operation(),
            Win32Event::SubmitRequested => self.handle_submit(),
            Win32Event::LineBreakRequested => self.handle_line_break(),
            Win32Event::TypingStarted => self.begin_typing(),
            Win32Event::TextInput(text) => self.capture_typed_text(text),
            Win32Event::BackspaceRequested => self.capture_backspace(),
            Win32Event::PasteTextRequested => self.capture_clipboard_text(),
            Win32Event::KeyboardCaptureFailed(error) => {
                if matches!(
                    self.state,
                    DictationState::Starting | DictationState::Listening | DictationState::Typing
                ) {
                    let task = self.cancel_operation();
                    self.send_win32(Win32Command::ShowMessageBox {
                        title: "Ashe Worker".to_string(),
                        text: error,
                    });
                    task
                } else {
                    Task::none()
                }
            }
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
            Win32Event::PasteImageRequested { target_hwnd } => self.begin_image_paste(target_hwnd),
            Win32Event::ToggleVoiceRequested => {
                self.toggle_voice();
                Task::none()
            }
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
            Win32Event::OpenArtifactsRequested => {
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
            Win32Event::PasteCompleted(result) => self.finish_insert(result),
            Win32Event::PathPasteCompleted(result) => {
                self.paste_in_flight = false;
                match result {
                    Ok(()) => logger::info("Remote clipboard image path pasted"),
                    Err(error) => logger::info(format!("Remote image path paste failed: {error}")),
                }
                Task::none()
            }
            Win32Event::ServiceStopped => {
                logger::info("Win32 service stopped event received");
                Task::none()
            }
        }
    }

    fn toggle(&mut self, target_hwnd: isize, position: Point) -> Task<Message> {
        if self.voice.is_some() {
            logger::info("Dictation unavailable during voice call");
            return Task::none();
        }
        match self.state {
            DictationState::Idle => self.start(target_hwnd, position),
            DictationState::Starting | DictationState::Listening | DictationState::Typing => {
                self.request_stop()
            }
            DictationState::Transcribing => {
                logger::info("Transcription already in progress");
                Task::none()
            }
            DictationState::Inserting => {
                logger::info("Insert already in progress");
                Task::none()
            }
            DictationState::FixingGrammar | DictationState::AnsweringQuestion => {
                logger::info("Text action already in progress");
                Task::none()
            }
        }
    }

    fn toggle_voice(&mut self) {
        if let Some(voice) = &self.voice {
            if !self.voice_stopping {
                self.voice_stopping = true;
                voice.stop();
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - Voice stopping - Win+Shift+A".to_string(),
                ));
            }
            return;
        }
        if self.state != DictationState::Idle {
            logger::info("Voice unavailable during dictation or text action");
            return;
        }
        match VoiceHandle::start(self.config.clone()) {
            Ok(voice) => {
                self.voice = Some(voice);
                self.voice_stopping = false;
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - Voice connecting - Win+Shift+A".to_string(),
                ));
            }
            Err(error) => {
                logger::info(format!("Voice could not start: {error:#}"));
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - Voice configuration error - Win+Shift+A".to_string(),
                ));
            }
        }
    }

    fn pump_voice(&mut self) {
        while let Some(event) = self.voice.as_ref().and_then(VoiceHandle::next_event) {
            match event {
                VoiceEvent::Connected => {
                    logger::info("Ashe voice connected");
                    self.send_win32(Win32Command::SetTooltip(
                        "Ashe Worker - Voice active - Win+Shift+A to stop".to_string(),
                    ));
                }
                VoiceEvent::Error(error) => {
                    logger::info(format!("Ashe voice failed: {error}"));
                    self.send_win32(Win32Command::SetTooltip(
                        "Ashe Worker - Voice error - Win+Shift+A".to_string(),
                    ));
                }
                VoiceEvent::Stopped => {
                    if let Some(mut voice) = self.voice.take() { voice.finish(); }
                    self.voice_stopping = false;
                    self.send_win32(Win32Command::SetTooltip(
                        "Ashe Worker - Voice stopped - Win+Shift+A".to_string(),
                    ));
                    break;
                }
            }
        }
    }

    fn cancel_operation(&mut self) -> Task<Message> {
        if self.state == DictationState::Idle {
            return Task::none();
        }
        logger::info("Cancel operation requested");
        self.stop_recording_timer();
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        self.audio_rx.take();
        self.current_audio.take();
        self.spectrum.reset();
        self.session = None;
        // The in-flight text-action request (if any) is discarded by the
        // state guard when it completes; drop it here so no stale action
        // survives the cancel.
        self.text_action = None;
        self.clear_dictation_bar();
        self.state = DictationState::Idle;
        self.visible = false;
        self.status = "Cancelled".to_string();
        self.polished = None;
        self.error = None;
        self.send_win32(Win32Command::SetKeyboardCapture(false));
        self.send_win32(Win32Command::SetActive(false));
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Cancelled - Win+Shift+H".to_string(),
        ));
        self.apply_window_state()
    }

    fn start(&mut self, target_hwnd: isize, position: Point) -> Task<Message> {
        if let Err(err) = self.config.validate_for_dictation() {
            logger::info(format!("Config validation failed: {err:#}"));
            self.send_win32(Win32Command::ShowMessageBox {
                title: "Ashe Worker".to_string(),
                text: format!("Cannot start dictation: {err}"),
            });
            return Task::none();
        }
        logger::info("Start dictation requested");
        self.state = DictationState::Starting;
        self.visible = true;
        self.position = Some(position);
        self.target_position = Some(position);
        self.status = "Starting...".to_string();
        self.transcript.clear();
        self.polished = None;
        self.error = None;
        self.dictation_bar_active = true;
        self.dictation_insertion_count = 0;
        self.recording_elapsed = Duration::ZERO;
        self.recording_started_at = None;
        self.line_break_feedback_until = None;
        self.session = Some(CompositionSession::new(
            target_hwnd,
            self.config.output_sample_rate,
        ));
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Starting... - Win+Shift+H".to_string(),
        ));
        if let Err(err) = self.start_audio_capture() {
            logger::info(format!("Audio start failed: {err:#}"));
            let hide_task = self.finish_without_transcript();
            self.send_win32(Win32Command::ShowMessageBox {
                title: "Ashe Worker".to_string(),
                text: format!("Audio capture failed: {err}"),
            });
            return hide_task;
        }
        self.send_win32(Win32Command::SetActive(true));
        self.send_win32(Win32Command::SetKeyboardCapture(true));
        self.apply_window_state()
    }

    fn start_audio_capture(&mut self) -> anyhow::Result<()> {
        let (audio_tx, audio_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let audio = AudioCapture::start(audio_tx, self.config.output_sample_rate)?;
        let sample_rate = audio.sample_rate();
        self.audio_rx = Some(audio_rx);
        self.current_audio = Some(SilenceCompactor::new(sample_rate));
        self.spectrum = SpectrumAnalyzer::new(sample_rate);
        self.audio = Some(audio);
        self.recording_started_at = Some(Instant::now());
        self.state = DictationState::Listening;
        self.status = "Listening...".to_string();
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Listening... - Win+Shift+H".to_string(),
        ));
        Ok(())
    }

    fn begin_typing(&mut self) -> Task<Message> {
        if !matches!(
            self.state,
            DictationState::Starting | DictationState::Listening
        ) {
            return Task::none();
        }
        self.seal_current_audio();
        self.line_break_feedback_until = None;
        self.state = DictationState::Typing;
        self.status = "Typing...".to_string();
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Typing - Enter to resume - Win+Shift+H".to_string(),
        ));
        Task::none()
    }

    fn capture_typed_text(&mut self, text: String) -> Task<Message> {
        if self.state != DictationState::Typing || text.is_empty() {
            return Task::none();
        }
        if let Some(session) = self.session.as_mut() {
            session.push_typed_text(&text);
        }
        Task::none()
    }

    fn capture_backspace(&mut self) -> Task<Message> {
        if self.state == DictationState::Typing
            && let Some(session) = self.session.as_mut()
            && session.backspace()
            && session.typing_is_empty()
        {
            // Clearing the buffer resumes listening without committing.
            logger::info("Dictation typing cleared; resuming listening");
            return self.resume_audio_capture();
        }
        Task::none()
    }

    fn capture_clipboard_text(&mut self) -> Task<Message> {
        if self.state != DictationState::Typing {
            return Task::none();
        }
        match injector::capture_clipboard_text() {
            Ok(Some(text)) if !text.is_empty() => {
                logger::info(format!("Captured dictation paste chars={}", text.len()));
                if let Some(session) = self.session.as_mut() {
                    session.push_pasted_text(text);
                }
            }
            Ok(_) => logger::info("Dictation paste ignored because clipboard text is empty"),
            Err(error) => logger::info(format!("Dictation paste capture failed: {error:#}")),
        }
        Task::none()
    }

    fn handle_submit(&mut self) -> Task<Message> {
        if self.state == DictationState::Typing
            && self
                .session
                .as_ref()
                .is_some_and(|session| !session.typing_is_empty())
        {
            let count = self.session.as_mut().map_or(0, |session| {
                session.commit_insertion();
                session.insertion_count()
            });
            self.dictation_insertion_count = count;
            logger::info(format!("Dictation insertion committed count={count}"));
            return self.resume_audio_capture();
        }
        if matches!(
            self.state,
            DictationState::Starting | DictationState::Listening | DictationState::Typing
        ) {
            self.request_stop()
        } else {
            Task::none()
        }
    }

    /// Restart capture after typing ends without submitting.
    fn resume_audio_capture(&mut self) -> Task<Message> {
        if let Err(error) = self.start_audio_capture() {
            logger::info(format!("Audio resume failed: {error:#}"));
            self.send_win32(Win32Command::ShowMessageBox {
                title: "Ashe Worker".to_string(),
                text: format!("Could not resume audio capture: {error}"),
            });
            return self.request_stop();
        }
        self.send_win32(Win32Command::EndTyping);
        Task::none()
    }

    fn handle_line_break(&mut self) -> Task<Message> {
        if self.state == DictationState::Typing {
            if let Some(session) = self.session.as_mut() {
                session.push_typed_line_break();
            }
            return Task::none();
        }
        if !matches!(
            self.state,
            DictationState::Starting | DictationState::Listening
        ) {
            return Task::none();
        }

        self.split_current_audio_segment();
        let Some(session) = self.session.as_mut() else {
            return Task::none();
        };
        session.commit_line_break();
        let count = session.insertion_count();
        self.dictation_insertion_count = count;
        self.line_break_feedback_until = Some(Instant::now() + LINE_BREAK_FEEDBACK_DURATION);
        logger::info(format!("Dictation line break committed count={count}"));
        Task::none()
    }

    fn split_current_audio_segment(&mut self) {
        self.drain_pending_audio();
        let sample_rate = self
            .audio
            .as_ref()
            .map_or(self.config.output_sample_rate, AudioCapture::sample_rate);
        self.commit_current_audio_segment();
        self.current_audio = Some(SilenceCompactor::new(sample_rate));
    }

    fn seal_current_audio(&mut self) {
        self.stop_recording_timer();
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        self.drain_pending_audio();
        self.audio_rx.take();
        self.commit_current_audio_segment();
        self.spectrum.reset();
    }

    fn drain_pending_audio(&mut self) {
        let mut chunks = Vec::new();
        if let Some(rx) = self.audio_rx.as_mut() {
            while let Ok(chunk) = rx.try_recv() {
                chunks.push(chunk);
            }
        }
        for chunk in chunks {
            self.process_audio_chunk(&chunk);
        }
    }

    fn commit_current_audio_segment(&mut self) {
        if let Some(compactor) = self.current_audio.take()
            && let Some(pcm) = compactor.finish()
        {
            logger::info(format!("Sealed local speech segment bytes={}", pcm.len()));
            if let Some(session) = self.session.as_mut() {
                session.push_speech(pcm);
            }
        }
    }

    fn request_stop(&mut self) -> Task<Message> {
        if matches!(
            self.state,
            DictationState::Idle
                | DictationState::Transcribing
                | DictationState::Inserting
                | DictationState::FixingGrammar
                | DictationState::AnsweringQuestion
        ) {
            return Task::none();
        }
        self.seal_current_audio();
        if self.state == DictationState::Typing
            && let Some(session) = self.session.as_mut()
        {
            session.commit_insertion();
        }
        self.send_win32(Win32Command::SetKeyboardCapture(false));
        let Some(session) = self.session.take() else {
            return self.finish_without_transcript();
        };
        self.dictation_insertion_count = session.insertion_count();
        let mut composition = session.finish();
        let target_hwnd = composition.target_hwnd();
        logger::info(format!(
            "Stop dictation requested audio_bytes={} has_speech={}",
            composition.audio_pcm_len(),
            composition.has_speech()
        ));
        if !composition.has_speech() {
            return match composition.assemble(None) {
                Ok(text) => self.begin_dictation_insert(target_hwnd, text),
                Err(error) => {
                    logger::info(format!("Dictation composition failed: {error:#}"));
                    self.finish_without_transcript()
                }
            };
        }

        self.state = DictationState::Transcribing;
        self.status = "Transcribing...".to_string();
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Transcribing... - Win+Shift+H".to_string(),
        ));
        let word_timestamps = composition.requires_word_timestamps();
        let pcm = composition.take_audio();
        let sample_rate = composition.sample_rate();
        let config = self.config.clone();
        Task::perform(
            async move {
                let transcript = transcribe_pcm(config, sample_rate, pcm, word_timestamps)
                    .await
                    .map_err(|err| format!("{err:#}"))?;
                composition
                    .assemble(Some(&transcript))
                    .map_err(|err| format!("{err:#}"))
            },
            move |result| Message::TranscriptionCompleted {
                target_hwnd,
                result,
            },
        )
    }

    fn finish_transcription(
        &mut self,
        target_hwnd: isize,
        result: Result<String, String>,
    ) -> Task<Message> {
        if self.state != DictationState::Transcribing {
            return Task::none();
        }
        match result {
            Ok(text) => {
                if text.is_empty() {
                    logger::info("Transcription returned no speech");
                    return self.finish_without_transcript();
                }
                logger::info(format!("Transcription completed chars={}", text.len()));
                self.polished = None;
                self.error = None;
                self.begin_dictation_insert(target_hwnd, text)
            }
            Err(err) => {
                logger::info(format!("Transcription failed: {err}"));
                self.error = Some("Transcription failed".to_string());
                self.status = "Transcription failed".to_string();
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - Transcription error - Win+Shift+H".to_string(),
                ));
                self.hide_overlay_after_session()
            }
        }
    }

    /// Insert the deterministic composition once, without LLM polishing.
    fn begin_dictation_insert(&mut self, target_hwnd: isize, text: String) -> Task<Message> {
        if text.is_empty() {
            return self.finish_without_transcript();
        }
        logger::info(format!(
            "Inserting dictation composition chars={}",
            text.len()
        ));
        self.state = DictationState::Inserting;
        self.status = "Inserting...".to_string();
        self.send_win32(Win32Command::PasteText { target_hwnd, text });
        Task::none()
    }

    fn finish_insert(&mut self, result: Result<(), String>) -> Task<Message> {
        if let Err(err) = result {
            logger::info(format!("Text injection failed: {err}"));
            self.error = Some("Paste failed".to_string());
            self.send_win32(Win32Command::SetTooltip(
                "Ashe Worker - Paste error - Win+Shift+H".to_string(),
            ));
        } else {
            self.send_win32(Win32Command::SetTooltip(
                "Ashe Worker - Inserted - Win+Shift+H".to_string(),
            ));
        }
        self.hide_overlay_after_session()
    }

    fn finish_without_transcript(&mut self) -> Task<Message> {
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Idle - Win+Shift+H".to_string(),
        ));
        self.hide_overlay_after_session()
    }

    fn reload_config(&mut self) {
        if self.state != DictationState::Idle || self.voice.is_some() {
            self.send_win32(Win32Command::ShowMessageBox {
                title: "Ashe Worker".to_string(),
                text: "Stop dictation or voice before reloading config.".to_string(),
            });
            return;
        }
        self.config = AppConfig::load();
        self.activity.shutdown();
        self.activity = ActivityHandle::spawn(self.config.clone());
        logger::info(format!("Config reloaded: {}", self.config.log_summary()));
        self.send_win32(Win32Command::SetTooltip(
            "Ashe Worker - Config reloaded - Win+Shift+H".to_string(),
        ));
    }

    fn show_about(&self) {
        self.send_win32(Win32Command::ShowMessageBox {
            title: "About Ashe Worker".to_string(),
            text: format!(
                "Ashe Worker\r\nVersion: {}\r\nBuild: {}\r\n\r\nDictate: Win+Shift+H\r\nVoice: Win+Shift+A\r\nGrammar: Win+Shift+G\r\nQuestion: Win+Shift+Q\r\nImage path: Ctrl+Alt+V\r\nActivity tracking: {}\r\nArtifacts: {}\r\nConfig: {}",
                APP_VERSION,
                BUILD_ID,
                self.activity.status().summary,
                self.activity.artifacts_dir().display(),
                self.config.log_summary()
            ),
        });
    }

    fn begin_image_paste(&mut self, target_hwnd: isize) -> Task<Message> {
        if self.state != DictationState::Idle || self.paste_in_flight {
            logger::info("Clipboard image paste ignored while another action is active");
            return Task::none();
        }
        if target_hwnd == 0 {
            logger::info("Clipboard image paste ignored without a foreground target");
            return Task::none();
        }
        if let Err(error) = self.config.validate_for_paste() {
            logger::info(format!(
                "Clipboard image paste configuration invalid: {error:#}"
            ));
            return Task::none();
        }
        let png = match injector::capture_clipboard_png() {
            Ok(png) => png,
            Err(error) => {
                logger::info(format!("Clipboard image capture failed: {error:#}"));
                return Task::none();
            }
        };
        logger::info(format!("Clipboard image captured bytes={}", png.len()));
        self.paste_in_flight = true;
        let uploader = self.paste_uploader.clone();
        let config = self.config.clone();
        Task::perform(
            async move {
                uploader
                    .upload_png(config, png, chrono::Utc::now())
                    .await
                    .map_err(|error| format!("{error:#}"))
            },
            move |result| Message::PasteImageUploaded {
                target_hwnd,
                result,
            },
        )
    }

    fn finish_image_upload(&mut self, target_hwnd: isize, result: PolishResult) -> Task<Message> {
        match result {
            Ok(path) => {
                logger::info(format!("Clipboard image uploaded path={path}"));
                self.send_win32(Win32Command::PastePath {
                    target_hwnd,
                    text: path,
                });
            }
            Err(error) => {
                self.paste_in_flight = false;
                logger::info(format!("Clipboard image upload failed: {error}"));
            }
        }
        Task::none()
    }

    fn begin_text_action(
        &mut self,
        kind: TextActionKind,
        target_hwnd: isize,
        position: Point,
    ) -> Task<Message> {
        if self.state != DictationState::Idle {
            logger::info("Text action ignored; not idle");
            return Task::none();
        }
        let validation = match kind {
            TextActionKind::FixGrammar => self.config.validate_for_grammar(),
            TextActionKind::AnswerQuestion => self.config.validate_for_question(),
        };
        if let Err(err) = validation {
            logger::info(format!("LLM config validation failed: {err:#}"));
            self.send_win32(Win32Command::ShowMessageBox {
                title: "Ashe Worker".to_string(),
                text: format!("Cannot run text action: {err}"),
            });
            return Task::none();
        }
        let selected = match injector::capture_selected_text() {
            Ok(Some(text)) => text,
            Ok(None) => {
                logger::info("Text action: no text selected");
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - No text selected - Win+Shift+H".to_string(),
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

        let (state, status, tooltip) = match kind {
            TextActionKind::FixGrammar => (
                DictationState::FixingGrammar,
                "Fixing grammar...",
                "Ashe Worker - Fixing grammar... - Win+Shift+H",
            ),
            TextActionKind::AnswerQuestion => (
                DictationState::AnsweringQuestion,
                "Answering...",
                "Ashe Worker - Answering... - Win+Shift+H",
            ),
        };
        logger::info(format!("Text action started chars={}", selected.len()));
        self.state = state;
        self.text_action = Some(TextAction { kind, target_hwnd });
        self.visible = true;
        self.position = Some(position);
        self.target_position = Some(position);
        self.status = status.to_string();
        self.transcript = selected.clone();
        self.polished = None;
        self.error = None;
        self.send_win32(Win32Command::SetTooltip(tooltip.to_string()));
        self.send_win32(Win32Command::SetFollowCursor(true));
        // Capture Escape only: the user keeps typing in the foreground app
        // while the request runs, so full keyboard capture would steal
        // keystrokes. Teardown disables capture on every exit path.
        self.send_win32(Win32Command::SetEscapeCapture(true));

        let config = self.config.clone();
        let llm_task = Task::perform(
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
        Task::batch([self.apply_window_state(), llm_task])
    }

    fn finish_text_action(&mut self, result: PolishResult) -> Task<Message> {
        if !matches!(
            self.state,
            DictationState::FixingGrammar | DictationState::AnsweringQuestion
        ) {
            return Task::none();
        }
        let Some(action) = self.text_action.as_ref() else {
            return self.finish_without_transcript();
        };
        let append = matches!(action.kind, TextActionKind::AnswerQuestion);
        let target_hwnd = action.target_hwnd;
        let text = match result {
            Ok(text) => text,
            Err(err) => {
                logger::info(format!("Text action failed: {err}"));
                self.error = Some("LLM request failed".to_string());
                self.send_win32(Win32Command::SetTooltip(
                    "Ashe Worker - LLM error - Win+Shift+H".to_string(),
                ));
                return self.hide_overlay_after_session();
            }
        };
        if text.trim().is_empty() {
            logger::info("Text action returned empty result");
            self.send_win32(Win32Command::SetTooltip(
                "Ashe Worker - Empty result - Win+Shift+H".to_string(),
            ));
            return self.hide_overlay_after_session();
        }
        logger::info("Text action completed");
        let inject_text = if append {
            format!("\n\n{text}")
        } else {
            text.clone()
        };
        self.polished = Some(text);
        self.state = DictationState::Inserting;
        self.status = "Inserting...".to_string();
        // Commit point: the paste is in flight, so Escape no longer cancels.
        self.send_win32(Win32Command::SetKeyboardCapture(false));
        self.send_win32(Win32Command::InjectText {
            target_hwnd,
            text: inject_text,
            append_after_selection: append,
        });
        Task::none()
    }

    fn quit(&mut self) -> Task<Message> {
        logger::info("Quit requested");
        if let Some(mut voice) = self.voice.take() {
            voice.stop();
            voice.finish();
        }
        self.activity.shutdown();
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        self.audio_rx.take();
        self.send_win32(Win32Command::Shutdown);
        if let Some(id) = self.window_id {
            window::close(id)
        } else {
            Task::none()
        }
    }

    fn hide_overlay_after_session(&mut self) -> Task<Message> {
        self.session = None;
        self.current_audio = None;
        self.clear_dictation_bar();
        self.text_action = None;
        self.state = DictationState::Idle;
        self.visible = false;
        self.target_position = self.position;
        self.send_win32(Win32Command::SetKeyboardCapture(false));
        self.send_win32(Win32Command::SetActive(false));
        self.send_win32(Win32Command::SetFollowCursor(false));
        self.apply_window_state()
    }

    fn stop_recording_timer(&mut self) {
        if let Some(started_at) = self.recording_started_at.take() {
            self.recording_elapsed = self.recording_elapsed.saturating_add(started_at.elapsed());
        }
    }

    fn recording_duration(&self) -> Duration {
        self.recording_started_at
            .map_or(self.recording_elapsed, |started_at| {
                self.recording_elapsed.saturating_add(started_at.elapsed())
            })
    }

    fn clear_dictation_bar(&mut self) {
        self.dictation_bar_active = false;
        self.dictation_insertion_count = 0;
        self.recording_elapsed = Duration::ZERO;
        self.recording_started_at = None;
        self.line_break_feedback_until = None;
    }

    fn advance_overlay_position(&mut self) -> bool {
        let Some(target) = self.target_position else {
            return false;
        };
        let Some(current) = self.position else {
            self.position = Some(target);
            return true;
        };
        let dx = target.x - current.x;
        let dy = target.y - current.y;
        if dx.hypot(dy) <= OVERLAY_SNAP_DISTANCE {
            if current != target {
                self.position = Some(target);
                return true;
            }
            return false;
        }
        self.position = Some(Point::new(
            current.x + dx * OVERLAY_SMOOTHING,
            current.y + dy * OVERLAY_SMOOTHING,
        ));
        true
    }

    fn apply_window_state(&self) -> Task<Message> {
        self.sync_overlay();
        let Some(id) = self.window_id else {
            return Task::none();
        };
        overlay_view::keep_helper_hidden(id)
    }

    fn sync_overlay(&self) {
        let position = self.position.unwrap_or(Point::new(120.0, 120.0));
        let top_bar = self.top_bar_content();
        // Grammar and question show a compact pill with a single status
        // word while the LLM request is in flight; dictation keeps the
        // full pill with visualizer and status bar.
        let mini = matches!(
            self.state,
            DictationState::FixingGrammar | DictationState::AnsweringQuestion
        );
        self.send_win32(Win32Command::UpdateOverlay {
            x: position.x,
            y: position.y,
            visible: self.visible,
            bars: if self.visible {
                self.spectrum.bars().to_vec()
            } else {
                Vec::new()
            },
            state: self.pill_state(),
            main_text: mini.then(|| "processing".to_string()),
            top_bar,
            mini,
        });
    }

    fn top_bar_content(&self) -> Option<pill_renderer::TopBarContent> {
        if !self.dictation_bar_active {
            return None;
        }
        let preview = self
            .session
            .as_ref()
            .map(|session| session.typing_preview_tail(TYPING_PREVIEW_CHARACTERS))
            .unwrap_or_default();
        let (content, alignment) = if self.state == DictationState::Typing && !preview.is_empty() {
            (preview, pill_renderer::TopBarAlignment::Trailing)
        } else if matches!(
            self.state,
            DictationState::Transcribing | DictationState::Inserting
        ) {
            (
                "processing...".to_string(),
                pill_renderer::TopBarAlignment::Center,
            )
        } else if matches!(
            self.state,
            DictationState::Starting | DictationState::Listening
        ) && self
            .current_audio
            .as_ref()
            .is_some_and(SilenceCompactor::silence_truncated)
        {
            (
                "silence skipped".to_string(),
                pill_renderer::TopBarAlignment::Center,
            )
        } else {
            (
                format_recording_duration(self.recording_duration()),
                pill_renderer::TopBarAlignment::Center,
            )
        };
        Some(pill_renderer::TopBarContent {
            count: format!("{} inserted", self.dictation_insertion_count),
            content,
            alignment,
            accessory: matches!(
                self.state,
                DictationState::Starting | DictationState::Listening
            )
            .then(|| self.line_break_feedback_until)
            .flatten()
            .filter(|deadline| Instant::now() < *deadline)
            .map(|_| LINE_BREAK_SYMBOL.to_string()),
        })
    }

    fn send_win32(&self, command: Win32Command) {
        if let Err(err) = self.win32_tx.send(command) {
            logger::info(format!("Win32 command send failed: {err}"));
        }
    }
}

fn format_recording_duration(duration: Duration) -> String {
    let total_seconds = duration.as_secs();
    let hours = total_seconds / 3_600;
    let minutes = total_seconds / 60 % 60;
    let seconds = total_seconds % 60;
    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes:02}:{seconds:02}")
    }
}

impl Drop for UiApp {
    fn drop(&mut self) {
        if let Some(mut audio) = self.audio.take() {
            audio.stop();
        }
        let _ = self.win32_tx.send(Win32Command::Shutdown);
    }
}
