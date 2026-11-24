//! Splits a streamed answer into sentences for display and speech.
//!
//! This only decides *where* to break text that is arriving piece by piece; it
//! never changes, drops, or reorders the model's words. Joining every emitted
//! sentence (with the whitespace between them) reproduces the answer exactly
//! up to leading/trailing whitespace of each sentence.

/// Common abbreviations whose trailing period is not a sentence end.
const ABBREVIATIONS: &[&str] = &[
    "e.g", "i.e", "etc", "vs", "mr", "mrs", "ms", "dr", "st", "prof", "approx", "no", "fig",
];

/// Chinese and Japanese full-width sentence ends. Unlike `.`, they never
/// appear inside numbers or abbreviations, and the text after them has no
/// space, so they end a sentence on their own.
const FULL_WIDTH_ENDS: [char; 3] = ['。', '！', '？'];

/// Closing quotes and brackets that belong to the sentence they follow.
const CLOSERS: [char; 13] = [
    '"', '\'', ')', ']', '”', '’', '」', '』', '）', '】', '.', '!', '?',
];

/// A sentence longer than this is flushed at the last comma or space (or, in
/// text without spaces such as Chinese, at the limit itself), so a run-on
/// answer still starts speaking within a bounded wait.
const MAX_SENTENCE_CHARS: usize = 220;

#[derive(Debug, Default)]
pub struct SentenceSplitter {
    pending: String,
}

impl SentenceSplitter {
    /// Adds streamed text and returns every sentence it completed.
    pub fn push(&mut self, piece: &str) -> Vec<String> {
        self.pending.push_str(piece);
        let mut sentences = Vec::new();
        while let Some(end) = self.next_boundary() {
            let sentence: String = self.pending.drain(..end).collect();
            let sentence = sentence.trim();
            if !sentence.is_empty() {
                sentences.push(sentence.to_owned());
            }
        }
        sentences
    }

    /// Returns whatever is left when the stream ends (an answer whose last
    /// sentence has no final punctuation, for example one cut off by the
    /// token cap).
    pub fn finish(&mut self) -> Option<String> {
        let rest = std::mem::take(&mut self.pending);
        let rest = rest.trim();
        (!rest.is_empty()).then(|| rest.to_owned())
    }

    /// The byte index just after a complete sentence, if there is one.
    fn next_boundary(&self) -> Option<usize> {
        let text = &self.pending;
        let mut characters = text.char_indices().peekable();
        while let Some((index, character)) = characters.next() {
            if character == '\n' {
                return Some(index + 1);
            }
            let full_width = FULL_WIDTH_ENDS.contains(&character);
            if !full_width && !matches!(character, '.' | '!' | '?' | '…') {
                continue;
            }
            // Include closing quotes/brackets and repeated punctuation.
            let mut end = index + character.len_utf8();
            while let Some(&(next_index, next)) = characters.peek() {
                if CLOSERS.contains(&next) || FULL_WIDTH_ENDS.contains(&next) {
                    end = next_index + next.len_utf8();
                    characters.next();
                } else {
                    break;
                }
            }
            // A boundary needs a following character, so a closing quote in
            // the next piece still joins this sentence. After `.`, `!`, or
            // `?` it must be whitespace: "3.5" or "e.g." mid-word is not an
            // end. After a full-width end, any character will do.
            match characters.peek() {
                Some(_) if full_width => return Some(end),
                Some(&(_, next)) if next.is_whitespace() => {
                    if character == '.' && ends_with_abbreviation(&text[..index]) {
                        continue;
                    }
                    return Some(end);
                }
                _ => continue,
            }
        }
        if text.chars().count() > MAX_SENTENCE_CHARS {
            // Break inside the limit: at its last comma, else its last space.
            let limit = text
                .char_indices()
                .nth(MAX_SENTENCE_CHARS)
                .map_or(text.len(), |(index, _)| index);
            let window = &text[..limit];
            let after_comma = [", ", "，", "、"]
                .iter()
                .filter_map(|comma| {
                    window
                        .rfind(comma)
                        .map(|index| index + comma.trim_end().len())
                })
                .max();
            let cut = after_comma
                .or_else(|| window.rfind(' '))
                .filter(|&index| index > 0)
                .unwrap_or(limit);
            return Some(cut);
        }
        None
    }
}

fn ends_with_abbreviation(before_period: &str) -> bool {
    let word = before_period
        .rsplit(|character: char| character.is_whitespace() || character == '(')
        .next()
        .unwrap_or("")
        .to_lowercase();
    ABBREVIATIONS.contains(&word.as_str())
}

#[cfg(test)]
mod tests {
    use super::SentenceSplitter;

    fn split_all(pieces: &[&str]) -> Vec<String> {
        let mut splitter = SentenceSplitter::default();
        let mut sentences = Vec::new();
        for piece in pieces {
            sentences.extend(splitter.push(piece));
        }
        sentences.extend(splitter.finish());
        sentences
    }

    #[test]
    fn sentences_are_emitted_as_soon_as_they_complete() {
        let mut splitter = SentenceSplitter::default();
        assert!(splitter.push("The fridge has").is_empty());
        assert!(
            splitter.push(" magnets.").is_empty(),
            "wait for the next character"
        );
        assert_eq!(
            splitter.push(" Some are round"),
            ["The fridge has magnets."]
        );
        assert_eq!(splitter.push("! Others"), ["Some are round!"]);
        assert_eq!(splitter.finish().as_deref(), Some("Others"));
    }

    #[test]
    fn token_sized_pieces_give_the_same_sentences_as_one_chunk() {
        let text = "It is 3.5 meters tall. Is that right? Yes, e.g. a door is shorter. Done";
        let whole = split_all(&[text]);
        let pieces: Vec<String> = text.chars().map(String::from).collect();
        let piecewise = split_all(&pieces.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(whole, piecewise);
        assert_eq!(
            whole,
            [
                "It is 3.5 meters tall.",
                "Is that right?",
                "Yes, e.g. a door is shorter.",
                "Done"
            ]
        );
    }

    #[test]
    fn closing_quotes_stay_with_their_sentence() {
        assert_eq!(
            split_all(&["He said \"hello.\" Then he left."]),
            ["He said \"hello.\"", "Then he left."]
        );
    }

    #[test]
    fn newlines_end_a_sentence() {
        assert_eq!(
            split_all(&["First line\nSecond line"]),
            ["First line", "Second line"]
        );
    }

    #[test]
    fn a_run_on_sentence_is_flushed_at_a_comma() {
        let long = format!("{}, and then more words follow", "word ".repeat(50));
        let sentences = split_all(&[&long]);
        assert!(sentences.len() >= 2, "{sentences:?}");
        assert!(
            sentences
                .iter()
                .all(|sentence| sentence.chars().count() <= 230)
        );
    }

    #[test]
    fn nothing_is_lost_or_reordered() {
        let text = "One. Two! Three? Four… Five";
        let joined = split_all(&[text]).join(" ");
        assert_eq!(joined, text);
    }

    #[test]
    fn chinese_sentences_split_without_spaces() {
        let mut splitter = SentenceSplitter::default();
        assert!(
            splitter.push("你好。").is_empty(),
            "wait for the next character"
        );
        assert_eq!(splitter.push("今天"), ["你好。"]);
        assert_eq!(
            split_all(&["这是苹果。", "你喜欢吗？我很喜欢！", "好"]),
            ["这是苹果。", "你喜欢吗？", "我很喜欢！", "好"]
        );
    }

    #[test]
    fn chinese_closing_quotes_stay_with_their_sentence() {
        assert_eq!(
            split_all(&["他说：「你好。", "」然后走了。"]),
            ["他说：「你好。」", "然后走了。"]
        );
    }

    #[test]
    fn token_sized_chinese_pieces_give_the_same_sentences() {
        let text = "苹果是红色的。香蕉呢？黄色的！";
        let pieces: Vec<String> = text.chars().map(String::from).collect();
        let piecewise = split_all(&pieces.iter().map(String::as_str).collect::<Vec<_>>());
        assert_eq!(piecewise, split_all(&[text]));
        assert_eq!(piecewise.concat(), text, "nothing lost or reordered");
    }

    #[test]
    fn a_run_on_chinese_sentence_is_flushed_at_a_comma_or_the_limit() {
        let with_comma = format!("{}，{}", "字".repeat(150), "字".repeat(150));
        let sentences = split_all(&[&with_comma]);
        assert_eq!(sentences[0], format!("{}，", "字".repeat(150)));
        assert_eq!(sentences.concat(), with_comma);

        let no_break = "字".repeat(500);
        let sentences = split_all(&[&no_break]);
        assert!(sentences.len() >= 2, "{sentences:?}");
        assert!(sentences.iter().all(|s| s.chars().count() <= 220));
        assert_eq!(sentences.concat(), no_break);
    }

    #[test]
    fn whitespace_only_input_emits_nothing() {
        assert!(split_all(&["   ", "\n\n"]).is_empty());
    }
}
