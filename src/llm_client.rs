use crate::block_artifact::ActivityNarrative;
use crate::config::AppConfig;
use anyhow::{Context, Result, anyhow};
use base64::Engine;
use serde_json::{Value, json};

const GRAMMAR_PROMPT: &str = "Fix grammar, spelling, punctuation, and casing in the user's text. Preserve the original meaning, tone, intent, and language. Do not add or remove information. Do not explain. Return only the corrected text.";
const QUESTION_PROMPT: &str = "You are a helpful assistant. Answer the user's question directly and concisely. Do not restate the question. Reply in the same language as the question. Return only the answer.";
const QUESTION_TEMPERATURE: f32 = 0.7;

const ACTIVITY_PROMPT: &str = "You are Ashe Worker's activity describer. Produce a factual account of what the user did during this time block, whether software development, learning, entertainment, social media, messaging, shopping, games, or personal administration. Treat the measured app/window timeline as ground truth. Read visible details closely and describe progression rather than listing images. Never score productivity. Do not mention screenshots, frames, telemetry, prompts, JSON, or yourself. Keep credentials, financial values, medical details, and intimate conversations at a safe high level.\n\nRepresent meaningful learning activity in the broad activity subjects whenever the user acquires, examines, follows, applies, or synthesizes information, including a term lookup or informational media consumption. Learning may overlap another supported activity subject. Informational or intellectual media is learning; narrative leisure media and sports are entertainment.\n\nGive additional preference to sustained software-development work. Record its concrete project separately from its namespaces. Mark agentic=true when an LLM coding agent primarily performs the development and agentic=false when the user primarily works manually. When an agent or development environment is visibly identifiable, record an evidence-backed medium and concrete tool. Common medium values include code-editor for graphical editors such as Cursor, Devin, Zed, or VSCode; tui for dedicated terminal interfaces such as OpenCode or Codex; and terminal for shell or command-line use. Other accurately observed media are allowed. Record a model identifier only for agentic work and only when it is actually visible; never infer a model from the tool, provider, task, or appearance. Use separate subjects when sustained work visibly switches project, development activity, tool, medium, or model so each subject's metadata remains accurate.";
const LEARNING_PROMPT: &str = "Independently validate and document intellectual learning within the same activity block. Broad learning activity subjects identify material worth inspecting, but they are not conclusions and may all fail the stricter learning-unit standard. Cross-check every candidate against the measured context and chronological visual evidence before deciding whether the block contains any genuine learning units.\n\nA learning unit is a discrete piece of transferable intellectual content: a concept, mechanism, causal relationship, rationale, method, procedure with explanatory substance, comparison, argument, model, or principle that the user meaningfully examined, followed, reasoned about, or applied. The unit must identify that knowledge content, not merely an activity, source, query, answer, outcome, or intention. Its value should remain intelligible beyond the immediate action that occasioned it.\n\nDistinguish intellectual engagement from information retrieval and task execution. Searching, reading, watching, asking a question, receiving an answer, operating software, executing commands, editing code, testing, or delegating work are only evidence channels; none independently establishes learning. Immediate usefulness is also insufficient when the information's value is exhausted by the current transaction, navigation step, status check, selection, or one-off decision. Applied learning requires visible evidence that a principle or method was reasoned through and used, not merely that an action was performed or succeeded.\n\nFor each candidate, determine whether the evidence exposes substantive meaning, operation, relationships, constraints, reasoning, or implications. Reject candidates supported only by a query, title, brief incidental answer, activity label, elapsed time, source prestige, or a broad activity subject. Consolidate repeated treatment of the same idea, and separate units only when their intellectual content is meaningfully distinct. Report only what was visibly examined; do not claim novelty, mastery, comprehension, retention, or educational intent. If nothing meets this standard, return no learning units. Do not manufacture a unit merely because learning activity is present. Keep credentials, financial values, medical details, and intimate conversations at a safe high level.";
const SUMMARY_PROMPT: &str = "Maintain a continuous rolling summary of the user's computer activity and learning from the structured JSON input. Merge previous_summary with blocks. Preserve concrete software-development project, agentic state, tool, visible model, app, document, site, game, media, person, open-task, and validated learning-topic names; compress routine detail; preserve rough chronology and current state. The blocks array is the only new material. Treat measured fields as ground truth and do not infer later activity. Return Markdown prose only, without a title or preamble.";

pub async fn describe_activity_block(
    config: AppConfig,
    context: String,
    frames: Vec<(String, Vec<u8>)>,
    duration_budget_s: u64,
) -> Result<ActivityNarrative> {
    let endpoint = format!("{}/responses", config.llm_base_url);
    let mut content = vec![json!({ "type": "input_text", "text": context })];
    for (label, bytes) in frames {
        content.push(json!({ "type": "input_text", "text": label }));
        content.push(json!({
            "type": "input_image",
            "image_url": format!(
                "data:image/webp;base64,{}",
                base64::engine::general_purpose::STANDARD.encode(bytes)
            )
        }));
    }
    let instructions = activity_instructions(duration_budget_s);
    let mut body = json!({
        "model": config.journal_model,
        "instructions": instructions,
        "input": [{ "role": "user", "content": content }]
    });
    apply_reasoning_effort(&mut body, config.llm_reasoning_effort.as_deref());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("failed to build activity HTTP client")?;
    let response = client
        .post(endpoint)
        .bearer_auth(&config.llm_api_key)
        .json(&body)
        .send()
        .await
        .context("activity description request failed")?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .context("activity description returned a non-JSON body")?;
    if !status.is_success() {
        let message = payload
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| payload.get("detail").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(anyhow!("activity description failed ({status}): {message}"));
    }
    let narrative = ActivityNarrative::parse(&extract_output_text(&payload))
        .map_err(|message| anyhow::Error::new(InvalidActivityOutput(message)))?;
    narrative
        .validate_duration_budget(duration_budget_s)
        .map_err(|message| anyhow::Error::new(InvalidActivityOutput(message)))?;
    Ok(narrative)
}

fn activity_instructions(duration_budget_s: u64) -> String {
    format!(
        "{ACTIVITY_PROMPT}\n\n{LEARNING_PROMPT}\n\nReturn exactly one JSON object and nothing else: no Markdown fence, preamble, or trailing commentary. The object must contain exactly title, report, subjects, and learning_subjects.\n\ntitle is a short factual plain-text phrase describing the dominant progression. Do not prepend an area, domain, or namespace label. report is 120-250 words of concise Markdown prose covering the block as a whole.\n\nsubjects is an array of 1-8 sustained, meaningfully distinct activity subjects; do not split incidental actions into separate entries. Every activity subject contains namespaces, subject, estimated_duration_s, unattended, and software_development only when required below. namespaces is an ordered array of 1-3 unique lowercase kebab-case strings. Level 1 is the broad domain; level 2 is the domain-specific activity mode or channel; level 3 is the concrete focus such as a feature, media type or title, event, thread topic, or activity topic. Prefer these exact roots when applicable: software-development, entertainment, social-media, learning. Under software-development use level 2 for the actual development activity, such as coding, debugging, testing, reviewing, planning, or operations; use level 3 for the concrete feature or technical focus, not the project. Agentic execution is metadata and must never appear as an agentic-coding namespace. Under entertainment use streaming followed by movie, tv-show, live-stream, or football-match when supported. Under social-media use the canonical service slug such as x-com. Under learning use a supported broad activity mode such as lookup, reading, watching, coursework, practice, or discussion. Do not invent specificity. Put actions and outcomes in subject, not namespaces, and use consistent namespace spelling within the response. subject is one self-contained factual statement. estimated_duration_s is a positive integer estimating time spent on that subject and must remain within the {duration_budget_s} seconds of measured coverage. unattended is true only when the subject appears to have continued for most of its estimated duration with little evidence of user interaction or attention. Measured input idle is evidence, not proof of absence. Default passive reading, films, television, and live streams to unattended=false unless the combined timeline and progression reasonably show that they were left running or left unchanged. Judge each subject independently: subjects may overlap when the user multitasks, so their estimated durations do not need to sum to the coverage duration. Uncertain, idle, or unclassified time may remain unallocated; passive activity may receive time even without keyboard or mouse input. Use the measured timeline and visible progression rather than dividing time evenly.\n\nEvery subject rooted at software-development must include software_development. software_development always contains project and agentic. project is the visible project, repository, or workspace name as a string, or null when it cannot be identified without guessing. agentic is true when an LLM coding agent primarily performs the development and false when the user primarily works manually. medium and tool must either both be present or both be omitted. agentic=true requires both: medium is an evidence-backed lowercase kebab-case interface type and tool is a canonical lowercase kebab-case product name. Prefer code-editor for graphical editors such as cursor, devin, zed, or vscode; tui for dedicated terminal interfaces such as opencode or codex; and terminal for shell or command-line use. These are examples, not an exhaustive medium enum. agentic=false may include medium and tool when the manual development environment is clearly visible. model_id is allowed only when agentic=true and the model identifier is visible; preserve it exactly as shown, including provider qualification, and otherwise omit it. Every non-software-development subject must omit software_development.\n\nlearning_subjects is an array containing zero or more independently validated atomic learning subjects; an empty array is the correct result when no candidate qualifies. Each learning subject contains exactly tags, subject, estimated_duration_s, and learning. tags contains 1-8 unique lowercase kebab-case strings describing only the knowledge fields, technologies, concepts, and atomic topic. Tags are flat and unordered. Do not include learning itself, a learning mode, a depth, or a source kind as a tag. subject is one cautious factual statement naming the intellectual content and using observable verbs such as examined, reviewed, worked through, compared, or applied. estimated_duration_s is positive and must remain within {duration_budget_s} seconds of measured coverage; independently judged subjects may overlap. Attention, unattended state, and software-development environment metadata belong exclusively to activity subjects and must not be returned here. learning contains exactly modes, search_queries, sources, and depth. modes contains 1-3 unique values chosen from lookup, reading, watching, coursework, practice, or discussion and records how the user engaged with the material. search_queries contains only visibly entered or displayed queries. sources contains visible source objects: each has kind and may have title, provider, and creator only when supported. kind is exactly one of search-results, documentation, article, paper, video, course, book, forum, social-post, code, or other. depth is the strongest observable treatment after a candidate has independently qualified as intellectual learning: lookup for a brief definition or short answer; orientation for purpose and high-level structure; focused-explanation for mechanism, relationships, rationale, examples, or tradeoffs; procedural for followed steps whose method was substantively examined; applied for visible reasoned use of a principle or method; synthesis for visibly comparing or combining multiple sources or ideas. Choose the lower supported depth when uncertain. A depth of lookup does not make merely transactional or incidental retrieval eligible. Time spent, source prestige, technical difficulty, multiple open tabs, and input-idle state do not by themselves establish either eligibility or depth."
    )
}

pub async fn refresh_activity_summary(
    config: AppConfig,
    source: Value,
    max_chars: usize,
) -> Result<String> {
    let endpoint = format!("{}/responses", config.llm_base_url);
    let input = serde_json::to_string_pretty(&source)
        .context("failed to serialize rolling-summary source")?;
    let mut body = json!({
        "model": config.journal_model,
        "instructions": format!("{SUMMARY_PROMPT}\n\nThe complete result must contain at most {max_chars} Unicode characters."),
        "input": [{
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": input,
            }]
        }]
    });
    apply_reasoning_effort(&mut body, config.llm_reasoning_effort.as_deref());
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .context("failed to build summary HTTP client")?;
    let response = client
        .post(endpoint)
        .bearer_auth(&config.llm_api_key)
        .json(&body)
        .send()
        .await
        .context("rolling-summary request failed")?;
    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .context("rolling-summary response was not JSON")?;
    if !status.is_success() {
        let message = payload
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| payload.get("detail").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(anyhow!(
            "rolling-summary request failed ({status}): {message}"
        ));
    }
    let summary = extract_output_text(&payload);
    let summary = summary.trim();
    if summary.is_empty() {
        return Err(anyhow!("rolling-summary response was empty"));
    }
    Ok(summary.chars().take(max_chars).collect())
}

#[derive(Debug)]
pub struct InvalidActivityOutput(pub String);

impl std::fmt::Display for InvalidActivityOutput {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "invalid structured activity output: {}", self.0)
    }
}

impl std::error::Error for InvalidActivityOutput {}

/// Correct grammar/spelling/punctuation in selected text and return only the rewrite.
///
/// Used by the `Win+Shift+G` flow, where the result replaces the original selection.
pub async fn fix_grammar(config: AppConfig, text: String) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Ok(String::new());
    }
    let message = format!("{GRAMMAR_PROMPT}\n\n{}", tagged("text", text));
    request_response(
        &config,
        &config.grammar_model,
        message,
        config.llm_temperature,
    )
    .await
}

/// Answer the selected text as a general question and return only the answer.
///
/// Used by the `Win+Shift+Q` flow, where the answer is appended after the selection.
pub async fn answer_question(config: AppConfig, question: String) -> Result<String> {
    let question = question.trim();
    if question.is_empty() {
        return Ok(String::new());
    }
    let message = format!("{QUESTION_PROMPT}\n\n{}", tagged("question", question));
    request_response(
        &config,
        &config.question_model,
        message,
        QUESTION_TEMPERATURE,
    )
    .await
}

/// Shared Open Responses (`/v1/responses`) call: sends a single `role: "user"` message
/// (prompt + tag-wrapped target text) and reads the assistant `output_text` back out.
/// The optional reasoning effort is forwarded when configured.
async fn request_response(
    config: &AppConfig,
    model: &str,
    user_content: String,
    temperature: f32,
) -> Result<String> {
    let endpoint = format!("{}/responses", config.llm_base_url);
    let mut body = json!({
        "model": model,
        "input": [
            {
                "type": "message",
                "role": "user",
                "content": user_content,
            }
        ],
        "stream": false,
        "temperature": temperature,
    });
    apply_reasoning_effort(&mut body, config.llm_reasoning_effort.as_deref());

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .context("failed to build reqwest client")?;
    let response = client
        .post(&endpoint)
        .bearer_auth(&config.llm_api_key)
        .json(&body)
        .send()
        .await
        .context("responses request failed")?;

    let status = response.status();
    let payload: Value = response
        .json()
        .await
        .context("responses returned a non-JSON body")?;

    if !status.is_success() {
        let message = payload
            .pointer("/error/message")
            .and_then(Value::as_str)
            .or_else(|| payload.get("detail").and_then(Value::as_str))
            .unwrap_or("unknown error");
        return Err(anyhow!("responses failed ({status}): {message}"));
    }

    let text = extract_output_text(&payload);
    let text = text.trim();
    if text.is_empty() {
        return Err(anyhow!("responses returned an empty response"));
    }
    Ok(text.to_string())
}

fn apply_reasoning_effort(body: &mut Value, effort: Option<&str>) {
    let Some(effort) = effort else {
        return;
    };
    if let Some(object) = body.as_object_mut() {
        object.insert("reasoning".to_string(), json!({ "effort": effort }));
    }
}

/// Collect the assistant-visible text from a Responses `output` array. Reasoning items
/// (which carry no `output_text`) are skipped; refusals are surfaced as their text.
fn extract_output_text(payload: &Value) -> String {
    let mut out = String::new();
    let Some(items) = payload.get("output").and_then(Value::as_array) else {
        return out;
    };
    for item in items {
        if item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("message")
            != "message"
        {
            continue;
        }
        match item.get("content") {
            Some(Value::String(content)) => out.push_str(content),
            Some(Value::Array(parts)) => {
                for part in parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("output_text") | Some("input_text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                out.push_str(text);
                            }
                        }
                        Some("refusal") => {
                            if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                                out.push_str(text);
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// Wrap `body` in XML-style tags to delimit the target text from the prompt and focus
/// the model's attention on what to operate on.
fn tagged(tag: &str, body: &str) -> String {
    format!("<{tag}>\n{body}\n</{tag}>")
}

#[cfg(test)]
mod unit {
    use super::{activity_instructions, apply_reasoning_effort};
    use serde_json::json;

    #[test]
    fn reasoning_is_omitted_by_default() {
        let mut body = json!({ "model": "example" });
        apply_reasoning_effort(&mut body, None);
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn arbitrary_reasoning_effort_is_forwarded_unchanged() {
        let mut body = json!({ "model": "example" });
        apply_reasoning_effort(&mut body, Some("upstream-specific-value"));
        assert_eq!(
            body.pointer("/reasoning/effort"),
            Some(&json!("upstream-specific-value"))
        );
    }

    #[test]
    fn unified_instructions_preserve_activity_learning_and_agentic_detail() {
        let instructions = activity_instructions(600);
        assert!(instructions.contains("exactly title, report, subjects, and learning_subjects"));
        assert!(instructions.contains("Do not prepend an area, domain, or namespace label"));
        assert!(instructions.contains("1-3 unique lowercase kebab-case strings"));
        assert!(instructions.contains("not conclusions"));
        assert!(instructions.contains("information retrieval and task execution"));
        assert!(instructions.contains("Immediate usefulness is also insufficient"));
        assert!(instructions.contains("not merely that an action was performed or succeeded"));
        assert!(instructions.contains("an empty array is the correct result"));
        assert!(instructions.contains("within 600 seconds of measured coverage"));
        assert!(instructions.contains("always contains project and agentic"));
        assert!(instructions.contains("terminal for shell or command-line use"));
        assert!(instructions.contains("not an exhaustive medium enum"));
        assert!(instructions.contains("model_id is allowed only when agentic=true"));
        assert!(instructions.contains("Agentic execution is metadata"));
        assert!(instructions.contains("Each learning subject contains exactly tags"));
        assert!(instructions.contains("learning contains exactly modes"));
    }
}
