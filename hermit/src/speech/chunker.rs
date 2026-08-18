//! Sentence chunker (spec §5).
//!
//! This is the single most important piece of latency engineering in the daemon:
//! it decides when the first clause stops waiting and starts being spoken. The rest
//! of the answer keeps generating while the speaker is already talking.
//!
//! First chunk is emitted at the EARLIEST of:
//!   - a `[.!?,;:]` boundary after >= 12 tokens
//!   - 250 ms since the first token
//!   - 40 tokens
//!
//! After that, on sentence boundaries (with a hard ceiling so a model that forgets
//! punctuation cannot buffer forever).

use std::time::{Duration, Instant};

/// First-chunk thresholds.
const MIN_TOKENS_FOR_PUNCT: usize = 12;
const FIRST_CHUNK_DEADLINE: Duration = Duration::from_millis(250);
const MAX_TOKENS_FIRST: usize = 40;
/// Ceiling for later chunks so an unpunctuated stream still gets spoken.
const MAX_TOKENS_LATER: usize = 60;

/// Punctuation that may end the FIRST chunk (a clause is good enough to start on).
const FIRST_BOUNDARY: &[char] = &['.', '!', '?', ',', ';', ':'];
/// Punctuation that ends later chunks — full sentences only, so prosody holds up.
const SENTENCE_BOUNDARY: &[char] = &['.', '!', '?'];

#[derive(Debug)]
pub struct Chunker {
    buffer: String,
    tokens: usize,
    first_token_at: Option<Instant>,
    emitted_first: bool,
}

impl Default for Chunker {
    fn default() -> Self {
        Self::new()
    }
}

impl Chunker {
    pub fn new() -> Self {
        Self { buffer: String::new(), tokens: 0, first_token_at: None, emitted_first: false }
    }

    /// When the caller should give up waiting and emit whatever it has.
    ///
    /// `None` once the first chunk is out — later chunks are boundary-driven, and a
    /// timer there would chop sentences mid-clause for no latency gain.
    pub fn deadline(&self) -> Option<Instant> {
        if self.emitted_first {
            return None;
        }
        self.first_token_at.map(|t| t + FIRST_CHUNK_DEADLINE)
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.trim().is_empty()
    }

    /// Feed one streamed token. Returns a chunk when one is ready to speak.
    pub fn push(&mut self, token: &str) -> Option<String> {
        if token.is_empty() {
            return None;
        }
        if self.first_token_at.is_none() {
            self.first_token_at = Some(Instant::now());
        }
        self.buffer.push_str(token);
        self.tokens += 1;

        if !self.emitted_first {
            if self.tokens >= MAX_TOKENS_FIRST {
                return self.take_all();
            }
            if self.tokens >= MIN_TOKENS_FOR_PUNCT
                && let Some(cut) = last_boundary(&self.buffer, FIRST_BOUNDARY)
            {
                return self.take_upto(cut);
            }
            if self
                .first_token_at
                .is_some_and(|t| t.elapsed() >= FIRST_CHUNK_DEADLINE)
            {
                return self.take_all();
            }
            return None;
        }

        if let Some(cut) = last_boundary(&self.buffer, SENTENCE_BOUNDARY) {
            return self.take_upto(cut);
        }
        if self.tokens >= MAX_TOKENS_LATER {
            return self.take_all();
        }
        None
    }

    /// Called when the 250 ms deadline fires with no new token.
    pub fn on_deadline(&mut self) -> Option<String> {
        if self.emitted_first || self.is_empty() {
            return None;
        }
        self.take_all()
    }

    /// End of stream: emit whatever remains.
    pub fn flush(&mut self) -> Option<String> {
        if self.is_empty() {
            self.buffer.clear();
            self.tokens = 0;
            return None;
        }
        self.take_all()
    }

    /// Discard buffered text (barge-in).
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.tokens = 0;
        self.first_token_at = None;
        self.emitted_first = false;
    }

    fn take_all(&mut self) -> Option<String> {
        let text = std::mem::take(&mut self.buffer);
        self.tokens = 0;
        self.emitted_first = true;
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    }

    /// Split at `cut` (a byte index just past the boundary char), keeping the
    /// remainder buffered.
    fn take_upto(&mut self, cut: usize) -> Option<String> {
        let rest = self.buffer.split_off(cut);
        let chunk = std::mem::replace(&mut self.buffer, rest);
        self.emitted_first = true;
        // Token count restarts; the tail is usually a word or two.
        self.tokens = if self.buffer.trim().is_empty() { 0 } else { 1 };
        let trimmed = chunk.trim().to_string();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    }
}

/// Byte index just past the last usable boundary character in `s`.
///
/// Skips boundaries that are not real ones:
/// - a `.` between digits ("3.5", "$1.20")
/// - a `.` ending a common abbreviation ("Dr.", "e.g.", "U.S.")
///   — speaking those as sentence ends sounds broken.
fn last_boundary(s: &str, boundaries: &[char]) -> Option<usize> {
    let bytes_len = s.len();
    let mut best: Option<usize> = None;

    for (i, c) in s.char_indices() {
        if !boundaries.contains(&c) {
            continue;
        }
        let after = i + c.len_utf8();

        if c == '.' {
            let prev = s[..i].chars().next_back();
            let next = s[after..].chars().next();
            // Decimal or version number: 3.5
            if prev.is_some_and(|p| p.is_ascii_digit()) && next.is_some_and(|n| n.is_ascii_digit()) {
                continue;
            }
            // Abbreviation like "e.g." / "U.S." — single letter before the dot.
            if prev.is_some_and(|p| p.is_alphabetic())
                && s[..i].chars().rev().nth(1).is_some_and(|p2| !p2.is_alphanumeric())
            {
                continue;
            }
            if ends_with_abbreviation(&s[..after]) {
                continue;
            }
        }

        // A boundary must be followed by whitespace or end-of-buffer; otherwise we
        // are mid-token and the stream will extend it.
        let next = s[after..].chars().next();
        if next.is_some_and(|n| !n.is_whitespace()) {
            continue;
        }
        if after <= bytes_len {
            best = Some(after);
        }
    }
    best
}

const ABBREVIATIONS: &[&str] = &[
    "mr.", "mrs.", "ms.", "dr.", "prof.", "st.", "vs.", "etc.", "approx.", "no.",
    "fig.", "jan.", "feb.", "mar.", "apr.", "jun.", "jul.", "aug.", "sep.", "sept.",
    "oct.", "nov.", "dec.",
];

fn ends_with_abbreviation(s: &str) -> bool {
    let tail = s
        .rsplit(|c: char| c.is_whitespace())
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    ABBREVIATIONS.contains(&tail.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed whitespace-separated words as tokens.
    fn feed(c: &mut Chunker, text: &str) -> Vec<String> {
        let mut out = Vec::new();
        for (i, w) in text.split(' ').enumerate() {
            let tok = if i == 0 { w.to_string() } else { format!(" {w}") };
            if let Some(chunk) = c.push(&tok) {
                out.push(chunk);
            }
        }
        out
    }

    #[test]
    fn first_chunk_emits_at_punctuation_after_twelve_tokens() {
        let mut c = Chunker::new();
        // 13 tokens with a comma at the end of token 13.
        let out = feed(&mut c, "one two three four five six seven eight nine ten eleven twelve,");
        assert_eq!(out.len(), 1);
        assert!(out[0].ends_with(','), "got {:?}", out[0]);
        assert!(out[0].starts_with("one two"));
    }

    #[test]
    fn punctuation_before_twelve_tokens_does_not_emit_early() {
        let mut c = Chunker::new();
        // Comma at token 3 — too early; keep buffering for a fuller first clause.
        let out = feed(&mut c, "yes, of course");
        assert!(out.is_empty(), "emitted too early: {out:?}");
    }

    #[test]
    fn first_chunk_emits_at_forty_tokens_without_punctuation() {
        let mut c = Chunker::new();
        let text = (0..45).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let out = feed(&mut c, &text);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].split_whitespace().count(), 40);
    }

    #[test]
    fn deadline_is_set_after_the_first_token_and_cleared_after_first_chunk() {
        let mut c = Chunker::new();
        assert!(c.deadline().is_none(), "no deadline before any token");
        c.push("hello");
        let d = c.deadline().expect("deadline after first token");
        assert!(d > Instant::now() - Duration::from_millis(1));
        // Force the first emit.
        let _ = c.on_deadline();
        assert!(c.deadline().is_none(), "later chunks are boundary-driven");
    }

    #[test]
    fn on_deadline_emits_a_short_first_clause() {
        let mut c = Chunker::new();
        feed(&mut c, "the answer is");
        let chunk = c.on_deadline().expect("deadline should flush the partial clause");
        assert_eq!(chunk, "the answer is");
        assert!(c.on_deadline().is_none(), "nothing left to emit");
    }

    #[test]
    fn later_chunks_split_on_sentences_not_commas() {
        let mut c = Chunker::new();
        feed(&mut c, "first clause here to get things going quickly now,");
        let _ = c.on_deadline();
        let out = feed(
            &mut c,
            "this is a sentence. and here, with a comma, is more text that continues on.",
        );
        assert!(out.iter().all(|s| s.ends_with('.')), "later chunks must end sentences: {out:?}");
        assert!(out.iter().any(|s| s.contains("with a comma")));
    }

    #[test]
    fn decimals_are_not_sentence_boundaries() {
        assert_eq!(last_boundary("it costs 3.5 million", SENTENCE_BOUNDARY), None);
        assert_eq!(last_boundary("version 2.0 is out", SENTENCE_BOUNDARY), None);
    }

    #[test]
    fn abbreviations_are_not_sentence_boundaries() {
        assert_eq!(last_boundary("Dr. Smith arrived", SENTENCE_BOUNDARY), None);
        assert_eq!(last_boundary("approx. 40 people", SENTENCE_BOUNDARY), None);
        assert_eq!(last_boundary("e.g. this one", SENTENCE_BOUNDARY), None);
    }

    #[test]
    fn real_sentence_ends_are_found() {
        let cut = last_boundary("It is done. And then", SENTENCE_BOUNDARY).unwrap();
        assert_eq!(&"It is done. And then"[..cut], "It is done.");
    }

    #[test]
    fn boundary_must_be_followed_by_whitespace() {
        // Mid-token period: the stream will extend it, so do not cut.
        assert_eq!(last_boundary("see example.co", SENTENCE_BOUNDARY), None);
    }

    #[test]
    fn flush_returns_the_tail_and_empties() {
        let mut c = Chunker::new();
        feed(&mut c, "trailing words with no punctuation");
        let tail = c.flush().unwrap();
        assert_eq!(tail, "trailing words with no punctuation");
        assert!(c.flush().is_none());
    }

    #[test]
    fn flush_on_empty_chunker_is_none() {
        let mut c = Chunker::new();
        assert!(c.flush().is_none());
        c.push("   ");
        assert!(c.flush().is_none(), "whitespace only is not a chunk");
    }

    #[test]
    fn reset_discards_everything_for_barge_in() {
        let mut c = Chunker::new();
        feed(&mut c, "half a sentence that will be interrupted");
        c.reset();
        assert!(c.is_empty());
        assert!(c.deadline().is_none());
        assert!(c.flush().is_none());
    }

    #[test]
    fn unpunctuated_later_text_still_gets_emitted() {
        let mut c = Chunker::new();
        feed(&mut c, "opening clause.");
        let _ = c.on_deadline();
        let text = (0..70).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ");
        let out = feed(&mut c, &text);
        assert!(!out.is_empty(), "must not buffer forever without punctuation");
    }

    #[test]
    fn no_text_is_lost_across_a_full_stream() {
        let mut c = Chunker::new();
        let script = "The tide in Bergen peaks at two twenty this afternoon, about four metres. \
                      That is higher than yesterday. Bring boots.";
        let mut chunks = feed(&mut c, script);
        if let Some(tail) = c.flush() {
            chunks.push(tail);
        }
        let rejoined = chunks.join(" ");
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(norm(&rejoined), norm(script), "chunking must be lossless");
    }

    #[test]
    fn multibyte_text_never_panics() {
        let mut c = Chunker::new();
        let mut chunks = feed(&mut c, "日本語のテキストです。これは二番目の文です。さらに続きます。");
        if let Some(t) = c.flush() {
            chunks.push(t);
        }
        assert!(!chunks.is_empty());
    }
}
