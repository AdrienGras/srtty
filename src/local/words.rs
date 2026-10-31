//! Turns whisper.cpp tokens into timed words.

use crate::transcribe::Word;

/// One non-special token of a whisper.cpp segment. Times are in centiseconds.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenPiece {
    pub bytes: Vec<u8>,
    pub t0_cs: i64,
    pub t1_cs: i64,
    /// DTW onset of the token, when DTW is on and found one.
    pub t_dtw_cs: Option<i64>,
}

/// Builds words from the tokens of each segment.
///
/// Works on bytes because an accented character can be split between two
/// tokens. A token starting with a space opens a word, the others join the
/// current word, and the first token of a segment always opens one.
///
/// A word starts at the DTW onset of its first token when there is one, else at
/// its `t0`: whisper.cpp's own token times are contiguous and absorb pauses.
/// Its end is the start plus whisper's duration estimate (first `t0` to last
/// `t1`, never negative).
///
/// Times are clamped: a word never ends before it starts, and never starts
/// before the previous word.
pub fn words_from_tokens(segments: &[Vec<TokenPiece>]) -> Vec<Word> {
    let mut words = Vec::new();
    for segment in segments {
        let mut current: Option<OpenWord> = None;
        for token in segment {
            if token.bytes.first() == Some(&b' ') || current.is_none() {
                if let Some(open) = current.take() {
                    open.close_into(&mut words);
                }
                current = Some(OpenWord {
                    bytes: Vec::new(),
                    start_cs: token.t_dtw_cs.unwrap_or(token.t0_cs),
                    t0_cs: token.t0_cs,
                    t1_cs: token.t1_cs,
                });
            }
            if let Some(open) = current.as_mut() {
                open.bytes.extend_from_slice(&token.bytes);
                open.t1_cs = token.t1_cs;
            }
        }
        if let Some(open) = current {
            open.close_into(&mut words);
        }
    }
    words
}

/// A word being accumulated from tokens.
struct OpenWord {
    bytes: Vec<u8>,
    /// DTW onset of the first token, or its `t0`.
    start_cs: i64,
    /// `t0` of the first token.
    t0_cs: i64,
    t1_cs: i64,
}

impl OpenWord {
    /// Decodes the word and pushes it unless it is blank.
    fn close_into(self, words: &mut Vec<Word>) {
        let text = String::from_utf8_lossy(&self.bytes).trim().to_string();
        if text.is_empty() {
            return;
        }
        let previous_start = words.last().map_or(f64::MIN, |w: &Word| w.start);
        let start = (self.start_cs as f64 / 100.0).max(previous_start);
        let duration_cs = (self.t1_cs - self.t0_cs).max(0);
        let end = ((self.start_cs + duration_cs) as f64 / 100.0).max(start);
        words.push(Word {
            start,
            end,
            text,
            speaker: None,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tp(text: impl AsRef<[u8]>, t0: i64, t1: i64) -> TokenPiece {
        TokenPiece {
            bytes: text.as_ref().to_vec(),
            t0_cs: t0,
            t1_cs: t1,
            t_dtw_cs: None,
        }
    }

    fn tpd(text: &str, t0: i64, t1: i64, dtw: i64) -> TokenPiece {
        TokenPiece {
            t_dtw_cs: Some(dtw),
            ..tp(text, t0, t1)
        }
    }

    fn summary(words: &[Word]) -> Vec<(&str, f64, f64)> {
        words
            .iter()
            .map(|w| (w.text.as_str(), w.start, w.end))
            .collect()
    }

    #[test]
    fn leading_space_opens_a_word() {
        let words = words_from_tokens(&[vec![
            tp(" La", 0, 20),
            tp(" Bar", 20, 40),
            tp("be", 40, 55),
            tp(" bleue", 55, 90),
            tp(",", 90, 92),
        ]]);
        assert_eq!(
            summary(&words),
            vec![
                ("La", 0.0, 0.2),
                ("Barbe", 0.2, 0.55),
                ("bleue,", 0.55, 0.92)
            ]
        );
        assert!(words.iter().all(|w| w.speaker.is_none()));
    }

    #[test]
    fn utf8_split_across_tokens_is_rejoined() {
        let words = words_from_tokens(&[vec![
            tp(" caf", 0, 30),
            tp([0xC3], 30, 35),
            tp([0xA9], 35, 40),
        ]]);
        assert_eq!(summary(&words), vec![("café", 0.0, 0.4)]);
    }

    #[test]
    fn segment_boundary_starts_a_word() {
        let words = words_from_tokens(&[vec![tp(" fin", 0, 20)], vec![tp("suite", 20, 50)]]);
        assert_eq!(
            summary(&words),
            vec![("fin", 0.0, 0.2), ("suite", 0.2, 0.5)]
        );
    }

    #[test]
    fn inverted_and_backward_times_are_clamped() {
        let inverted = words_from_tokens(&[vec![tp(" a", 50, 40)]]);
        assert_eq!(summary(&inverted), vec![("a", 0.5, 0.5)]);

        let backward = words_from_tokens(&[vec![tp(" a", 100, 120), tp(" b", 90, 130)]]);
        assert_eq!(summary(&backward), vec![("a", 1.0, 1.2), ("b", 1.0, 1.3)]);
    }

    #[test]
    fn blank_tokens_are_dropped() {
        assert!(words_from_tokens(&[vec![tp(" ", 0, 10)]]).is_empty());
    }

    #[test]
    fn invalid_utf8_is_replaced_not_panicking() {
        let words = words_from_tokens(&[vec![tp(" a", 0, 10), tp([0xFF], 10, 20)]]);
        assert_eq!(summary(&words), vec![("a\u{FFFD}", 0.0, 0.2)]);
    }

    #[test]
    fn dtw_onset_moves_the_word_and_keeps_its_duration() {
        let words = words_from_tokens(&[vec![tpd(" Ce", 5800, 5808, 5856)]]);
        assert_eq!(summary(&words), vec![("Ce", 58.56, 58.64)]);

        // A multi-token word starts at the DTW onset of its first token and
        // lasts from the first t0 to the last t1.
        let words =
            words_from_tokens(&[vec![tpd(" Bar", 100, 120, 150), tpd("be", 120, 140, 170)]]);
        assert_eq!(summary(&words), vec![("Barbe", 1.5, 1.9)]);
    }

    #[test]
    fn missing_dtw_falls_back_to_t0() {
        let words = words_from_tokens(&[vec![
            tpd(" Ce", 0, 10, 20),
            tp(" jour", 30, 50),
            tpd(" la", 50, 60, 70),
        ]]);
        assert_eq!(
            summary(&words),
            vec![("Ce", 0.2, 0.3), ("jour", 0.3, 0.5), ("la", 0.7, 0.8)]
        );
    }
}
