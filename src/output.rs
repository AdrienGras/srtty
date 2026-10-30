use crate::assemble::cues::Cue;
use clap::ValueEnum;
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Srt,
    Vtt,
    Txt,
}

impl Format {
    pub fn extension(&self) -> &'static str {
        match self {
            Format::Srt => "srt",
            Format::Vtt => "vtt",
            Format::Txt => "txt",
        }
    }
}

/// Seconds to whole milliseconds, rounded to nearest, negatives clamped to 0.
fn total_millis(t: f64) -> u64 {
    (t * 1000.0).round().max(0.0) as u64
}

fn timestamp(t: f64, decimal_sep: char) -> String {
    let ms = total_millis(t);
    format!(
        "{:02}:{:02}:{:02}{}{:03}",
        ms / 3_600_000,
        ms / 60_000 % 60,
        ms / 1000 % 60,
        decimal_sep,
        ms % 1000
    )
}

fn paragraph_clock(t: f64) -> String {
    let secs = t.max(0.0) as u64;
    format!(
        "[{:02}:{:02}:{:02}]",
        secs / 3600,
        secs / 60 % 60,
        secs % 60
    )
}

/// Gap between two cues that starts a new TXT paragraph.
const PARAGRAPH_GAP_S: f64 = 2.0;

pub fn render(format: Format, cues: &[Cue], speaker_label: &str) -> String {
    match format {
        Format::Srt => render_srt(cues, speaker_label),
        Format::Vtt => render_vtt(cues, speaker_label),
        Format::Txt => render_txt(cues, speaker_label),
    }
}

/// Characters the SRT speaker prefix `[{label} {n}] ` takes, counting two
/// digits for `n`. Cue building reserves them on the first line.
pub fn speaker_prefix_reserve(speaker_label: &str) -> usize {
    speaker_label.chars().count() + 4 + 2
}

fn render_srt(cues: &[Cue], speaker_label: &str) -> String {
    let mut out = String::new();
    let mut previous: Option<u32> = None;
    for (i, cue) in cues.iter().enumerate() {
        out.push_str(&format!(
            "{}\n{} --> {}\n",
            i + 1,
            timestamp(cue.start, ','),
            timestamp(cue.end, ',')
        ));
        for (j, line) in cue.lines.iter().enumerate() {
            if j == 0
                && let Some(n) = cue.speaker.filter(|n| Some(*n) != previous)
            {
                out.push_str(&format!("[{speaker_label} {n}] "));
            }
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        previous = cue.speaker;
    }
    out
}

/// Escapes the characters WebVTT treats as markup in cue text.
fn escape_vtt(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn render_vtt(cues: &[Cue], speaker_label: &str) -> String {
    if cues.is_empty() {
        return "WEBVTT\n".to_string();
    }
    let speaker_label = escape_vtt(speaker_label);
    let mut out = String::from("WEBVTT\n\n");
    for cue in cues {
        out.push_str(&format!(
            "{} --> {}\n",
            timestamp(cue.start, '.'),
            timestamp(cue.end, '.')
        ));
        for (j, line) in cue.lines.iter().enumerate() {
            if j == 0
                && let Some(n) = cue.speaker
            {
                out.push_str(&format!("<v {speaker_label} {n}>"));
            }
            out.push_str(&escape_vtt(line));
            out.push('\n');
        }
        out.push('\n');
    }
    out
}

fn render_txt(cues: &[Cue], speaker_label: &str) -> String {
    let mut paragraphs: Vec<String> = Vec::new();
    let mut previous: Option<&Cue> = None;
    for cue in cues {
        let text = cue.lines.join(" ");
        let continues = previous
            .is_some_and(|p| p.speaker == cue.speaker && cue.start - p.end < PARAGRAPH_GAP_S);
        match paragraphs.last_mut() {
            Some(paragraph) if continues => {
                paragraph.push(' ');
                paragraph.push_str(&text);
            }
            _ => {
                let clock = paragraph_clock(cue.start);
                paragraphs.push(match cue.speaker {
                    Some(n) => format!("{clock} {speaker_label} {n}: {text}"),
                    None => format!("{clock} {text}"),
                });
            }
        }
        previous = Some(cue);
    }
    paragraphs
        .iter()
        .map(|p| format!("{p}\n"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Output file for each requested format.
///
/// Without `out`, the input's extension is replaced (or added) and its
/// directory kept. With `out` and a single format, `out` is used verbatim.
/// With `out` and several formats, `out` is a base and each extension is
/// appended to it.
pub fn output_paths(
    input: &Path,
    out: Option<&Path>,
    formats: &[Format],
) -> Vec<(Format, PathBuf)> {
    formats
        .iter()
        .map(|&format| {
            let path = match out {
                None => input.with_extension(format.extension()),
                Some(out) if formats.len() == 1 => out.to_path_buf(),
                Some(out) => {
                    let mut name = out.as_os_str().to_owned();
                    name.push(".");
                    name.push(format.extension());
                    PathBuf::from(name)
                }
            };
            (format, path)
        })
        .collect()
}

/// Writes `content` to `<path>.srtty-tmp`, then renames it over `path`, so a
/// failure never leaves a partial output file.
pub fn write_atomic(path: &Path, content: &str) -> std::io::Result<()> {
    write_atomic_with(path, |file| file.write_all(content.as_bytes()))
}

/// Creates `<path>.srtty-tmp`, fills it with `write`, then renames it over
/// `path`. Once created, the temporary file is removed on any failure.
fn write_atomic_with(
    path: &Path,
    write: impl FnOnce(&mut std::fs::File) -> std::io::Result<()>,
) -> std::io::Result<()> {
    let mut tmp_name = path.file_name().unwrap_or_default().to_owned();
    tmp_name.push(".srtty-tmp");
    let tmp = path.with_file_name(tmp_name);
    let mut file = std::fs::File::create(&tmp)?;
    let written = write(&mut file);
    drop(file);
    written
        .and_then(|()| std::fs::rename(&tmp, path))
        .inspect_err(|_| {
            let _ = std::fs::remove_file(&tmp);
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(a: f64, b: f64, l: &[&str], s: Option<u32>) -> Cue {
        Cue {
            start: a,
            end: b,
            lines: l.iter().map(|x| x.to_string()).collect(),
            speaker: s,
        }
    }

    #[test]
    fn srt_format() {
        let s = render(
            Format::Srt,
            &[
                cue(0.0, 1.5, &["Bonjour."], None),
                cue(3661.25, 3662.0, &["a", "b"], None),
            ],
            "Speaker",
        );
        assert_eq!(
            s,
            "1\n00:00:00,000 --> 00:00:01,500\nBonjour.\n\n2\n01:01:01,250 --> 01:01:02,000\na\nb\n\n"
        );
    }

    #[test]
    fn vtt_with_voices_on_every_cue() {
        let s = render(
            Format::Vtt,
            &[
                cue(0.0, 1.0, &["Hi"], Some(1)),
                cue(1.0, 2.0, &["Yo"], Some(1)),
            ],
            "Speaker",
        );
        assert_eq!(
            s,
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n<v Speaker 1>Hi\n\n00:00:01.000 --> 00:00:02.000\n<v Speaker 1>Yo\n\n"
        );
    }

    #[test]
    fn vtt_escapes_markup_in_text_and_voice_label() {
        let s = render(
            Format::Vtt,
            &[cue(0.0, 1.0, &["R&D <3 --> ok", "a>b"], Some(1))],
            "Q&A <x>",
        );
        assert_eq!(
            s,
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n<v Q&amp;A &lt;x&gt; 1>R&amp;D &lt;3 --&gt; ok\na&gt;b\n\n"
        );
    }

    #[test]
    fn speaker_prefix_reserve_covers_two_digit_speakers() {
        assert_eq!(speaker_prefix_reserve("Speaker"), 13);
        let s = render(
            Format::Srt,
            &[cue(0.0, 1.0, &["x"], Some(99))],
            "Intervenant",
        );
        let prefix = s.lines().nth(2).unwrap().strip_suffix('x').unwrap();
        assert_eq!(
            prefix.chars().count(),
            speaker_prefix_reserve("Intervenant")
        );
    }

    #[test]
    fn srt_prefixes_speaker_only_on_change() {
        let s = render(
            Format::Srt,
            &[
                cue(0.0, 1.0, &["A"], Some(1)),
                cue(1.0, 2.0, &["B"], Some(1)),
                cue(2.0, 3.0, &["C"], Some(2)),
            ],
            "Intervenant",
        );
        assert!(
            s.contains("[Intervenant 1] A")
                && s.contains("\nB\n")
                && s.contains("[Intervenant 2] C")
        );
    }

    #[test]
    fn txt_paragraphs() {
        let s = render(
            Format::Txt,
            &[
                cue(35.0, 36.0, &["Mesdames,"], Some(1)),
                cue(36.1, 37.0, &["bonjour."], Some(1)),
                cue(48.0, 49.0, &["Si"], Some(2)),
            ],
            "Speaker",
        );
        assert_eq!(
            s,
            "[00:00:35] Speaker 1: Mesdames, bonjour.\n\n[00:00:48] Speaker 2: Si\n"
        );
    }

    #[test]
    fn txt_splits_on_gap_without_speakers() {
        let s = render(
            Format::Txt,
            &[
                cue(0.0, 1.0, &["a"], None),
                cue(3.0, 4.0, &["b"], None),
                cue(4.5, 5.0, &["c"], None),
            ],
            "Speaker",
        );
        assert_eq!(s, "[00:00:00] a\n\n[00:00:03] b c\n");
    }

    #[test]
    fn timestamps_round_to_millisecond_and_clamp_negatives() {
        let s = render(Format::Srt, &[cue(-1.0, 0.0004, &["x"], None)], "Speaker");
        assert!(s.contains("00:00:00,000 --> 00:00:00,000"));
        let s = render(Format::Srt, &[cue(1.0006, 2.0, &["x"], None)], "Speaker");
        assert!(s.contains("00:00:01,001 --> 00:00:02,000"));
    }

    #[test]
    fn render_empty_cues() {
        assert_eq!(render(Format::Srt, &[], "Speaker"), "");
        assert_eq!(render(Format::Vtt, &[], "Speaker"), "WEBVTT\n");
    }

    #[test]
    fn output_paths_edge_cases() {
        use std::path::PathBuf;
        let p = |s: &str| PathBuf::from(s);
        assert_eq!(
            output_paths(&p("dir/mon talk é.mkv"), None, &[Format::Srt]),
            vec![(Format::Srt, p("dir/mon talk é.srt"))]
        );
        assert_eq!(
            output_paths(&p("noext"), None, &[Format::Vtt]),
            vec![(Format::Vtt, p("noext.vtt"))]
        );
        assert_eq!(
            output_paths(&p("a.mkv"), Some(&p("out/x.srt")), &[Format::Srt]),
            vec![(Format::Srt, p("out/x.srt"))]
        );
        assert_eq!(
            output_paths(&p("a.mkv"), Some(&p("out/x")), &[Format::Srt, Format::Txt]),
            vec![(Format::Srt, p("out/x.srt")), (Format::Txt, p("out/x.txt"))]
        );
    }

    #[test]
    fn atomic_write_leaves_no_temp_file() {
        let d = tempfile::tempdir().unwrap();
        let f = d.path().join("a.srt");
        write_atomic(&f, "x").unwrap();
        assert_eq!(std::fs::read_to_string(&f).unwrap(), "x");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 1);
    }

    #[test]
    fn atomic_write_cleans_temp_file_when_write_fails() {
        let d = tempfile::tempdir().unwrap();
        let err = write_atomic_with(&d.path().join("a.srt"), |f| {
            f.write_all(b"partial")?;
            Err(std::io::Error::other("disk full"))
        })
        .unwrap_err();
        assert_eq!(err.to_string(), "disk full");
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn atomic_write_keeps_a_foreign_directory_in_the_temp_name() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir(d.path().join("a.srt.srtty-tmp")).unwrap();
        assert!(write_atomic(&d.path().join("a.srt"), "x").is_err());
        assert!(d.path().join("a.srt.srtty-tmp").is_dir());
        assert!(!d.path().join("a.srt").exists());
    }

    #[test]
    fn atomic_write_cleans_temp_file_when_rename_fails() {
        let d = tempfile::tempdir().unwrap();
        let target = d.path().join("dir");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("keep"), "k").unwrap();
        assert!(write_atomic(&target, "x").is_err());
        let names: Vec<_> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("dir")]);
    }
}
