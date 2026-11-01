//! The schema of every input the worker reads, and the loader that layers,
//! defaults, and validates them before any work starts.
//!
//! Layers apply lowest first: schema defaults, the overlay file, the protected
//! file, then the process environment. The overlay and protected file paths
//! come from the process environment alone. Protected inputs are read only
//! from the protected file, which must be readable by its owner alone.

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub const PROFILE: &str = "ASHE_WORKER_PROFILE";
pub const OVERLAY_FILE: &str = "ASHE_WORKER_OVERLAY_FILE";
pub const PROTECTED_FILE: &str = "ASHE_WORKER_PROTECTED_FILE";
const OWN_PREFIX: &str = "ASHE_WORKER_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Profile {
    Development,
    Production,
}

impl Profile {
    pub fn name(self) -> &'static str {
        match self {
            Self::Development => "development",
            Self::Production => "production",
        }
    }

    fn parse(value: &str) -> Option<Self> {
        match value {
            "development" => Some(Self::Development),
            "production" => Some(Self::Production),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class {
    /// Grants no access; the overlay file and the process environment may set it.
    Configuration,
    /// Grants access or identity; only the protected file may set it.
    Protected,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// Read from the process environment alone, because it names a layer.
    Process,
    /// Read from every layer its class admits.
    Layered,
}

#[derive(Clone, Copy, Debug)]
pub enum Shape {
    /// Any text, possibly empty.
    Text,
    /// Text with at least one non-whitespace character.
    NonEmpty,
    Bool,
    Integer {
        min: i64,
        max: i64,
    },
    Number {
        min: f64,
        max: f64,
    },
    /// An `http` or `https` URL without credentials, query, or fragment.
    HttpUrl,
    /// An `http` or `https` origin: scheme, host, and optional port only.
    HttpOrigin,
    /// An absolute filesystem path.
    Path,
    Choice(&'static [&'static str]),
    /// Entries separated by `;`, possibly none.
    List,
}

impl Shape {
    fn describe(self) -> String {
        match self {
            Self::Text => "text".to_string(),
            Self::NonEmpty => "non-empty text".to_string(),
            Self::Bool => "true or false".to_string(),
            Self::Integer { min, max } => format!("an integer from {min} to {max}"),
            Self::Number { min, max } => format!("a number from {min} to {max}"),
            Self::HttpUrl => "an http(s) URL".to_string(),
            Self::HttpOrigin => "an http(s) origin".to_string(),
            Self::Path => "an absolute path".to_string(),
            Self::Choice(values) => format!("one of {}", values.join(", ")),
            Self::List => "entries separated by ;".to_string(),
        }
    }

    fn check(self, value: &str) -> bool {
        match self {
            Self::Text | Self::List => true,
            Self::NonEmpty => !value.trim().is_empty(),
            Self::Bool => matches!(value, "true" | "false"),
            Self::Integer { min, max } => value
                .parse::<i64>()
                .is_ok_and(|number| (min..=max).contains(&number)),
            Self::Number { min, max } => value
                .parse::<f64>()
                .is_ok_and(|number| number.is_finite() && number >= min && number <= max),
            Self::HttpUrl => http_url(value, false),
            Self::HttpOrigin => http_url(value, true),
            Self::Path => is_absolute_path(value),
            Self::Choice(values) => values.contains(&value),
        }
    }
}

fn http_url(value: &str, origin_only: bool) -> bool {
    let Ok(url) = reqwest::Url::parse(value) else {
        return false;
    };
    matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && (!origin_only || url.path() == "/")
}

/// A Windows path with a drive or UNC prefix, or a Unix absolute path.
fn is_absolute_path(value: &str) -> bool {
    let bytes = value.as_bytes();
    let drive = bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && matches!(bytes[2], b'\\' | b'/');
    drive || value.starts_with("\\\\") || value.starts_with('/')
}

#[derive(Clone, Copy, Debug)]
pub enum Default {
    /// The same default in every profile.
    Both(&'static str),
    /// Defaults per profile; `None` makes the input required in that profile.
    Each {
        development: Option<&'static str>,
        production: Option<&'static str>,
    },
    /// Required in every profile.
    Required,
}

impl Default {
    pub fn for_profile(self, profile: Profile) -> Option<&'static str> {
        match (self, profile) {
            (Self::Both(value), _) => Some(value),
            (Self::Each { development, .. }, Profile::Development) => development,
            (Self::Each { production, .. }, Profile::Production) => production,
            (Self::Required, _) => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Input {
    pub name: &'static str,
    pub class: Class,
    pub origin: Origin,
    pub shape: Shape,
    pub default: Default,
    pub description: &'static str,
}

const fn config(
    name: &'static str,
    shape: Shape,
    default: Default,
    description: &'static str,
) -> Input {
    Input {
        name,
        class: Class::Configuration,
        origin: Origin::Layered,
        shape,
        default,
        description,
    }
}

const fn protected(
    name: &'static str,
    shape: Shape,
    default: Default,
    description: &'static str,
) -> Input {
    Input {
        name,
        class: Class::Protected,
        origin: Origin::Layered,
        shape,
        default,
        description,
    }
}

const fn process(name: &'static str, default: &'static str, description: &'static str) -> Input {
    Input {
        name,
        class: Class::Configuration,
        origin: Origin::Process,
        shape: Shape::Path,
        default: Default::Both(default),
        description,
    }
}

const EXPORTERS: &[&str] = &["otlp", "console", "none"];
const PROFILES: &[&str] = &["development", "production"];
const BOTH_ENVIRONMENTS: Default = Default::Each {
    development: Some("development"),
    production: Some("production"),
};

/// Every input of the worker. `%APPDATA%` and `%LOCALAPPDATA%` at the start of
/// a default stand for the user's Roaming and Local application data folders.
pub const INPUTS: &[Input] = &[
    process(
        OVERLAY_FILE,
        "%APPDATA%\\ashe-worker\\config.env",
        "Overlay file of NAME=value configuration lines.",
    ),
    process(
        PROTECTED_FILE,
        "%APPDATA%\\ashe-worker\\protected.env",
        "Protected file of NAME='value' lines, readable by its owner alone.",
    ),
    config(
        PROFILE,
        Shape::Choice(PROFILES),
        Default::Required,
        "Profile whose defaults apply.",
    ),
    config(
        "ASHE_WORKER_DEPLOYMENT_ENVIRONMENT",
        Shape::Choice(PROFILES),
        BOTH_ENVIRONMENTS,
        "Deployment environment that telemetry reports.",
    ),
    config(
        "ASHE_WORKER_DATA_DIR",
        Shape::Path,
        Default::Both("%LOCALAPPDATA%\\ashe-worker"),
        "Directory of activity records, pending blocks, and upload state.",
    ),
    config(
        "ASHE_WORKER_LLM_BASE_URL",
        Shape::HttpUrl,
        Default::Required,
        "Base URL of an OpenAI Responses API; requests go to <base>/responses.",
    ),
    protected(
        "ASHE_WORKER_LLM_API_KEY",
        Shape::NonEmpty,
        Default::Required,
        "Bearer key of the Responses API.",
    ),
    config(
        "ASHE_WORKER_GRAMMAR_MODEL",
        Shape::NonEmpty,
        Default::Required,
        "Model that corrects selected text.",
    ),
    config(
        "ASHE_WORKER_QUESTION_MODEL",
        Shape::NonEmpty,
        Default::Required,
        "Model that answers a question about selected text.",
    ),
    config(
        "ASHE_WORKER_JOURNAL_MODEL",
        Shape::NonEmpty,
        Default::Required,
        "Model that describes activity blocks and keeps the rolling summary; it must accept images.",
    ),
    config(
        "ASHE_WORKER_LLM_TEMPERATURE",
        Shape::Number { min: 0.0, max: 2.0 },
        Default::Both("0.2"),
        "Sampling temperature of grammar correction.",
    ),
    config(
        "ASHE_WORKER_LLM_REASONING_EFFORT",
        Shape::Text,
        Default::Both(""),
        "Reasoning effort passed unchanged with every request; empty omits it.",
    ),
    config(
        "ASHE_WORKER_ACTIVITY_ENABLED",
        Shape::Bool,
        Default::Both("true"),
        "Whether activity tracking starts with the worker.",
    ),
    config(
        "ASHE_WORKER_MONITOR",
        Shape::Integer { min: 1, max: 16 },
        Default::Both("1"),
        "Display that activity tracking captures, counted from 1.",
    ),
    config(
        "ASHE_WORKER_CAPTURE_INTERVAL_SECONDS",
        Shape::Integer { min: 1, max: 3600 },
        Default::Both("10"),
        "Seconds between screen captures.",
    ),
    config(
        "ASHE_WORKER_SAMPLE_INTERVAL_SECONDS",
        Shape::Number {
            min: 0.5,
            max: 60.0,
        },
        Default::Both("2"),
        "Seconds between foreground-window samples.",
    ),
    config(
        "ASHE_WORKER_BLOCK_MINUTES",
        Shape::Integer { min: 1, max: 240 },
        Default::Both("10"),
        "Length of one activity block in minutes.",
    ),
    config(
        "ASHE_WORKER_DEDUP_THRESHOLD",
        Shape::Number {
            min: 0.0,
            max: 100.0,
        },
        Default::Both("2"),
        "Visual difference below which a capture repeats the previous one.",
    ),
    config(
        "ASHE_WORKER_MAX_FRAME_GAP_SECONDS",
        Shape::Integer { min: 1, max: 3600 },
        Default::Both("30"),
        "Longest gap between retained captures; raised to the capture interval.",
    ),
    config(
        "ASHE_WORKER_MAX_FRAMES_PER_CALL",
        Shape::Integer { min: 2, max: 1000 },
        Default::Both("100"),
        "Most captures sent with one block description.",
    ),
    config(
        "ASHE_WORKER_MAX_PAYLOAD_MB",
        Shape::Number {
            min: 1.0,
            max: 512.0,
        },
        Default::Both("48"),
        "Largest image payload of one block description, in megabytes.",
    ),
    config(
        "ASHE_WORKER_IDLE_THRESHOLD_SECONDS",
        Shape::Number {
            min: 10.0,
            max: 86400.0,
        },
        Default::Both("120"),
        "Input idle time after which the user counts as idle.",
    ),
    config(
        "ASHE_WORKER_MIN_ACTIVE_SECONDS",
        Shape::Integer { min: 0, max: 86400 },
        Default::Both("30"),
        "Active input below which a static block is stored without a description.",
    ),
    config(
        "ASHE_WORKER_DENYLIST",
        Shape::List,
        Default::Both(""),
        "Executable or window-title fragments that are never captured.",
    ),
    config(
        "ASHE_WORKER_FRAME_RETENTION_MINUTES",
        Shape::Integer { min: 1, max: 10080 },
        Default::Both("30"),
        "Minutes captures are kept after their block is described.",
    ),
    config(
        "ASHE_WORKER_CONTEXT_BLOCKS",
        Shape::Integer { min: 0, max: 100 },
        Default::Both("6"),
        "Earlier blocks given as context to a block description.",
    ),
    config(
        "ASHE_WORKER_MAX_SUMMARY_CHARS",
        Shape::Integer {
            min: 1,
            max: 100000,
        },
        Default::Both("1500"),
        "Longest rolling summary, in characters.",
    ),
    config(
        "ASHE_WORKER_DAILY_REPORT_ENABLED",
        Shape::Bool,
        Default::Both("true"),
        "Whether each finished day gets a daily report.",
    ),
    config(
        "ASHE_WORKER_DAILY_REPORT_GRACE_MINUTES",
        Shape::Integer { min: 0, max: 1440 },
        Default::Both("15"),
        "Minutes after midnight before the previous day's report is written.",
    ),
    config(
        "ASHE_WORKER_SERVER_URL",
        Shape::HttpOrigin,
        Default::Required,
        "Origin of the server that receives activity records.",
    ),
    protected(
        "ASHE_WORKER_SERVER_TOKEN",
        Shape::NonEmpty,
        Default::Required,
        "Bearer token of record uploads.",
    ),
    config(
        "ASHE_WORKER_UPLOAD_SCAN_MINUTES",
        Shape::Integer { min: 1, max: 1440 },
        Default::Both("5"),
        "Minutes between scans for finished days to upload.",
    ),
    config(
        "ASHE_WORKER_LOCAL_RETENTION_DAYS",
        Shape::Integer { min: 1, max: 3650 },
        Default::Both("7"),
        "Days an uploaded day stays on this machine.",
    ),
    config(
        "OTEL_TRACES_EXPORTER",
        Shape::Choice(EXPORTERS),
        Default::Both("otlp"),
        "Trace exporter.",
    ),
    config(
        "OTEL_METRICS_EXPORTER",
        Shape::Choice(EXPORTERS),
        Default::Both("otlp"),
        "Metric exporter.",
    ),
    config(
        "OTEL_LOGS_EXPORTER",
        Shape::Choice(EXPORTERS),
        Default::Both("otlp"),
        "Log exporter.",
    ),
    config(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        Shape::HttpUrl,
        Default::Each {
            development: Some("http://127.0.0.1:4318"),
            production: None,
        },
        "OTLP/HTTP endpoint; each signal goes to <endpoint>/v1/<signal>.",
    ),
    protected(
        "OTEL_EXPORTER_OTLP_HEADERS",
        Shape::Text,
        Default::Each {
            development: Some(""),
            production: None,
        },
        "OTLP headers as comma-separated key=value pairs.",
    ),
];

pub fn input(name: &str) -> Option<&'static Input> {
    INPUTS.iter().find(|input| input.name == name)
}

/// The application data folders that defaults may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KnownFolder {
    RoamingAppData,
    LocalAppData,
}

impl KnownFolder {
    const ALL: [(Self, &'static str); 2] = [
        (Self::RoamingAppData, "%APPDATA%"),
        (Self::LocalAppData, "%LOCALAPPDATA%"),
    ];
}

/// The result of reading one file layer.
pub enum FileLayer {
    Absent,
    Present(String),
    /// Present, but readable by someone other than its owner.
    Unprotected,
    Unreadable(String),
}

/// What the loader reads, injected so that loading itself touches nothing.
pub struct Sources<'a> {
    pub process: &'a BTreeMap<String, String>,
    pub known_folder: &'a dyn Fn(KnownFolder) -> Option<PathBuf>,
    /// Reads the overlay file.
    pub read_overlay: &'a dyn Fn(&Path) -> FileLayer,
    /// Reads the protected file, reporting one that others can read.
    pub read_protected: &'a dyn Fn(&Path) -> FileLayer,
}

/// The validated effective inputs of one process.
#[derive(Clone, Debug)]
pub struct Environment {
    profile: Profile,
    values: BTreeMap<&'static str, String>,
}

impl Environment {
    pub fn profile(&self) -> Profile {
        self.profile
    }

    pub fn text(&self, name: &str) -> &str {
        self.values
            .get(name)
            .map(String::as_str)
            .unwrap_or_else(|| panic!("{name} is not a declared input"))
    }

    pub fn path(&self, name: &str) -> PathBuf {
        PathBuf::from(self.text(name))
    }

    pub fn bool(&self, name: &str) -> bool {
        self.text(name) == "true"
    }

    pub fn integer(&self, name: &str) -> i64 {
        self.text(name).parse().expect("validated integer")
    }

    pub fn number(&self, name: &str) -> f64 {
        self.text(name).parse().expect("validated number")
    }

    pub fn list(&self, name: &str) -> Vec<String> {
        self.text(name)
            .split(';')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(str::to_string)
            .collect()
    }
}

/// Every problem found while loading. Problems name inputs and layers, never values.
#[derive(Debug, PartialEq, Eq)]
pub struct InputError {
    pub problems: Vec<String>,
}

impl fmt::Display for InputError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "invalid inputs: {}", self.problems.join("; "))
    }
}

impl std::error::Error for InputError {}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Layer {
    Overlay,
    Protected,
    Process,
}

impl Layer {
    fn name(self) -> &'static str {
        match self {
            Self::Overlay => "the overlay file",
            Self::Protected => "the protected file",
            Self::Process => "the process environment",
        }
    }
}

pub fn load(sources: &Sources<'_>) -> Result<Environment, InputError> {
    let mut problems = Vec::new();
    let mut layered: BTreeMap<&'static str, String> = BTreeMap::new();

    let overlay_path = bootstrap_path(OVERLAY_FILE, sources, &mut problems);
    let protected_path = bootstrap_path(PROTECTED_FILE, sources, &mut problems);
    if let Some(path) = &overlay_path {
        apply_file(
            Layer::Overlay,
            (sources.read_overlay)(path),
            &mut layered,
            &mut problems,
        );
    }
    if let Some(path) = &protected_path {
        apply_file(
            Layer::Protected,
            (sources.read_protected)(path),
            &mut layered,
            &mut problems,
        );
    }
    for (name, value) in sources.process {
        match input(name) {
            Some(input) if input.origin == Origin::Process => {}
            Some(input) if admits(input, Layer::Process) => {
                layered.insert(input.name, value.clone());
            }
            Some(_) => problems.push(format!(
                "{name} is protected and may not be set in {}",
                Layer::Process.name()
            )),
            None if name.starts_with(OWN_PREFIX) => {
                problems.push(format!("{name} is not a declared input"));
            }
            None => {}
        }
    }

    let profile = match layered.get(PROFILE).map(|value| Profile::parse(value)) {
        Some(Some(profile)) => profile,
        Some(None) => {
            problems.push(format!(
                "{PROFILE} must be {}",
                Shape::Choice(PROFILES).describe()
            ));
            return Err(InputError { problems });
        }
        None => {
            problems.push(format!("{PROFILE} is required"));
            return Err(InputError { problems });
        }
    };

    let mut values = BTreeMap::new();
    for input in INPUTS {
        let value = match input.origin {
            Origin::Process => {
                let path = if input.name == OVERLAY_FILE {
                    &overlay_path
                } else {
                    &protected_path
                };
                path.as_ref().map(|path| path.display().to_string())
            }
            Origin::Layered => match layered.get(input.name) {
                Some(value) => Some(value.clone()),
                None => match input.default.for_profile(profile) {
                    Some(default) => expand(input.name, default, sources, &mut problems),
                    None => {
                        problems.push(format!(
                            "{} is required in the {} profile",
                            input.name,
                            profile.name()
                        ));
                        None
                    }
                },
            },
        };
        let Some(value) = value else {
            continue;
        };
        if input.shape.check(&value) {
            values.insert(input.name, value);
        } else {
            problems.push(format!("{} must be {}", input.name, input.shape.describe()));
        }
    }
    if problems.is_empty() {
        Ok(Environment { profile, values })
    } else {
        Err(InputError { problems })
    }
}

fn bootstrap_path(
    name: &'static str,
    sources: &Sources<'_>,
    problems: &mut Vec<String>,
) -> Option<PathBuf> {
    let value = match sources.process.get(name) {
        Some(value) => value.clone(),
        None => {
            let default = input(name)
                .and_then(|input| input.default.for_profile(Profile::Production))
                .expect("each layer path has a default");
            expand(name, default, sources, problems)?
        }
    };
    if Shape::Path.check(&value) {
        Some(PathBuf::from(value))
    } else {
        problems.push(format!("{name} must be {}", Shape::Path.describe()));
        None
    }
}

fn expand(
    name: &str,
    default: &str,
    sources: &Sources<'_>,
    problems: &mut Vec<String>,
) -> Option<String> {
    for (folder, token) in KnownFolder::ALL {
        if let Some(rest) = default.strip_prefix(token) {
            let Some(base) = (sources.known_folder)(folder) else {
                problems.push(format!("{name} has no default because {token} is unknown"));
                return None;
            };
            let base = base.display().to_string();
            return Some(format!("{}{rest}", base.trim_end_matches(['\\', '/'])));
        }
    }
    Some(default.to_string())
}

fn admits(input: &Input, layer: Layer) -> bool {
    match input.class {
        Class::Protected => layer == Layer::Protected,
        Class::Configuration => layer != Layer::Protected,
    }
}

fn apply_file(
    layer: Layer,
    read: FileLayer,
    layered: &mut BTreeMap<&'static str, String>,
    problems: &mut Vec<String>,
) {
    let content = match read {
        FileLayer::Absent => return,
        FileLayer::Present(content) => content,
        FileLayer::Unprotected => {
            problems.push(format!(
                "{} is readable by someone other than its owner",
                layer.name()
            ));
            return;
        }
        FileLayer::Unreadable(reason) => {
            problems.push(format!("{} is unreadable: {reason}", layer.name()));
            return;
        }
    };
    let mut seen = Vec::new();
    for (index, line) in content.lines().enumerate() {
        let line = line.trim_end_matches('\r');
        if line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        let Some((name, value)) = parse_line(line) else {
            problems.push(format!(
                "{} line {} is not NAME=value",
                layer.name(),
                index + 1
            ));
            continue;
        };
        let Some(input) = input(name) else {
            problems.push(format!("{} sets undeclared input {name}", layer.name()));
            continue;
        };
        if input.origin == Origin::Process || !admits(input, layer) {
            problems.push(format!("{} may not set {name}", layer.name()));
            continue;
        }
        if seen.contains(&input.name) {
            problems.push(format!("{} sets {name} twice", layer.name()));
            continue;
        }
        seen.push(input.name);
        layered.insert(input.name, value);
    }
}

/// Parse `NAME=value` or `NAME='value'`; both values are taken literally.
fn parse_line(line: &str) -> Option<(&str, String)> {
    let (name, value) = line.split_once('=')?;
    let valid_name = !name.is_empty()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_');
    if !valid_name {
        return None;
    }
    let value = match value.strip_prefix('\'') {
        Some(quoted) => quoted.strip_suffix('\'')?.to_string(),
        None => value.to_string(),
    };
    Some((name, value))
}

/// Render the tracked `.env`, which lists every input with its development
/// default for reading. Nothing loads it.
pub fn render_env() -> String {
    let mut output = String::from(
        "# Generated by `cargo run --bin ashe-worker-env`; never loaded.\n\
         # Every input of Ashe Worker with its development default.\n\
         # %APPDATA% and %LOCALAPPDATA% stand for the user's application data folders.\n",
    );
    for input in INPUTS {
        let class = match input.class {
            Class::Configuration => "configuration",
            Class::Protected => "protected",
        };
        let layers = match (input.origin, input.class) {
            (Origin::Process, _) => "process environment",
            (Origin::Layered, Class::Configuration) => "overlay file, process environment",
            (Origin::Layered, Class::Protected) => "protected file",
        };
        let production = match input.default.for_profile(Profile::Production) {
            Some("") => "empty".to_string(),
            Some(value) => value.to_string(),
            None => "none, required".to_string(),
        };
        output.push_str(&format!(
            "\n# {}\n# {class}, {}; set in: {layers}; production default: {production}\n",
            input.description,
            input.shape.describe(),
        ));
        match input.default.for_profile(Profile::Development) {
            Some(value) => output.push_str(&format!("{}={value}\n", input.name)),
            None => output.push_str(&format!("# required\n{}=\n", input.name)),
        }
    }
    output
}

#[cfg(test)]
mod unit {
    use super::*;
    use std::cell::RefCell;

    fn known(folder: KnownFolder) -> Option<PathBuf> {
        Some(PathBuf::from(match folder {
            KnownFolder::RoamingAppData => "C:\\Users\\u\\AppData\\Roaming",
            KnownFolder::LocalAppData => "C:\\Users\\u\\AppData\\Local",
        }))
    }

    fn overlay_lines() -> String {
        [
            "ASHE_WORKER_PROFILE=production",
            "ASHE_WORKER_LLM_BASE_URL=http://llm.test:8317/v1",
            "ASHE_WORKER_GRAMMAR_MODEL=g",
            "ASHE_WORKER_QUESTION_MODEL=q",
            "ASHE_WORKER_JOURNAL_MODEL=j",
            "ASHE_WORKER_SERVER_URL=http://server.test:8087",
            "OTEL_EXPORTER_OTLP_ENDPOINT=http://collector.test:24350",
        ]
        .join("\n")
    }

    fn protected_lines() -> String {
        [
            "ASHE_WORKER_LLM_API_KEY='synthetic-llm'",
            "ASHE_WORKER_SERVER_TOKEN='synthetic-server'",
            "OTEL_EXPORTER_OTLP_HEADERS='Authorization=Basic%20c3ludGhldGlj'",
        ]
        .join("\n")
    }

    fn load_with(
        process: &[(&str, &str)],
        overlay: FileLayer,
        protected_layer: FileLayer,
    ) -> Result<Environment, InputError> {
        let process: BTreeMap<String, String> = process
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        let overlay = RefCell::new(Some(overlay));
        let protected_layer = RefCell::new(Some(protected_layer));
        let read_overlay = |_: &Path| overlay.borrow_mut().take().unwrap_or(FileLayer::Absent);
        let read_protected = |_: &Path| {
            protected_layer
                .borrow_mut()
                .take()
                .unwrap_or(FileLayer::Absent)
        };
        load(&Sources {
            process: &process,
            known_folder: &known,
            read_overlay: &read_overlay,
            read_protected: &read_protected,
        })
    }

    fn has(error: &InputError, problem: &str) -> bool {
        error.problems.iter().any(|candidate| candidate == problem)
    }

    #[test]
    fn layers_apply_defaults_overlay_protected_file_and_process_in_order() {
        let environment = load_with(
            &[("ASHE_WORKER_BLOCK_MINUTES", "20"), ("PATH", "unrelated")],
            FileLayer::Present(format!(
                "{}\nASHE_WORKER_BLOCK_MINUTES=15\n",
                overlay_lines()
            )),
            FileLayer::Present(protected_lines()),
        )
        .unwrap();
        assert_eq!(environment.profile(), Profile::Production);
        assert_eq!(environment.integer("ASHE_WORKER_BLOCK_MINUTES"), 20);
        assert_eq!(environment.integer("ASHE_WORKER_CONTEXT_BLOCKS"), 6);
        assert_eq!(
            environment.text("ASHE_WORKER_SERVER_TOKEN"),
            "synthetic-server"
        );
        assert_eq!(
            environment.text("ASHE_WORKER_DATA_DIR"),
            "C:\\Users\\u\\AppData\\Local\\ashe-worker"
        );
        assert_eq!(
            environment.text(PROTECTED_FILE),
            "C:\\Users\\u\\AppData\\Roaming\\ashe-worker\\protected.env"
        );
    }

    #[test]
    fn protected_inputs_come_only_from_the_protected_file() {
        let error = load_with(
            &[("ASHE_WORKER_SERVER_TOKEN", "x")],
            FileLayer::Present(format!("{}\nASHE_WORKER_LLM_API_KEY=x", overlay_lines())),
            FileLayer::Present(format!(
                "{}\nASHE_WORKER_BLOCK_MINUTES=5",
                protected_lines()
            )),
        )
        .unwrap_err();
        assert!(has(
            &error,
            "ASHE_WORKER_SERVER_TOKEN is protected and may not be set in the process environment"
        ));
        assert!(has(
            &error,
            "the overlay file may not set ASHE_WORKER_LLM_API_KEY"
        ));
        assert!(has(
            &error,
            "the protected file may not set ASHE_WORKER_BLOCK_MINUTES"
        ));
    }

    #[test]
    fn a_present_layer_that_is_unprotected_or_malformed_fails_closed() {
        let error = load_with(
            &[],
            FileLayer::Present(format!("{}\nnot a line", overlay_lines())),
            FileLayer::Unprotected,
        )
        .unwrap_err();
        assert!(has(&error, "the overlay file line 8 is not NAME=value"));
        assert!(has(
            &error,
            "the protected file is readable by someone other than its owner"
        ));
    }

    #[test]
    fn required_inputs_are_reported_by_name_without_values() {
        let error = load_with(
            &[(PROFILE, "production")],
            FileLayer::Absent,
            FileLayer::Absent,
        )
        .unwrap_err();
        assert!(has(
            &error,
            "ASHE_WORKER_SERVER_URL is required in the production profile"
        ));
        assert!(has(
            &error,
            "OTEL_EXPORTER_OTLP_HEADERS is required in the production profile"
        ));
        let development = load_with(
            &[(PROFILE, "development")],
            FileLayer::Absent,
            FileLayer::Absent,
        )
        .unwrap_err();
        assert!(!has(
            &development,
            "OTEL_EXPORTER_OTLP_HEADERS is required in the development profile"
        ));
    }

    #[test]
    fn the_profile_is_required() {
        let error = load_with(&[], FileLayer::Absent, FileLayer::Absent).unwrap_err();
        assert_eq!(error.problems, vec![format!("{PROFILE} is required")]);
    }

    #[test]
    fn undeclared_names_duplicates_and_bad_shapes_are_refused() {
        let error = load_with(
            &[
                ("ASHE_WORKER_TYPO", "1"),
                ("ASHE_WORKER_SERVER_URL", "http://server.test/path"),
            ],
            FileLayer::Present(format!(
                "{}\nASHE_WORKER_MONITOR=1\nASHE_WORKER_MONITOR=2",
                overlay_lines()
            )),
            FileLayer::Present(protected_lines()),
        )
        .unwrap_err();
        assert!(has(&error, "ASHE_WORKER_TYPO is not a declared input"));
        assert!(has(
            &error,
            "ASHE_WORKER_SERVER_URL must be an http(s) origin"
        ));
        assert!(has(
            &error,
            "the overlay file sets ASHE_WORKER_MONITOR twice"
        ));
    }

    #[test]
    fn shapes_accept_only_their_values() {
        assert!(Shape::HttpOrigin.check("http://hq.test:8087"));
        assert!(!Shape::HttpOrigin.check("ftp://hq.test"));
        assert!(Shape::HttpUrl.check("https://api.test/v1"));
        assert!(!Shape::HttpUrl.check("https://user:pass@api.test/v1"));
        assert!(Shape::Path.check("D:\\data"));
        assert!(Shape::Path.check("\\\\server\\share"));
        assert!(!Shape::Path.check("data"));
        assert!(Shape::Bool.check("true") && !Shape::Bool.check("yes"));
        assert!(!Shape::Integer { min: 1, max: 2 }.check("3"));
    }

    #[test]
    fn quoted_and_literal_values_parse_alike() {
        assert_eq!(parse_line("A_B='x=y'"), Some(("A_B", "x=y".to_string())));
        assert_eq!(parse_line("A_B=x=y"), Some(("A_B", "x=y".to_string())));
        assert_eq!(parse_line("a=b"), None);
        assert_eq!(parse_line("A='unterminated"), None);
    }
}
