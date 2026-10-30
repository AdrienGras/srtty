//! Word normalization: providers return words as bare fragments, leading-space
//! fragments or whole words. This module turns them into punctuated words.

use crate::transcribe::{Segment, Transcript, Usability, Word};

/// Produces display words with timings from a transcript, whatever the provider's token style.
pub fn normalize_words(t: &Transcript) -> Vec<Word> {
    if !t.words.is_empty() {
        return align_text(&t.text, &t.words).unwrap_or_else(|| merge_fragments(&t.words));
    }
    if t.usability() == Usability::FineSegments {
        return words_from_segments(&t.segments);
    }
    Vec::new()
}

/// Lowercased alphanumeric characters only.
fn norm(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Gives each whitespace-separated word of `text` the timing of the tokens that spell it.
/// Returns `None` when the tokens do not spell `text`.
pub fn align_text(text: &str, tokens: &[Word]) -> Option<Vec<Word>> {
    let mut i = 0;
    let mut out: Vec<Word> = Vec::new();
    let mut prefix = String::new();
    for d in text.split_whitespace() {
        let nd = norm(d);
        if nd.is_empty() {
            match out.last_mut() {
                Some(last) => {
                    last.text.push(' ');
                    last.text.push_str(d);
                }
                None => {
                    if !prefix.is_empty() {
                        prefix.push(' ');
                    }
                    prefix.push_str(d);
                }
            }
            continue;
        }
        let target_len = nd.chars().count();
        let mut acc = String::new();
        let mut first: Option<usize> = None;
        while acc.chars().count() < target_len {
            let token = tokens.get(i)?;
            let nt = norm(&token.text);
            i += 1;
            if nt.is_empty() {
                continue;
            }
            first.get_or_insert(i - 1);
            acc.push_str(&nt);
            if !nd.starts_with(&acc) {
                return None;
            }
        }
        let first = tokens[first?].clone();
        let text = if prefix.is_empty() {
            d.to_string()
        } else {
            format!("{} {d}", std::mem::take(&mut prefix))
        };
        out.push(Word {
            start: first.start,
            end: tokens[i - 1].end,
            text,
            speaker: first.speaker,
        });
    }
    Some(out)
}

/// Joins fragments into words. With leading-space tokens, a token starting with a space opens a
/// word and any other token sticks to the previous one; without any, tokens are kept as they are.
pub fn merge_fragments(tokens: &[Word]) -> Vec<Word> {
    let spaced = tokens
        .iter()
        .any(|w| w.text.starts_with(char::is_whitespace));
    let mut out: Vec<Word> = Vec::new();
    if !spaced {
        for w in tokens {
            let text = w.text.trim();
            if !text.is_empty() {
                out.push(Word {
                    text: text.to_string(),
                    ..w.clone()
                });
            }
        }
        return out;
    }
    let mut open_new = true;
    for w in tokens {
        if w.text.starts_with(char::is_whitespace) {
            open_new = true;
        }
        let text = w.text.trim();
        if text.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if !open_new => {
                last.text.push_str(text);
                last.end = w.end;
            }
            _ => out.push(Word {
                text: text.to_string(),
                ..w.clone()
            }),
        }
        open_new = false;
    }
    out
}

/// Splits each segment into words, sharing its duration in proportion to word length.
pub fn words_from_segments(segs: &[Segment]) -> Vec<Word> {
    let mut out = Vec::new();
    for seg in segs {
        let parts: Vec<&str> = seg.text.split_whitespace().collect();
        let total: usize = parts.iter().map(|p| p.chars().count()).sum();
        if total == 0 {
            continue;
        }
        let duration = seg.end - seg.start;
        let mut consumed = 0;
        for (n, part) in parts.iter().enumerate() {
            let start = seg.start + duration * consumed as f64 / total as f64;
            consumed += part.chars().count();
            let end = if n + 1 == parts.len() {
                seg.end
            } else {
                seg.start + duration * consumed as f64 / total as f64
            };
            out.push(Word {
                start,
                end,
                text: part.to_string(),
                speaker: seg.speaker,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fx;
    use crate::transcribe::{Segment, Transcript, Word};

    fn t(name: &str) -> Transcript {
        crate::transcribe::parse::parse_verbose_json(&fx(name)).unwrap()
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-6
    }

    #[test]
    fn whisper1_apostrophe_fragments_are_joined_with_punctuation() {
        let w = normalize_words(&t("whisper1"));
        assert_eq!(w[0].text, "C'est");
        assert!(close(w[0].start, 0.0) && close(w[0].end, 0.4));
        assert_eq!(w[1].text, "qu'il");
        assert!(w.iter().any(|x| x.text == "femmes,"));
    }

    #[test]
    fn whisper_turbo_leading_space_tokens_and_lone_guillemet() {
        let w = normalize_words(&t("whisper_turbo"));
        assert_eq!(w[0].text, "c'est");
        assert!(close(w[0].start, 0.18) && close(w[0].end, 0.4));
        assert!(w.iter().any(|x| x.text == "femmes. »"));
    }

    #[test]
    fn full_words_keep_their_timings() {
        let w = normalize_words(&t("scribe"));
        assert_eq!(w[0].text, "C'est");
        assert!(close(w[0].start, 0.32) && close(w[0].end, 0.4));
    }

    #[test]
    fn merge_fragments_fallback() {
        let tok = |s: &str, a: f64, b: f64| Word {
            start: a,
            end: b,
            text: s.into(),
            speaker: None,
        };
        let w = merge_fragments(&[
            tok(" c", 0.0, 0.1),
            tok("'est", 0.1, 0.2),
            tok(" bon", 0.3, 0.5),
        ]);
        assert_eq!(
            w.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            ["c'est", "bon"]
        );
        assert_eq!((w[0].start, w[0].end), (0.0, 0.2));
    }

    #[test]
    fn misaligned_text_falls_back_to_merge() {
        let tok = |s: &str| Word {
            start: 0.0,
            end: 1.0,
            text: s.into(),
            speaker: None,
        };
        let tr = Transcript {
            text: "totally different".into(),
            words: vec![tok("hello")],
            ..Default::default()
        };
        assert_eq!(normalize_words(&tr)[0].text, "hello");
    }

    #[test]
    fn segments_fallback_splits_time_by_characters() {
        let s = Segment {
            start: 10.0,
            end: 14.0,
            text: "ab abcdef".into(),
            speaker: Some(1),
        };
        let w = words_from_segments(&[s]);
        assert_eq!(w.len(), 2);
        assert!((w[0].end - 11.0).abs() < 1e-9 && (w[1].end - 14.0).abs() < 1e-9);
        assert_eq!(w[1].speaker, Some(1));
    }
}
