//! The local path of `srtty setup`: pick a catalog model and download it
//! (verified), or point at a ggml file already on disk, then optionally test
//! it on the embedded sample. Compiled only with the `local` feature.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;

use super::prompt::Prompter;
use crate::config::available_cores;
use crate::error::SrttyError;
use crate::local::catalog::{CatalogModel, human_size, model_url};
use crate::local::download::{download, http_client, is_complete};
use crate::local::{LocalWhisper, check_ggml_magic};
use crate::paths::expand_tilde;
use crate::probe::{SAMPLE, SAMPLE_S, write_sample};
use crate::transcribe::{TranscribeOptions, Transcriber};

const OWN_FILE: &str = "Use a model file I already have";
/// Minimum time between two redraws of the download progress line.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Downloads a catalog model to its final path.
#[async_trait]
pub trait ModelFetcher: Send + Sync {
    async fn fetch(&self, model: &CatalogModel, dest: &Path) -> Result<(), SrttyError>;
}

/// Transcribes the embedded sample with a model file, for free.
#[async_trait]
pub trait LocalTester: Send + Sync {
    async fn test(&self, model_path: &Path) -> Result<LocalTestOutcome, SrttyError>;
}

/// What the sample test produced.
#[derive(Debug, Clone, PartialEq)]
pub struct LocalTestOutcome {
    pub words: usize,
    pub secs: f64,
    pub language: Option<String>,
}

/// Where models go, which ones are offered, and how they are fetched and tested.
pub struct LocalSetup<'a> {
    pub models_dir: PathBuf,
    pub catalog: &'a [CatalogModel],
    pub fetcher: &'a dyn ModelFetcher,
    pub tester: &'a dyn LocalTester,
    /// What a leading `~` in a typed path stands for (`None`: kept as typed).
    pub home: Option<PathBuf>,
}

/// Asks for a model until one is kept and returns its path: a catalog model
/// (downloaded after confirmation unless already complete) or a ggml file the
/// user already has. The free sample test is offered for either.
pub async fn choose_local_model(
    p: &mut dyn Prompter,
    local: &LocalSetup<'_>,
) -> Result<PathBuf, SrttyError> {
    loop {
        let mut options: Vec<String> = local
            .catalog
            .iter()
            .map(|m| model_label(m, &local.models_dir.join(m.file)))
            .collect();
        options.push(OWN_FILE.to_string());
        let i = p.select("Whisper model:", &options)?;

        let path = match local.catalog.get(i) {
            Some(model) => {
                let dest = local.models_dir.join(model.file);
                if is_complete(&dest, model.bytes, model.sha256)? {
                    p.say(&format!("✔ {} is already downloaded.", model.file));
                } else {
                    if !p.confirm(
                        &format!(
                            "Download {} ({}) from Hugging Face to {}?",
                            model.file,
                            human_size(model.bytes),
                            local.models_dir.display()
                        ),
                        true,
                    )? {
                        continue;
                    }
                    match local.fetcher.fetch(model, &dest).await {
                        Ok(()) => {}
                        Err(SrttyError::Interrupted) => return Err(SrttyError::Interrupted),
                        Err(e) => {
                            let e = e.to_string();
                            // The download errors already say what failed.
                            let cause = e.strip_prefix("download failed: ").unwrap_or(&e);
                            p.say(&format!("✘ Download failed: {cause}"));
                            continue;
                        }
                    }
                }
                dest
            }
            None => {
                let home = local.home.as_deref();
                let input = p.text("Path to a ggml model file:", None, &|input| {
                    validate_model_file(input, home)
                })?;
                // The config must not depend on the directory `setup` ran in.
                let path = expand(&input, home);
                std::path::absolute(&path).unwrap_or(path)
            }
        };

        if !p.confirm(
            "Test the model on a 30 s sample? It runs locally and costs nothing.",
            true,
        )? {
            return Ok(path);
        }
        match local.tester.test(&path).await {
            Ok(outcome) => {
                p.say(&format!(
                    "✔ Test passed: {} words in {:.1} s (language: {}).",
                    outcome.words,
                    outcome.secs,
                    outcome.language.as_deref().unwrap_or("unknown")
                ));
                return Ok(path);
            }
            Err(SrttyError::Interrupted) => return Err(SrttyError::Interrupted),
            Err(e) => {
                p.say(&format!("✘ The model could not be used: {e}"));
                if p.confirm("Keep this model anyway?", false)? {
                    return Ok(path);
                }
            }
        }
    }
}

/// `name  size  note`, marked when a file of the right size is already there
/// (the full check, with the hash, runs once the model is chosen).
fn model_label(model: &CatalogModel, dest: &Path) -> String {
    let mut label = format!(
        "{:<20} {:>7}  {}",
        model.name,
        human_size(model.bytes),
        model.note
    );
    if std::fs::metadata(dest).is_ok_and(|m| m.len() == model.bytes) {
        label.push_str("  (downloaded)");
    }
    label
}

/// The typed path, trimmed and with a leading `~` expanded.
fn expand(input: &str, home: Option<&Path>) -> PathBuf {
    expand_tilde(Path::new(input.trim()), home)
}

/// Accepts an existing file that starts with the ggml magic bytes. The
/// messages never quote the input.
fn validate_model_file(input: &str, home: Option<&Path>) -> Result<(), String> {
    let path = expand(input, home);
    if !path.is_file() {
        return Err("No file at that path.".into());
    }
    match check_ggml_magic(&path) {
        Ok(()) => Ok(()),
        Err(SrttyError::Io(e)) => Err(format!("Cannot read that file: {e}.")),
        Err(_) => Err("That file is not a ggml Whisper model.".into()),
    }
}

/// Downloads from Hugging Face with a progress line on stderr. Ctrl-C drops
/// the download, which removes its temporary file.
pub struct LiveFetcher;

#[async_trait]
impl ModelFetcher for LiveFetcher {
    async fn fetch(&self, model: &CatalogModel, dest: &Path) -> Result<(), SrttyError> {
        let client = http_client()?;
        let url = model_url(model.file);
        let total = human_size(model.bytes);
        let mut last_draw: Option<Instant> = None;
        let mut progress = |received: u64| {
            let now = Instant::now();
            let done = received >= model.bytes;
            if !done && last_draw.is_some_and(|t| now.duration_since(t) < PROGRESS_EVERY) {
                return;
            }
            last_draw = Some(now);
            let percent = received.saturating_mul(100) / model.bytes.max(1);
            let mut err = std::io::stderr().lock();
            let _ = write!(
                err,
                "\rDownloading {}: {percent}% ({} of {total})",
                model.file,
                human_size(received)
            );
            let _ = err.flush();
        };
        let result = tokio::select! {
            r = download(&client, &url, dest, model.bytes, model.sha256, &mut progress) => r,
            _ = tokio::signal::ctrl_c() => Err(SrttyError::Interrupted),
        };
        if last_draw.is_some() {
            eprintln!();
        }
        result
    }
}

/// Loads the model and transcribes the embedded sample without a language.
/// Ctrl-C abandons the test (and aborts whisper.cpp).
pub struct LiveLocalTester;

#[async_trait]
impl LocalTester for LiveLocalTester {
    async fn test(&self, model_path: &Path) -> Result<LocalTestOutcome, SrttyError> {
        let work = async {
            let sample = write_sample(SAMPLE, SAMPLE_S)?;
            let whisper = LocalWhisper::load(model_path, available_cores().min(8)).await?;
            let started = Instant::now();
            let transcript = whisper
                .transcribe(&sample.chunk, &TranscribeOptions::default())
                .await?;
            Ok(LocalTestOutcome {
                words: transcript.words.len(),
                secs: started.elapsed().as_secs_f64(),
                language: transcript.language,
            })
        };
        tokio::select! {
            r = work => r,
            _ = tokio::signal::ctrl_c() => Err(SrttyError::Interrupted),
        }
    }
}
