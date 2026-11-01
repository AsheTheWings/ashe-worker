use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const BLOCK_SCHEMA_VERSION: u32 = 3;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SoftwareDevelopmentRecord {
    #[serde(deserialize_with = "deserialize_nullable_string")]
    pub project: Option<String>,
    pub agentic: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub medium: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LearningMode {
    Lookup,
    Reading,
    Watching,
    Coursework,
    Practice,
    Discussion,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LearningSourceKind {
    SearchResults,
    Documentation,
    Article,
    Paper,
    Video,
    Course,
    Book,
    Forum,
    SocialPost,
    Code,
    Other,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum LearningDepth {
    Lookup,
    Orientation,
    FocusedExplanation,
    Procedural,
    Applied,
    Synthesis,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LearningSource {
    pub kind: LearningSourceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LearningRecord {
    pub modes: Vec<LearningMode>,
    pub search_queries: Vec<String>,
    pub sources: Vec<LearningSource>,
    pub depth: LearningDepth,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivitySubject {
    pub namespaces: Vec<String>,
    pub subject: String,
    pub estimated_duration_s: u64,
    pub unattended: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub software_development: Option<SoftwareDevelopmentRecord>,
}

impl ActivitySubject {
    pub fn is_software_development(&self) -> bool {
        self.namespaces
            .first()
            .is_some_and(|value| value.trim().eq_ignore_ascii_case("software-development"))
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityNarrative {
    pub title: String,
    pub report: String,
    pub subjects: Vec<ActivitySubject>,
    pub learning_subjects: Vec<LearningSubject>,
}

impl ActivityNarrative {
    pub fn parse(text: &str) -> Result<Self, String> {
        let generated: GeneratedActivityNarrative = serde_json::from_str(json_payload(text))
            .map_err(|error| format!("response was not the required JSON object: {error}"))?;
        let narrative = Self {
            title: generated.title,
            report: generated.report,
            subjects: generated
                .subjects
                .into_iter()
                .map(ActivitySubject::from)
                .collect(),
            learning_subjects: generated
                .learning_subjects
                .into_iter()
                .map(LearningSubject::from)
                .collect(),
        };
        narrative.validate()?;
        Ok(narrative)
    }

    pub fn validate_duration_budget(&self, duration_budget_s: u64) -> Result<(), String> {
        if let Some(subject) = self
            .subjects
            .iter()
            .find(|subject| subject.estimated_duration_s > duration_budget_s)
        {
            return Err(format!(
                "subject estimated_duration_s {} exceeds measured coverage {duration_budget_s}",
                subject.estimated_duration_s
            ));
        }
        if let Some(subject) = self
            .learning_subjects
            .iter()
            .find(|subject| subject.estimated_duration_s > duration_budget_s)
        {
            return Err(format!(
                "learning subject estimated_duration_s {} exceeds measured coverage {duration_budget_s}",
                subject.estimated_duration_s
            ));
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        if self.title.trim().is_empty() {
            return Err("title is empty".to_string());
        }
        if self.report.trim().is_empty() {
            return Err("report is empty".to_string());
        }
        if !(1..=8).contains(&self.subjects.len()) {
            return Err("subjects must contain between 1 and 8 entries".to_string());
        }
        for subject in &self.subjects {
            validate_common_subject(subject)?;
        }
        for subject in &self.learning_subjects {
            validate_learning_subject(subject)?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedActivityNarrative {
    title: String,
    report: String,
    subjects: Vec<GeneratedActivitySubject>,
    learning_subjects: Vec<GeneratedLearningSubject>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedActivitySubject {
    namespaces: Vec<String>,
    subject: String,
    estimated_duration_s: u64,
    unattended: bool,
    software_development: Option<SoftwareDevelopmentRecord>,
}

impl From<GeneratedActivitySubject> for ActivitySubject {
    fn from(subject: GeneratedActivitySubject) -> Self {
        Self {
            namespaces: subject.namespaces,
            subject: subject.subject,
            estimated_duration_s: subject.estimated_duration_s,
            unattended: subject.unattended,
            software_development: subject.software_development,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GeneratedLearningSubject {
    tags: Vec<String>,
    subject: String,
    estimated_duration_s: u64,
    learning: LearningRecord,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LearningSubject {
    pub tags: Vec<String>,
    pub subject: String,
    pub estimated_duration_s: u64,
    pub learning: LearningRecord,
}

impl From<GeneratedLearningSubject> for LearningSubject {
    fn from(subject: GeneratedLearningSubject) -> Self {
        Self {
            tags: subject.tags,
            subject: subject.subject,
            estimated_duration_s: subject.estimated_duration_s,
            learning: subject.learning,
        }
    }
}

fn validate_common_subject(subject: &ActivitySubject) -> Result<(), String> {
    validate_subject_fields(
        &subject.namespaces,
        &subject.subject,
        subject.estimated_duration_s,
    )?;
    if subject.is_software_development() != subject.software_development.is_some() {
        return Err(
            "software_development must be present exactly on software-development subjects"
                .to_string(),
        );
    }
    if subject
        .namespaces
        .iter()
        .any(|namespace| namespace.trim().eq_ignore_ascii_case("agentic-coding"))
    {
        return Err("agentic-coding is not an activity namespace".to_string());
    }
    if let Some(development) = &subject.software_development {
        if development
            .project
            .as_ref()
            .is_some_and(|project| project.trim().is_empty())
        {
            return Err("software_development project is empty".to_string());
        }
        if development.medium.is_some() != development.tool.is_some() {
            return Err(
                "software_development medium and tool must be present together".to_string(),
            );
        }
        if development
            .medium
            .as_ref()
            .is_some_and(|medium| !is_lowercase_kebab_case(medium))
        {
            return Err("software_development medium must be lowercase kebab-case".to_string());
        }
        if development.agentic && development.medium.is_none() {
            return Err(
                "agentic software_development requires a visible medium and tool".to_string(),
            );
        }
        if !development.agentic && development.model_id.is_some() {
            return Err("software_development model_id requires agentic=true".to_string());
        }
        if development
            .tool
            .as_ref()
            .is_some_and(|tool| tool.trim().is_empty())
        {
            return Err("software_development tool is empty".to_string());
        }
        if development
            .model_id
            .as_ref()
            .is_some_and(|model_id| model_id.trim().is_empty())
        {
            return Err("software_development model_id is empty".to_string());
        }
    }
    Ok(())
}

fn validate_learning_subject(subject: &LearningSubject) -> Result<(), String> {
    validate_statement(&subject.subject, subject.estimated_duration_s)?;
    if !(1..=8).contains(&subject.tags.len()) {
        return Err("learning subject tags must contain between 1 and 8 entries".to_string());
    }
    let mut tags = BTreeSet::new();
    for tag in &subject.tags {
        if !is_lowercase_kebab_case(tag) {
            return Err("learning subject tags must be lowercase kebab-case".to_string());
        }
        if !tags.insert(tag.as_str()) {
            return Err("learning subject tags must be unique".to_string());
        }
        if reserved_learning_tag(tag) {
            return Err(
                "learning subject tags must not repeat learning, modes, depth, or source kinds"
                    .to_string(),
            );
        }
    }
    if !(1..=3).contains(&subject.learning.modes.len()) {
        return Err("learning modes must contain between 1 and 3 entries".to_string());
    }
    let mut modes = BTreeSet::new();
    for mode in &subject.learning.modes {
        if !modes.insert(learning_mode_name(mode)) {
            return Err("learning modes must be unique".to_string());
        }
    }
    Ok(())
}

fn validate_subject_fields(
    namespaces: &[String],
    subject: &str,
    estimated_duration_s: u64,
) -> Result<(), String> {
    if !(1..=3).contains(&namespaces.len()) {
        return Err("subject namespaces must contain between 1 and 3 entries".to_string());
    }
    let mut unique = BTreeSet::new();
    for namespace in namespaces {
        if !is_lowercase_kebab_case(namespace) {
            return Err("subject namespaces must be lowercase kebab-case".to_string());
        }
        if !unique.insert(namespace.as_str()) {
            return Err("subject namespaces must be unique".to_string());
        }
    }
    validate_statement(subject, estimated_duration_s)
}

fn deserialize_nullable_string<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<String>::deserialize(deserializer)
}

fn validate_statement(subject: &str, estimated_duration_s: u64) -> Result<(), String> {
    if subject.trim().is_empty() {
        return Err("subject statement is empty".to_string());
    }
    if estimated_duration_s == 0 {
        return Err("subject estimated_duration_s must be positive".to_string());
    }
    Ok(())
}

fn is_lowercase_kebab_case(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.ends_with('-')
        && !value.contains("--")
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

fn learning_mode_name(mode: &LearningMode) -> &'static str {
    match mode {
        LearningMode::Lookup => "lookup",
        LearningMode::Reading => "reading",
        LearningMode::Watching => "watching",
        LearningMode::Coursework => "coursework",
        LearningMode::Practice => "practice",
        LearningMode::Discussion => "discussion",
    }
}

fn reserved_learning_tag(tag: &str) -> bool {
    tag == "learning"
        || [
            "lookup",
            "reading",
            "watching",
            "coursework",
            "practice",
            "discussion",
            "orientation",
            "focused-explanation",
            "procedural",
            "applied",
            "synthesis",
            "search-results",
            "documentation",
            "article",
            "paper",
            "video",
            "course",
            "book",
            "forum",
            "social-post",
            "code",
            "other",
        ]
        .contains(&tag)
}

fn json_payload(text: &str) -> &str {
    let trimmed = text.trim();
    strip_outer_fence(trimmed, "```json")
        .or_else(|| strip_outer_fence(trimmed, "```"))
        .unwrap_or(trimmed)
        .trim()
}

fn strip_outer_fence<'a>(text: &'a str, opening: &str) -> Option<&'a str> {
    let body = text.strip_prefix(opening)?;
    let body = body
        .strip_prefix("\r\n")
        .or_else(|| body.strip_prefix('\n'))?;
    body.strip_suffix("```")
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BlockArtifact {
    pub schema_version: u32,
    pub block: String,
    pub window_start: i64,
    pub window_end: i64,
    pub outcome: String,
    pub active_seconds: u64,
    pub idle_seconds: u64,
    pub app_seconds: BTreeMap<String, u64>,
    pub timeline: Vec<String>,
    pub frames_captured: usize,
    pub frames_sent: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keyframe: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    pub subjects: Vec<ActivitySubject>,
    pub learning_subjects: Vec<LearningSubject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct BlockDocumentInput {
    pub block: String,
    pub window_start: i64,
    pub window_end: i64,
    pub outcome: String,
    pub active_seconds: u64,
    pub idle_seconds: u64,
    pub app_seconds: BTreeMap<String, u64>,
    pub timeline: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub report: Option<String>,
    pub subjects: Vec<ActivitySubject>,
    pub learning_subjects: Vec<LearningSubject>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl BlockArtifact {
    pub fn has_current_schema(&self) -> bool {
        self.schema_version == BLOCK_SCHEMA_VERSION
    }

    pub fn document_input(&self) -> BlockDocumentInput {
        BlockDocumentInput {
            block: self.block.clone(),
            window_start: self.window_start,
            window_end: self.window_end,
            outcome: self.outcome.clone(),
            active_seconds: self.active_seconds,
            idle_seconds: self.idle_seconds,
            app_seconds: self.app_seconds.clone(),
            timeline: self.timeline.clone(),
            title: self.title.clone(),
            report: self.report.clone(),
            subjects: self.subjects.clone(),
            learning_subjects: self.learning_subjects.clone(),
            reason: self.reason.clone(),
            error: self.error.clone(),
        }
    }
}

#[cfg(test)]
mod unit {
    use super::{
        ActivityNarrative, BLOCK_SCHEMA_VERSION, BlockArtifact, LearningDepth, LearningMode,
        LearningSourceKind,
    };

    const UNIFIED: &str = r#"{
        "title":"Unified activity and learning generation",
        "report":"Used Codex to revise the worker and reviewed the schema implications.",
        "subjects":[{
            "namespaces":["software-development","coding","schema-update"],
            "subject":"Used Codex to unify activity and learning generation.",
            "estimated_duration_s":300,
            "unattended":false,
            "software_development":{"project":"ashe-worker","agentic":true,"medium":"tui","tool":"codex","model_id":"gpt-5.4"}
        }],
        "learning_subjects":[{
            "tags":["rust","serde","schema-evolution"],
            "subject":"Applied strict Serde fields to a unified artifact schema.",
            "estimated_duration_s":120,
            "learning":{
                "modes":["reading","practice"],
                "search_queries":[],
                "sources":[{"kind":"code","title":"block_artifact.rs"}],
                "depth":"applied"
            }
        }]
    }"#;

    #[test]
    fn unified_narrative_preserves_software_development_and_learning_metadata() {
        let narrative = ActivityNarrative::parse(UNIFIED).unwrap();
        let development = narrative.subjects[0].software_development.as_ref().unwrap();
        assert_eq!(development.project.as_deref(), Some("ashe-worker"));
        assert!(development.agentic);
        assert_eq!(development.medium.as_deref(), Some("tui"));
        assert_eq!(development.tool.as_deref(), Some("codex"));
        assert_eq!(development.model_id.as_deref(), Some("gpt-5.4"));
        assert_eq!(narrative.learning_subjects[0].tags[0], "rust");
        let learning = &narrative.learning_subjects[0].learning;
        assert_eq!(
            learning.modes,
            vec![LearningMode::Reading, LearningMode::Practice]
        );
        assert_eq!(learning.depth, LearningDepth::Applied);
        assert_eq!(learning.sources[0].kind, LearningSourceKind::Code);
    }

    #[test]
    fn unified_narrative_accepts_a_validated_empty_learning_result() {
        let mut value: serde_json::Value = serde_json::from_str(UNIFIED).unwrap();
        value["learning_subjects"] = serde_json::json!([]);
        let narrative = ActivityNarrative::parse(&serde_json::to_string(&value).unwrap()).unwrap();
        assert!(narrative.learning_subjects.is_empty());
    }

    #[test]
    fn software_development_metadata_is_required_exactly_on_software_subjects() {
        assert!(ActivityNarrative::parse(&UNIFIED.replace(
            ",\n            \"software_development\":{\"project\":\"ashe-worker\",\"agentic\":true,\"medium\":\"tui\",\"tool\":\"codex\",\"model_id\":\"gpt-5.4\"}",
            "",
        ))
        .is_err());
        assert!(
            ActivityNarrative::parse(
                &UNIFIED.replace("\"software-development\"", "\"personal-administration\"",)
            )
            .is_err()
        );
    }

    #[test]
    fn manual_software_development_accepts_an_editor_without_a_model() {
        let text = UNIFIED
            .replace("\"agentic\":true", "\"agentic\":false")
            .replace("\"medium\":\"tui\"", "\"medium\":\"code-editor\"")
            .replace("\"tool\":\"codex\"", "\"tool\":\"cursor\"")
            .replace(",\"model_id\":\"gpt-5.4\"", "");
        let narrative = ActivityNarrative::parse(&text).unwrap();
        let development = narrative.subjects[0].software_development.as_ref().unwrap();
        assert!(!development.agentic);
        assert_eq!(development.medium.as_deref(), Some("code-editor"));
        assert_eq!(development.tool.as_deref(), Some("cursor"));
        assert_eq!(development.model_id, None);
    }

    #[test]
    fn software_development_requires_an_explicit_project_field() {
        assert!(
            ActivityNarrative::parse(&UNIFIED.replace("\"project\":\"ashe-worker\",", "")).is_err()
        );
        assert!(
            ActivityNarrative::parse(
                &UNIFIED.replace("\"project\":\"ashe-worker\"", "\"project\":null")
            )
            .is_ok()
        );
    }

    #[test]
    fn software_development_medium_accepts_terminal_and_extensible_values() {
        assert!(ActivityNarrative::parse(&UNIFIED.replace("\"tui\"", "\"terminal\"")).is_ok());
        assert!(ActivityNarrative::parse(&UNIFIED.replace("\"tui\"", "\"web-ide\"")).is_ok());
        assert!(ActivityNarrative::parse(&UNIFIED.replace("\"tui\"", "\"Web IDE\"")).is_err());
    }

    #[test]
    fn model_id_requires_agentic_software_development() {
        assert!(
            ActivityNarrative::parse(&UNIFIED.replace("\"agentic\":true", "\"agentic\":false"))
                .is_err()
        );
    }

    #[test]
    fn unified_narrative_enforces_learning_tags_modes_source_kind_and_depth() {
        assert!(ActivityNarrative::parse(&UNIFIED.replace("\"rust\"", "\"Learning\"")).is_err());
        assert!(
            ActivityNarrative::parse(&UNIFIED.replace(
                "\"rust\",\"serde\",\"schema-evolution\"",
                "\"rust\",\"rust\",\"schema-evolution\"",
            ))
            .is_err()
        );
        assert!(
            ActivityNarrative::parse(
                &UNIFIED.replace("\"reading\",\"practice\"", "\"reading\",\"reading\"",)
            )
            .is_err()
        );
        assert!(ActivityNarrative::parse(&UNIFIED.replace("\"code\"", "\"webpage\"")).is_err());
        assert!(ActivityNarrative::parse(&UNIFIED.replace("\"applied\"", "\"deep\"")).is_err());
    }

    #[test]
    fn activity_namespaces_are_limited_to_three() {
        assert!(
            ActivityNarrative::parse(&UNIFIED.replace(
                "\"software-development\",\"coding\",\"schema-update\"",
                "\"software-development\",\"coding\",\"schema-update\",\"extra\"",
            ))
            .is_err()
        );
    }

    #[test]
    fn only_the_current_strict_block_schema_is_accepted() {
        let current = format!(
            r#"{{
                "schema_version":{BLOCK_SCHEMA_VERSION},"block":"2026-07-28T1200",
                "window_start":1,"window_end":2,"outcome":"described",
                "active_seconds":1,"idle_seconds":0,"app_seconds":{{}},"timeline":[],
                "frames_captured":1,"frames_sent":1,"subjects":[
                    {{"namespaces":["software-development"],"subject":"Worked.",
                    "estimated_duration_s":1,"unattended":false,
                    "software_development":{{"project":"ashe-worker","agentic":false}}}}
                ],"learning_subjects":[]
            }}"#
        );
        let artifact: BlockArtifact = serde_json::from_str(&current).unwrap();
        assert!(artifact.has_current_schema());
        assert_eq!(artifact.frames_sent, 1);

        assert!(
            serde_json::from_str::<BlockArtifact>(&current.replace(
                &format!("\"schema_version\":{BLOCK_SCHEMA_VERSION}"),
                "\"schema_version\":1"
            ))
            .is_ok_and(|artifact| !artifact.has_current_schema())
        );
        assert!(
            serde_json::from_str::<BlockArtifact>(
                &current.replace("\"frames_sent\":1", "\"base_frames_sent\":1")
            )
            .is_err()
        );
        assert!(
            serde_json::from_str::<BlockArtifact>(&current.replace(",\"unattended\":false", ""))
                .is_err()
        );
    }

    #[test]
    fn structured_outputs_accept_one_exact_outer_fence_only() {
        let object = r#"{"title":"Focused work","report":"A concise report.","subjects":[{"namespaces":["personal-administration"],"subject":"Worked.","estimated_duration_s":60,"unattended":false}],"learning_subjects":[]}"#;
        assert!(ActivityNarrative::parse(&format!("```json\n{object}\n```")).is_ok());
        assert!(ActivityNarrative::parse(&format!("preamble\n{object}")).is_err());
    }
}
