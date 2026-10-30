use miette::Diagnostic;
use thiserror::Error;

#[derive(Debug, Error, Diagnostic)]
pub enum SrttyError {
    #[error("{0}")]
    Usage(String),

    #[error("{0}")]
    Config(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error("model `{model}` cannot be used for transcription")]
    #[help("run `srtty models` to see compatible models")]
    UnusableModel { model: String },

    #[error("{0}")]
    Ffmpeg(String),

    #[error("interrupted")]
    Interrupted,

    #[error("estimated cost ${estimate_usd:.2} not confirmed; pass --yes to proceed")]
    CostNotConfirmed { estimate_usd: f64 },
}

impl SrttyError {
    /// Process exit code for this error.
    pub fn exit_code(&self) -> i32 {
        match self {
            SrttyError::Usage(_)
            | SrttyError::Config(_)
            | SrttyError::Io(_)
            | SrttyError::CostNotConfirmed { .. } => 1,
            SrttyError::UnusableModel { .. } => 2,
            SrttyError::Ffmpeg(_) => 3,
            SrttyError::Interrupted => 130,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_spec() {
        assert_eq!(SrttyError::Usage("x".into()).exit_code(), 1);
        assert_eq!(
            SrttyError::UnusableModel { model: "m".into() }.exit_code(),
            2
        );
        assert_eq!(SrttyError::Ffmpeg("x".into()).exit_code(), 3);
        assert_eq!(SrttyError::Interrupted.exit_code(), 130);
        assert_eq!(
            SrttyError::CostNotConfirmed { estimate_usd: 2.0 }.exit_code(),
            1
        );
    }
}
