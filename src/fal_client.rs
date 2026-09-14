//! Speech-to-text through fal.ai's queue API (ElevenLabs Scribe v2).
//!
//! Dictation buffers microphone audio locally while recording and calls
//! [`transcribe_pcm`] once on stop: the WAV is embedded directly in a queue
//! request and polling waits for completion. A single full-utterance request
//! produces a more accurate result than committing streaming partials.

use crate::config::AppConfig;
use crate::logger;
use anyhow::{Context, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use serde_json::{Map, Value};
use std::time::{Duration, Instant};

/// fal.ai queue gateway; the model id appends as the path.
const QUEUE_BASE_URL: &str = "https://queue.fal.run";
/// Speech-to-text engine. Dictation is built around this model's input
/// and output shape, so it is fixed here rather than configured.
const STT_MODEL: &str = "fal-ai/elevenlabs/speech-to-text/scribe-v2";
/// Pause between queue status polls.
const POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Status polls before giving up (~10 minutes, past any dictation length).
const POLL_ATTEMPTS: u32 = 1_200;
/// Stop a dead route from inheriting Windows' roughly 21-second TCP wait.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Retry one transient submit failure before surfacing an error.
const SUBMIT_ATTEMPTS: u32 = 2;
const SUBMIT_RETRY_DELAY: Duration = Duration::from_millis(250);
/// Per-request ceiling for submit, status, and result fetches.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
const WAV_DATA_URI_PREFIX: &str = "data:audio/wav;base64,";

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptWord {
    pub text: String,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub kind: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct FalTranscript {
    pub text: String,
    pub words: Vec<TranscriptWord>,
}

/// Transcribe a complete recording with one queue request.
pub async fn transcribe_pcm(
    config: AppConfig,
    sample_rate: u32,
    pcm: Vec<u8>,
) -> Result<FalTranscript> {
    validate_capture(&pcm, sample_rate)?;
    let seconds = pcm.len() as f64 / f64::from(sample_rate) / 2.0;
    logger::info(format!(
        "fal transcription model={} sample_rate={} bytes={} seconds={:.1}",
        STT_MODEL,
        sample_rate,
        pcm.len(),
        seconds,
    ));
    let started = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .context("failed to create HTTP client")?;
    let wav = encode_wav_mono16(&pcm, sample_rate);
    drop(pcm);
    let audio_url = wav_data_uri(&wav);
    drop(wav);
    let encoded = Instant::now();
    let queued = submit_transcription(&client, &config, audio_url).await?;
    let submitted = Instant::now();
    wait_completed(&client, &config.fal_api_key, &queued).await?;
    let completed = Instant::now();
    let fetched = fetch_transcript(&client, &config.fal_api_key, &queued).await?;
    // Verbalized punctuation ("comma", "double quote") arrives as literal
    // words: the transcription model has no dictation-command layer, so the
    // conversion runs here, on the timed word stream the composition
    // mapping aligns on.
    let transcript = if config.spoken_punctuation {
        crate::spoken_punctuation::apply_spoken_punctuation(&fetched)
    } else {
        fetched
    };
    let finished = Instant::now();
    logger::info(format!(
        "fal transcript chars={} encode_ms={} submit_ms={} queue_ms={} result_ms={} total_ms={}",
        transcript.text.len(),
        encoded.duration_since(started).as_millis(),
        submitted.duration_since(encoded).as_millis(),
        completed.duration_since(submitted).as_millis(),
        finished.duration_since(completed).as_millis(),
        finished.duration_since(started).as_millis()
    ));
    Ok(transcript)
}

fn validate_capture(pcm: &[u8], sample_rate: u32) -> Result<()> {
    if pcm.is_empty() {
        return Err(anyhow!("no audio captured"));
    }
    if !pcm.len().is_multiple_of(2) {
        return Err(anyhow!("captured PCM has an odd byte count"));
    }
    if sample_rate == 0 {
        return Err(anyhow!("capture sample rate must be greater than zero"));
    }
    Ok(())
}

fn auth_header(api_key: &str) -> String {
    format!("Key {}", api_key.trim())
}

/// Encode a complete WAV as the Base64 data URI accepted by Scribe v2.
fn wav_data_uri(wav: &[u8]) -> String {
    let encoded_len = wav.len().div_ceil(3) * 4;
    let mut uri = String::with_capacity(WAV_DATA_URI_PREFIX.len() + encoded_len);
    uri.push_str(WAV_DATA_URI_PREFIX);
    BASE64_STANDARD.encode_string(wav, &mut uri);
    uri
}

/// A queued request with the follow-up URLs the queue assigns it. The
/// queue answers under its canonical app route, which can differ from
/// the submitted model alias, so polling and fetching must use these
/// URLs instead of rebuilding routes from the model id.
struct QueuedRequest {
    request_id: String,
    status_url: String,
    response_url: String,
}

/// Queue the transcription and return its follow-up URLs for polling.
async fn submit_transcription(
    client: &reqwest::Client,
    config: &AppConfig,
    audio_url: String,
) -> Result<QueuedRequest> {
    let url = format!("{QUEUE_BASE_URL}/{STT_MODEL}");
    let input = scribe_input_owned(audio_url, &config.fal_language);
    let mut attempt = 1;
    let response = loop {
        match client
            .post(&url)
            .header("Authorization", auth_header(&config.fal_api_key))
            .json(&input)
            .send()
            .await
        {
            Ok(response) => break response,
            Err(error)
                if attempt < SUBMIT_ATTEMPTS && (error.is_connect() || error.is_timeout()) =>
            {
                logger::info(format!(
                    "fal queue submit attempt={attempt} failed transiently; retrying: {error}"
                ));
                attempt += 1;
                tokio::time::sleep(SUBMIT_RETRY_DELAY).await;
            }
            Err(error) => return Err(error).context("fal queue submit request failed"),
        }
    };
    let submit = response
        .error_for_status()
        .context("fal queue submit rejected")?
        .json::<Value>()
        .await
        .context("fal queue submit returned invalid JSON")?;
    let queued = parse_submit_response(&submit)?;
    logger::info(format!(
        "fal transcription queued request_id={}",
        queued.request_id
    ));
    Ok(queued)
}

/// Poll the request status until it completes or fails.
async fn wait_completed(
    client: &reqwest::Client,
    api_key: &str,
    queued: &QueuedRequest,
) -> Result<()> {
    for _ in 1..=POLL_ATTEMPTS {
        let status = client
            .get(&queued.status_url)
            .header("Authorization", auth_header(api_key))
            .send()
            .await
            .context("fal status request failed")?
            .error_for_status()
            .context("fal status request rejected")?
            .json::<Value>()
            .await
            .context("fal status returned invalid JSON")?;
        match parse_status(&status)? {
            QueueState::Done => return Ok(()),
            QueueState::Pending => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
    Err(anyhow!(
        "fal transcription request {} did not complete in time",
        queued.request_id
    ))
}

/// Fetch the completed output and read its text and word-level timing.
async fn fetch_transcript(
    client: &reqwest::Client,
    api_key: &str,
    queued: &QueuedRequest,
) -> Result<FalTranscript> {
    let response = client
        .get(&queued.response_url)
        .header("Authorization", auth_header(api_key))
        .send()
        .await
        .context("fal result request failed")?;
    if response.status() == reqwest::StatusCode::ACCEPTED {
        return Err(anyhow!(
            "fal result not ready for request {}",
            queued.request_id
        ));
    }
    let output = response
        .error_for_status()
        .context("fal result request rejected")?
        .json::<Value>()
        .await
        .context("fal result returned invalid JSON")?;
    Ok(extract_transcript(&output))
}

/// Build the Scribe v2 input. Single-speaker dictation wants clean
/// insertable text, so diarization and audio-event tags stay off.
#[cfg(test)]
pub fn scribe_input(audio_url: &str, language: &str) -> Value {
    scribe_input_owned(audio_url.to_string(), language)
}

fn scribe_input_owned(audio_url: String, language: &str) -> Value {
    let mut input = Map::new();
    input.insert("audio_url".to_string(), Value::String(audio_url));
    input.insert("diarize".to_string(), Value::Bool(false));
    input.insert("tag_audio_events".to_string(), Value::Bool(false));
    if !language.trim().is_empty() {
        input.insert(
            "language_code".to_string(),
            Value::String(language.trim().to_string()),
        );
    }
    Value::Object(input)
}

fn parse_submit_response(body: &Value) -> Result<QueuedRequest> {
    let field = |name: &str| {
        body.get(name)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|value| !value.is_empty())
    };
    match (
        field("request_id"),
        field("status_url"),
        field("response_url"),
    ) {
        (Some(request_id), Some(status_url), Some(response_url)) => Ok(QueuedRequest {
            request_id,
            status_url,
            response_url,
        }),
        _ => Err(anyhow!(
            "fal queue submit response is missing request_id, status_url, or response_url"
        )),
    }
}

enum QueueState {
    Pending,
    Done,
}

fn parse_status(body: &Value) -> Result<QueueState> {
    match body
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default()
    {
        "COMPLETED" => Ok(QueueState::Done),
        "IN_QUEUE" | "IN_PROGRESS" => Ok(QueueState::Pending),
        other => {
            let detail: String = body.to_string().chars().take(300).collect();
            Err(anyhow!(
                "fal transcription request failed status={other} detail={detail}"
            ))
        }
    }
}

/// Read the transcript and word timeline from a Scribe v2 output object.
pub fn extract_transcript(output: &Value) -> FalTranscript {
    let text = output
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    let words = match output.get("words") {
        Some(Value::Array(words)) => words.iter().filter_map(parse_word).collect(),
        Some(Value::Object(_)) => output
            .get("words")
            .and_then(parse_word)
            .into_iter()
            .collect(),
        _ => Vec::new(),
    };
    FalTranscript { text, words }
}

fn parse_word(word: &Value) -> Option<TranscriptWord> {
    let text = word.get("text")?.as_str()?.to_string();
    Some(TranscriptWord {
        text,
        start: word.get("start").and_then(Value::as_f64),
        end: word.get("end").and_then(Value::as_f64),
        kind: word
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("word")
            .to_string(),
    })
}

/// Wrap little-endian mono 16-bit PCM in a 44-byte WAV header so the
/// model decodes the buffered capture without extra encoding parameters.
pub fn encode_wav_mono16(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let mut wav = Vec::with_capacity(44 + pcm.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36u32.wrapping_add(data_len)).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&(sample_rate.wrapping_mul(2)).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm);
    wav
}

#[cfg(test)]
mod tests {
    use super::{encode_wav_mono16, extract_transcript, parse_status, scribe_input};
    use super::{parse_submit_response, wav_data_uri};
    use base64::Engine as _;
    use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
    use serde_json::json;

    #[test]
    fn wav_header_describes_mono16_capture() {
        let pcm = vec![0x01, 0x02, 0x03, 0x04];
        let wav = encode_wav_mono16(&pcm, 48_000);
        assert_eq!(wav.len(), 48);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            48_000
        );
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 4);
        assert_eq!(&wav[44..], &pcm[..]);
    }

    #[test]
    fn scribe_input_stays_plain_text_by_default() {
        let input = scribe_input("https://example.com/a.wav", "");
        assert_eq!(input["audio_url"], json!("https://example.com/a.wav"));
        assert_eq!(input["diarize"], json!(false));
        assert_eq!(input["tag_audio_events"], json!(false));
        assert!(input.get("language_code").is_none());
    }

    #[test]
    fn scribe_input_passes_language() {
        let input = scribe_input("https://example.com/a.wav", "eng");
        assert_eq!(input["language_code"], json!("eng"));
    }

    #[test]
    fn wav_data_uri_round_trips_without_storage() {
        let wav = encode_wav_mono16(&[1, 2, 3, 4], 48_000);
        let uri = wav_data_uri(&wav);
        let encoded = uri.strip_prefix(super::WAV_DATA_URI_PREFIX).unwrap();
        assert_eq!(BASE64_STANDARD.decode(encoded).unwrap(), wav);
    }

    #[test]
    fn submit_response_yields_the_canonical_follow_up_urls() {
        let queued = parse_submit_response(&json!({
            "status": "IN_QUEUE",
            "request_id": "abc",
            "response_url": "https://queue.fal.run/fal-ai/elevenlabs/requests/abc",
            "status_url": "https://queue.fal.run/fal-ai/elevenlabs/requests/abc/status",
        }))
        .unwrap();
        assert_eq!(queued.request_id, "abc");
        assert_eq!(
            queued.status_url,
            "https://queue.fal.run/fal-ai/elevenlabs/requests/abc/status"
        );
        assert_eq!(
            queued.response_url,
            "https://queue.fal.run/fal-ai/elevenlabs/requests/abc"
        );
        assert!(parse_submit_response(&json!({})).is_err());
        assert!(parse_submit_response(&json!({"request_id": "abc"})).is_err());
    }

    #[test]
    fn queue_status_maps_lifecycle_to_pending_or_done() {
        assert!(matches!(
            parse_status(&json!({"status": "IN_QUEUE"})).unwrap(),
            super::QueueState::Pending
        ));
        assert!(matches!(
            parse_status(&json!({"status": "IN_PROGRESS"})).unwrap(),
            super::QueueState::Pending
        ));
        assert!(matches!(
            parse_status(&json!({"status": "COMPLETED"})).unwrap(),
            super::QueueState::Done
        ));
        assert!(parse_status(&json!({"status": "FAILED"})).is_err());
    }

    #[test]
    fn transcript_extraction_trims_the_scribe_text() {
        let output = json!({
            "text": "  Hey, this is a test.  ",
            "words": [{"text": "Hey,", "start": 0.079, "end": 0.539}],
            "language_code": "eng",
            "language_probability": 1.0
        });
        let transcript = extract_transcript(&output);
        assert_eq!(transcript.text, "Hey, this is a test.");
        assert_eq!(transcript.words.len(), 1);
        assert_eq!(transcript.words[0].text, "Hey,");
        assert_eq!(transcript.words[0].start, Some(0.079));
        assert_eq!(transcript.words[0].end, Some(0.539));
        assert_eq!(transcript.words[0].kind, "word");
        let empty = extract_transcript(&json!({}));
        assert!(empty.text.is_empty());
        assert!(empty.words.is_empty());
    }

    #[test]
    fn transcript_extraction_preserves_word_types_and_optional_times() {
        let output = json!({
            "text": "Hello world",
            "words": [
                {"text": "Hello", "start": 0.1, "end": 0.4, "type": "word"},
                {"text": " ", "type": "spacing"},
                {"text": "world", "start": 0.5, "end": 0.9, "type": "word"}
            ]
        });
        let transcript = extract_transcript(&output);
        assert_eq!(transcript.words.len(), 3);
        assert_eq!(transcript.words[1].kind, "spacing");
        assert_eq!(transcript.words[1].start, None);
        assert_eq!(transcript.words[1].text, " ");
    }

    #[test]
    fn transcript_extraction_accepts_a_single_word_object() {
        let transcript = extract_transcript(&json!({
            "text": "Hello",
            "words": {"text": "Hello", "start": 0.1, "end": 0.4, "type": "word"}
        }));
        assert_eq!(transcript.words.len(), 1);
        assert_eq!(transcript.words[0].text, "Hello");
    }
}
