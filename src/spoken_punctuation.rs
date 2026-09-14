//! Spoken punctuation commands for dictation.
//!
//! Assumes the transcription model transcribes verbalized punctuation
//! literally ("double quote" stays words) while separately predicting
//! structural marks from grammar and prosody, which can *also* wrap the
//! clause in inferred symbols. Saying a mark out loud therefore yields both
//! the words and the symbol. This pass converts command phrases in the
//! transcript into symbols and drops the command words.
//!
//! The transform runs on the timed word stream rather than the plain text:
//! symbols attach to neighboring content tokens (which keep their
//! timestamps), and the plain text is rebuilt with the same spacing rules
//! the composition uses. Say `literal` before a command to keep the words,
//! e.g. `literal comma` inserts the word "comma".

use crate::stt::{Transcript, TranscriptWord};
use crate::logger;

/// Punctuation the transcription model may glue onto a command word from
/// prosody (e.g. the comma in "Double quote, ..."). Command words carry no
/// content, so marks attached to them are discarded with the command.
const COMMAND_EDGE_MARKS: &[char] = &[
    // ASCII marks plus the curly quotes transcription models also emit.
    '.',
    ',',
    '?',
    '!',
    ';',
    ':',
    '"',
    '\'',
    '(',
    ')',
    '[',
    ']',
    '“',
    '”',
    '‘',
    '’',
];
/// Straight and curly double quotes: inferred marks stripped from the
/// content neighbor of a quote command, so the model's own quotation guess
/// does not double the symbol.
const QUOTE_MARKS: &[char] = &['"', '“', '”'];

enum Command {
    /// Symbol glued to the previous content word (`,`, `.`, ...).
    AttachLeft(String),
    /// Newline(s) between the neighbors.
    LineBreak(String),
    /// Apostrophe glued to both sides (`don't`).
    Glue(String),
    OpenQuote,
    CloseQuote,
    ToggleQuote,
}

/// Convert spoken punctuation commands in `transcript`, dropping the
/// command words. Returns the transcript unchanged when it holds none.
pub fn apply_spoken_punctuation(transcript: &Transcript) -> Transcript {
    let Some(converted) = convert(&transcript.words) else {
        return transcript.clone();
    };
    logger::info(format!(
        "Spoken punctuation converted commands={}",
        converted.commands,
    ));
    Transcript {
        text: rebuild_text(&converted.words),
        words: converted.words,
    }
}

struct Conversion {
    words: Vec<TranscriptWord>,
    commands: usize,
}

struct Edit {
    /// Content position of the command phrase start.
    position: usize,
    /// Content words consumed by the phrase.
    consumed: usize,
    command: Command,
}

fn convert(words: &[TranscriptWord]) -> Option<Conversion> {
    // Content-word token positions; spacing and audio events pass through.
    let content: Vec<usize> = words
        .iter()
        .enumerate()
        .filter(|(_, word)| word.kind == "word")
        .map(|(index, _)| index)
        .collect();
    let mut skip = vec![false; words.len()];
    let mut literal = vec![false; words.len()];
    let mut edits: Vec<Edit> = Vec::new();
    let mut position = 0;
    while position < content.len() {
        let index = content[position];
        if literal[index] {
            position += 1;
            continue;
        }
        if core_word(&words[index].text).eq_ignore_ascii_case("literal")
            && let Some((_, consumed)) = match_command(words, &content, position + 1)
        {
            // `literal` quotes the command phrase: drop the escape word and
            // keep the phrase as ordinary content.
            skip[index] = true;
            for offset in 1..=consumed {
                literal[content[position + offset]] = true;
            }
            position += 1;
            continue;
        }
        if let Some((command, consumed)) = match_command(words, &content, position) {
            for offset in 0..consumed {
                skip[content[position + offset]] = true;
            }
            edits.push(Edit {
                position,
                consumed,
                command,
            });
            position += consumed;
        } else {
            position += 1;
        }
    }
    // A lone `literal` escape still removes the escape word even though no
    // symbol is emitted.
    if edits.is_empty() && !skip.iter().any(|skipped| *skipped) {
        return None;
    }
    // Attach symbols to surviving neighbors first, then filter. Original
    // indices stay valid throughout because nothing is removed mid-pass.
    let mut texts: Vec<String> = words.iter().map(|word| word.text.clone()).collect();
    // Command words and `literal` escapes always leave the stream; the span
    // loop below adds spacing inside command spans, and each arm adds the
    // spacing on its attach side.
    let mut drop = skip.clone();
    for edit in &edits {
        let first = content[edit.position];
        let last = content[edit.position + edit.consumed - 1];
        for slot in drop.iter_mut().take(last + 1).skip(first) {
            *slot = true;
        }
    }
    let mut quote_open = false;
    let command_count = edits.len();
    for edit in edits {
        let first = content[edit.position];
        let last = content[edit.position + edit.consumed - 1];
        let prev = nearest_content(&content, &skip, edit.position, -1);
        let next = nearest_content(&content, &skip, edit.position + edit.consumed - 1, 1);
        match edit.command {
            Command::AttachLeft(symbol) => {
                // The spacing between anchor and span is redundant once the
                // symbol glues to the anchor; the far side stays as the
                // post-symbol space.
                drop_adjacent(words, &mut drop, first, -1);
                if let Some(anchor) = prev {
                    texts[anchor].push_str(&symbol);
                } else if let Some(anchor) = next {
                    texts[anchor].insert_str(0, &symbol);
                }
            }
            Command::Glue(symbol) => {
                drop_adjacent(words, &mut drop, first, -1);
                drop_adjacent(words, &mut drop, last, 1);
                if let Some(anchor) = prev {
                    texts[anchor].push_str(&symbol);
                } else if let Some(anchor) = next {
                    texts[anchor].insert_str(0, &symbol);
                }
            }
            Command::LineBreak(breaks) => {
                drop_adjacent(words, &mut drop, first, -1);
                drop_adjacent(words, &mut drop, last, 1);
                if let Some(anchor) = prev {
                    texts[anchor].push_str(&breaks);
                } else if let Some(anchor) = next {
                    texts[anchor].insert_str(0, &breaks);
                }
            }
            Command::OpenQuote => {
                quote_open = true;
                if let Some(anchor) = next {
                    drop_adjacent(words, &mut drop, last, 1);
                    strip_leading_quotes(&mut texts[anchor]);
                    texts[anchor].insert(0, '"');
                } else if let Some(anchor) = prev {
                    drop_adjacent(words, &mut drop, first, -1);
                    strip_trailing_quotes(&mut texts[anchor]);
                    texts[anchor].push('"');
                }
            }
            Command::CloseQuote => {
                quote_open = false;
                if let Some(anchor) = prev {
                    drop_adjacent(words, &mut drop, first, -1);
                    strip_trailing_quotes(&mut texts[anchor]);
                    texts[anchor].push('"');
                } else if let Some(anchor) = next {
                    strip_leading_quotes(&mut texts[anchor]);
                    texts[anchor].insert(0, '"');
                }
            }
            Command::ToggleQuote => {
                quote_open = !quote_open;
                if quote_open {
                    if let Some(anchor) = next {
                        drop_adjacent(words, &mut drop, last, 1);
                        strip_leading_quotes(&mut texts[anchor]);
                        texts[anchor].insert(0, '"');
                    } else if let Some(anchor) = prev {
                        drop_adjacent(words, &mut drop, first, -1);
                        strip_trailing_quotes(&mut texts[anchor]);
                        texts[anchor].push('"');
                    }
                } else if let Some(anchor) = prev {
                    drop_adjacent(words, &mut drop, first, -1);
                    strip_trailing_quotes(&mut texts[anchor]);
                    texts[anchor].push('"');
                } else if let Some(anchor) = next {
                    strip_leading_quotes(&mut texts[anchor]);
                    texts[anchor].insert(0, '"');
                }
            }
        }
    }
    let mut out = Vec::with_capacity(words.len());
    for (index, mut word) in words.iter().cloned().enumerate() {
        if drop[index] {
            continue;
        }
        word.text = std::mem::take(&mut texts[index]);
        out.push(word);
    }
    Some(Conversion {
        words: out,
        commands: command_count,
    })
}

/// Match a command phrase starting at `content[position]`. Returns the
/// command and the number of content words consumed.
fn match_command(
    words: &[TranscriptWord],
    content: &[usize],
    position: usize,
) -> Option<(Command, usize)> {
    let first = core_word(&words[content.get(position).copied()?].text).to_lowercase();
    let second = content
        .get(position + 1)
        .map(|index| core_word(&words[*index].text).to_lowercase());
    let pair = second.as_deref();
    match (first.as_str(), pair) {
        ("period", _) => Some((Command::AttachLeft(".".to_string()), 1)),
        ("full", Some("stop" | "stops")) => Some((Command::AttachLeft(".".to_string()), 2)),
        ("comma", _) => Some((Command::AttachLeft(",".to_string()), 1)),
        ("question", Some("mark" | "marks")) => Some((Command::AttachLeft("?".to_string()), 2)),
        ("exclamation", Some("mark" | "marks" | "point" | "points")) => {
            Some((Command::AttachLeft("!".to_string()), 2))
        }
        ("colon", _) | ("colons", _) => Some((Command::AttachLeft(":".to_string()), 1)),
        ("semicolon", _) | ("semicolons", _) => Some((Command::AttachLeft(";".to_string()), 1)),
        ("semi", Some("colon" | "colons")) => Some((Command::AttachLeft(";".to_string()), 2)),
        ("new", Some("line" | "lines")) | ("next", Some("line" | "lines")) => {
            Some((Command::LineBreak("\n".to_string()), 2))
        }
        ("new", Some("paragraph" | "paragraphs")) | ("next", Some("paragraph" | "paragraphs")) => {
            Some((Command::LineBreak("\n\n".to_string()), 2))
        }
        ("double", Some("quote" | "quotes")) => Some((Command::ToggleQuote, 2)),
        ("open", Some("quote" | "quotes")) | ("begin", Some("quote" | "quotes")) => {
            Some((Command::OpenQuote, 2))
        }
        ("close", Some("quote" | "quotes" | "quotation" | "quotations"))
        | ("closed", Some("quote" | "quotes" | "quotation" | "quotations"))
        | ("end", Some("quote" | "quotes" | "quotation" | "quotations")) => {
            Some((Command::CloseQuote, 2))
        }
        ("single", Some("quote" | "quotes")) => Some((Command::Glue("'".to_string()), 2)),
        ("apostrophe", _) | ("apostrophes", _) => Some((Command::Glue("'".to_string()), 1)),
        _ => None,
    }
}

/// Nearest surviving original content index before (`direction = -1`) or
/// after (`+1`) `content[position]`.
fn nearest_content(
    content: &[usize],
    skip: &[bool],
    position: usize,
    direction: i32,
) -> Option<usize> {
    let mut cursor = position as i32 + direction;
    while cursor >= 0 && (cursor as usize) < content.len() {
        let original = content[cursor as usize];
        if !skip[original] {
            return Some(original);
        }
        cursor += direction;
    }
    None
}

/// Drop spacing tokens directly after (`direction = 1`) or before (`-1`)
/// the span edge at `from`, so glued symbols meet their anchor.
fn drop_adjacent(words: &[TranscriptWord], drop: &mut [bool], from: usize, direction: i32) {
    let mut cursor = from as i32 + direction;
    while cursor >= 0 && (cursor as usize) < words.len() {
        if words[cursor as usize].kind != "spacing" {
            break;
        }
        drop[cursor as usize] = true;
        cursor += direction;
    }
}

/// The matchable core of a token: text without the model's edge marks. The
/// marks are discarded with the command when matched.
fn core_word(text: &str) -> String {
    text.trim_matches(COMMAND_EDGE_MARKS).to_string()
}

fn strip_leading_quotes(text: &mut String) {
    while text.starts_with(QUOTE_MARKS) {
        text.remove(0);
    }
}

fn strip_trailing_quotes(text: &mut String) {
    while text.ends_with(QUOTE_MARKS) {
        text.pop();
    }
}

fn rebuild_text(words: &[TranscriptWord]) -> String {
    let mut text = String::new();
    for word in words {
        if word.kind == "spacing" {
            text.push_str(&word.text);
        } else if word.kind == "word" {
            append_piece(&mut text, &word.text);
        }
    }
    text.trim().to_string()
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

/// Same spacing contract as the composition assembler: no space before
/// closing marks, none after opening marks, none around whitespace.
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
    // Mirrors the composition assembler exactly: attached quote symbols
    // always travel inside token text, so no boundary ever sees a bare `"`.
    !matches!(left, '(' | '[' | '{' | '/' | '\'' | '‘' | '“')
}

#[cfg(test)]
mod tests {
    use super::apply_spoken_punctuation;
    use crate::stt::{Transcript, TranscriptWord};

    fn word(text: &str) -> TranscriptWord {
        TranscriptWord {
            text: text.to_string(),
            start: Some(1.0),
            end: Some(2.0),
            kind: "word".to_string(),
        }
    }

    fn spacing() -> TranscriptWord {
        TranscriptWord {
            text: " ".to_string(),
            start: None,
            end: None,
            kind: "spacing".to_string(),
        }
    }

    /// Interleave words with single spaces, as the transcription endpoint
    /// lays out word and spacing tokens.
    fn stream(words: &[&str]) -> Vec<TranscriptWord> {
        let mut out = Vec::new();
        for (index, text) in words.iter().enumerate() {
            if index > 0 {
                out.push(spacing());
            }
            out.push(word(text));
        }
        out
    }

    fn transcript(text: &str, words: Vec<TranscriptWord>) -> Transcript {
        Transcript {
            text: text.to_string(),
            words,
        }
    }

    fn converted(groups: &[&str]) -> Transcript {
        let words = stream(groups);
        let text = words
            .iter()
            .map(|word| word.text.as_str())
            .collect::<Vec<_>>()
            .join("");
        apply_spoken_punctuation(&transcript(&text, words))
    }

    #[test]
    fn reported_quote_doubling_collapses_to_one_pair() {
        let result = converted(&[
            "Double", "quote,", "\"The", "harness", "tree.", "Double", "quote.",
        ]);
        assert_eq!(result.text, "\"The harness tree.\"");
        assert!(result.words.iter().all(|word| {
            word.text != "Double" && word.text != "quote," && word.text != "quote."
        }));
    }

    #[test]
    fn comma_and_period_glue_left() {
        let result = converted(&["hello", "comma", "world", "period"]);
        assert_eq!(result.text, "hello, world.");
    }

    #[test]
    fn question_and_exclamation_marks() {
        let result = converted(&["really", "question", "mark"]);
        assert_eq!(result.text, "really?");
        let result = converted(&["wow", "exclamation", "mark", "note", "colon", "go"]);
        assert_eq!(result.text, "wow! note: go");
    }

    #[test]
    fn new_paragraph_breaks_without_stray_spaces() {
        let result = converted(&["first", "new", "paragraph", "second"]);
        assert_eq!(result.text, "first\n\nsecond");
        let result = converted(&["a", "new", "line", "b"]);
        assert_eq!(result.text, "a\nb");
    }

    #[test]
    fn explicit_open_and_close_quotes() {
        let result = converted(&["open", "quote", "hi", "there", "close", "quote"]);
        assert_eq!(result.text, "\"hi there\"");
    }

    #[test]
    fn apostrophe_glues_both_sides() {
        let result = converted(&["don", "single", "quote", "t"]);
        assert_eq!(result.text, "don't");
    }

    #[test]
    fn literal_escape_keeps_the_words() {
        let result = converted(&["literal", "comma", "ok"]);
        assert_eq!(result.text, "comma ok");
    }

    #[test]
    fn bare_command_words_still_convert() {
        // Documented tradeoff: without `literal`, a command word is a
        // command even when meant literally.
        let result = converted(&["the", "word", "comma"]);
        assert_eq!(result.text, "the word,");
    }

    #[test]
    fn transcript_without_commands_returns_unchanged() {
        let words = stream(&["just", "talking", "here"]);
        let text = "just talking here";
        let result = apply_spoken_punctuation(&transcript(text, words.clone()));
        assert_eq!(result.text, text);
        assert_eq!(result.words, words);
    }

    #[test]
    fn surviving_words_keep_their_timestamps() {
        let result = converted(&["hello", "comma", "world"]);
        let hello = result
            .words
            .iter()
            .find(|word| word.text == "hello,")
            .expect("comma attaches to hello");
        assert_eq!(hello.start, Some(1.0));
        assert_eq!(hello.end, Some(2.0));
        let world = result
            .words
            .iter()
            .find(|word| word.text == "world")
            .expect("world survives");
        assert_eq!(world.start, Some(1.0));
    }

    #[test]
    fn lone_command_leaves_no_content() {
        let result = converted(&["comma"]);
        assert_eq!(result.text, "");
        assert!(result.words.iter().all(|word| word.kind != "word"));
    }
}
