//! Cue building: groups display words into subtitle cues, filters hallucinated
//! repetitions and renumbers speakers.

use super::words::norm;
use crate::config::SubtitleConfig;
use crate::transcribe::Word;

/// One subtitle: a time span, up to `max_lines` lines of text and an optional speaker.
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
    /// Characters the rendered first line holds before the text, such as the
    /// SRT speaker prefix `[Speaker 1] `, taken from that line's budget.
    pub first_line_reserve: usize,
}

impl From<&SubtitleConfig> for CueRules {
    fn from(c: &SubtitleConfig) -> Self {
        Self {
            max_chars_per_line: c.max_chars_per_line,
            max_lines: c.max_lines,
            max_duration_s: c.max_duration_s,
            min_duration_s: 1.0,
            pause_break_s: 0.8,
            first_line_reserve: 0,
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

/// Lays `words` out in the fewest lines possible, at most `rules.max_lines`,
/// then balances those lines by minimizing the longest one. The first line
/// also counts `rules.first_line_reserve` characters. A word is never cut: a
/// line may exceed `max_chars_per_line` only when it holds a single word.
/// `None` when the words cannot be laid out within the limits.
fn layout(words: &[&str], rules: &CueRules) -> Option<Vec<String>> {
    let n = words.len();
    let lens: Vec<usize> = words.iter().map(|w| char_len(w)).collect();
    // Width that a line made of `words[i..j]` weighs in the balance, or `None`
    // when it does not fit. A lone overlong word fits and weighs nothing, so
    // the other lines are still balanced.
    let width = |i: usize, j: usize| {
        let reserve = if i == 0 { rules.first_line_reserve } else { 0 };
        let w = reserve + lens[i..j].iter().sum::<usize>() + (j - i - 1);
        if w <= rules.max_chars_per_line {
            Some(w)
        } else {
            (j - i == 1).then_some(0)
        }
    };
    // best[l][i]: (longest line, end of the first line) for `words[i..]` in exactly `l` lines.
    let mut best: Vec<Vec<Option<(usize, usize)>>> = vec![vec![None; n + 1]];
    best[0][n] = Some((0, n));
    for l in 1..=rules.max_lines.max(1) {
        let row: Vec<Option<(usize, usize)>> = (0..=n)
            .map(|i| {
                (i + 1..=n)
                    .filter_map(|j| {
                        let (rest, _) = best[l - 1][j]?;
                        Some((width(i, j)?.max(rest), j))
                    })
                    .min_by_key(|&(longest, _)| longest)
            })
            .collect();
        best.push(row);
        if best[l][0].is_some() {
            let mut lines = Vec::with_capacity(l);
            let mut i = 0;
            for k in (1..=l).rev() {
                let (_, j) = best[k][i]?;
                lines.push(words[i..j].join(" "));
                i = j;
            }
            return Some(lines);
        }
    }
    None
}

/// Closes the words of one cue into a `Cue`; `next_start` bounds the minimum-duration extension.
fn close_cue(words: &[Word], next_start: Option<f64>, rules: &CueRules) -> Cue {
    let start = words[0].start;
    let mut end = words[words.len() - 1].end;
    if end - start < rules.min_duration_s {
        let target = start + rules.min_duration_s;
        end = end.max(next_start.map_or(target, |n| target.min(n)));
    }
    let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
    Cue {
        start,
        end,
        // Every cue was grown only while its words could be laid out.
        lines: layout(&texts, rules).unwrap_or_else(|| vec![texts.join(" ")]),
        speaker: words[0].speaker,
    }
}

/// Groups words into cues following `rules`.
pub fn build_cues(words: &[Word], rules: &CueRules) -> Vec<Cue> {
    let texts: Vec<&str> = words.iter().map(|w| w.text.as_str()).collect();
    let mut cues = Vec::new();
    let mut from = 0;
    let mut len = 0;
    for (i, w) in words.iter().enumerate() {
        if i > from {
            let first = &words[from];
            let prev = &words[i - 1];
            if w.speaker != first.speaker
                || w.start - prev.end >= rules.pause_break_s
                || w.end - first.start > rules.max_duration_s
                || layout(&texts[from..=i], rules).is_none()
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
            first_line_reserve: 0,
        }
    }

    /// Words of the given lengths, 0.2 s apart, with no punctuation.
    fn words_of(lens: &[usize]) -> Vec<Word> {
        lens.iter()
            .enumerate()
            .map(|(i, &n)| {
                let letter = (b'a' + i as u8) as char;
                w(
                    &letter.to_string().repeat(n),
                    i as f64 * 0.2,
                    i as f64 * 0.2 + 0.15,
                )
            })
            .collect()
    }

    fn widths(c: &Cue) -> Vec<usize> {
        c.lines.iter().map(|l| l.chars().count()).collect()
    }

    #[test]
    fn closes_cue_before_a_word_that_cannot_be_wrapped_within_limits() {
        let c = build_cues(&words_of(&[30, 30, 22]), &rules());
        assert!(c.iter().all(|c| c.lines.len() <= 2), "{c:?}");
        assert!(
            c.iter().all(|c| widths(c).iter().all(|&n| n <= 42)),
            "{c:?}"
        );
        assert_eq!(c.iter().map(|c| c.lines.len()).sum::<usize>(), 3);
    }

    #[test]
    fn wraps_into_up_to_max_lines_balanced_lines() {
        let c = build_cues(
            &words_of(&[4, 4, 4, 4, 4, 4]),
            &CueRules {
                max_chars_per_line: 10,
                max_lines: 3,
                ..rules()
            },
        );
        assert_eq!(c.len(), 1, "{c:?}");
        assert_eq!(c[0].lines, ["aaaa bbbb", "cccc dddd", "eeee ffff"]);
    }

    #[test]
    fn first_line_reserve_shortens_the_first_line() {
        let r = CueRules {
            first_line_reserve: 12,
            ..rules()
        };
        let c = build_cues(&words_of(&[5; 12]), &r);
        for cue in &c {
            let n = widths(cue);
            assert!(n[0] <= 30, "{cue:?}");
            assert!(n.iter().all(|&n| n <= 42) && n.len() <= 2, "{cue:?}");
        }
        assert_eq!(c[0].lines.len(), 2);
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
