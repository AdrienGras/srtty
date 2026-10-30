//! Cue building: groups display words into subtitle cues, filters hallucinated
//! repetitions and renumbers speakers.

use super::words::norm;
use crate::config::SubtitleConfig;
use crate::transcribe::Word;

/// One subtitle: a time span, one or two lines of text and an optional speaker.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub lines: Vec<String>,
    pub speaker: Option<u32>,
}

impl Cue {
    /// Text with lines joined by a space.
    fn text(&self) -> String {
        self.lines.join(" ")
    }
}

/// Limits that decide where one cue ends and the next begins.
#[derive(Debug, Clone, PartialEq)]
pub struct CueRules {
    pub max_chars_per_line: usize,
    pub max_lines: usize,
    pub max_duration_s: f64,
    pub min_duration_s: f64,
    pub pause_break_s: f64,
}

impl From<&SubtitleConfig> for CueRules {
    fn from(c: &SubtitleConfig) -> Self {
        Self {
            max_chars_per_line: c.max_chars_per_line,
            max_lines: c.max_lines,
            max_duration_s: c.max_duration_s,
            min_duration_s: 1.0,
            pause_break_s: 0.8,
        }
    }
}

/// Word text without trailing whitespace and a trailing closing guillemet.
fn core_text(text: &str) -> &str {
    let t = text.trim_end();
    t.strip_suffix('»').unwrap_or(t).trim_end()
}

fn ends_sentence(text: &str) -> bool {
    core_text(text).ends_with(['.', '?', '!', '…'])
}

fn ends_clause(text: &str) -> bool {
    core_text(text).ends_with([',', ';', ':'])
}

fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Splits `text` into two lines at the space that minimizes the longer line.
/// A word longer than the line is never cut.
fn wrap(text: String, max_chars: usize) -> Vec<String> {
    if char_len(&text) <= max_chars {
        return vec![text];
    }
    let best = text
        .char_indices()
        .filter(|&(_, c)| c == ' ')
        .map(|(i, _)| {
            let longest = char_len(&text[..i]).max(char_len(&text[i + 1..]));
            (longest, i)
        })
        .min_by_key(|&(longest, _)| longest);
    match best {
        Some((_, i)) => vec![text[..i].to_string(), text[i + 1..].to_string()],
        None => vec![text],
    }
}

/// Closes the words of one cue into a `Cue`; `next_start` bounds the minimum-duration extension.
fn close_cue(words: &[Word], next_start: Option<f64>, rules: &CueRules) -> Cue {
    let start = words[0].start;
    let mut end = words[words.len() - 1].end;
    if end - start < rules.min_duration_s {
        let target = start + rules.min_duration_s;
        end = end.max(next_start.map_or(target, |n| target.min(n)));
    }
    let text = words
        .iter()
        .map(|w| w.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    Cue {
        start,
        end,
        lines: wrap(text, rules.max_chars_per_line),
        speaker: words[0].speaker,
    }
}

/// Groups words into cues following `rules`.
pub fn build_cues(words: &[Word], rules: &CueRules) -> Vec<Cue> {
    let max_len = rules.max_chars_per_line * rules.max_lines;
    let mut cues = Vec::new();
    let mut from = 0;
    let mut len = 0;
    for (i, w) in words.iter().enumerate() {
        if i > from {
            let first = &words[from];
            let prev = &words[i - 1];
            let w_len = char_len(&w.text);
            if w.speaker != first.speaker
                || w.start - prev.end >= rules.pause_break_s
                || len + 1 + w_len > max_len
                || w.end - first.start > rules.max_duration_s
            {
                cues.push(close_cue(&words[from..i], Some(w.start), rules));
                from = i;
            }
        }
        len = if i == from {
            char_len(&w.text)
        } else {
            len + 1 + char_len(&w.text)
        };
        if ends_sentence(&w.text) || (ends_clause(&w.text) && len >= rules.max_chars_per_line) {
            cues.push(close_cue(
                &words[from..=i],
                words.get(i + 1).map(|n| n.start),
                rules,
            ));
            from = i + 1;
        }
    }
    if from < words.len() {
        cues.push(close_cue(&words[from..], None, rules));
    }
    cues
}

/// Drops empty cues and collapses runs of 3 or more identical cues to the first one.
pub fn filter_cues(cues: Vec<Cue>) -> Vec<Cue> {
    let keyed: Vec<(String, Cue)> = cues
        .into_iter()
        .map(|c| (norm(&c.text()), c))
        .filter(|(k, _)| !k.is_empty())
        .collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < keyed.len() {
        let run = keyed[i..]
            .iter()
            .take_while(|(k, _)| *k == keyed[i].0)
            .count();
        if run >= 3 {
            out.push(keyed[i].1.clone());
        } else {
            out.extend(keyed[i..i + run].iter().map(|(_, c)| c.clone()));
        }
        i += run;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn w(text: &str, a: f64, b: f64) -> Word {
        Word {
            start: a,
            end: b,
            text: text.into(),
            speaker: None,
        }
    }

    fn rules() -> CueRules {
        CueRules {
            max_chars_per_line: 42,
            max_lines: 2,
            max_duration_s: 7.0,
            min_duration_s: 1.0,
            pause_break_s: 0.8,
        }
    }

    #[test]
    fn sentence_end_closes_cue() {
        let c = build_cues(
            &[
                w("Bonjour.", 0.0, 0.5),
                w("Ça", 0.6, 0.7),
                w("va ?", 0.7, 0.9),
            ],
            &rules(),
        );
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].lines, ["Bonjour."]);
        assert_eq!(c[0].end, 0.6); // prolongée vers 1,0 s mais bornée par le mot suivant
    }

    #[test]
    fn long_pause_closes_cue() {
        let c = build_cues(&[w("un", 0.0, 0.2), w("deux", 1.5, 1.8)], &rules());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn max_duration_closes_cue() {
        let words: Vec<Word> = (0..20)
            .map(|i| w("mot", i as f64 * 0.5, i as f64 * 0.5 + 0.4))
            .collect();
        assert!(
            build_cues(&words, &rules())
                .iter()
                .all(|c| c.end - c.start <= 7.0 + 1e-9)
        );
    }

    #[test]
    fn wraps_into_two_balanced_lines_within_limits() {
        let text = "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda";
        let words: Vec<Word> = text
            .split(' ')
            .enumerate()
            .map(|(i, s)| w(s, i as f64 * 0.3, i as f64 * 0.3 + 0.25))
            .collect();
        for c in build_cues(&words, &rules()) {
            assert!(c.lines.len() <= 2);
            assert!(c.lines.iter().all(|l| l.chars().count() <= 42));
        }
    }

    #[test]
    fn speaker_change_closes_cue() {
        let mut a = w("Madame", 0.0, 0.4);
        a.speaker = Some(4);
        let mut b = w("Holà", 0.45, 0.8);
        b.speaker = Some(2);
        assert_eq!(build_cues(&[a, b], &rules()).len(), 2);
    }

    #[test]
    fn repeated_hallucinations_collapse() {
        let cue = |s: &str, t: f64| Cue {
            start: t,
            end: t + 1.0,
            lines: vec![s.into()],
            speaker: None,
        };
        let out = filter_cues(vec![
            cue("Merci.", 0.0),
            cue("Merci.", 1.0),
            cue("merci", 2.0),
            cue("", 3.0),
            cue("Fin.", 4.0),
        ]);
        assert_eq!(
            out.iter().map(|c| c.lines[0].as_str()).collect::<Vec<_>>(),
            ["Merci.", "Fin."]
        );
    }

    #[test]
    fn wraps_long_text_at_the_most_balanced_space() {
        let c = build_cues(
            &[
                w("alpha", 0.0, 0.2),
                w("beta", 0.2, 0.4),
                w("gamma", 0.4, 0.6),
                w("delta", 0.6, 0.8),
            ],
            &CueRules {
                max_chars_per_line: 12,
                ..rules()
            },
        );
        assert_eq!(c[0].lines, ["alpha beta", "gamma delta"]);
    }

    #[test]
    fn overlong_word_is_never_cut() {
        let c = build_cues(
            &[w("anticonstitutionnellement", 0.0, 1.0)],
            &CueRules {
                max_chars_per_line: 10,
                ..rules()
            },
        );
        assert_eq!(c[0].lines, ["anticonstitutionnellement"]);
    }

    #[test]
    fn closing_guillemet_still_ends_sentence() {
        let c = build_cues(&[w("fin. »", 0.0, 0.5), w("suite", 0.6, 0.9)], &rules());
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn comma_closes_cue_only_once_line_is_full() {
        let short = build_cues(&[w("Oui,", 0.0, 0.3), w("bien", 0.3, 0.6)], &rules());
        assert_eq!(short.len(), 1);
        let long_word = "x".repeat(41);
        let long = build_cues(
            &[w(&format!("{long_word},"), 0.0, 0.5), w("bien", 0.5, 0.8)],
            &rules(),
        );
        assert_eq!(long.len(), 2);
    }

    #[test]
    fn short_cue_is_extended_to_minimum_duration() {
        let c = build_cues(&[w("ok", 0.0, 0.2)], &rules());
        assert_eq!(c[0].end, 1.0);
    }
}
