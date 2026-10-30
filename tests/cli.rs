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
        let mut sb = Self::config_only(base_url, model, target_s);
        sb.input = tone_mkv(sb.dir.path());
        sb
    }

    /// Same config as `new`, without generating a media file (`input` is empty).
    fn config_only(base_url: &str, model: &str, target_s: f64) -> Self {
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
        Self {
            dir,
            input: PathBuf::new(),
        }
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

#[tokio::test(flavor = "multi_thread")]
async fn config_show_with_unset_key_prints_placeholder() {
    let sb = Sandbox::new("http://127.0.0.1:9/v1", "some/model", 300.0);
    let mut cmd = sb.cmd();
    cmd.args(["config", "show"]);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let expected = format!("api_key = \"<unset: {KEY_ENV}>\"");
    assert!(
        text(&out.stdout).contains(&expected),
        "{}",
        text(&out.stdout)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn clap_error_with_json_prints_json() {
    let sb = Sandbox::new("http://127.0.0.1:9/v1", "m", 300.0);
    let mut cmd = sb.cmd_with_key();
    cmd.args(["--json", "a.mp4", "--format", "both"]);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(1));
    let err: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    assert_eq!(err["exit_code"], 1);
    let message = err["error"].as_str().unwrap();
    assert!(
        !message.is_empty() && !message.starts_with("error:"),
        "{err}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn help_with_json_is_not_an_error() {
    let sb = Sandbox::new("http://127.0.0.1:9/v1", "m", 300.0);
    let mut cmd = sb.cmd_with_key();
    cmd.args(["--json", "--help"]);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(0));
    let parsed: Result<serde_json::Value, _> = serde_json::from_slice(&out.stdout);
    assert!(
        parsed.map_or(true, |v| v.get("error").is_none()),
        "{}",
        text(&out.stdout)
    );
}

const COMPAT_BODY: &str = r#"{"schema":1,"generated_at":"2030-01-02","models":[]}"#;

/// Serves `/v1/models` (two Whisper models) and `/compat.json`.
async fn models_server(models_status: u16, compat_status: u16) -> MockServer {
    let server = MockServer::start().await;
    let listing = r#"{"data":[{"id":"whisper-1"},{"id":"whisper-large-v3"}]}"#;
    Mock::given(method("GET"))
        .and(path("/v1/models"))
        .respond_with(
            ResponseTemplate::new(models_status).set_body_raw(listing, "application/json"),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/compat.json"))
        .respond_with(
            ResponseTemplate::new(compat_status).set_body_raw(COMPAT_BODY, "application/json"),
        )
        .mount(&server)
        .await;
    server
}

/// A sandbox whose provider is `server`, and a `models` command that reads its compat list too.
fn models_cmd(server: &MockServer, with_key: bool) -> (Sandbox, assert_cmd::Command) {
    let sb = Sandbox::config_only(&format!("{}/v1", server.uri()), "m", 300.0);
    let mut cmd = if with_key {
        sb.cmd_with_key()
    } else {
        sb.cmd()
    };
    cmd.env("SRTTY_COMPAT_URL", format!("{}/compat.json", server.uri()));
    (sb, cmd)
}

#[tokio::test(flavor = "multi_thread")]
async fn models_lists_and_refreshes_cache() {
    let server = models_server(200, 200).await;
    let (sb, mut cmd) = models_cmd(&server, true);
    cmd.arg("models");
    let out = run(cmd).await;
    let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stdout.starts_with("MODEL"), "{stdout}");
    assert!(stdout.contains("whisper-1"), "{stdout}");
    assert!(
        stderr.contains("compatibility list: 2030-01-02 (remote)"),
        "{stderr}"
    );
    assert!(sb.dir.path().join("cache/srtty/compat.json").exists());
    assert!(!stdout.contains(KEY) && !stderr.contains(KEY));
}

#[tokio::test(flavor = "multi_thread")]
async fn models_json_is_a_single_object() {
    let server = models_server(200, 200).await;
    let (_sb, mut cmd) = models_cmd(&server, true);
    cmd.args(["models", "--json"]);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(0), "stderr: {}", text(&out.stderr));
    let v: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    assert_eq!(v["provider"], "mock");
    assert_eq!(v["models"].as_array().unwrap().len(), 2);
    assert_eq!(v["compat"]["source"], "remote");
    assert_eq!(v["compat"]["generated_at"], "2030-01-02");
    assert!(v["warnings"].as_array().unwrap().is_empty());
    assert_eq!(v["models"][0]["status"], "unknown");
}

#[tokio::test(flavor = "multi_thread")]
async fn models_without_key_names_the_variable() {
    let server = models_server(200, 200).await;
    let (_sb, mut cmd) = models_cmd(&server, false);
    cmd.arg("models");
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(1));
    assert!(text(&out.stderr).contains(KEY_ENV), "{}", text(&out.stderr));
}

#[tokio::test(flavor = "multi_thread")]
async fn models_http_error_exits_2() {
    let server = models_server(500, 200).await;
    let (_sb, mut cmd) = models_cmd(&server, true);
    cmd.arg("models");
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(2), "stderr: {}", text(&out.stderr));

    let (_sb, mut cmd) = models_cmd(&server, true);
    cmd.args(["models", "--json"]);
    let out = run(cmd).await;
    assert_eq!(out.status.code(), Some(2));
    let err: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("stdout is one JSON object");
    assert_eq!(err["exit_code"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn models_works_offline_with_compat_fallback() {
    let server = models_server(200, 404).await;
    let (_sb, mut cmd) = models_cmd(&server, true);
    cmd.arg("models");
    let out = run(cmd).await;
    let stderr = text(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "stderr: {stderr}");
    assert!(stderr.contains("could not refresh"), "{stderr}");
    assert!(stderr.contains("(embedded)"), "{stderr}");
    assert!(text(&out.stdout).contains("whisper-1"));

    let (_sb, mut cmd) = models_cmd(&server, true);
    cmd.args(["models", "--json"]);
    let out = run(cmd).await;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["compat"]["source"], "embedded");
    assert_eq!(v["warnings"].as_array().unwrap().len(), 1);
}
