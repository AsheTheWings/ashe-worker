//! Ordered dictation composition and long-silence compaction.
//!
//! A session keeps speech and exact user insertions in chronological order.
//! Speech is compacted locally, concatenated into one PCM timeline, and sent
//! to speech-to-text only when the session finishes.

use crate::stt::Transcript;
use anyhow::{Result, anyhow};

/// A safety ceiling for an abandoned session, not a normal duration limit.
/// At 48 kHz mono 16-bit this is roughly 46 minutes of retained audio.
pub const MAX_SESSION_PCM_BYTES: usize = 256 * 1024 * 1024;

const BYTES_PER_SAMPLE: usize = 2;
const LONG_SILENCE_SECONDS: usize = 5;
const GAP_EDGE_MILLISECONDS: usize = 250;
const SPEECH_SEPARATOR_MILLISECONDS: usize = 500;
const PASTE_MARKER: &str = "[pasted]";
const LINE_BREAK_MARKER: char = '↵';

/// Retains natural pauses exactly and compacts only proven five-second gaps.
pub struct SilenceCompactor {
    sample_rate: u32,
    retained: Vec<u8>,
    pending_silence: Vec<u8>,
    resume_preroll: Vec<u8>,
    silent_samples: usize,
    has_speech: bool,
    skipping: bool,
}

impl SilenceCompactor {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            sample_rate,
            retained: Vec::new(),
            pending_silence: Vec::new(),
            resume_preroll: Vec::new(),
            silent_samples: 0,
            has_speech: false,
            skipping: false,
        }
    }

    /// Consume one capture chunk using the same raw signal decision as the
    /// visualizer. Chunks remain byte-for-byte intact unless a long gap is
    /// proven.
    pub fn push_pcm(&mut self, pcm: &[u8], signal_active: bool) {
        let even_len = pcm.len() - pcm.len() % BYTES_PER_SAMPLE;
        let pcm = &pcm[..even_len];
        if pcm.is_empty() {
            return;
        }

        if signal_active {
            if self.skipping {
                self.retained.extend_from_slice(&self.resume_preroll);
            } else {
                self.retained.extend_from_slice(&self.pending_silence);
            }
            self.pending_silence.clear();
            self.resume_preroll.clear();
            self.retained.extend_from_slice(pcm);
            self.silent_samples = 0;
            self.has_speech = true;
            self.skipping = false;
            return;
        }

        self.silent_samples = self
            .silent_samples
            .saturating_add(pcm.len() / BYTES_PER_SAMPLE);
        if self.skipping {
            self.push_preroll(pcm);
            return;
        }

        self.pending_silence.extend_from_slice(pcm);
        if self.silent_samples >= self.samples_for_seconds(LONG_SILENCE_SECONDS) {
            let edge_bytes = self.bytes_for_milliseconds(GAP_EDGE_MILLISECONDS);
            if self.has_speech {
                let prefix_len = edge_bytes.min(self.pending_silence.len());
                self.retained
                    .extend_from_slice(&self.pending_silence[..prefix_len]);
            }
            let suffix_start = self.pending_silence.len().saturating_sub(edge_bytes);
            self.resume_preroll
                .extend_from_slice(&self.pending_silence[suffix_start..]);
            self.pending_silence.clear();
            self.skipping = true;
        }
    }

    pub fn silence_truncated(&self) -> bool {
        self.skipping
    }

    pub fn estimated_bytes(&self) -> usize {
        self.retained
            .len()
            .saturating_add(self.pending_silence.len())
            .saturating_add(self.resume_preroll.len())
    }

    pub fn finish(mut self) -> Option<Vec<u8>> {
        if !self.has_speech {
            return None;
        }
        if self.skipping {
            self.retained.extend_from_slice(&self.resume_preroll);
        } else {
            self.retained.extend_from_slice(&self.pending_silence);
        }
        Some(self.retained)
    }

    fn push_preroll(&mut self, pcm: &[u8]) {
        let max_bytes = self.bytes_for_milliseconds(GAP_EDGE_MILLISECONDS);
        if max_bytes == 0 {
            return;
        }
        self.resume_preroll.extend_from_slice(pcm);
        if self.resume_preroll.len() > max_bytes {
            let discard = self.resume_preroll.len() - max_bytes;
            self.resume_preroll.drain(..discard);
        }
    }

    fn samples_for_seconds(&self, seconds: usize) -> usize {
        self.sample_rate as usize * seconds
    }

    fn bytes_for_milliseconds(&self, milliseconds: usize) -> usize {
        (self.sample_rate as usize * milliseconds / 1_000) * BYTES_PER_SAMPLE
    }
}

#[derive(Debug, Clone, PartialEq)]
enum CompositionEntry {
    Speech { start: f64, end: f64 },
    Insertion(String),
}

#[derive(Debug, Clone, PartialEq)]
enum TypingAtom {
    Character(char),
    Paste(String),
    LineBreak,
}

#[derive(Debug, Default, Clone, PartialEq)]
struct TypingBuffer {
    atoms: Vec<TypingAtom>,
}

impl TypingBuffer {
    fn push_text(&mut self, text: &str) {
        self.atoms.extend(
            text.chars()
                .filter(|character| !character.is_control())
                .map(TypingAtom::Character),
        );
    }

    fn push_paste(&mut self, text: String) {
        if !text.is_empty() {
            self.atoms.push(TypingAtom::Paste(text));
        }
    }

    fn push_line_break(&mut self) {
        self.atoms.push(TypingAtom::LineBreak);
    }

    fn backspace(&mut self) -> bool {
        self.atoms.pop().is_some()
    }

    fn is_empty(&self) -> bool {
        self.atoms.is_empty()
    }

    fn take_actual(&mut self) -> String {
        let mut actual = String::new();
        for atom in self.atoms.drain(..) {
            match atom {
                TypingAtom::Character(character) => actual.push(character),
                TypingAtom::Paste(text) => actual.push_str(&text),
                TypingAtom::LineBreak => actual.push('\n'),
            }
        }
        actual
    }

    fn preview_tail(&self, max_characters: usize) -> String {
        if max_characters == 0 {
            return String::new();
        }
        let mut reverse = Vec::with_capacity(max_characters);
        for atom in self.atoms.iter().rev() {
            match atom {
                TypingAtom::Character(character) => {
                    if reverse.len() == max_characters {
                        break;
                    }
                    reverse.push(*character);
                }
                TypingAtom::Paste(_) => {
                    let marker: Vec<char> = PASTE_MARKER.chars().collect();
                    if marker.len() > max_characters.saturating_sub(reverse.len()) {
                        break;
                    }
                    reverse.extend(marker.into_iter().rev());
                }
                TypingAtom::LineBreak => {
                    if reverse.len() == max_characters {
                        break;
                    }
                    reverse.push(LINE_BREAK_MARKER);
                }
            }
        }
        reverse.reverse();
        reverse.into_iter().collect()
    }
}

pub struct CompositionSession {
    target_hwnd: isize,
    sample_rate: u32,
    audio_pcm: Vec<u8>,
    entries: Vec<CompositionEntry>,
    typing: TypingBuffer,
    insertion_count: usize,
    speech_count: usize,
}

impl CompositionSession {
    pub fn new(target_hwnd: isize, sample_rate: u32) -> Self {
        Self {
            target_hwnd,
            sample_rate,
            audio_pcm: Vec::new(),
            entries: Vec::new(),
            typing: TypingBuffer::default(),
            insertion_count: 0,
            speech_count: 0,
        }
    }

    pub fn push_speech(&mut self, pcm: Vec<u8>) {
        if pcm.is_empty() || self.sample_rate == 0 {
            return;
        }
        if self.speech_count > 0 {
            let separator_bytes = self.sample_rate as usize * SPEECH_SEPARATOR_MILLISECONDS / 1_000
                * BYTES_PER_SAMPLE;
            self.audio_pcm
                .resize(self.audio_pcm.len().saturating_add(separator_bytes), 0);
        }
        let bytes_per_second = self.sample_rate as f64 * BYTES_PER_SAMPLE as f64;
        let start = self.audio_pcm.len() as f64 / bytes_per_second;
        self.audio_pcm.extend_from_slice(&pcm);
        let end = self.audio_pcm.len() as f64 / bytes_per_second;
        self.entries.push(CompositionEntry::Speech { start, end });
        self.speech_count += 1;
    }

    pub fn push_typed_text(&mut self, text: &str) {
        self.typing.push_text(text);
    }

    pub fn push_pasted_text(&mut self, text: String) {
        self.typing.push_paste(text);
    }

    pub fn push_typed_line_break(&mut self) {
        self.typing.push_line_break();
    }

    pub fn backspace(&mut self) -> bool {
        self.typing.backspace()
    }

    pub fn typing_is_empty(&self) -> bool {
        self.typing.is_empty()
    }

    pub fn typing_preview_tail(&self, max_characters: usize) -> String {
        self.typing.preview_tail(max_characters)
    }

    pub fn commit_insertion(&mut self) -> bool {
        if self.typing.is_empty() {
            return false;
        }
        let text = self.typing.take_actual();
        self.entries.push(CompositionEntry::Insertion(text));
        self.insertion_count += 1;
        true
    }

    pub fn commit_line_break(&mut self) {
        self.entries
            .push(CompositionEntry::Insertion("\n".to_string()));
        self.insertion_count += 1;
    }

    pub fn insertion_count(&self) -> usize {
        self.insertion_count
    }

    pub fn audio_bytes(&self) -> usize {
        self.audio_pcm.len()
    }

    pub fn finish(self) -> FinalComposition {
        FinalComposition {
            target_hwnd: self.target_hwnd,
            sample_rate: self.sample_rate,
            audio_pcm: self.audio_pcm,
            entries: self.entries,
            speech_count: self.speech_count,
        }
    }
}

pub struct FinalComposition {
    target_hwnd: isize,
    sample_rate: u32,
    audio_pcm: Vec<u8>,
    entries: Vec<CompositionEntry>,
    speech_count: usize,
}

impl FinalComposition {
    pub fn target_hwnd(&self) -> isize {
        self.target_hwnd
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn audio_pcm_len(&self) -> usize {
        self.audio_pcm.len()
    }

    pub fn has_speech(&self) -> bool {
        self.speech_count > 0
    }

    pub fn take_audio(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.audio_pcm)
    }

    /// Word alignment is needed only to split one full-context transcript
    /// around insertions between multiple speech ranges.
    pub fn requires_word_timestamps(&self) -> bool {
        self.speech_count > 1
            && self
                .entries
                .iter()
                .any(|entry| matches!(entry, CompositionEntry::Insertion(_)))
    }

    pub fn assemble(&self, transcript: Option<&Transcript>) -> Result<String> {
        if self.speech_count == 0 {
            return Ok(self.assemble_entries(&[]));
        }
        let transcript = transcript.ok_or_else(|| anyhow!("speech transcript is missing"))?;
        if self.speech_count == 1 {
            return Ok(self.assemble_entries(&[transcript.text.trim().to_string()]));
        }
        if !self
            .entries
            .iter()
            .any(|entry| matches!(entry, CompositionEntry::Insertion(_)))
        {
            return Ok(transcript.text.trim().to_string());
        }

        let speech = map_speech_entries(&self.entries, transcript)?;
        Ok(self.assemble_entries(&speech))
    }

    fn assemble_entries(&self, speech: &[String]) -> String {
        let mut result = String::new();
        let mut speech_index = 0;
        for entry in &self.entries {
            match entry {
                CompositionEntry::Speech { .. } => {
                    if let Some(text) = speech.get(speech_index) {
                        append_piece(&mut result, text.trim());
                    }
                    speech_index += 1;
                }
                CompositionEntry::Insertion(text) => append_piece(&mut result, text),
            }
        }
        result
    }
}

fn map_speech_entries(
    entries: &[CompositionEntry],
    transcript: &Transcript,
) -> Result<Vec<String>> {
    let ranges: Vec<(f64, f64)> = entries
        .iter()
        .filter_map(|entry| match entry {
            CompositionEntry::Speech { start, end } => Some((*start, *end)),
            CompositionEntry::Insertion(_) => None,
        })
        .collect();
    let mut mapped = vec![String::new(); ranges.len()];
    let mut assignments = vec![None; transcript.words.len()];
    let mut timed_words = 0;

    for (index, word) in transcript.words.iter().enumerate() {
        if word.kind == "audio_event" {
            continue;
        }
        if let (Some(start), Some(end)) = (word.start, word.end) {
            let midpoint = (start + end) / 2.0;
            assignments[index] = nearest_range(midpoint, &ranges);
            if word.kind != "spacing" {
                timed_words += 1;
            }
        }
    }
    if timed_words == 0 {
        return Err(anyhow!(
            "transcript lacks timed words for ordered speech composition"
        ));
    }

    for index in 0..assignments.len() {
        if assignments[index].is_some() || transcript.words[index].kind != "spacing" {
            continue;
        }
        assignments[index] = assignments[..index]
            .iter()
            .rev()
            .copied()
            .flatten()
            .next()
            .or_else(|| assignments[index + 1..].iter().copied().flatten().next());
    }

    for (word, assignment) in transcript.words.iter().zip(assignments) {
        if word.kind == "audio_event" {
            continue;
        }
        let Some(range_index) = assignment else {
            if word.kind == "spacing" || word.text.is_empty() {
                continue;
            }
            return Err(anyhow!(
                "transcript contains an untimed word in ordered composition"
            ));
        };
        if word.kind == "spacing" {
            mapped[range_index].push_str(&word.text);
        } else {
            append_piece(&mut mapped[range_index], &word.text);
        }
    }
    for text in &mut mapped {
        *text = text.trim().to_string();
    }
    Ok(mapped)
}

fn nearest_range(timestamp: f64, ranges: &[(f64, f64)]) -> Option<usize> {
    ranges
        .iter()
        .enumerate()
        .min_by(|(_, left), (_, right)| {
            distance_to_range(timestamp, **left).total_cmp(&distance_to_range(timestamp, **right))
        })
        .map(|(index, _)| index)
}

fn distance_to_range(timestamp: f64, range: (f64, f64)) -> f64 {
    if timestamp < range.0 {
        range.0 - timestamp
    } else if timestamp > range.1 {
        timestamp - range.1
    } else {
        0.0
    }
}

fn append_piece(output: &mut String, piece: &str) {
    if piece.is_empty() {
        return;
    }
    if let (Some(left), Some(right)) = (output.chars().next_back(), piece.chars().next())
        && needs_boundary_space(left, right)
    {
        output.push(' ');
    }
    output.push_str(piece);
}

fn needs_boundary_space(left: char, right: char) -> bool {
    if left.is_whitespace() || right.is_whitespace() {
        return false;
    }
    if matches!(
        right,
        '.' | ',' | '!' | '?' | ';' | ':' | '%' | ')' | ']' | '}' | '\'' | '’'
    ) {
        return false;
    }
    !matches!(left, '(' | '[' | '{' | '/' | '\'' | '‘' | '“')
}

#[cfg(test)]
mod tests {
    use super::{CompositionSession, MAX_SESSION_PCM_BYTES, SilenceCompactor, TypingBuffer};
    use crate::stt::{Transcript, TranscriptWord};

    fn pcm(sample: i16, samples: usize) -> Vec<u8> {
        sample.to_le_bytes().repeat(samples)
    }

    fn transcript(words: Vec<TranscriptWord>) -> Transcript {
        Transcript {
            text: "speech one speech two".to_string(),
            words,
        }
    }

    fn word(text: &str, start: f64, end: f64) -> TranscriptWord {
        TranscriptWord {
            text: text.to_string(),
            start: Some(start),
            end: Some(end),
            kind: "word".to_string(),
        }
    }

    fn spacing(text: &str) -> TranscriptWord {
        TranscriptWord {
            text: text.to_string(),
            start: None,
            end: None,
            kind: "spacing".to_string(),
        }
    }

    #[test]
    fn natural_silence_under_five_seconds_is_unchanged() {
        let rate = 100;
        let speech_a = pcm(2_000, 10);
        let silence = pcm(0, 499);
        let speech_b = pcm(2_000, 10);
        let mut compact = SilenceCompactor::new(rate);
        compact.push_pcm(&speech_a, true);
        compact.push_pcm(&silence, false);
        compact.push_pcm(&speech_b, true);
        let output = compact.finish().unwrap();
        assert_eq!(output, [speech_a, silence, speech_b].concat());
    }

    #[test]
    fn five_second_gap_is_compacted_to_half_a_second() {
        let rate = 100;
        let speech_a = pcm(2_000, 10);
        let silence = pcm(0, 800);
        let speech_b = pcm(2_000, 10);
        let mut compact = SilenceCompactor::new(rate);
        compact.push_pcm(&speech_a, true);
        compact.push_pcm(&silence, false);
        assert!(compact.silence_truncated());
        compact.push_pcm(&speech_b, true);
        let output = compact.finish().unwrap();
        assert_eq!(
            output.len(),
            speech_a.len() + pcm(0, 50).len() + speech_b.len()
        );
        assert_eq!(&output[..speech_a.len()], speech_a);
        assert_eq!(&output[output.len() - speech_b.len()..], speech_b);
    }

    #[test]
    fn leading_silence_is_not_sent_but_preroll_is_restored() {
        let rate = 100;
        let silence = pcm(0, 800);
        let speech = pcm(2_000, 10);
        let mut compact = SilenceCompactor::new(rate);
        compact.push_pcm(&silence, false);
        assert!(compact.silence_truncated());
        compact.push_pcm(&speech, true);
        let output = compact.finish().unwrap();
        assert_eq!(output.len(), pcm(0, 25).len() + speech.len());
        assert_eq!(&output[output.len() - speech.len()..], speech);
    }

    #[test]
    fn all_silent_capture_has_no_audio() {
        let mut compact = SilenceCompactor::new(100);
        compact.push_pcm(&pcm(0, 900), false);
        assert!(compact.finish().is_none());
    }

    #[test]
    fn all_silent_session_still_inserts_exact_user_content() {
        let mut session = CompositionSession::new(0, 100);
        session.push_typed_text("typed");
        session.push_pasted_text("\nPASTED".to_string());
        assert!(session.commit_insertion());
        let final_composition = session.finish();
        assert!(!final_composition.has_speech());
        assert_eq!(final_composition.assemble(None).unwrap(), "typed\nPASTED");
    }

    #[test]
    fn silence_status_changes_only_after_truncation() {
        let mut compact = SilenceCompactor::new(100);
        compact.push_pcm(&pcm(0, 499), false);
        assert!(!compact.silence_truncated());
        compact.push_pcm(&pcm(0, 1), false);
        assert!(compact.silence_truncated());
    }

    #[test]
    fn paste_preview_is_private_and_backspace_is_atomic() {
        let mut buffer = TypingBuffer::default();
        buffer.push_text("alpha ");
        buffer.push_paste("private text".to_string());
        assert_eq!(buffer.preview_tail(64), "alpha [pasted]");
        assert!(buffer.backspace());
        assert_eq!(buffer.preview_tail(64), "alpha ");
        assert_eq!(buffer.take_actual(), "alpha ");
    }

    #[test]
    fn line_break_is_one_editable_typing_atom() {
        let mut buffer = TypingBuffer::default();
        buffer.push_text("alpha");
        buffer.push_line_break();
        assert_eq!(buffer.preview_tail(64), "alpha↵");
        assert!(buffer.backspace());
        assert_eq!(buffer.preview_tail(64), "alpha");
        assert_eq!(buffer.take_actual(), "alpha");
    }

    #[test]
    fn typed_and_listening_line_breaks_emit_exact_newlines() {
        let mut session = CompositionSession::new(0, 100);
        session.push_typed_text("alpha");
        session.push_typed_line_break();
        session.push_typed_text("beta");
        assert!(session.commit_insertion());
        session.commit_line_break();
        assert_eq!(session.insertion_count(), 2);
        assert_eq!(session.finish().assemble(None).unwrap(), "alpha\nbeta\n");
    }

    #[test]
    fn preview_is_a_trailing_sliding_window() {
        let mut buffer = TypingBuffer::default();
        buffer.push_text("abcdefghij");
        assert_eq!(buffer.preview_tail(4), "ghij");
    }

    #[test]
    fn speech_and_insertions_are_assembled_in_order() {
        let mut session = CompositionSession::new(42, 100);
        session.push_speech(pcm(2_000, 100));
        session.push_typed_text("typed");
        assert!(session.commit_insertion());
        session.push_speech(pcm(2_000, 100));
        let final_composition = session.finish();
        let transcript = transcript(vec![
            word("speech", 0.0, 0.4),
            spacing(" "),
            word("one", 0.5, 0.9),
            spacing(" "),
            word("speech", 1.6, 2.0),
            spacing(" "),
            word("two", 2.1, 2.4),
        ]);
        assert_eq!(
            final_composition.assemble(Some(&transcript)).unwrap(),
            "speech one typed speech two"
        );
    }

    #[test]
    fn typed_and_pasted_values_remain_exact() {
        let mut session = CompositionSession::new(0, 100);
        session.push_typed_text("before:");
        session.push_pasted_text("\nEXACT\r\n".to_string());
        session.push_typed_text("after");
        assert!(session.commit_insertion());
        let final_composition = session.finish();
        assert_eq!(
            final_composition.assemble(None).unwrap(),
            "before:\nEXACT\r\nafter"
        );
    }

    #[test]
    fn insertion_only_composition_preserves_edge_whitespace() {
        let mut session = CompositionSession::new(0, 100);
        session.push_pasted_text("\n  exact  \r\n".to_string());
        assert!(session.commit_insertion());
        assert_eq!(session.finish().assemble(None).unwrap(), "\n  exact  \r\n");
    }

    #[test]
    fn insertion_count_increments_per_committed_buffer() {
        let mut session = CompositionSession::new(0, 100);
        session.push_typed_text("one");
        assert!(session.commit_insertion());
        session.push_pasted_text("two".to_string());
        assert!(session.commit_insertion());
        assert_eq!(session.insertion_count(), 2);
    }

    #[test]
    fn missing_timestamps_do_not_guess_across_insertions() {
        let mut session = CompositionSession::new(0, 100);
        session.push_speech(pcm(2_000, 100));
        session.push_typed_text("typed");
        session.commit_insertion();
        session.push_speech(pcm(2_000, 100));
        let final_composition = session.finish();
        let transcript = Transcript {
            text: "speech one speech two".to_string(),
            words: vec![],
        };
        assert!(final_composition.assemble(Some(&transcript)).is_err());
    }

    #[test]
    fn alignment_is_requested_only_for_multiple_speech_ranges_with_insertions() {
        let mut single = CompositionSession::new(0, 100);
        single.push_typed_text("before");
        single.commit_insertion();
        single.push_speech(pcm(2_000, 100));
        assert!(!single.finish().requires_word_timestamps());

        let mut uninterrupted = CompositionSession::new(0, 100);
        uninterrupted.push_speech(pcm(2_000, 100));
        uninterrupted.push_speech(pcm(2_000, 100));
        assert!(!uninterrupted.finish().requires_word_timestamps());

        let mut interleaved = CompositionSession::new(0, 100);
        interleaved.push_speech(pcm(2_000, 100));
        interleaved.push_typed_text("typed");
        interleaved.commit_insertion();
        interleaved.push_speech(pcm(2_000, 100));
        assert!(interleaved.finish().requires_word_timestamps());
    }

    #[test]
    fn deterministic_joining_respects_punctuation_and_explicit_whitespace() {
        let mut session = CompositionSession::new(0, 100);
        session.push_speech(pcm(2_000, 100));
        session.push_typed_text(", exactly");
        session.push_pasted_text("\n".to_string());
        session.commit_insertion();
        let final_composition = session.finish();
        let transcript = Transcript {
            text: "Hello".to_string(),
            words: vec![],
        };
        assert_eq!(
            final_composition.assemble(Some(&transcript)).unwrap(),
            "Hello, exactly\n"
        );
    }

    #[test]
    fn fail_safe_is_well_above_expected_session_size() {
        let ten_minutes_at_48khz = 48_000 * 2 * 60 * 10;
        assert!(MAX_SESSION_PCM_BYTES > ten_minutes_at_48khz);
    }
}
