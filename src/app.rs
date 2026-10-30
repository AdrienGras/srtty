//! What the binary does once the command line is parsed: `config show`,
//! `setup`, or a transcription run with its checks, dry run and cost confirmation.

use std::io::{BufRead, IsTerminal, Write};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use serde::Serialize;
use tokio_util::sync::CancellationToken;

use crate::assemble::CueRules;
use crate::cache::{CacheParams, ChunkCache};
use crate::cli::{Cli, Command, CompatAction, ConfigAction};
use crate::compat::{CompatEntry, CompatList, compat_url, estimate_cost_usd, provider_key};
use crate::config::{self, Config, KeyState, ProviderKind, RequestMode, Resolved, SubtitleConfig};
use crate::error::SrttyError;
use crate::media::plan_chunks;
use crate::output::speaker_prefix_reserve;
use crate::paths;
use crate::report::{Event, PlainReporter, Reporter, Summary};
use crate::run::{RunRequest, SILENCE_WINDOW_S, prepare, run};
use crate::setup::{InquirePrompter, LiveTester, SetupContext, run_setup};
use crate::transcribe::TranscribeOptions;
use crate::transcribe::openai::{OpenAiCompatible, is_openrouter};

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
    if r.jobs == 0 {
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
}

fn show_config(cli: &Cli) -> Result<(), SrttyError> {
    let r = load_settings(cli)?;
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
        request_mode: r.provider.request_mode,
        language: r.language.as_deref(),
        confirm_above_usd: r.confirm_above_usd,
        subtitles: &r.subtitles,
        chunking: ChunkingView {
            target_s: r.target_s,
            jobs: r.jobs,
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

async fn transcribe(cli: Cli) -> Result<(), SrttyError> {
    let started = Instant::now();
    let input = cli.input.clone().ok_or_else(|| {
        SrttyError::Usage("no input file given; run `srtty --help` for usage".into())
    })?;
    let r = load_settings(&cli)?;
    let api_key = r.require_key()?;
    if r.provider.kind == ProviderKind::Local {
        return Err(SrttyError::Config(
            "local backend is not available in this build".into(),
        ));
    }
    let base_url = r.provider.base_url.clone().ok_or_else(|| {
        SrttyError::Config(format!(
            "provider `{}` has no `base_url` in the config file",
            r.provider_name
        ))
    })?;
    let cache_dir = paths::cache_dir();
    let compat = CompatList::load(&cache_dir);
    let entry = provider_key(&base_url).and_then(|provider| compat.find(provider, &r.model));

    let progress: Box<dyn Write + Send> = if cli.quiet {
        Box::new(std::io::sink())
    } else {
        Box::new(std::io::stderr())
    };
    let reporter: Arc<dyn Reporter> = Arc::new(PlainReporter::new(progress));
    let mut warnings = Vec::new();
    if r.prompt.is_some()
        && r.provider.request_mode == RequestMode::Multipart
        && is_openrouter(&base_url)
    {
        let warning = "OpenRouter ignores --prompt in multipart mode".to_string();
        reporter.event(&Event::Warning(warning.clone()));
        warnings.push(warning);
    }

    let cancel = CancellationToken::new();
    cancel_on_ctrl_c(cancel.clone());
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
        return print_dry_run(&plan, estimate, cli.json);
    }

    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();
    let (threshold, yes) = (r.confirm_above_usd, cli.yes);
    // The answer is read on a blocking thread so that Ctrl-C still interrupts the prompt.
    let decision = tokio::task::spawn_blocking(move || {
        confirm_cost(estimate, threshold, yes, interactive, &mut ask_on_terminal)
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
    let req = RunRequest {
        formats: cli.format.clone(),
        output: cli.output.clone(),
        chunks_override,
        target_s: r.target_s,
        jobs: r.jobs,
        opts,
        rules: cue_rules(&r.subtitles, cli.speakers),
        speaker_label: r.subtitles.speaker_label.clone(),
        cache: ChunkCache::new(&cache_dir, !cli.no_cache),
        cache_params: CacheParams {
            base_url: base_url.clone(),
            model: r.model.clone(),
            language: r.language.clone(),
            prompt: r.prompt.clone(),
            provider_options,
        },
        provider: r.provider_name.clone(),
    };
    let transcriber = Arc::new(OpenAiCompatible::new(
        base_url,
        api_key,
        r.model,
        r.provider.request_mode,
    ));
    let mut summary = run(prep, req, transcriber, reporter, cancel).await?;
    summary.elapsed_s = started.elapsed().as_secs_f64();
    warnings.append(&mut summary.warnings);
    summary.warnings = warnings;

    if cli.json {
        print_stdout(&serde_json::to_string(&summary).map_err(std::io::Error::from)?)
    } else {
        if !cli.quiet {
            eprintln!("{}", summary_line(&summary));
        }
        Ok(())
    }
}

/// Cancels `cancel` on the first Ctrl-C.
fn cancel_on_ctrl_c(cancel: CancellationToken) {
    tokio::spawn(async move {
        if tokio::signal::ctrl_c().await.is_ok() {
            cancel.cancel();
        }
    });
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

fn print_dry_run(plan: &[(f64, f64)], estimate: Option<f64>, json: bool) -> Result<(), SrttyError> {
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
        Some(usd) => format!("estimated cost: ${usd:.4}"),
        None => "cost unknown: the model is not in the compatibility list".into(),
    });
    let mut err = std::io::stderr().lock();
    writeln!(err, "{}", lines.join("\n"))?;
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
    use crate::compat::CompatList;
    use clap::Parser;

    #[test]
    fn preflight_rules() {
        let l = CompatList::embedded();
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
