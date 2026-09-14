//! Self-hosted speech-to-text through the Ashe API server.
//!
//! POSTs the captured WAV to `/v1/stt/transcribe` (faster-whisper behind
//! the server) and reads back text with word timings, the same shape the
//! composition mapping and spoken punctuation expect from the cloud path.
//! The configured language is ISO-639-3 (the Scribe-era convention); the
//! daemon speaks ISO-639-1, so common codes are mapped here and anything
//! else fails loudly instead of transcribing in the wrong language.

use crate::config::AppConfig;
use crate::fal_client::{FalTranscript, TranscriptWord};
use crate::logger;
use anyhow::{Context, Result, ensure};
use std::time::{Duration, Instant};

/// Per-request ceiling. Turbo runs ~1.5x real-time, so even minutes-long
/// captures finish well inside this; it only bounds a wedged engine.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);

/// Map a configured language to the daemon's ISO-639-1 code. Empty means
/// auto-detect; two-letter codes pass through; anything unmapped errors.
pub fn map_language(configured: &str) -> Result<Option<String>> {
    let code = configured.trim();
    if code.is_empty() {
        return Ok(None);
    }
    if code.len() == 2 && code.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Ok(Some(code.to_ascii_lowercase()));
    }
    let mapped = match code.to_ascii_lowercase().as_str() {
        "ara" => "ar",
        "ces" => "cs",
        "dan" => "da",
        "deu" => "de",
        "ell" => "el",
        "eng" => "en",
        "fin" => "fi",
        "fra" => "fr",
        "heb" => "he",
        "hin" => "hi",
        "hun" => "hu",
        "ind" => "id",
        "ita" => "it",
        "jpn" => "ja",
        "kor" => "ko",
        "msa" => "ms",
        "nld" => "nl",
        "nor" => "no",
        "pol" => "pl",
        "por" => "pt",
        "ron" => "ro",
        "rus" => "ru",
        "spa" => "es",
        "swe" => "sv",
        "tha" => "th",
        "tur" => "tr",
        "ukr" => "uk",
        "vie" => "vi",
        "zho" => "zh",
        _ => return Err(anyhow::anyhow!("unsupported STT language code `{code}`")),
    };
    Ok(Some(mapped.to_string()))
}

/// Read the daemon answer into a transcript. Malformed word entries are
/// skipped like the cloud path skips malformed words; the text stands.
pub fn parse_transcript(body: &serde_json::Value) -> Result<FalTranscript> {
    let text = body
        .get("text")
        .and_then(serde_json::Value::as_str)
        .context("self-hosted transcript is missing text")?
        .to_string();
    let words = body
        .get("words")
        .and_then(serde_json::Value::as_array)
        .map(|words| {
            words
                .iter()
                .filter_map(|word| {
                    Some(TranscriptWord {
                        text: word.get("text")?.as_str()?.to_string(),
                        start: word.get("start")?.as_f64(),
                        end: word.get("end")?.as_f64(),
                        kind: "word".to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(FalTranscript { text, words })
}

/// Transcribe a complete WAV with the self-hosted engine.
pub async fn transcribe_wav(config: &AppConfig, wav: Vec<u8>) -> Result<FalTranscript> {
    config.validate_for_self_hosted_stt()?;
    ensure!(!wav.is_empty(), "captured WAV is empty");
    let started = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to create HTTP client")?;
    let mut endpoint = config.worker_endpoint("/v1/stt/transcribe")?;
    if let Some(language) = map_language(&config.fal_language)? {
        endpoint.push_str("?language=");
        endpoint.push_str(&language);
    }
    let response = client
        .post(endpoint)
        .bearer_auth(config.stt_token.trim())
        .header("content-type", "audio/wav")
        .body(wav)
        .send()
        .await
        .context("self-hosted transcription request failed")?;
    let status = response.status();
    let bytes = response
        .bytes()
        .await
        .context("failed to read self-hosted transcription response")?;
    ensure!(
        status.is_success(),
        "self-hosted transcription returned HTTP {status}"
    );
    let body: serde_json::Value =
        serde_json::from_slice(&bytes).context("self-hosted transcription returned invalid JSON")?;
    let transcript = parse_transcript(&body)?;
    logger::info(format!(
        "self-hosted transcript chars={} words={} elapsed_ms={}",
        transcript.text.len(),
        transcript.words.len(),
        started.elapsed().as_millis()
    ));
    Ok(transcript)
}

#[cfg(test)]
mod tests {
    use super::{map_language, parse_transcript};
    use serde_json::json;

    #[test]
    fn language_mapping_covers_configured_codes() {
        assert_eq!(map_language("").unwrap(), None);
        assert_eq!(map_language("en").unwrap(), Some("en".to_string()));
        assert_eq!(map_language("EN").unwrap(), Some("en".to_string()));
        assert_eq!(map_language("eng").unwrap(), Some("en".to_string()));
        assert_eq!(map_language("deu").unwrap(), Some("de".to_string()));
        assert_eq!(map_language("zho").unwrap(), Some("zh".to_string()));
        assert!(map_language("klingon").is_err());
        assert!(map_language("e").is_err());
    }

    #[test]
    fn transcript_parsing_keeps_text_and_timed_words() {
        let transcript = parse_transcript(&json!({
            "text": "hi there",
            "words": [
                {"text": "hi", "start": 0.0, "end": 0.2},
                {"text": "there", "start": 0.2, "end": 0.5},
                {"text": "broken"},
            ],
            "duration": 0.5,
            "model": "small",
        }))
        .unwrap();
        assert_eq!(transcript.text, "hi there");
        assert_eq!(transcript.words.len(), 2);
        assert_eq!(transcript.words[0].text, "hi");
        assert_eq!(transcript.words[1].end, Some(0.5));
        assert_eq!(transcript.words[0].kind, "word");
    }

    #[test]
    fn transcript_parsing_requires_text() {
        assert!(parse_transcript(&json!({"words": []})).is_err());
        let empty = parse_transcript(&json!({"text": ""})).unwrap();
        assert!(empty.text.is_empty() && empty.words.is_empty());
    }
}
