//! The recap printed under the erased dashboard once a run succeeds.

use crate::dashboard::view::fmt_clock;
use crate::report::Summary;

/// The recap, one plain line per item; the caller colors the leading `✔`.
pub fn recap_lines(s: &Summary) -> Vec<String> {
    let files = s
        .files
        .iter()
        .map(|f| f.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    let cost = s
        .cost_usd
        .map_or_else(|| "n/a".to_string(), |c| format!("${c:.4}"));
    vec![
        format!("✔ Done in {}", fmt_clock(s.elapsed_s)),
        format!("  files    {files}"),
        format!("  audio    {}", fmt_clock(s.audio_duration_s)),
        format!("  cues     {}", s.cues),
        format!("  model    {}", s.model),
        format!("  cost     {cost}"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn summary(cost_usd: Option<f64>) -> Summary {
        Summary {
            files: vec![PathBuf::from("talk.srt"), PathBuf::from("talk.vtt")],
            audio_duration_s: 3723.0,
            provider: "openrouter".into(),
            model: "x-ai/grok-stt-1.0".into(),
            cost_usd,
            cues: 1204,
            chunks: 13,
            elapsed_s: 72.4,
            warnings: vec![],
        }
    }

    #[test]
    fn recap_lists_files_audio_cues_model_cost() {
        assert_eq!(
            recap_lines(&summary(Some(0.0213))),
            [
                "✔ Done in 1:12",
                "  files    talk.srt, talk.vtt",
                "  audio    1:02:03",
                "  cues     1204",
                "  model    x-ai/grok-stt-1.0",
                "  cost     $0.0213",
            ]
        );
    }

    #[test]
    fn recap_unknown_cost() {
        let lines = recap_lines(&summary(None));
        assert_eq!(lines.last().unwrap(), "  cost     n/a");
    }
}
