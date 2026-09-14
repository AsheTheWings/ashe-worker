//! Self-hosted speech-to-text through the Ashe API server.
//!
//! Dictation buffers microphone audio locally while recording and calls
//! [`transcribe_pcm`] once on stop: the WAV is POSTed to
//! `/v1/stt/transcribe` (faster-whisper behind the server) and read back
//! as text. Word alignment is skipped unless ordered composition needs it,
//! while the full utterance remains one request for recognition context.
//! The configured language is ISO-639-3; the daemon speaks ISO-639-1, so
//! common codes are mapped here and anything else fails loudly instead of
//! transcribing in the wrong language.

use crate::config::AppConfig;
use crate::logger;
use anyhow::{Context, Result, anyhow, ensure};
use std::time::{Duration, Instant};

/// A transcript with word tokens and optional timing for composition mapping.
#[derive(Debug, Clone, PartialEq)]
pub struct Transcript {
    pub text: String,
    pub words: Vec<TranscriptWord>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TranscriptWord {
    pub text: String,
    pub start: Option<f64>,
    pub end: Option<f64>,
    pub kind: String,
}

/// Per-request ceiling. Small runs ~5x real-time, so even minutes-long
/// captures finish well inside this; it only bounds a wedged engine.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(600);
const TRANSIENT_RETRY_DELAY: Duration = Duration::from_millis(250);
const REQUEST_ATTEMPTS: usize = 2;

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
/// skipped; the text stands.
pub fn parse_transcript(body: &serde_json::Value) -> Result<Transcript> {
    let text = body
        .get("text")
        .and_then(serde_json::Value::as_str)
        .context("self-hosted transcript is missing text")?
        .to_string();
    let mut words: Vec<TranscriptWord> = body
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
    if words.is_empty() && !text.is_empty() {
        words = text
            .split_whitespace()
            .map(|text| TranscriptWord {
                text: text.to_string(),
                start: None,
                end: None,
                kind: "word".to_string(),
            })
            .collect();
    }
    Ok(Transcript { text, words })
}

/// Transcribe a complete recording with one engine request.
pub async fn transcribe_pcm(
    config: AppConfig,
    sample_rate: u32,
    pcm: Vec<u8>,
    word_timestamps: bool,
) -> Result<Transcript> {
    validate_capture(&pcm, sample_rate)?;
    let seconds = pcm.len() as f64 / f64::from(sample_rate) / 2.0;
    logger::info(format!(
        "self-hosted transcription sample_rate={sample_rate} bytes={} seconds={seconds:.1}",
        pcm.len(),
    ));
    let started = Instant::now();
    let wav = encode_wav_mono16(&pcm, sample_rate);
    drop(pcm);
    let fetched = transcribe_wav(&config, wav, word_timestamps).await?;
    // Verbalized punctuation ("comma", "double quote") arrives as literal
    // words: the transcription model has no dictation-command layer, so the
    // conversion runs here. Requests without alignment reconstruct untimed
    // words from the exact transcript text for command recognition.
    let transcript = if config.spoken_punctuation {
        crate::spoken_punctuation::apply_spoken_punctuation(&fetched)
    } else {
        fetched
    };
    logger::info(format!(
        "transcription chars={} total_ms={}",
        transcript.text.len(),
        started.elapsed().as_millis()
    ));
    Ok(transcript)
}

fn validate_capture(pcm: &[u8], sample_rate: u32) -> Result<()> {
    if pcm.is_empty() {
        return Err(anyhow::anyhow!("no audio captured"));
    }
    if !pcm.len().is_multiple_of(2) {
        return Err(anyhow::anyhow!("captured PCM has an odd byte count"));
    }
    if sample_rate == 0 {
        return Err(anyhow::anyhow!(
            "capture sample rate must be greater than zero"
        ));
    }
    Ok(())
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

/// Transcribe a complete WAV with the self-hosted engine. Retry one transient
/// transport, overload, timeout, or server failure before surfacing it.
pub async fn transcribe_wav(
    config: &AppConfig,
    mut wav: Vec<u8>,
    word_timestamps: bool,
) -> Result<Transcript> {
    config.validate_for_dictation()?;
    ensure!(!wav.is_empty(), "captured WAV is empty");
    let started = Instant::now();
    let client = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .build()
        .context("failed to create HTTP client")?;
    let mut endpoint = config.worker_endpoint("/v1/stt/transcribe")?;
    if let Some(language) = map_language(&config.stt_language)? {
        endpoint.push_str("?language=");
        endpoint.push_str(&language);
    }
    endpoint.push(if endpoint.contains('?') { '&' } else { '?' });
    endpoint.push_str(if word_timestamps {
        "word_timestamps=1"
    } else {
        "word_timestamps=0"
    });

    let mut last_error = None;
    for attempt in 1..=REQUEST_ATTEMPTS {
        let request_body = if attempt == REQUEST_ATTEMPTS {
            std::mem::take(&mut wav)
        } else {
            wav.clone()
        };
        match transcribe_wav_once(&client, &endpoint, config.stt_token.trim(), request_body).await {
            Ok(transcript) => {
                logger::info(format!(
                    "self-hosted transcript chars={} words={} elapsed_ms={} attempt={attempt} word_timestamps={word_timestamps}",
                    transcript.text.len(),
                    transcript.words.len(),
                    started.elapsed().as_millis()
                ));
                return Ok(transcript);
            }
            Err((retryable, error)) => {
                if !retryable || attempt == REQUEST_ATTEMPTS {
                    return Err(error);
                }
                logger::info(format!(
                    "self-hosted transcription retry attempt={} error={error:#}",
                    attempt + 1
                ));
                last_error = Some(error);
                tokio::time::sleep(TRANSIENT_RETRY_DELAY).await;
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("self-hosted transcription failed")))
}

async fn transcribe_wav_once(
    client: &reqwest::Client,
    endpoint: &str,
    token: &str,
    wav: Vec<u8>,
) -> std::result::Result<Transcript, (bool, anyhow::Error)> {
    let response = client
        .post(endpoint)
        .bearer_auth(token)
        .header("content-type", "audio/wav")
        .body(wav)
        .send()
        .await
        .map_err(|error| {
            (
                true,
                anyhow!(error).context("self-hosted transcription request failed"),
            )
        })?;
    let status = response.status();
    let retryable_status = status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
    let bytes = response.bytes().await.map_err(|error| {
        (
            true,
            anyhow!(error).context("failed to read self-hosted transcription response"),
        )
    })?;
    if !status.is_success() {
        return Err((
            retryable_status,
            anyhow!("self-hosted transcription returned HTTP {status}"),
        ));
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes).map_err(|error| {
        (
            false,
            anyhow!(error).context("self-hosted transcription returned invalid JSON"),
        )
    })?;
    parse_transcript(&body).map_err(|error| (false, error))
}

#[cfg(test)]
mod tests {
    use super::{encode_wav_mono16, map_language, parse_transcript};
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
    fn transcript_parsing_reconstructs_unaligned_words() {
        let transcript = parse_transcript(&json!({
            "text": "Write comma then continue.",
            "words": [],
        }))
        .unwrap();
        assert_eq!(
            transcript
                .words
                .iter()
                .map(|word| word.text.as_str())
                .collect::<Vec<_>>(),
            ["Write", "comma", "then", "continue."]
        );
        assert!(
            transcript
                .words
                .iter()
                .all(|word| word.start.is_none() && word.end.is_none())
        );
    }

    #[test]
    fn transcript_parsing_requires_text() {
        assert!(parse_transcript(&json!({"words": []})).is_err());
        let empty = parse_transcript(&json!({"text": ""})).unwrap();
        assert!(empty.text.is_empty() && empty.words.is_empty());
    }
}
