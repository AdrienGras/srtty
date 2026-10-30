//! End-to-end tests of the `srtty` binary against a mock transcription server.

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::cargo::cargo_bin_cmd;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY_ENV: &str = "SRTTY_E2E_API_KEY";
const KEY: &str = "sk-e2e-secret-4242";
const ENDPOINT: &str = "/v1/audio/transcriptions";

/// A temporary config, cache and input file for one test.
struct Sandbox {
    dir: tempfile::TempDir,
    input: PathBuf,
}

impl Sandbox {
    /// Writes a config whose only provider points at `base_url` with `model`,
    /// and generates a 27 s input: 8 s of tone, 1 s of silence, repeated.
    fn new(base_url: &str, model: &str, target_s: f64) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let config = format!(
            "default_provider = \"mock\"\n\
             [providers.mock]\n\
             base_url = \"{base_url}\"\n\
             api_key_env = \"{KEY_ENV}\"\n\
             model = \"{model}\"\n\
             [chunking]\n\
             target_s = {target_s:.1}\n"
        );
        std::fs::write(dir.path().join("config.toml"), config).unwrap();
        let input = tone_mkv(dir.path());
        Self { dir, input }
    }

    /// The binary with the sandbox config and cache, and no API key.
    fn cmd(&self) -> assert_cmd::Command {
        let mut cmd = cargo_bin_cmd!("srtty");
        cmd.current_dir(self.dir.path())
            .env("SRTTY_CONFIG", self.dir.path().join("config.toml"))
            .env("XDG_CACHE_HOME", self.dir.path().join("cache"))
            .env_remove("SRTTY_PROVIDER")
            .env_remove("SRTTY_MODEL")
            .env_remove("SRTTY_LANGUAGE")
            .env_remove(KEY_ENV);
        cmd
    }

    fn cmd_with_key(&self) -> assert_cmd::Command {
        let mut cmd = self.cmd();
        cmd.env(KEY_ENV, KEY);
        cmd
    }

    fn out(&self, ext: &str) -> PathBuf {
        self.input.with_extension(ext)
    }
}

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

fn fixture(name: &str) -> String {
    std::fs::read_to_string(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Runs the command off the async runtime, so the mock server keeps serving.
async fn run(mut cmd: assert_cmd::Command) -> Output {
    tokio::task::spawn_blocking(move || cmd.output().unwrap())
        .await
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_writes_srt_and_json_summary() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(fixture("deepgram.json"), "application/json"),
        )
        .expect(1)
        .mount(&server)
        .await;
    let sb = Sandbox::new(&format!("{}/v1", server.uri()), "deepgram/nova-3", 300.0);

    let mut cmd = sb.cmd_with_key();
    cmd.arg(&sb.input).args(["--format", "srt,txt", "--json"]);
    let out = run(cmd).await;

    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    let summary: serde_json::Value =
        serde_json::from_str(&stdout).expect("stdout is one JSON object");
    assert_eq!(summary["files"].as_array().unwrap().len(), 2);
    assert!(summary["cues"].as_u64().unwrap() > 0);
    assert!(summary["elapsed_s"].as_f64().unwrap() > 0.0);
    assert!(sb.out("srt").exists() && sb.out("txt").exists());
    assert!(!stdout.contains("\x1b["));
    assert!(!stderr.contains(KEY) && !stdout.contains(KEY));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_dry_run_makes_no_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let sb = Sandbox::new(&format!("{}/v1", server.uri()), "deepgram/nova-3", 9.0);

    let mut cmd = sb.cmd_with_key();
    cmd.arg(&sb.input).arg("--dry-run");
    let out = run(cmd).await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.contains("3 chunks"), "stderr: {stderr}");
    assert!(stderr.contains("cost unknown"), "stderr: {stderr}");
    assert!(out.stdout.is_empty());
    assert!(!sb.out("srt").exists());

    let mut cmd = sb.cmd_with_key();
    cmd.arg(&sb.input).args(["--dry-run", "--json"]);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(0));
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan["dry_run"], true);
    assert_eq!(plan["chunks"].as_array().unwrap().len(), 3);
    assert!(plan["estimated_cost_usd"].is_null());
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_unsupported_model_exit_2_no_file() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(ENDPOINT))
        .respond_with(
            ResponseTemplate::new(400)
                .set_body_raw(fixture("gpt4o_error.json"), "application/json"),
        )
        .mount(&server)
        .await;
    let sb = Sandbox::new(
        &format!("{}/v1", server.uri()),
        "openai/gpt-4o-transcribe",
        300.0,
    );

    let mut cmd = sb.cmd_with_key();
    cmd.arg(&sb.input);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", text(&out.stderr));
    assert!(!sb.out("srt").exists());

    let mut cmd = sb.cmd_with_key();
    cmd.arg(&sb.input).arg("--json");
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    assert_eq!(err["exit_code"], 2);
    assert!(
        err["error"].as_str().unwrap().contains("verbose_json"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_missing_key_exit_1() {
    let sb = Sandbox::new("http://127.0.0.1:9/v1", "m", 300.0);
    let mut cmd = sb.cmd();
    cmd.arg(&sb.input);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains(KEY_ENV));
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_missing_input_exit_1() {
    let sb = Sandbox::new("http://127.0.0.1:9/v1", "m", 300.0);
    let mut cmd = sb.cmd_with_key();
    cmd.arg("does-not-exist.mkv");
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains("does-not-exist.mkv"));

    let out = run(sb.cmd_with_key()).await;
    assert_eq!(out.status.code(), Some(1), "no input argument at all");
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_speakers_refused_for_non_diarizing_model() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .expect(0)
        .mount(&server)
        .await;
    let sb = Sandbox::new(
        &format!("{}/v1", server.uri()),
        "openai/whisper-large-v3-turbo",
        300.0,
    );
    let mut cmd = sb.cmd_with_key();
    cmd.arg(&sb.input).arg("--speakers");
    let out = run(cmd).await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("--speakers"), "stderr: {stderr}");
    assert!(!sb.out("srt").exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn e2e_config_show_masks_the_key() {
    let sb = Sandbox::new("http://127.0.0.1:9/v1", "some/model", 300.0);
    let mut cmd = sb.cmd_with_key();
    cmd.args(["config", "show"]);
    let out = run(cmd).await;
    let stdout = text(&out.stdout);
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    assert!(stdout.contains("some/model") && stdout.contains(KEY_ENV));
    assert!(stdout.contains("***"));
    assert!(!stdout.contains(KEY) && !text(&out.stderr).contains(KEY));
}
