//! Ordered dictation composition and long-silence compaction.
//!
//! A session keeps speech and exact user insertions in chronological order.
//! Each sealed speech segment is transcribed in the background while
//! dictation continues, so stopping only waits for the outstanding tail
//! instead of the whole capture.

use crate::stt::Transcript;
use anyhow::{Result, anyhow};

/// A safety ceiling for an abandoned session, not a normal duration limit.
/// At 48 kHz mono 16-bit this is roughly 46 minutes of retained audio.
pub const MAX_SESSION_PCM_BYTES: usize = 256 * 1024 * 1024;

const BYTES_PER_SAMPLE: usize = 2;
const LONG_SILENCE_SECONDS: usize = 5;
const GAP_EDGE_MILLISECONDS: usize = 250;
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
    Speech,
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
    speech_transcripts: Vec<Option<Transcript>>,
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
            speech_transcripts: Vec::new(),
        }
    }

    /// Record one sealed speech segment and reserve its transcript slot.
    /// Returns the segment index the background transcription fills in.
    /// Each segment is transcribed on its own, so no separator silence or
    /// global timing map is needed.
    pub fn push_speech(&mut self, pcm: Vec<u8>) -> Option<usize> {
        if pcm.is_empty() || self.sample_rate == 0 {
            return None;
        }
        self.audio_pcm.extend_from_slice(&pcm);
        self.entries.push(CompositionEntry::Speech);
        self.speech_count += 1;
        self.speech_transcripts.push(None);
        Some(self.speech_count - 1)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn speech_count(&self) -> usize {
        self.speech_count
    }

    /// Store the background transcript for one sealed segment.
    /// Returns false when the index does not belong to this session.
    pub fn set_speech_transcript(&mut self, index: usize, transcript: Transcript) -> bool {
        if let Some(slot) = self.speech_transcripts.get_mut(index) {
            *slot = Some(transcript);
            return true;
        }
        false
    }

    pub fn pending_transcript_count(&self) -> usize {
        self.speech_transcripts
            .iter()
            .filter(|slot| slot.is_none())
            .count()
    }

    pub fn all_transcripts_ready(&self) -> bool {
        self.speech_transcripts.iter().all(|slot| slot.is_some())
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
            entries: self.entries,
            speech_count: self.speech_count,
            segment_transcripts: self.speech_transcripts,
        }
    }
}

pub struct FinalComposition {
    target_hwnd: isize,
    entries: Vec<CompositionEntry>,
    speech_count: usize,
    segment_transcripts: Vec<Option<Transcript>>,
}

impl FinalComposition {
    pub fn target_hwnd(&self) -> isize {
        self.target_hwnd
    }

    pub fn speech_count(&self) -> usize {
        self.speech_count
    }

    /// Interleave one transcript per speech segment with the exact typed
    /// insertions in chronological order. Every segment transcribed in
    /// the background fills its slot; assembly only waits for the tail.
    pub fn assemble(&self) -> Result<String> {
        if self.speech_count == 0 {
            return Ok(self.assemble_entries(&[]));
        }
        if self.segment_transcripts.len() != self.speech_count {
            return Err(anyhow!("speech transcripts do not match speech segments"));
        }
        let mut speech = Vec::with_capacity(self.speech_count);
        for slot in &self.segment_transcripts {
            let transcript = slot
                .as_ref()
                .ok_or_else(|| anyhow!("speech transcript is missing"))?;
            speech.push(transcript.text.trim().to_string());
        }
        Ok(self.assemble_entries(&speech))
    }

    fn assemble_entries(&self, speech: &[String]) -> String {
        let mut result = String::new();
        let mut speech_index = 0;
        for entry in &self.entries {
            match entry {
                CompositionEntry::Speech => {
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
    use crate::stt::Transcript;

    fn pcm(sample: i16, samples: usize) -> Vec<u8> {
        sample.to_le_bytes().repeat(samples)
    }

    fn text_transcript(text: &str) -> Transcript {
        Transcript {
            text: text.to_string(),
            words: Vec::new(),
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
        assert_eq!(final_composition.speech_count(), 0);
        assert_eq!(final_composition.assemble().unwrap(), "typed\nPASTED");
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
        assert_eq!(session.finish().assemble().unwrap(), "alpha\nbeta\n");
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
        let first = session.push_speech(pcm(2_000, 100)).unwrap();
        session.push_typed_text("typed");
        assert!(session.commit_insertion());
        let second = session.push_speech(pcm(2_000, 100)).unwrap();
        assert!(session.set_speech_transcript(first, text_transcript("speech one")));
        assert!(session.set_speech_transcript(second, text_transcript("speech two")));
        let final_composition = session.finish();
        assert_eq!(
            final_composition.assemble().unwrap(),
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
            final_composition.assemble().unwrap(),
            "before:\nEXACT\r\nafter"
        );
    }

    #[test]
    fn insertion_only_composition_preserves_edge_whitespace() {
        let mut session = CompositionSession::new(0, 100);
        session.push_pasted_text("\n  exact  \r\n".to_string());
        assert!(session.commit_insertion());
        assert_eq!(session.finish().assemble().unwrap(), "\n  exact  \r\n");
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
    fn missing_segment_transcript_fails_assembly() {
        let mut session = CompositionSession::new(0, 100);
        let first = session.push_speech(pcm(2_000, 100)).unwrap();
        session.push_typed_text("typed");
        session.commit_insertion();
        session.push_speech(pcm(2_000, 100));
        assert!(session.set_speech_transcript(first, text_transcript("speech one")));
        let final_composition = session.finish();
        assert!(final_composition.assemble().is_err());
    }

    #[test]
    fn deterministic_joining_respects_punctuation_and_explicit_whitespace() {
        let mut session = CompositionSession::new(0, 100);
        let index = session.push_speech(pcm(2_000, 100)).unwrap();
        session.push_typed_text(", exactly");
        session.push_pasted_text("\n".to_string());
        session.commit_insertion();
        assert!(session.set_speech_transcript(index, text_transcript("Hello")));
        let final_composition = session.finish();
        assert_eq!(final_composition.assemble().unwrap(), "Hello, exactly\n");
    }

    #[test]
    fn sealed_segments_get_sequential_transcript_slots() {
        let mut session = CompositionSession::new(0, 100);
        assert_eq!(session.pending_transcript_count(), 0);
        let first = session.push_speech(pcm(2_000, 10)).unwrap();
        let second = session.push_speech(pcm(2_000, 10)).unwrap();
        assert_eq!((first, second), (0, 1));
        assert_eq!(session.speech_count(), 2);
        assert_eq!(session.pending_transcript_count(), 2);
        assert!(!session.all_transcripts_ready());
        assert!(!session.set_speech_transcript(7, text_transcript("stale")));
        assert!(session.set_speech_transcript(first, text_transcript("one")));
        assert_eq!(session.pending_transcript_count(), 1);
        assert!(session.set_speech_transcript(second, text_transcript("  two  ")));
        assert!(session.all_transcripts_ready());
        assert_eq!(session.finish().assemble().unwrap(), "one two");
    }

    #[test]
    fn fail_safe_is_well_above_expected_session_size() {
        let ten_minutes_at_48khz = 48_000 * 2 * 60 * 10;
        assert!(MAX_SESSION_PCM_BYTES > ten_minutes_at_48khz);
    }
}
