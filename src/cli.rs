use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::output::Format;

/// Turn video and audio into subtitles with OpenAI-compatible transcription APIs.
#[derive(Debug, Parser)]
#[command(name = "srtty", version, about)]
pub struct Cli {
    /// Input video or audio file.
    pub input: Option<PathBuf>,

    /// Output path (default: next to the input file).
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Output formats, comma-separated.
    #[arg(long, value_enum, value_delimiter = ',', default_value = "srt")]
    pub format: Vec<Format>,

    /// Provider name from the config file.
    #[arg(long, global = true)]
    pub provider: Option<String>,

    /// Transcription model.
    #[arg(long)]
    pub model: Option<String>,

    /// Spoken language (ISO 639-1 code).
    #[arg(long)]
    pub language: Option<String>,

    /// Prompt to guide the transcription.
    #[arg(long)]
    pub prompt: Option<String>,

    /// Label speakers (single request, requires diarization support).
    #[arg(long)]
    pub speakers: bool,

    /// Target chunk duration in seconds.
    #[arg(long)]
    pub chunk: Option<f64>,

    /// Number of parallel transcription jobs.
    #[arg(long)]
    pub jobs: Option<usize>,

    /// Show the plan and cost estimate without sending anything.
    #[arg(long)]
    pub dry_run: bool,

    /// Skip the cost confirmation.
    #[arg(short, long)]
    pub yes: bool,

    /// Emit machine-readable JSON.
    #[arg(long, global = true)]
    pub json: bool,

    /// Suppress progress output.
    #[arg(short, long)]
    pub quiet: bool,

    /// Ignore the chunk cache.
    #[arg(long)]
    pub no_cache: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Inspect the configuration.
    Config {
        #[command(subcommand)]
        action: ConfigAction,
    },
    /// List the provider's transcription models with price and compatibility.
    Models,
    /// Maintainer tools for compat.json.
    #[command(hide = true)]
    Compat {
        #[command(subcommand)]
        action: CompatAction,
    },
}

#[derive(Debug, Subcommand)]
pub enum CompatAction {
    /// Probe the OpenRouter transcription models and rewrite compat.json.
    Probe(ProbeArgs),
}

#[derive(Debug, clap::Args)]
pub struct ProbeArgs {
    /// compat.json to read and rewrite.
    #[arg(long, default_value = "compat.json")]
    pub output: PathBuf,

    /// Skip models priced above this many USD per second.
    #[arg(long, default_value_t = 0.001)]
    pub max_usd_per_s: f64,

    /// Stop before a probe could take the run above this many USD.
    #[arg(long, default_value_t = 0.50)]
    pub budget_usd: f64,

    /// Probe only this model id (repeatable).
    #[arg(long)]
    pub only: Vec<String>,

    /// Environment variable holding the OpenRouter API key.
    #[arg(long, default_value = "OPENROUTER_API_KEY")]
    pub api_key_env: String,

    /// OpenRouter API base URL.
    #[arg(long, hide = true, default_value = "https://openrouter.ai/api/v1")]
    pub base_url: String,
}

#[derive(Debug, Subcommand)]
pub enum ConfigAction {
    /// Print the effective configuration.
    Show,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::output::Format;
    use clap::Parser;

    #[test]
    fn parses_multiple_formats_and_flags() {
        let cli = Cli::try_parse_from([
            "srtty",
            "talk.mkv",
            "--format",
            "srt,vtt,txt",
            "--speakers",
            "--jobs",
            "8",
            "--yes",
        ])
        .unwrap();
        assert_eq!(cli.input.as_deref(), Some(std::path::Path::new("talk.mkv")));
        assert_eq!(cli.format, vec![Format::Srt, Format::Vtt, Format::Txt]);
        assert!(cli.speakers && cli.yes);
        assert_eq!(cli.jobs, Some(8));
    }
    #[test]
    fn format_defaults_to_srt() {
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        assert_eq!(cli.format, vec![Format::Srt]);
    }
    #[test]
    fn rejects_unknown_format() {
        assert!(Cli::try_parse_from(["srtty", "a.mp4", "--format", "both"]).is_err());
    }
}
