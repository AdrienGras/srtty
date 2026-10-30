//! ffmpeg integration: checks, probing, extraction, silence detection, chunk planning and cutting.
//!
//! Commands are built from argument lists and never go through a shell.

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

use crate::error::SrttyError;

/// Minimum silence length reported by `silencedetect`, in seconds.
const SILENCE_MIN_S: f64 = 0.4;

/// A silent interval, in seconds from the start of the audio.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Silence {
    pub start: f64,
    pub end: f64,
}

fn not_found() -> SrttyError {
    SrttyError::Ffmpeg(
        "ffmpeg not found; install it (apt install ffmpeg / brew install ffmpeg)".into(),
    )
}

/// Check that `ffmpeg` and `ffprobe` can be run.
pub fn ensure_ffmpeg() -> Result<(), SrttyError> {
    for bin in ["ffmpeg", "ffprobe"] {
        let ok = std::process::Command::new(bin)
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            return Err(not_found());
        }
    }
    Ok(())
}

fn stderr_tail(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim_end();
    let skip = text.chars().count().saturating_sub(500);
    text.chars().skip(skip).collect()
}

/// Run `bin` with `args`; return (stdout, stderr) on success, an `Ffmpeg` error otherwise.
///
/// On Unix the child leads its own process group, so a terminal Ctrl-C reaches
/// only srtty, which cancels the run and kills the child through `kill_on_drop`.
async fn run(bin: &str, args: &[&std::ffi::OsStr]) -> Result<(String, String), SrttyError> {
    let mut cmd = Command::new(bin);
    cmd.args(args).stdin(Stdio::null()).kill_on_drop(true);
    #[cfg(unix)]
    cmd.process_group(0);
    let out = cmd.output().await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            not_found()
        } else {
            SrttyError::Ffmpeg(format!("failed to run {bin}: {e}"))
        }
    })?;
    if !out.status.success() {
        return Err(SrttyError::Ffmpeg(format!(
            "{bin} failed ({}): {}",
            out.status,
            stderr_tail(&out.stderr)
        )));
    }
    Ok((
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

async fn ffmpeg(args: &[&std::ffi::OsStr]) -> Result<(String, String), SrttyError> {
    let mut full: Vec<&std::ffi::OsStr> = ["-nostdin", "-hide_banner"]
        .iter()
        .map(std::ffi::OsStr::new)
        .collect();
    full.extend_from_slice(args);
    run("ffmpeg", &full).await
}

/// Duration of a media file in seconds, via `ffprobe`.
pub async fn probe_duration(path: &Path) -> Result<f64, SrttyError> {
    let args: [&std::ffi::OsStr; 8] = [
        "-v".as_ref(),
        "error".as_ref(),
        "-show_entries".as_ref(),
        "format=duration".as_ref(),
        "-of".as_ref(),
        "default=noprint_wrappers=1:nokey=1".as_ref(),
        "-i".as_ref(),
        path.as_os_str(),
    ];
    let (stdout, _) = run("ffprobe", &args).await?;
    stdout
        .trim()
        .parse::<f64>()
        .map_err(|_| SrttyError::Ffmpeg(format!("could not read duration of {}", path.display())))
}

/// Mean volume in dB, via the `volumedetect` filter.
pub async fn mean_volume_db(path: &Path) -> Result<f64, SrttyError> {
    let args: [&std::ffi::OsStr; 8] = [
        "-i".as_ref(),
        path.as_os_str(),
        "-vn".as_ref(),
        "-af".as_ref(),
        "volumedetect".as_ref(),
        "-f".as_ref(),
        "null".as_ref(),
        "-".as_ref(),
    ];
    let (_, stderr) = ffmpeg(&args).await?;
    stderr
        .lines()
        .find_map(|l| {
            let rest = l.split("mean_volume:").nth(1)?;
            rest.trim().strip_suffix("dB")?.trim().parse::<f64>().ok()
        })
        .ok_or_else(|| {
            SrttyError::Ffmpeg(format!("could not read mean volume of {}", path.display()))
        })
}

const OPUS_ARGS: [&str; 11] = [
    "-vn",
    "-ac",
    "1",
    "-ar",
    "16000",
    "-c:a",
    "libopus",
    "-b:a",
    "32k",
    "-application",
    "voip",
];

/// Re-encode `input` (video or audio) to mono 16 kHz Opus at 32 kbit/s.
pub async fn extract_audio(input: &Path, out: &Path) -> Result<(), SrttyError> {
    let mut args: Vec<&std::ffi::OsStr> = vec!["-y".as_ref(), "-i".as_ref(), input.as_os_str()];
    args.extend(OPUS_ARGS.iter().map(std::ffi::OsStr::new));
    args.extend(["-f".as_ref(), "ogg".as_ref(), out.as_os_str()]);
    ffmpeg(&args).await.map(|_| ())
}

/// Detect silences of at least 0.4 s below `threshold_db`.
pub async fn detect_silences(audio: &Path, threshold_db: f64) -> Result<Vec<Silence>, SrttyError> {
    let filter = format!("silencedetect=noise={threshold_db}dB:d={SILENCE_MIN_S}");
    let args: [&std::ffi::OsStr; 8] = [
        "-i".as_ref(),
        audio.as_os_str(),
        "-af".as_ref(),
        filter.as_ref(),
        "-vn".as_ref(),
        "-f".as_ref(),
        "null".as_ref(),
        "-".as_ref(),
    ];
    let (_, stderr) = ffmpeg(&args).await?;
    Ok(parse_silencedetect(&stderr))
}

/// Pair each `silence_start` (possibly negative) with the next `silence_end`.
/// A trailing start without an end is ignored.
pub fn parse_silencedetect(stderr: &str) -> Vec<Silence> {
    fn value_after(line: &str, key: &str) -> Option<f64> {
        let rest = line.split(key).nth(1)?;
        rest.split_whitespace().next()?.parse().ok()
    }
    let mut out = Vec::new();
    let mut start = None;
    for line in stderr.lines() {
        if let Some(s) = value_after(line, "silence_start:") {
            start = Some(s);
        } else if let Some(end) = value_after(line, "silence_end:")
            && let Some(start) = start.take()
        {
            out.push(Silence { start, end });
        }
    }
    out
}

/// Silence threshold relative to the mean volume, clamped to [-50, -20] dB.
pub fn silence_threshold_db(mean_db: f64) -> f64 {
    (mean_db + 4.0).clamp(-50.0, -20.0)
}

/// Split `total_s` into chunks of about `target_s`, cutting in the middle of the silence
/// closest to each target (within `window_s`), or exactly on the target if there is none.
/// A chunk is only split off while more than `target_s * 1.25` remains.
pub fn plan_chunks(
    total_s: f64,
    target_s: f64,
    silences: &[Silence],
    window_s: f64,
) -> Vec<(f64, f64)> {
    let mut chunks = Vec::new();
    let mut pos = 0.0;
    while total_s - pos > target_s * 1.25 {
        let goal = pos + target_s;
        let cut = silences
            .iter()
            .map(|s| (s.start + s.end) / 2.0)
            .filter(|mid| (mid - goal).abs() <= window_s && *mid > pos)
            .min_by(|a, b| (a - goal).abs().total_cmp(&(b - goal).abs()))
            .unwrap_or(goal);
        chunks.push((pos, cut));
        pos = cut;
    }
    chunks.push((pos, total_s));
    chunks
}

/// Cut `[start, end]` out of `audio`, re-encoding so the bounds are exact.
/// The seek is on the input side, so ffmpeg does not decode everything before `start`.
pub async fn cut_chunk(audio: &Path, start: f64, end: f64, out: &Path) -> Result<(), SrttyError> {
    let (start, len) = (start.to_string(), (end - start).to_string());
    let mut args: Vec<&std::ffi::OsStr> = vec![
        "-y".as_ref(),
        "-ss".as_ref(),
        start.as_ref(),
        "-t".as_ref(),
        len.as_ref(),
        "-i".as_ref(),
        audio.as_os_str(),
    ];
    args.extend(OPUS_ARGS.iter().map(std::ffi::OsStr::new));
    args.extend(["-f".as_ref(), "ogg".as_ref(), out.as_os_str()]);
    ffmpeg(&args).await.map(|_| ())
}

#[cfg(test)]
pub(crate) fn tone_with_silences(dir: &std::path::Path, secs: u32) -> std::path::PathBuf {
    let src = dir.join("in.wav");
    let expr = format!("aevalsrc='if(lt(mod(t,9),8),0.5*sin(440*2*PI*t),0)':s=16000:d={secs}");
    let ok = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i"])
        .arg(expr)
        .arg(&src)
        .status()
        .unwrap();
    assert!(ok.success());
    src
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_pairs_starts_and_ends_including_negative_start() {
        let err = "[silencedetect] silence_start: -0.0123\n[silencedetect] silence_end: 0.51 | silence_duration: 0.52\n\
                   [silencedetect] silence_start: 10.2\n[silencedetect] silence_end: 11.0 | silence_duration: 0.8\n\
                   [silencedetect] silence_start: 20.0\n";
        let s = parse_silencedetect(err);
        assert_eq!(
            s,
            vec![
                Silence {
                    start: -0.0123,
                    end: 0.51
                },
                Silence {
                    start: 10.2,
                    end: 11.0
                }
            ]
        );
    }

    #[test]
    fn plan_chunks_cuts_in_middle_of_nearest_silence() {
        // pos 0 -> target 300: 301.5 (gap 1.5) beats 290.5; pos 301.5 -> target 601.5: 601.0; remaining 299 <= 375 -> end
        let sil = [
            Silence {
                start: 290.0,
                end: 291.0,
            },
            Silence {
                start: 301.0,
                end: 302.0,
            },
            Silence {
                start: 600.0,
                end: 602.0,
            },
        ];
        assert_eq!(
            plan_chunks(900.0, 300.0, &sil, 30.0),
            vec![(0.0, 301.5), (301.5, 601.0), (601.0, 900.0)]
        );
    }

    #[test]
    fn plan_chunks_without_silences_cuts_at_target() {
        let c = plan_chunks(950.0, 300.0, &[], 30.0); // 350 left <= 375 after 600 -> longer last chunk
        assert_eq!(c, vec![(0.0, 300.0), (300.0, 600.0), (600.0, 950.0)]);
        assert!(c.iter().all(|(a, b)| b > a));
    }

    #[test]
    fn plan_chunks_ignores_silence_just_outside_window() {
        // midpoint 331 is 31 s from the goal (300), window is 30 -> cut at the goal
        let sil = [Silence {
            start: 330.0,
            end: 332.0,
        }];
        assert_eq!(
            plan_chunks(500.0 + 300.0, 300.0, &sil, 30.0)[0],
            (0.0, 300.0)
        );
    }

    #[test]
    fn plan_chunks_never_stalls_or_goes_backwards() {
        // midpoints at or before the current position (0.0 and 300.0) must be ignored
        let sil = [
            Silence {
                start: -0.5,
                end: 0.5,
            },
            Silence {
                start: 299.0,
                end: 301.0,
            },
            Silence {
                start: 280.0,
                end: 290.0,
            },
        ];
        let c = plan_chunks(1000.0, 300.0, &sil, 400.0);
        assert!(c.iter().all(|(a, b)| b > a), "{c:?}");
        assert_eq!(c[0].0, 0.0);
        assert!(c.windows(2).all(|w| w[0].1 == w[1].0), "{c:?}");
        assert_eq!(c.last().unwrap().1, 1000.0);
    }

    #[test]
    fn short_input_is_one_chunk() {
        assert_eq!(plan_chunks(0.8, 300.0, &[], 30.0), vec![(0.0, 0.8)]);
    }

    #[test]
    fn threshold_is_relative_and_clamped() {
        assert_eq!(silence_threshold_db(-34.0), -30.0);
        assert_eq!(silence_threshold_db(-70.0), -50.0);
        assert_eq!(silence_threshold_db(-5.0), -20.0);
    }

    #[tokio::test]
    async fn real_ffmpeg_detects_generated_silences() {
        // 3 x (8 s of sine + 1 s of silence)
        let d = tempfile::tempdir().unwrap();
        let src = tone_with_silences(d.path(), 27);
        let ogg = d.path().join("a.ogg");
        extract_audio(&src, &ogg).await.unwrap();
        assert!((probe_duration(&ogg).await.unwrap() - 27.0).abs() < 0.2);
        let s = detect_silences(&ogg, -30.0).await.unwrap();
        assert!(
            s.iter()
                .any(|x| (x.start - 8.0).abs() < 0.2 && (x.end - 9.0).abs() < 0.2)
        );
    }

    #[tokio::test]
    async fn volume_and_cut_work_on_real_audio() {
        let d = tempfile::tempdir().unwrap();
        let src = tone_with_silences(d.path(), 27);
        let ogg = d.path().join("a.ogg");
        extract_audio(&src, &ogg).await.unwrap();
        let mean = mean_volume_db(&ogg).await.unwrap();
        assert!(mean < 0.0 && mean > -40.0, "mean {mean}");
        let part = d.path().join("p.ogg");
        cut_chunk(&ogg, 5.0, 15.0, &part).await.unwrap();
        assert!((probe_duration(&part).await.unwrap() - 10.0).abs() < 0.2);
    }

    #[tokio::test]
    async fn output_is_ogg_regardless_of_extension() {
        let d = tempfile::tempdir().unwrap();
        let src = tone_with_silences(d.path(), 12);
        let a = d.path().join("a.part");
        extract_audio(&src, &a).await.unwrap();
        let b = d.path().join("b.part");
        cut_chunk(&a, 2.0, 7.0, &b).await.unwrap();
        for f in [&a, &b] {
            let head = std::fs::read(f).unwrap();
            assert_eq!(&head[..4], b"OggS", "{}", f.display());
        }
    }

    #[tokio::test]
    async fn ffmpeg_failure_is_ffmpeg_error_with_stderr_tail() {
        let d = tempfile::tempdir().unwrap();
        let err = extract_audio(&d.path().join("missing.wav"), &d.path().join("o.ogg"))
            .await
            .unwrap_err();
        assert!(matches!(err, crate::error::SrttyError::Ffmpeg(_)));
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn ensure_ffmpeg_ok_on_this_machine() {
        ensure_ffmpeg().unwrap();
    }

    /// Children lead their own process group, so a terminal Ctrl-C reaches only srtty.
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn children_run_in_their_own_process_group() {
        let (stat, _) = run("cat", &["/proc/self/stat".as_ref()]).await.unwrap();
        // Fields after the `(comm)` field: state, ppid, pgrp, ...
        let pid: u32 = stat.split(' ').next().unwrap().parse().unwrap();
        let after_comm = &stat[stat.rfind(')').unwrap() + 2..];
        let pgrp: u32 = after_comm.split(' ').nth(2).unwrap().parse().unwrap();
        assert_eq!(pgrp, pid);
    }
}
