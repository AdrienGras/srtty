//! `srtty compat probe`: regenerates `compat.json` by probing the OpenRouter
//! transcription models on the embedded samples, within a price cap and a
//! budget. Run weekly in CI; a model that cannot be judged keeps its entry.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::cli::ProbeArgs;
use crate::compat::{
    COMPAT_SCHEMA, CompatEntry, CompatList, CompatStatus, parse_compat, utc_today,
};
use crate::config::{ApiKey, RequestMode, check_key_var_name};
use crate::error::SrttyError;
use crate::fsutil::write_atomic;
use crate::probe::{
    ProbeOutcome, SAMPLE, SAMPLE_S, SPEAKERS_SAMPLE, SPEAKERS_SAMPLE_S, SampleFile, Verdict,
    probe_diarization, probe_model, write_sample,
};
use crate::providers::{ListedModel, Listing, endpoint_tags, list_models};
use crate::report::PlainReporter;
use crate::transcribe::TranscribeError;
use crate::transcribe::openai::OpenAiCompatible;

/// Provider key of the entries this command maintains.
const PROVIDER: &str = "openrouter";

/// What a run may probe and spend.
pub struct ProbePolicy {
    pub max_usd_per_s: f64,
    pub budget_usd: f64,
    /// Model ids to probe; every model when empty.
    pub only: Vec<String>,
    /// Date stamped on the list and on new entries (`YYYY-MM-DD`).
    pub today: String,
}

/// The requests `probe_all` makes, so tests can answer them without a network.
#[async_trait]
pub trait Prober: Send + Sync {
    async fn probe(&self, model: &str) -> Result<ProbeOutcome, TranscribeError>;
    async fn endpoint_tags(&self, model: &str) -> Result<Vec<String>, SrttyError>;
    async fn diarize(
        &self,
        model: &str,
        option: &serde_json::Value,
    ) -> Result<(Option<bool>, Option<f64>), TranscribeError>;
}

/// What a run did.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ProbeReport {
    pub spent_usd: f64,
    /// Every model sent to `Prober::probe`.
    pub probed: Vec<String>,
    /// Models whose previous entry was kept (if any), with the reason.
    pub kept: Vec<(String, String)>,
    pub budget_exhausted: bool,
}

/// Whether two lists say the same thing, ignoring `generated_at` and `checked_at`.
pub fn same_content(a: &CompatList, b: &CompatList) -> bool {
    let undated = |list: &CompatList| {
        let models: Vec<CompatEntry> = list
            .models
            .iter()
            .cloned()
            .map(|e| CompatEntry {
                checked_at: None,
                ..e
            })
            .collect();
        (list.schema, models)
    };
    undated(a) == undated(b)
}

/// What a request cost: the price the API reported when it is usable,
/// otherwise the estimate.
fn charged(reported: Option<f64>, estimate: f64) -> f64 {
    reported
        .filter(|c| c.is_finite() && *c >= 0.0)
        .unwrap_or(estimate)
}

/// Keeps the previous entry of `id`, if there was one, and says why.
fn keep(
    models: &mut Vec<CompatEntry>,
    report: &mut ProbeReport,
    old: Option<&CompatEntry>,
    id: &str,
    why: &str,
) {
    models.extend(old.cloned());
    report.kept.push((id.to_string(), why.to_string()));
}

/// Probes the listed models and merges the answers with `previous`.
pub async fn probe_all(
    listed: &[ListedModel],
    previous: &CompatList,
    policy: &ProbePolicy,
    prober: &dyn Prober,
) -> Result<(CompatList, ProbeReport), SrttyError> {
    let mut models: Vec<CompatEntry> = previous
        .models
        .iter()
        .filter(|e| e.provider != PROVIDER)
        .cloned()
        .collect();
    let mut report = ProbeReport::default();
    let mut order: Vec<&ListedModel> = listed.iter().collect();
    order.sort_by(|a, b| a.id.cmp(&b.id));
    order.dedup_by(|a, b| a.id == b.id);

    for model in order {
        let id = model.id.as_str();
        let old = previous.find(PROVIDER, id);
        if report.budget_exhausted {
            keep(&mut models, &mut report, old, id, "budget");
            continue;
        }
        if !policy.only.is_empty() && !policy.only.iter().any(|o| o == id) {
            keep(&mut models, &mut report, old, id, "not selected");
            continue;
        }
        let Some(rate) = model
            .usd_per_s
            .filter(|p| p.is_finite() && *p >= 0.0 && *p <= policy.max_usd_per_s)
        else {
            keep(&mut models, &mut report, old, id, "price");
            continue;
        };
        let estimate = rate * SAMPLE_S;
        if report.spent_usd + estimate > policy.budget_usd {
            report.budget_exhausted = true;
            keep(&mut models, &mut report, old, id, "budget");
            continue;
        }

        report.probed.push(id.to_string());
        let outcome = prober.probe(id).await.map_err(SrttyError::Transcribe)?;
        report.spent_usd += charged(outcome.cost_usd, estimate);
        let (status, words, fine_segments) = match outcome.verdict {
            Verdict::Inconclusive(why) => {
                keep(&mut models, &mut report, old, id, &why);
                continue;
            }
            Verdict::Usable {
                words,
                fine_segments,
            } => (CompatStatus::Compatible, words, fine_segments),
            Verdict::Unusable => (CompatStatus::Unusable, false, false),
            Verdict::Incompatible => (CompatStatus::Incompatible, false, false),
        };

        let mut diarization = old.and_then(|e| e.diarization.clone());
        if words {
            // With several endpoints the answer would depend on the routing.
            if let Ok(tags) = prober.endpoint_tags(id).await
                && let [tag] = tags.as_slice()
                && let Some(provider) = tag.split('/').next().filter(|p| !p.is_empty())
            {
                let option = serde_json::json!({ provider: { "diarize": true } });
                let estimate = rate * SPEAKERS_SAMPLE_S;
                if report.spent_usd + estimate > policy.budget_usd {
                    report.budget_exhausted = true;
                } else {
                    let (found, cost) = prober
                        .diarize(id, &option)
                        .await
                        .map_err(SrttyError::Transcribe)?;
                    report.spent_usd += charged(cost, estimate);
                    match found {
                        Some(true) => diarization = Some(option),
                        Some(false) => diarization = None,
                        None => {}
                    }
                }
            }
        }

        models.push(CompatEntry {
            provider: PROVIDER.into(),
            id: id.into(),
            status,
            words,
            fine_segments,
            diarization,
            usd_per_hour: Some(rate * 3600.0),
            checked_at: Some(policy.today.clone()),
        });
    }

    models.sort_by(|a, b| (&a.provider, &a.id).cmp(&(&b.provider, &b.id)));
    let list = CompatList {
        schema: COMPAT_SCHEMA,
        generated_at: policy.today.clone(),
        models,
    };
    Ok((list, report))
}

/// Sends the requests to OpenRouter, one `OpenAiCompatible` per model, and
/// remembers the cost each model reported.
struct LiveProber {
    base_url: String,
    key: ApiKey,
    sample: SampleFile,
    speakers_sample: SampleFile,
    reporter: PlainReporter,
    costs: Mutex<HashMap<String, f64>>,
}

impl LiveProber {
    fn transcriber(&self, model: &str) -> OpenAiCompatible {
        OpenAiCompatible::new(
            self.base_url.clone(),
            self.key.clone(),
            model.to_string(),
            RequestMode::Multipart,
        )
    }

    fn add_cost(&self, model: &str, cost: Option<f64>) {
        if let Some(cost) = cost.filter(|c| c.is_finite()) {
            let mut costs = self.costs.lock().unwrap_or_else(|p| p.into_inner());
            *costs.entry(model.to_string()).or_default() += cost;
        }
    }

    fn cost_of(&self, model: &str) -> Option<f64> {
        let costs = self.costs.lock().unwrap_or_else(|p| p.into_inner());
        costs.get(model).copied()
    }
}

#[async_trait]
impl Prober for LiveProber {
    async fn probe(&self, model: &str) -> Result<ProbeOutcome, TranscribeError> {
        let t = self.transcriber(model);
        let outcome = probe_model(&t, &self.sample.chunk, &self.reporter).await?;
        self.add_cost(model, outcome.cost_usd);
        Ok(outcome)
    }

    async fn endpoint_tags(&self, model: &str) -> Result<Vec<String>, SrttyError> {
        endpoint_tags(&self.base_url, model).await
    }

    async fn diarize(
        &self,
        model: &str,
        option: &serde_json::Value,
    ) -> Result<(Option<bool>, Option<f64>), TranscribeError> {
        let t = self.transcriber(model);
        let result =
            probe_diarization(&t, &self.speakers_sample.chunk, option, &self.reporter).await?;
        self.add_cost(model, result.1);
        Ok(result)
    }
}

/// One stderr line for `id`: its new status or why it was kept.
fn model_line(id: &str, new: &CompatList, report: &ProbeReport, cost: Option<f64>) -> String {
    let outcome = match report.kept.iter().find(|(kept, _)| kept == id) {
        Some((_, why)) => format!("kept ({why})"),
        None => {
            let entry = new.find(PROVIDER, id);
            let status = match entry.map(|e| e.status) {
                Some(CompatStatus::Compatible) => "compatible",
                Some(CompatStatus::Unusable) => "unusable",
                Some(CompatStatus::Incompatible) => "incompatible",
                None => "kept (no entry)",
            };
            let speakers = entry.is_some_and(|e| e.diarization.is_some());
            format!("{status}{}", if speakers { ", speakers" } else { "" })
        }
    };
    match cost {
        Some(usd) => format!("{id}: {outcome}, ${usd:.4}"),
        None => format!("{id}: {outcome}"),
    }
}

pub async fn run(args: &ProbeArgs) -> Result<(), SrttyError> {
    check_key_var_name(&args.api_key_env)?;
    let key = std::env::var(&args.api_key_env)
        .ok()
        .filter(|v| !v.is_empty())
        .map(ApiKey)
        .ok_or_else(|| {
            SrttyError::Config(format!(
                "environment variable {} is not set",
                args.api_key_env
            ))
        })?;

    let listed = list_models(Listing::OpenRouter, &args.base_url, None).await?;
    let previous = match std::fs::read_to_string(&args.output) {
        Ok(text) => parse_compat(&text)
            .map_err(|e| SrttyError::Config(format!("{}: {e}", args.output.display())))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CompatList {
            schema: COMPAT_SCHEMA,
            generated_at: String::new(),
            models: vec![],
        },
        Err(e) => return Err(e.into()),
    };

    let prober = LiveProber {
        base_url: args.base_url.clone(),
        key,
        sample: write_sample(SAMPLE, SAMPLE_S)?,
        speakers_sample: write_sample(SPEAKERS_SAMPLE, SPEAKERS_SAMPLE_S)?,
        reporter: PlainReporter::new(Box::new(std::io::stderr())),
        costs: Mutex::new(HashMap::new()),
    };
    let policy = ProbePolicy {
        max_usd_per_s: args.max_usd_per_s,
        budget_usd: args.budget_usd,
        only: args.only.clone(),
        today: utc_today(),
    };
    let (new, report) = probe_all(&listed, &previous, &policy, &prober).await?;

    let mut ids: Vec<&str> = listed.iter().map(|m| m.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    let mut err = std::io::stderr().lock();
    for id in ids {
        writeln!(err, "{}", model_line(id, &new, &report, prober.cost_of(id)))?;
    }
    writeln!(
        err,
        "spent ${:.4} of ${:.2}",
        report.spent_usd, policy.budget_usd
    )?;
    if same_content(&new, &previous) {
        writeln!(err, "compat.json unchanged")?;
    } else {
        let mut text = serde_json::to_string_pretty(&new).map_err(std::io::Error::from)?;
        text.push('\n');
        write_atomic(&args.output, text.as_bytes())?;
        writeln!(err, "wrote {}", args.output.display())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    #[derive(Default)]
    struct FakeProber {
        outcomes: HashMap<String, ProbeOutcome>,
        auth_failures: Vec<String>,
        tags: HashMap<String, Vec<String>>,
        diarization: HashMap<String, (Option<bool>, Option<f64>)>,
        calls: Mutex<Vec<String>>,
    }

    impl FakeProber {
        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
        fn log(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
    }

    #[async_trait]
    impl Prober for FakeProber {
        async fn probe(&self, model: &str) -> Result<ProbeOutcome, TranscribeError> {
            self.log(format!("probe {model}"));
            if self.auth_failures.iter().any(|m| m == model) {
                return Err(TranscribeError::Auth { status: 401 });
            }
            Ok(self.outcomes[model].clone())
        }
        async fn endpoint_tags(&self, model: &str) -> Result<Vec<String>, SrttyError> {
            self.log(format!("tags {model}"));
            self.tags
                .get(model)
                .cloned()
                .ok_or_else(|| SrttyError::Network("no endpoints".into()))
        }
        async fn diarize(
            &self,
            model: &str,
            option: &serde_json::Value,
        ) -> Result<(Option<bool>, Option<f64>), TranscribeError> {
            self.log(format!("diarize {model} {option}"));
            Ok(self.diarization[model])
        }
    }

    fn usable(words: bool, fine_segments: bool, cost: Option<f64>) -> ProbeOutcome {
        ProbeOutcome {
            verdict: Verdict::Usable {
                words,
                fine_segments,
            },
            cost_usd: cost,
        }
    }

    fn outcome(verdict: Verdict) -> ProbeOutcome {
        ProbeOutcome {
            verdict,
            cost_usd: None,
        }
    }

    fn listed(id: &str, usd_per_s: Option<f64>) -> ListedModel {
        ListedModel {
            id: id.into(),
            usd_per_s,
            token_priced: usd_per_s.is_none(),
            url: None,
        }
    }

    fn entry(provider: &str, id: &str, status: CompatStatus) -> CompatEntry {
        CompatEntry {
            provider: provider.into(),
            id: id.into(),
            status,
            words: false,
            fine_segments: false,
            diarization: None,
            usd_per_hour: Some(1.0),
            checked_at: Some("2020-01-01".into()),
        }
    }

    fn list(models: Vec<CompatEntry>) -> CompatList {
        CompatList {
            schema: COMPAT_SCHEMA,
            generated_at: "2020-01-01".into(),
            models,
        }
    }

    fn policy() -> ProbePolicy {
        ProbePolicy {
            max_usd_per_s: 0.001,
            budget_usd: 0.5,
            only: vec![],
            today: "2030-05-06".into(),
        }
    }

    fn tag(tags: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        tags.iter()
            .map(|(m, t)| (m.to_string(), t.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[tokio::test]
    async fn probes_and_classifies_new_models() {
        let prober = FakeProber {
            outcomes: HashMap::from([
                ("a/good".into(), usable(true, true, Some(0.001))),
                ("b/bad".into(), outcome(Verdict::Incompatible)),
                ("c/flat".into(), outcome(Verdict::Unusable)),
            ]),
            tags: tag(&[("a/good", &["xai/fp8"])]),
            diarization: HashMap::from([("a/good".into(), (Some(true), Some(0.002)))]),
            ..Default::default()
        };
        let models = [
            listed("c/flat", Some(0.0001)),
            listed("a/good", Some(0.0001)),
            listed("b/bad", Some(0.0001)),
        ];
        let (new, report) = probe_all(&models, &list(vec![]), &policy(), &prober)
            .await
            .unwrap();
        assert_eq!(new.schema, 1);
        assert_eq!(new.generated_at, "2030-05-06");
        let ids: Vec<_> = new.models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["a/good", "b/bad", "c/flat"]);
        let good = &new.models[0];
        assert_eq!(good.provider, "openrouter");
        assert_eq!(good.status, CompatStatus::Compatible);
        assert!(good.words && good.fine_segments);
        assert_eq!(good.diarization, Some(json!({"xai": {"diarize": true}})));
        assert!((good.usd_per_hour.unwrap() - 0.36).abs() < 1e-9);
        assert_eq!(good.checked_at.as_deref(), Some("2030-05-06"));
        assert_eq!(new.models[1].status, CompatStatus::Incompatible);
        assert_eq!(new.models[2].status, CompatStatus::Unusable);
        assert!(!new.models[2].words && new.models[2].diarization.is_none());
        assert_eq!(report.probed, ["a/good", "b/bad", "c/flat"]);
        assert!(report.kept.is_empty() && !report.budget_exhausted);
        assert!((report.spent_usd - (0.001 + 0.002 + 0.003 + 0.003)).abs() < 1e-9);
        assert_eq!(
            prober.calls(),
            [
                "probe a/good",
                "tags a/good",
                r#"diarize a/good {"xai":{"diarize":true}}"#,
                "probe b/bad",
                "probe c/flat"
            ]
        );
    }

    #[tokio::test]
    async fn skips_models_above_price_or_without_price() {
        let prober = FakeProber::default();
        let previous = list(vec![
            entry("openrouter", "a/dear", CompatStatus::Compatible),
            entry("openrouter", "b/tokens", CompatStatus::Unusable),
        ]);
        let models = [
            listed("a/dear", Some(0.002)),
            listed("b/tokens", None),
            listed("c/new", Some(0.01)),
        ];
        let (new, report) = probe_all(&models, &previous, &policy(), &prober)
            .await
            .unwrap();
        assert!(prober.calls().is_empty());
        assert_eq!(new.models, previous.models);
        assert_eq!(
            report.kept,
            [
                ("a/dear".to_string(), "price".to_string()),
                ("b/tokens".to_string(), "price".to_string()),
                ("c/new".to_string(), "price".to_string())
            ]
        );
        assert!(report.probed.is_empty() && report.spent_usd == 0.0);
    }

    #[tokio::test]
    async fn models_outside_only_keep_their_entry() {
        let prober = FakeProber {
            outcomes: HashMap::from([("b/two".into(), outcome(Verdict::Unusable))]),
            ..Default::default()
        };
        let previous = list(vec![entry("openrouter", "a/one", CompatStatus::Compatible)]);
        let models = [listed("a/one", Some(0.0001)), listed("b/two", Some(0.0001))];
        let policy = ProbePolicy {
            only: vec!["b/two".into()],
            ..policy()
        };
        let (new, report) = probe_all(&models, &previous, &policy, &prober)
            .await
            .unwrap();
        assert_eq!(prober.calls(), ["probe b/two"]);
        assert_eq!(new.models[0], previous.models[0]);
        assert_eq!(new.models[1].status, CompatStatus::Unusable);
        assert_eq!(
            report.kept,
            [("a/one".to_string(), "not selected".to_string())]
        );
    }

    #[tokio::test]
    async fn budget_exhaustion_keeps_previous_entries() {
        let flat = outcome(Verdict::Usable {
            words: false,
            fine_segments: true,
        });
        let prober = FakeProber {
            outcomes: HashMap::from([("a/1".into(), flat)]),
            ..Default::default()
        };
        let previous = list(vec![
            entry("openrouter", "a/1", CompatStatus::Compatible),
            entry("openrouter", "b/2", CompatStatus::Compatible),
            entry("openrouter", "c/3", CompatStatus::Compatible),
        ]);
        let models = [
            listed("a/1", Some(0.00005)),
            listed("b/2", Some(0.00005)),
            listed("c/3", Some(0.00005)),
        ];
        let policy = ProbePolicy {
            budget_usd: 0.002,
            ..policy()
        };
        let (new, report) = probe_all(&models, &previous, &policy, &prober)
            .await
            .unwrap();
        assert_eq!(prober.calls(), ["probe a/1"]);
        assert!(report.budget_exhausted);
        assert_eq!(report.probed, ["a/1"]);
        assert_eq!(report.kept.len(), 2);
        assert!(report.kept.iter().all(|(_, why)| why == "budget"));
        assert!((report.spent_usd - 0.0015).abs() < 1e-9);
        assert_eq!(new.models[1], previous.models[1]);
        assert_eq!(new.models[2], previous.models[2]);
        assert_eq!(new.models[0].checked_at.as_deref(), Some("2030-05-06"));
    }

    #[tokio::test]
    async fn negative_or_non_finite_cost_counts_as_the_estimate() {
        let weird = |cost| ProbeOutcome {
            verdict: Verdict::Unusable,
            cost_usd: Some(cost),
        };
        let prober = FakeProber {
            outcomes: HashMap::from([("a/1".into(), weird(-5.0)), ("b/2".into(), weird(f64::NAN))]),
            ..Default::default()
        };
        let models = [listed("a/1", Some(0.0001)), listed("b/2", Some(0.0001))];
        let (_, report) = probe_all(&models, &list(vec![]), &policy(), &prober)
            .await
            .unwrap();
        assert!((report.spent_usd - 0.006).abs() < 1e-9);
    }

    #[tokio::test]
    async fn transient_failure_keeps_previous_entry() {
        let prober = FakeProber {
            outcomes: HashMap::from([(
                "a/1".into(),
                outcome(Verdict::Inconclusive("HTTP 503".into())),
            )]),
            ..Default::default()
        };
        let previous = list(vec![entry("openrouter", "a/1", CompatStatus::Compatible)]);
        let (new, report) = probe_all(
            &[listed("a/1", Some(0.0001))],
            &previous,
            &policy(),
            &prober,
        )
        .await
        .unwrap();
        assert_eq!(new.models, previous.models);
        assert_eq!(new.models[0].checked_at.as_deref(), Some("2020-01-01"));
        assert_eq!(report.probed, ["a/1"]);
        assert_eq!(report.kept, [("a/1".to_string(), "HTTP 503".to_string())]);
        assert!((report.spent_usd - 0.003).abs() < 1e-9);
    }

    #[tokio::test]
    async fn auth_failure_aborts() {
        let prober = FakeProber {
            auth_failures: vec!["a/1".into()],
            ..Default::default()
        };
        let err = probe_all(
            &[listed("a/1", Some(0.0001))],
            &list(vec![]),
            &policy(),
            &prober,
        )
        .await
        .unwrap_err();
        assert!(matches!(
            err,
            SrttyError::Transcribe(TranscribeError::Auth { .. })
        ));
        assert_eq!(err.exit_code(), 2);
    }

    #[tokio::test]
    async fn diarization_needs_a_single_endpoint() {
        let prober = FakeProber {
            outcomes: HashMap::from([("a/1".into(), usable(true, true, None))]),
            tags: tag(&[("a/1", &["xai/fp8", "other"])]),
            ..Default::default()
        };
        let mut old = entry("openrouter", "a/1", CompatStatus::Compatible);
        old.diarization = Some(json!({"xai": {"diarize": true}}));
        let previous = list(vec![old.clone()]);
        let (new, _) = probe_all(
            &[listed("a/1", Some(0.0001))],
            &previous,
            &policy(),
            &prober,
        )
        .await
        .unwrap();
        assert_eq!(prober.calls(), ["probe a/1", "tags a/1"]);
        assert_eq!(new.models[0].diarization, old.diarization);
        assert_eq!(new.models[0].checked_at.as_deref(), Some("2030-05-06"));
    }

    #[tokio::test]
    async fn diarization_results_and_failures() {
        let mut old = entry("openrouter", "keep/me", CompatStatus::Compatible);
        old.diarization = Some(json!({"p": {"diarize": true}}));
        let prober = FakeProber {
            outcomes: HashMap::from([
                ("no/voices".into(), usable(true, false, None)),
                ("keep/me".into(), usable(true, false, None)),
                ("no/tags".into(), usable(true, false, None)),
                ("no/words".into(), usable(false, true, None)),
            ]),
            tags: tag(&[("no/voices", &["p"]), ("keep/me", &["p"])]),
            diarization: HashMap::from([
                ("no/voices".into(), (Some(false), None)),
                ("keep/me".into(), (None, None)),
            ]),
            ..Default::default()
        };
        let mut no_voices = old.clone();
        no_voices.id = "no/voices".into();
        let previous = list(vec![old.clone(), no_voices]);
        let models = [
            listed("keep/me", Some(0.0001)),
            listed("no/tags", Some(0.0001)),
            listed("no/voices", Some(0.0001)),
            listed("no/words", Some(0.0001)),
        ];
        let (new, _) = probe_all(&models, &previous, &policy(), &prober)
            .await
            .unwrap();
        let by_id = |id: &str| new.models.iter().find(|m| m.id == id).unwrap();
        assert_eq!(by_id("keep/me").diarization, old.diarization);
        assert_eq!(by_id("no/voices").diarization, None);
        assert_eq!(by_id("no/tags").diarization, None);
        assert!(
            !prober
                .calls()
                .iter()
                .any(|c| c.contains("no/words") && c != "probe no/words")
        );
    }

    #[tokio::test]
    async fn diarization_over_budget_stops_the_run() {
        let prober = FakeProber {
            outcomes: HashMap::from([
                ("a/1".into(), usable(true, true, Some(0.001))),
                ("b/2".into(), usable(true, true, None)),
            ]),
            tags: tag(&[("a/1", &["p"])]),
            ..Default::default()
        };
        let previous = list(vec![entry("openrouter", "b/2", CompatStatus::Unusable)]);
        let models = [listed("a/1", Some(0.0001)), listed("b/2", Some(0.0001))];
        // The probe fits (0.003), the 45 s diarization (0.0045) does not.
        let policy = ProbePolicy {
            budget_usd: 0.005,
            ..policy()
        };
        let (new, report) = probe_all(&models, &previous, &policy, &prober)
            .await
            .unwrap();
        assert_eq!(prober.calls(), ["probe a/1", "tags a/1"]);
        assert!(report.budget_exhausted);
        assert_eq!(new.models[0].status, CompatStatus::Compatible);
        assert_eq!(new.models[0].diarization, None);
        assert_eq!(new.models[1], previous.models[0]);
        assert_eq!(report.kept, [("b/2".to_string(), "budget".to_string())]);
    }

    #[tokio::test]
    async fn hand_maintained_and_delisted_entries() {
        let prober = FakeProber::default();
        let previous = list(vec![
            entry("openai", "whisper-1", CompatStatus::Compatible),
            entry("groq", "whisper-large-v3", CompatStatus::Compatible),
            entry("openrouter", "gone/model", CompatStatus::Compatible),
        ]);
        let (new, _) = probe_all(&[], &previous, &policy(), &prober).await.unwrap();
        let ids: Vec<_> = new
            .models
            .iter()
            .map(|m| (m.provider.as_str(), m.id.as_str()))
            .collect();
        assert_eq!(ids, [("groq", "whisper-large-v3"), ("openai", "whisper-1")]);
    }

    #[test]
    fn same_content_ignores_dates() {
        let a = list(vec![entry("openrouter", "m", CompatStatus::Compatible)]);
        let mut b = a.clone();
        b.generated_at = "2031-01-01".into();
        b.models[0].checked_at = Some("2031-01-01".into());
        assert!(same_content(&a, &b));
        b.models[0].checked_at = None;
        assert!(same_content(&a, &b));
        b.models[0].status = CompatStatus::Unusable;
        assert!(!same_content(&a, &b));
        let mut c = a.clone();
        c.models.pop();
        assert!(!same_content(&a, &c));
    }
}
