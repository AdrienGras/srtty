//! What the binary does once the command line is parsed: `config show`,
//! `setup`, or a transcription run with its checks, dry run and cost confirmation.

use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use ratatui::crossterm::style::Stylize;
use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::assemble::CueRules;
use crate::cache::{CacheParams, ChunkCache};
use crate::cli::{Cli, Command, CompatAction, ConfigAction};
use crate::compat::{CompatEntry, CompatList, compat_url, estimate_cost_usd, provider_key};
use crate::config::{
    self, Backend, Config, GPU_BUILD, KeyState, ProviderKind, RequestMode, Resolved,
    SubtitleConfig, available_cores, parallelism,
};
use crate::dashboard::recap::recap_lines;
use crate::dashboard::{Dashboard, DisplayMode, TICK, display_mode, stderr_screen};
use crate::error::SrttyError;
use crate::media::plan_chunks;
use crate::output::speaker_prefix_reserve;
use crate::paths;
use crate::report::{Event, PlainReporter, Reporter, Summary};
use crate::run::{RunRequest, SILENCE_WINDOW_S, check_input, prepare, run};
use crate::setup::{InquirePrompter, LiveTester, SetupContext, run_setup};
use crate::transcribe::openai::{OpenAiCompatible, is_openrouter};
use crate::transcribe::{TranscribeOptions, Transcriber};

/// Longest audio `--speakers` accepts, in seconds (75 minutes).
const SPEAKERS_MAX_DURATION_S: f64 = 4500.0;
/// Largest extracted audio `--speakers` sends in its single request.
const SPEAKERS_MAX_AUDIO_BYTES: u64 = 25 * 1024 * 1024;

pub async fn main(cli: Cli) -> Result<(), SrttyError> {
    // `--provider` is global so that it reaches `models`; these commands ignore it.
    let ignores_provider = match &cli.command {
        Some(Command::Setup) => Some("srtty setup"),
        Some(Command::Compat { .. }) => Some("srtty compat probe"),
        _ => None,
    };
    if let (Some(command), Some(_)) = (ignores_provider, &cli.provider) {
        return Err(SrttyError::Usage(format!(
            "--provider is not used by `{command}`"
        )));
    }
    match cli.command {
        Some(Command::Config {
            action: ConfigAction::Show,
        }) => show_config(&cli),
        Some(Command::Models) => crate::models::run(&cli).await,
        Some(Command::Setup) => setup(&cli).await,
        Some(Command::Compat {
            action: CompatAction::Probe(args),
        }) => crate::compat_probe::run(&args).await,
        None => transcribe(cli).await,
    }
}

/// `srtty setup`: only on a terminal, never with `--json`.
async fn setup(cli: &Cli) -> Result<(), SrttyError> {
    if cli.json {
        return Err(SrttyError::Usage(
            "`srtty setup` is interactive and does not support --json".into(),
        ));
    }
    if !(std::io::stdin().is_terminal() && std::io::stderr().is_terminal()) {
        return Err(SrttyError::Usage(
            "`srtty setup` is interactive; run it in a terminal".into(),
        ));
    }
    let env = |name: &str| std::env::var(name).ok();
    let ctx = SetupContext {
        config_path: paths::config_file(),
        cache_dir: paths::cache_dir(),
        compat_url: compat_url(),
        env: &env,
        tester: &LiveTester,
    };
    run_setup(&mut InquirePrompter, &ctx).await?;
    Ok(())
}

/// Checks that a `--speakers` run can be sent as one request and returns the
/// provider options that turn diarization on. Without `--speakers`, `Ok(None)`.
pub fn preflight(
    duration_s: f64,
    audio_bytes: u64,
    speakers: bool,
    entry: Option<&CompatEntry>,
    model: &str,
) -> Result<Option<serde_json::Value>, SrttyError> {
    if !speakers {
        return Ok(None);
    }
    if duration_s > SPEAKERS_MAX_DURATION_S {
        return Err(SrttyError::Usage(
            "--speakers supports at most 75 minutes of audio".into(),
        ));
    }
    if audio_bytes > SPEAKERS_MAX_AUDIO_BYTES {
        return Err(SrttyError::Usage(format!(
            "--speakers supports at most 25 MB of extracted audio; this input gives {:.1} MB",
            audio_bytes as f64 / (1024.0 * 1024.0)
        )));
    }
    match entry.and_then(|e| e.diarization.clone()) {
        Some(options) => Ok(Some(options)),
        None => Err(SrttyError::Usage(format!(
            "model {model} does not support --speakers; see `srtty models`"
        ))),
    }
}

/// Lets the run go ahead when its estimated cost is unknown, at most
/// `threshold`, or accepted with `--yes`. Otherwise asks on a terminal and
/// refuses elsewhere.
pub fn confirm_cost(
    estimate: Option<f64>,
    threshold: f64,
    yes: bool,
    is_tty: bool,
    ask: &mut dyn FnMut(f64) -> bool,
) -> Result<(), SrttyError> {
    let Some(estimate_usd) = estimate.filter(|&usd| usd > threshold) else {
        return Ok(());
    };
    if yes || (is_tty && ask(estimate_usd)) {
        return Ok(());
    }
    Err(SrttyError::CostNotConfirmed { estimate_usd })
}

/// Loads the config file, merges the environment and the flags, and rejects
/// values a run cannot work with.
fn load_settings(cli: &Cli) -> Result<Resolved, SrttyError> {
    let cfg = Config::load(&paths::config_file())?;
    let resolved = config::resolve(cfg, cli, &|name| std::env::var(name).ok())?;
    check_settings(&resolved)?;
    Ok(resolved)
}

/// Cue rules for a run. With `--speakers`, the first line of each cue keeps
/// room for the SRT speaker prefix.
fn cue_rules(subtitles: &SubtitleConfig, speakers: bool) -> CueRules {
    CueRules {
        first_line_reserve: if speakers {
            speaker_prefix_reserve(&subtitles.speaker_label)
        } else {
            0
        },
        ..CueRules::from(subtitles)
    }
}

fn check_settings(r: &Resolved) -> Result<(), SrttyError> {
    if r.jobs == Some(0) {
        return Err(SrttyError::Config(
            "jobs must be at least 1 (`chunking.jobs` or --jobs)".into(),
        ));
    }
    if !r.target_s.is_finite() || r.target_s <= 0.0 {
        return Err(SrttyError::Config(
            "chunk duration must be a positive number of seconds (`chunking.target_s` or --chunk)"
                .into(),
        ));
    }
    Ok(())
}

/// The effective configuration as `config show` prints it. The key is masked.
#[derive(Serialize)]
struct ConfigView<'a> {
    config_file: PathBuf,
    provider: &'a str,
    kind: ProviderKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    base_url: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key_env: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    model: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    model_path: Option<&'a Path>,
    request_mode: RequestMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    language: Option<&'a str>,
    confirm_above_usd: f64,
    subtitles: &'a SubtitleConfig,
    chunking: ChunkingView,
}

#[derive(Serialize)]
struct ChunkingView {
    target_s: f64,
    jobs: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    threads_per_job: Option<usize>,
}

fn show_config(cli: &Cli) -> Result<(), SrttyError> {
    let r = load_settings(cli)?;
    let par = parallelism(r.provider.kind, r.jobs, available_cores(), GPU_BUILD);
    let view = ConfigView {
        config_file: paths::config_file(),
        provider: &r.provider_name,
        kind: r.provider.kind,
        base_url: r.provider.base_url.as_deref(),
        api_key_env: r.provider.api_key_env.as_deref(),
        api_key: match &r.api_key {
            KeyState::Set(key) => Some(key.to_string()),
            KeyState::Unset { var } => Some(format!("<unset: {var}>")),
            KeyState::NotNeeded => None,
        },
        model: &r.model,
        model_path: r.model_path.as_deref(),
        request_mode: r.provider.request_mode,
        language: r.language.as_deref(),
        confirm_above_usd: r.confirm_above_usd,
        subtitles: &r.subtitles,
        chunking: ChunkingView {
            target_s: r.target_s,
            jobs: par.jobs,
            threads_per_job: par.threads_per_job,
        },
    };
    let text = if cli.json {
        serde_json::to_string_pretty(&view).map_err(std::io::Error::from)?
    } else {
        toml::to_string(&view).map_err(|e| SrttyError::Config(e.to_string()))?
    };
    print_stdout(text.trim_end())
}

/// Writes `text` and a newline to stdout; a closed stdout is an I/O error, not a panic.
pub(crate) fn print_stdout(text: &str) -> Result<(), SrttyError> {
    let mut out = std::io::stdout().lock();
    writeln!(out, "{text}")?;
    out.flush()?;
    Ok(())
}

pub(crate) const LOCAL_UNAVAILABLE: &str = "the local backend is not available in this build; reinstall with: cargo install srtty --features local";
pub(crate) const LOCAL_NO_SPEAKERS: &str = "--speakers is not supported by the local backend; use an API provider with a diarization model (see `srtty models`)";

#[cfg(not(feature = "local"))]
fn local_unavailable() -> SrttyError {
    SrttyError::Config(LOCAL_UNAVAILABLE.into())
}

/// What a run needs once its settings are checked.
struct Job {
    input: PathBuf,
    r: Resolved,
    backend: Backend,
}

/// How a run ended, before anything is printed.
enum Outcome {
    DryRun {
        plan: Vec<(f64, f64)>,
        estimate: Option<f64>,
    },
    Done(Summary),
}

/// A transcription run: checks the settings, shows progress in the chosen
/// display mode, and erases any live display before printing the outcome or
/// returning an error.
async fn transcribe(cli: Cli) -> Result<(), SrttyError> {
    let started = Instant::now();
    let input = cli.input.clone().ok_or_else(|| {
        SrttyError::Usage("no input file given; run `srtty --help` for usage".into())
    })?;
    let r = load_settings(&cli)?;
    let backend = r.backend()?;
    let local = matches!(backend, Backend::Local { .. });
    if local {
        if !cfg!(feature = "local") {
            return Err(SrttyError::Config(LOCAL_UNAVAILABLE.into()));
        }
        if cli.speakers {
            return Err(SrttyError::Usage(LOCAL_NO_SPEAKERS.into()));
        }
    }
    check_input(&input)?;
    #[cfg(feature = "local")]
    if let Backend::Local { model_path } = &backend {
        crate::local::check_ggml_magic(model_path)?;
        if let Some(language) = &r.language {
            crate::local::check_language(language)?;
        }
    }

    // Before the dashboard opens: an error here must not leave it on screen.
    let signals = stop_signals()?;
    let mut mode = display_mode(
        cli.quiet,
        cli.json,
        std::io::stdout().is_terminal(),
        std::io::stderr().is_terminal(),
        &|name| std::env::var(name).ok(),
    );
    let reporter: Arc<dyn Reporter> = match mode {
        DisplayMode::Quiet => Arc::new(PlainReporter::new(Box::new(std::io::sink()))),
        DisplayMode::Plain => Arc::new(PlainReporter::new(Box::new(std::io::stderr()))),
        DisplayMode::Dashboard => {
            match Dashboard::open(stderr_screen(), input_name(&input), r.model.clone()) {
                Ok(dashboard) => {
                    dashboard.start_ticker(TICK);
                    dashboard
                }
                // The terminal could not host the viewport.
                Err(_) => {
                    mode = DisplayMode::Plain;
                    Arc::new(PlainReporter::new(Box::new(std::io::stderr())))
                }
            }
        }
    };

    let cancel = CancellationToken::new();
    tokio::spawn(on_interrupts(
        signals,
        {
            let (cancel, reporter) = (cancel.clone(), Arc::clone(&reporter));
            move || {
                cancel.cancel();
                reporter.event(&Event::Cancelling);
            }
        },
        {
            let (reporter, json) = (Arc::clone(&reporter), cli.json);
            move || {
                reporter.finish();
                let code = SrttyError::Interrupted.exit_code();
                if json {
                    // Same shape as the error report of `main`.
                    let report = serde_json::json!({ "error": "interrupted", "exit_code": code });
                    let _ = print_stdout(&report.to_string());
                } else {
                    eprintln!("interrupted");
                }
                std::process::exit(code);
            }
        },
    ));
    let job = Job { input, r, backend };
    let outcome = run_job(&cli, job, Arc::clone(&reporter), cancel, started).await;
    reporter.finish();

    match outcome? {
        Outcome::DryRun { plan, estimate } => print_dry_run(&plan, estimate, cli.json, local),
        Outcome::Done(summary) if cli.json => {
            print_stdout(&serde_json::to_string(&summary).map_err(std::io::Error::from)?)
        }
        Outcome::Done(summary) => match mode {
            DisplayMode::Dashboard => print_recap(&summary),
            DisplayMode::Plain => {
                eprintln!("{}", summary_line(&summary));
                Ok(())
            }
            DisplayMode::Quiet => Ok(()),
        },
    }
}

/// The input file name shown in the dashboard title, or the whole path if it has none.
fn input_name(input: &Path) -> String {
    input.file_name().map_or_else(
        || input.display().to_string(),
        |name| name.to_string_lossy().into_owned(),
    )
}

/// Everything from extraction to the written files. Prints nothing itself
/// beyond progress events and the cost question.
async fn run_job(
    cli: &Cli,
    job: Job,
    reporter: Arc<dyn Reporter>,
    cancel: CancellationToken,
    started: Instant,
) -> Result<Outcome, SrttyError> {
    let Job { input, r, backend } = job;
    let cache_dir = paths::cache_dir();
    let compat = CompatList::load(&cache_dir);
    let mut warnings = Vec::new();
    // Compatibility data and `--prompt` handling only concern API providers.
    let entry = match &backend {
        Backend::Api { base_url, mode, .. } => {
            if r.prompt.is_some() && *mode == RequestMode::Multipart && is_openrouter(base_url) {
                let warning = "OpenRouter ignores --prompt in multipart mode".to_string();
                reporter.event(&Event::Warning(warning.clone()));
                warnings.push(warning);
            }
            provider_key(base_url).and_then(|provider| compat.find(provider, &r.model))
        }
        Backend::Local { .. } => None,
    };

    let prep = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(SrttyError::Interrupted),
        prep = prepare(&input, reporter.as_ref()) => prep?,
    };

    let provider_options = preflight(
        prep.duration_s,
        prep.audio_bytes,
        cli.speakers,
        entry,
        &r.model,
    )?;
    let estimate = estimate_cost_usd(entry, prep.duration_s);
    let chunks_override = cli.speakers.then(|| vec![(0.0, prep.duration_s)]);

    if cli.dry_run {
        let plan = chunks_override.unwrap_or_else(|| {
            plan_chunks(
                prep.duration_s,
                r.target_s,
                &prep.silences,
                SILENCE_WINDOW_S,
            )
        });
        return Ok(Outcome::DryRun { plan, estimate });
    }

    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let (threshold, yes) = (r.confirm_above_usd, cli.yes);
    let asker = Arc::clone(&reporter);
    // The answer is read on a blocking thread so that Ctrl-C still interrupts the prompt.
    let decision = tokio::task::spawn_blocking(move || {
        confirm_cost(estimate, threshold, yes, interactive, &mut |usd| {
            asker.suspend();
            ask_on_terminal(usd)
        })
    });
    tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(SrttyError::Interrupted),
        decision = decision => decision.map_err(std::io::Error::from)??,
    }

    let opts = TranscribeOptions {
        language: r.language.clone(),
        prompt: r.prompt.clone(),
        provider_options: provider_options.clone(),
    };
    let parallel = parallelism(r.provider.kind, r.jobs, available_cores(), GPU_BUILD);
    // A local model is identified by its file name and size, so replacing the
    // file invalidates the cache without hashing it.
    let (cache_base_url, cache_model) = match &backend {
        Backend::Api { base_url, .. } => (base_url.clone(), r.model.clone()),
        Backend::Local { model_path } => (
            "local".to_string(),
            format!("{}#{}", r.model, std::fs::metadata(model_path)?.len()),
        ),
    };
    let req = RunRequest {
        formats: cli.format.clone(),
        output: cli.output.clone(),
        chunks_override,
        target_s: r.target_s,
        jobs: parallel.jobs,
        opts,
        rules: cue_rules(&r.subtitles, cli.speakers),
        speaker_label: r.subtitles.speaker_label.clone(),
        cache: ChunkCache::new(&cache_dir, !cli.no_cache),
        cache_params: CacheParams {
            base_url: cache_base_url,
            model: cache_model,
            language: r.language.clone(),
            prompt: r.prompt.clone(),
            provider_options,
        },
        provider: r.provider_name.clone(),
    };
    let transcriber: Arc<dyn Transcriber> = match backend {
        Backend::Api {
            base_url,
            key,
            mode,
        } => Arc::new(OpenAiCompatible::new(base_url, key, r.model, mode)?),
        Backend::Local { model_path } => {
            load_local(&model_path, parallel.threads_per_job.unwrap_or(1), &cancel).await?
        }
    };
    let mut summary = run(prep, req, transcriber, reporter, cancel).await?;
    summary.elapsed_s = started.elapsed().as_secs_f64();
    warnings.append(&mut summary.warnings);
    summary.warnings = warnings;
    Ok(Outcome::Done(summary))
}

/// Loads the whisper model, giving up if the run is cancelled meanwhile.
#[cfg(feature = "local")]
async fn load_local(
    model_path: &Path,
    threads: usize,
    cancel: &CancellationToken,
) -> Result<Arc<dyn Transcriber>, SrttyError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(SrttyError::Interrupted),
        loaded = crate::local::LocalWhisper::load(model_path, threads) => Ok(Arc::new(loaded?)),
    }
}

#[cfg(not(feature = "local"))]
async fn load_local(
    _model_path: &Path,
    _threads: usize,
    _cancel: &CancellationToken,
) -> Result<Arc<dyn Transcriber>, SrttyError> {
    Err(local_unavailable())
}

/// SIGINT and SIGTERM as one stream: `kill` or `timeout` stop the run like
/// Ctrl-C, so the dashboard is erased. SIGHUP is left alone, so that `nohup`
/// keeps working (a handler would replace its ignore disposition), and SIGQUIT
/// keeps its default behavior.
fn stop_signals() -> std::io::Result<impl futures::Stream<Item = ()> + Unpin> {
    use tokio::signal::unix::{SignalKind, signal};
    let streams = [SignalKind::interrupt(), SignalKind::terminate()]
        .into_iter()
        .map(|kind| {
            let sig = signal(kind)?;
            Ok(Box::pin(futures::stream::unfold(
                sig,
                |mut sig| async move { sig.recv().await.map(|()| ((), sig)) },
            )))
        })
        .collect::<std::io::Result<Vec<_>>>()?;
    Ok(futures::stream::select_all(streams))
}

/// Runs `first` on the first item of `signals` and `second` on the next one.
///
/// A forced exit (`second`) skips the normal cleanup: temporary files, such as
/// the extracted audio or the `.tmp` of a write in progress, may be left behind.
pub(crate) async fn on_interrupts<S>(mut signals: S, first: impl FnOnce(), second: impl FnOnce())
where
    S: futures::Stream<Item = ()> + Unpin,
{
    use futures::StreamExt;
    if signals.next().await.is_none() {
        return;
    }
    first();
    if signals.next().await.is_some() {
        second();
    }
}

/// Asks on stderr whether to spend about `estimate_usd`, reading the answer from stdin.
fn ask_on_terminal(estimate_usd: f64) -> bool {
    eprint!("This run will cost about ${estimate_usd:.2}. Continue? [y/N] ");
    let _ = std::io::stderr().flush();
    let mut answer = String::new();
    std::io::stdin().lock().read_line(&mut answer).is_ok() && is_yes(&answer)
}

fn is_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[derive(Serialize)]
struct DryRun<'a> {
    dry_run: bool,
    chunks: &'a [(f64, f64)],
    estimated_cost_usd: Option<f64>,
}

fn print_dry_run(
    plan: &[(f64, f64)],
    estimate: Option<f64>,
    json: bool,
    local: bool,
) -> Result<(), SrttyError> {
    if json {
        let dry_run = DryRun {
            dry_run: true,
            chunks: plan,
            estimated_cost_usd: estimate,
        };
        return print_stdout(&serde_json::to_string(&dry_run).map_err(std::io::Error::from)?);
    }
    let s = if plan.len() == 1 { "" } else { "s" };
    let mut lines = vec![format!("dry run: {} chunk{s} planned", plan.len())];
    lines.extend(
        plan.iter()
            .enumerate()
            .map(|(i, (start, end))| format!("  #{} {start:.1}-{end:.1} s", i + 1)),
    );
    lines.push(match estimate {
        _ if local => "no API cost: the local backend runs on this machine".into(),
        Some(usd) => format!("estimated cost: ${usd:.4}"),
        None => "cost unknown: the model is not in the compatibility list".into(),
    });
    let mut err = std::io::stderr().lock();
    writeln!(err, "{}", lines.join("\n"))?;
    Ok(())
}

/// Prints the dashboard recap on stderr, with a green check mark.
fn print_recap(s: &Summary) -> Result<(), SrttyError> {
    let mut err = std::io::stderr().lock();
    for line in recap_lines(s) {
        match line.strip_prefix('✔') {
            Some(rest) => writeln!(err, "{}{rest}", "✔".green())?,
            None => writeln!(err, "{line}")?,
        }
    }
    Ok(())
}

fn summary_line(s: &Summary) -> String {
    let files = s
        .files
        .iter()
        .map(|f| f.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let cost = s.cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default();
    format!(
        "done: {} cues from {:.1} s of audio with {}{cost} in {:.1} s; wrote {files}",
        s.cues, s.audio_duration_s, s.model, s.elapsed_s
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
    use std::time::Duration;

    #[tokio::test]
    async fn first_interrupt_cancels_second_forces() {
        let (tx, rx) = futures::channel::mpsc::unbounded::<()>();
        let (first, second) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (f, s) = (first.clone(), second.clone());
        let task = tokio::spawn(on_interrupts(
            rx,
            move || {
                f.fetch_add(1, SeqCst);
            },
            move || {
                s.fetch_add(1, SeqCst);
            },
        ));
        tx.unbounded_send(()).unwrap();
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!((first.load(SeqCst), second.load(SeqCst)), (1, 0));
        tx.unbounded_send(()).unwrap();
        task.await.unwrap();
        assert_eq!((first.load(SeqCst), second.load(SeqCst)), (1, 1));
    }

    #[tokio::test]
    async fn terminate_counts_as_an_interrupt() {
        use futures::StreamExt;
        let mut signals = stop_signals().unwrap();
        // SIGTERM only: nothing handles SIGHUP, which would kill the test binary.
        let sent = std::process::Command::new("kill")
            .args(["-TERM", &std::process::id().to_string()])
            .status()
            .unwrap();
        assert!(sent.success());
        let got = tokio::time::timeout(Duration::from_secs(5), signals.next()).await;
        assert_eq!(got, Ok(Some(())));
    }

    #[test]
    fn preflight_rules() {
        let l = crate::test_support::compat_seed();
        let grok = l.find("openrouter", "x-ai/grok-stt-1.0");
        let turbo = l.find("openrouter", "openai/whisper-large-v3-turbo");
        assert_eq!(
            preflight(600.0, 1_000_000, false, turbo, "t").unwrap(),
            None
        );
        assert_eq!(
            preflight(3500.0, 13_000_000, true, grok, "g").unwrap(),
            Some(serde_json::json!({"xai": {"diarize": true}}))
        );
        assert!(preflight(4600.0, 13_000_000, true, grok, "g").is_err());
        assert!(preflight(600.0, 30 * 1024 * 1024, true, grok, "g").is_err());
        assert!(
            preflight(600.0, 1_000_000, true, turbo, "t")
                .unwrap_err()
                .to_string()
                .contains("--speakers")
        );
        assert!(preflight(600.0, 1_000_000, true, None, "x").is_err());
    }

    #[test]
    fn cost_confirmation() {
        let mut never = |_| panic!("should not ask");
        assert!(confirm_cost(Some(0.5), 1.0, false, false, &mut never).is_ok());
        assert!(confirm_cost(None, 1.0, false, false, &mut never).is_ok());
        assert!(confirm_cost(Some(2.0), 1.0, true, false, &mut never).is_ok());
        assert!(matches!(
            confirm_cost(Some(2.0), 1.0, false, false, &mut never),
            Err(SrttyError::CostNotConfirmed { .. })
        ));
        assert!(confirm_cost(Some(2.0), 1.0, false, true, &mut |_| false).is_err());
        assert!(confirm_cost(Some(2.0), 1.0, false, true, &mut |_| true).is_ok());
    }

    #[test]
    fn yes_answers() {
        for answer in ["y\n", "Y", " yes \n", "YES"] {
            assert!(is_yes(answer), "{answer:?}");
        }
        for answer in ["", "\n", "n", "no", "yess", "o"] {
            assert!(!is_yes(answer), "{answer:?}");
        }
    }

    fn settings(flags: &[&str], toml: &str) -> Result<(), SrttyError> {
        let cfg: Config = toml::from_str(&format!(
            "default_provider = \"p\"\n{toml}\n[providers.p]\napi_key_env = \"K\"\nmodel = \"m\"\n"
        ))
        .unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"].iter().chain(flags)).unwrap();
        let r = config::resolve(cfg, &cli, &|k| (k == "K").then(|| "secret".into())).unwrap();
        check_settings(&r)
    }

    #[test]
    fn speaker_prefix_is_reserved_only_with_speakers() {
        let subtitles = SubtitleConfig::default();
        assert_eq!(cue_rules(&subtitles, false).first_line_reserve, 0);
        let with = cue_rules(&subtitles, true);
        assert_eq!(with.first_line_reserve, 13);
        assert_eq!(with.max_chars_per_line, 42);
    }

    #[test]
    fn rejects_zero_jobs_and_non_positive_chunks() {
        assert!(settings(&[], "").is_ok());
        assert!(matches!(
            settings(&["--jobs", "0"], ""),
            Err(SrttyError::Config(_))
        ));
        assert!(settings(&[], "[chunking]\njobs = 0").is_err());
        assert!(settings(&["--chunk", "0"], "").is_err());
        assert!(settings(&["--chunk=-5"], "").is_err());
        assert!(settings(&[], "[chunking]\ntarget_s = 0.0").is_err());
    }
}
