//! End-to-end tests of the local whisper.cpp backend. Most of them stop at the
//! checks that run before the model is loaded, so they need no real model.
#![cfg(feature = "local")]

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::cargo::cargo_bin_cmd;

/// Environment variable naming a real ggml model for the tests that load one.
const MODEL_ENV: &str = "SRTTY_TEST_WHISPER_MODEL";

/// A temporary config and cache for one test, with a local provider.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A config whose only provider is local, with `model_path`.
    fn new(model_path: &Path) -> Self {
        Self::in_dir(tempfile::tempdir().unwrap(), model_path)
    }

    /// A sandbox whose model is a 4-byte file with the ggml magic bytes: it
    /// passes the checks made before loading, and whisper.cpp would refuse to
    /// load it.
    fn with_fake_model() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let model = dir.path().join("ggml-fake.bin");
        std::fs::write(&model, b"lmgg").unwrap();
        Self::in_dir(dir, &model)
    }

    fn in_dir(dir: tempfile::TempDir, model_path: &Path) -> Self {
        let config = format!(
            "default_provider = \"local\"\n\
             [providers.local]\n\
             kind = \"local\"\n\
             model_path = \"{}\"\n",
            model_path.display()
        );
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        Self { dir }
    }

    fn cmd(&self) -> assert_cmd::Command {
        let mut cmd = cargo_bin_cmd!("srtty");
        cmd.current_dir(self.dir.path())
            .env("SRTTY_CONFIG", self.dir.path().join("config.toml"))
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env_remove("SRTTY_PROVIDER")
            .env_remove("SRTTY_MODEL")
            .env_remove("SRTTY_LANGUAGE");
        cmd
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }
}

/// 27 s of tone and silence, as an input that `check_input` accepts.
fn tone_mkv(dir: &Path) -> PathBuf {
    let out = dir.join("in.mkv");
    let expr = "aevalsrc='if(lt(mod(t,9),8),0.5*sin(440*2*PI*t),0)':s=16000:d=27";
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-v", "error", "-y", "-f", "lavfi", "-i", expr, "-c:a", "flac",
        ])
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "ffmpeg could not generate the test input");
    out
}

fn run(mut cmd: assert_cmd::Command) -> Output {
    cmd.output().unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// The error report wraps long messages across lines: put one back on a line.
fn message(bytes: &[u8]) -> String {
    text(bytes)
        .replace('│', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[test]
fn e2e_local_speakers_refused_before_anything() {
    let sb = Sandbox::new(Path::new("/nonexistent/ggml-base.bin"));
    let mut cmd = sb.cmd();
    cmd.arg("missing.mkv").arg("--speakers");
    let out = run(cmd);
    let stderr = message(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(
        stderr.contains("--speakers is not supported by the local backend"),
        "{stderr}"
    );
    assert!(!stderr.contains("model file not found"), "{stderr}");
}

#[test]
fn e2e_local_missing_input_comes_first() {
    let sb = Sandbox::new(Path::new("/nonexistent/ggml-base.bin"));
    let mut cmd = sb.cmd();
    cmd.arg("missing.mkv");
    let out = run(cmd);
    let stderr = message(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("input file not found"), "{stderr}");
    assert!(!stderr.contains("model file not found"), "{stderr}");
}

#[test]
fn e2e_local_missing_model_exit_1() {
    let sb = Sandbox::new(Path::new("/nonexistent/ggml-base.bin"));
    let input = tone_mkv(sb.dir.path());
    let mut cmd = sb.cmd();
    cmd.arg(&input);
    let out = run(cmd);
    let stderr = message(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("model file not found"), "{stderr}");
    assert!(stderr.contains("srtty setup"), "{stderr}");
}

#[test]
fn e2e_local_unknown_language_exit_1() {
    let sb = Sandbox::with_fake_model();
    let input = tone_mkv(sb.dir.path());
    let mut cmd = sb.cmd();
    cmd.arg(&input).args(["--language", "french"]);
    let out = run(cmd);
    let stderr = message(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("ISO 639-1"), "{stderr}");
}

#[test]
fn e2e_local_dry_run_has_no_cost() {
    let sb = Sandbox::with_fake_model();
    let input = tone_mkv(sb.dir.path());

    let mut cmd = sb.cmd();
    cmd.arg(&input).arg("--dry-run");
    let out = run(cmd);
    let stderr = message(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.contains("no API cost"), "{stderr}");

    let mut cmd = sb.cmd();
    cmd.arg(&input).args(["--dry-run", "--json"]);
    let out = run(cmd);
    let (stdout, stderr) = (text(&out.stdout), message(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.contains("\"estimated_cost_usd\":null"), "{stdout}");
}

#[test]
fn e2e_local_json_run_is_offline_and_quiet() {
    let Ok(model) = std::env::var(MODEL_ENV) else {
        eprintln!("skipped: {MODEL_ENV} is not set");
        return;
    };
    let sb = Sandbox::new(Path::new(&model));
    let input = sb.path("sample.ogg");
    std::fs::copy(
        concat!(env!("CARGO_MANIFEST_DIR"), "/assets/sample-30s.ogg"),
        &input,
    )
    .unwrap();

    let mut cmd = sb.cmd();
    cmd.arg(&input)
        .args(["--json", "--format", "srt,txt"])
        // A network call would fail instead of reaching a server.
        .env("HTTPS_PROXY", "http://127.0.0.1:9")
        .env("HTTP_PROXY", "http://127.0.0.1:9")
        .env("ALL_PROXY", "http://127.0.0.1:9");
    let out = run(cmd);
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");

    let summary: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout is one JSON object");
    assert!(summary["cost_usd"].is_null(), "{stdout}");
    assert_eq!(summary["provider"], "local", "{stdout}");

    let srt = std::fs::read_to_string(input.with_extension("srt")).unwrap();
    let cues = srt.split("\n\n").filter(|b| !b.trim().is_empty()).count();
    assert!(cues >= 5, "{cues} cues in:\n{srt}");
    assert!(input.with_extension("txt").exists());
    assert!(
        !stderr.contains("whisper_") && !stderr.contains("ggml_"),
        "{stderr}"
    );
}
