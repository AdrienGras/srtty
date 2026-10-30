//! Turning transcripts into subtitle cues.

pub mod cues;
pub mod words;

pub use cues::{Cue, CueRules, build_cues, filter_cues};

use crate::transcribe::{Transcript, Word};

/// Builds the final cues from transcripts of consecutive chunks.
/// Each chunk comes with its start offset in the full timeline.
pub fn assemble(chunks: &[(f64, Transcript)], rules: &CueRules) -> Vec<Cue> {
    let words: Vec<Word> = chunks
        .iter()
        .flat_map(|(offset, t)| {
            words::normalize_words(t).into_iter().map(move |w| Word {
                start: w.start + offset,
                end: w.end + offset,
                ..w
            })
        })
        .collect();
    let mut cues = filter_cues(build_cues(&words, rules));
    renumber_speakers(&mut cues);
    cues
}

/// Renumbers speakers from 1 in order of first appearance.
fn renumber_speakers(cues: &mut [Cue]) {
    let mut seen: Vec<u32> = Vec::new();
    for cue in cues {
        if let Some(id) = cue.speaker {
            let pos = seen.iter().position(|&s| s == id).unwrap_or_else(|| {
                seen.push(id);
                seen.len() - 1
            });
            cue.speaker = Some(pos as u32 + 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fx;

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

    #[test]
    fn assemble_offsets_chunks_and_renumbers_speakers() {
        let mut t1 = Transcript {
            text: "A.".into(),
            ..Default::default()
        };
        t1.words = vec![Word {
            start: 1.0,
            end: 1.5,
            text: "A.".into(),
            speaker: Some(7),
        }];
        let mut t2 = Transcript {
            text: "B.".into(),
            ..Default::default()
        };
        t2.words = vec![Word {
            start: 0.5,
            end: 0.9,
            text: "B.".into(),
            speaker: Some(3),
        }];
        let c = assemble(&[(0.0, t1), (300.0, t2)], &rules());
        assert_eq!((c[0].start, c[1].start), (1.0, 300.5));
        assert_eq!((c[0].speaker, c[1].speaker), (Some(1), Some(2)));
    }

    #[test]
    fn real_fixture_produces_valid_cues() {
        let c = assemble(
            &[(
                0.0,
                crate::transcribe::parse::parse_verbose_json(&fx("whisper1")).unwrap(),
            )],
            &rules(),
        );
        assert!(c[0].lines[0].starts_with("C'est qu'il avait déjà"));
        assert!(
            c.windows(2)
                .all(|p| p[0].start < p[1].start && p[0].end <= p[1].start + 1e-6)
        );
    }

    fn fixture_cues(name: &str, rules: &CueRules) -> Vec<Cue> {
        let t = crate::transcribe::parse::parse_verbose_json(&fx(name)).unwrap();
        assemble(&[(0.0, t)], rules)
    }

    #[test]
    fn real_fixtures_keep_every_line_within_limits() {
        for name in [
            "whisper1",
            "deepgram",
            "grok_diar",
            "whisper_turbo",
            "scribe",
            "fish",
        ] {
            for c in fixture_cues(name, &rules()) {
                assert!(c.lines.len() <= 2, "{name}: {c:?}");
                assert!(
                    c.lines.iter().all(|l| l.chars().count() <= 42),
                    "{name}: {c:?}"
                );
            }
        }
    }

    #[test]
    fn real_diarized_srt_lines_fit_with_speaker_prefix() {
        let rules = CueRules {
            first_line_reserve: crate::output::speaker_prefix_reserve("Speaker"),
            ..rules()
        };
        let cues = fixture_cues("grok_diar", &rules);
        assert!(cues.iter().any(|c| c.speaker.is_some()));
        let srt = crate::output::render(crate::output::Format::Srt, &cues, "Speaker");
        for line in srt.lines().filter(|l| !l.contains(" --> ")) {
            assert!(line.chars().count() <= 42, "{line}");
        }
    }
}
