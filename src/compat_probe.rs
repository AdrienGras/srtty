//! `srtty compat probe`: regenerates `compat.json` by probing the OpenRouter
//! transcription models on the embedded samples, within a price cap and a
//! budget. Run weekly in CI; a model that cannot be judged keeps its entry.

use std::collections::HashMap;
use std::io::Write;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::cli::ProbeArgs;
use crate::compat::{
    COMPAT_SCHEMA, CompatEntry, CompatList, CompatStatus, FailedProbe, parse_compat, utc_today,
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
    /// Models whose conclusive probe this run found unusable or incompatible,
    /// whether they were demoted or put on probation.
    pub failed: Vec<String>,
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
    valid_cost(reported).unwrap_or(estimate)
}

/// A reported cost, if it is a usable amount.
fn valid_cost(reported: Option<f64>) -> Option<f64> {
    reported.filter(|c| c.is_finite() && *c >= 0.0)
}

/// Keeps the previous entry of `model`, if there was one, at the price the
/// listing states now, and says why.
fn keep(
    models: &mut Vec<CompatEntry>,
    report: &mut ProbeReport,
    old: Option<&CompatEntry>,
    model: &ListedModel,
    why: &str,
) {
    models.extend(old.cloned().map(|e| CompatEntry {
        usd_per_hour: per_hour(model.usd_per_s),
        ..e
    }));
    report.kept.push((model.id.clone(), why.to_string()));
}

/// Hourly price for a listed per-second price, rounded to 6 decimals so the
/// file holds clean values (0.1, not 0.10000000000008) that read back equal;
/// `None` when the listing has no usable one (token-priced, negative or not
/// finite).
fn per_hour(usd_per_s: Option<f64>) -> Option<f64> {
    usd_per_s
        .filter(|p| p.is_finite() && *p >= 0.0)
        .map(|p| (p * 3600.0 * 1e6).round() / 1e6)
}

/// Fewest previously compatible models, probed conclusively, for the
/// mass-demotion guard to apply.
const DEMOTION_MIN_MODELS: usize = 4;

/// Refuses a run in which more than half of the previously compatible models
/// probed conclusively failed their probe, demoted or put on probation: a
/// broken endpoint or sample is likelier than a mass regression.
fn check_demotions(previous: &CompatList, report: &ProbeReport) -> Result<(), SrttyError> {
    let conclusive = |id: &str| {
        report.probed.iter().any(|p| p == id) && !report.kept.iter().any(|(k, _)| k == id)
    };
    let checked: Vec<&str> = previous
        .models
        .iter()
        .filter(|e| e.provider == PROVIDER && e.status == CompatStatus::Compatible)
        .map(|e| e.id.as_str())
        .filter(|id| conclusive(id))
        .collect();
    let failed = checked
        .iter()
        .filter(|id| report.failed.iter().any(|f| f == *id))
        .count();
    let total = checked.len();
    if total >= DEMOTION_MIN_MODELS && failed * 2 > total {
        return Err(SrttyError::Network(format!(
            "{failed} of {total} compatible models failed this run; compat.json left unchanged"
        )));
    }
    Ok(())
}

/// A warning for each `--only` id the listing does not have.
fn unlisted_only(only: &[String], listed: &[ListedModel]) -> Vec<String> {
    only.iter()
        .filter(|id| !listed.iter().any(|m| &m.id == *id))
        .map(|id| format!("{id}: not in the OpenRouter listing"))
        .collect()
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
            keep(&mut models, &mut report, old, model, "budget");
            continue;
        }
        if !policy.only.is_empty() && !policy.only.iter().any(|o| o == id) {
            keep(&mut models, &mut report, old, model, "not selected");
            continue;
        }
        let Some(rate) = model
            .usd_per_s
            .filter(|p| p.is_finite() && *p >= 0.0 && *p <= policy.max_usd_per_s)
        else {
            keep(&mut models, &mut report, old, model, "price");
            continue;
        };
        let estimate = rate * SAMPLE_S;
        if report.spent_usd + estimate > policy.budget_usd {
            report.budget_exhausted = true;
            keep(&mut models, &mut report, old, model, "budget");
            continue;
        }

        report.probed.push(id.to_string());
        let outcome = prober.probe(id).await.map_err(SrttyError::Transcribe)?;
        report.spent_usd += charged(outcome.cost_usd, estimate);
        let (status, words, fine_segments) = match outcome.verdict {
            Verdict::Inconclusive(why) => {
                keep(&mut models, &mut report, old, model, &why);
                continue;
            }
            Verdict::Usable {
                words,
                fine_segments,
            } => (CompatStatus::Compatible, words, fine_segments),
            Verdict::Unusable => (CompatStatus::Unusable, false, false),
            Verdict::Incompatible => (CompatStatus::Incompatible, false, false),
        };
        if status != CompatStatus::Compatible {
            report.failed.push(id.to_string());
            // A compatible model's first failure only puts it on probation.
            if let Some(old) =
                old.filter(|e| e.status == CompatStatus::Compatible && e.failed_probe.is_none())
            {
                models.push(CompatEntry {
                    usd_per_hour: per_hour(Some(rate)),
                    checked_at: Some(policy.today.clone()),
                    failed_probe: Some(FailedProbe {
                        status,
                        checked_at: policy.today.clone(),
                    }),
                    ..old.clone()
                });
                continue;
            }
        }

        let mut diarization = None;
        if words {
            diarization = old.and_then(|e| e.diarization.clone());
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
            usd_per_hour: per_hour(Some(rate)),
            checked_at: Some(policy.today.clone()),
            failed_probe: None,
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
    /// Per model: the cost the API reported, and the seconds of audio sent
    /// without a reported cost (billed at the estimate).
    costs: Mutex<HashMap<String, (f64, f64)>>,
}

impl LiveProber {
    fn transcriber(&self, model: &str) -> Result<OpenAiCompatible, TranscribeError> {
        OpenAiCompatible::new(
            self.base_url.clone(),
            self.key.clone(),
            model.to_string(),
            RequestMode::Multipart,
        )
    }

    fn add_cost(&self, model: &str, cost: Option<f64>, sample_s: f64) {
        let mut costs = self.costs.lock().unwrap_or_else(|p| p.into_inner());
        let entry = costs.entry(model.to_string()).or_default();
        match valid_cost(cost) {
            Some(usd) => entry.0 += usd,
            None => entry.1 += sample_s,
        }
    }

    /// What `model` cost, and whether part of it is an estimate. Matches what
    /// `probe_all` counted in `spent_usd`.
    fn cost_of(&self, model: &str, usd_per_s: Option<f64>) -> Option<(f64, bool)> {
        let costs = self.costs.lock().unwrap_or_else(|p| p.into_inner());
        let (reported, unreported_s) = costs.get(model).copied()?;
        let estimated = unreported_s > 0.0;
        let estimate = usd_per_s.unwrap_or(0.0) * unreported_s;
        Some((reported + estimate, estimated))
    }
}

#[async_trait]
impl Prober for LiveProber {
    async fn probe(&self, model: &str) -> Result<ProbeOutcome, TranscribeError> {
        let t = self.transcriber(model)?;
        let outcome = probe_model(&t, &self.sample.chunk, &self.reporter).await?;
        self.add_cost(model, outcome.cost_usd, SAMPLE_S);
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
        let t = self.transcriber(model)?;
        let result =
            probe_diarization(&t, &self.speakers_sample.chunk, option, &self.reporter).await?;
        self.add_cost(model, result.1, SPEAKERS_SAMPLE_S);
        Ok(result)
    }
}

/// How `status` is written in `compat.json` and in the command's output.
fn status_name(status: CompatStatus) -> &'static str {
    match status {
        CompatStatus::Compatible => "compatible",
        CompatStatus::Unusable => "unusable",
        CompatStatus::Incompatible => "incompatible",
    }
}

/// One stderr line for `id`: its new status or why it was kept.
fn model_line(
    id: &str,
    new: &CompatList,
    report: &ProbeReport,
    cost: Option<(f64, bool)>,
) -> String {
    let outcome = match report.kept.iter().find(|(kept, _)| kept == id) {
        Some((_, why)) if new.find(PROVIDER, id).is_some() => format!("kept ({why})"),
        Some((_, why)) => format!("skipped ({why})"),
        None => {
            let entry = new.find(PROVIDER, id);
            match entry.and_then(|e| e.failed_probe.as_ref()) {
                Some(failed) => {
                    format!("compatible, on probation ({})", status_name(failed.status))
                }
                None => {
                    let status = entry.map_or("kept (no entry)", |e| status_name(e.status));
                    let speakers = entry.is_some_and(|e| e.diarization.is_some());
                    format!("{status}{}", if speakers { ", speakers" } else { "" })
                }
            }
        }
    };
    match cost {
        Some((usd, estimated)) => {
            let tilde = if estimated { "~" } else { "" };
            format!("{id}: {outcome}, {tilde}${usd:.4}")
        }
        None => format!("{id}: {outcome}"),
    }
}

/// Rejects limits a run cannot work with, before any request is made.
fn check_limits(max_usd_per_s: f64, budget_usd: f64) -> Result<(), SrttyError> {
    for (flag, value) in [
        ("--max-usd-per-s", max_usd_per_s),
        ("--budget-usd", budget_usd),
    ] {
        if !value.is_finite() || value < 0.0 {
            return Err(SrttyError::Usage(format!(
                "{flag} must be a finite, non-negative amount in USD"
            )));
        }
    }
    Ok(())
}

pub async fn run(args: &ProbeArgs) -> Result<(), SrttyError> {
    check_limits(args.max_usd_per_s, args.budget_usd)?;
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
    if listed.is_empty() {
        // An empty listing would delist every OpenRouter entry.
        return Err(SrttyError::Network(
            "the OpenRouter model listing is empty; compat.json left unchanged".into(),
        ));
    }
    for warning in unlisted_only(&args.only, &listed) {
        writeln!(std::io::stderr(), "{warning}")?;
    }
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
        let rate = listed.iter().find(|m| m.id == id).and_then(|m| m.usd_per_s);
        let cost = prober.cost_of(id, rate);
        writeln!(err, "{}", model_line(id, &new, &report, cost))?;
    }
    writeln!(
        err,
        "spent ${:.4} of ${:.2}",
        report.spent_usd, policy.budget_usd
    )?;
    check_demotions(&previous, &report)?;
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
            failed_probe: None,
        }
    }

    /// `e` at the hourly price of `usd_per_s`, as a kept entry is written.
    fn repriced(e: &CompatEntry, usd_per_s: Option<f64>) -> CompatEntry {
        CompatEntry {
            usd_per_hour: per_hour(usd_per_s),
            ..e.clone()
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
    async fn written_prices_survive_a_file_round_trip() {
        let prober = FakeProber {
            outcomes: HashMap::from([
                ("a/grok".into(), outcome(Verdict::Incompatible)),
                ("b/mid".into(), outcome(Verdict::Incompatible)),
                ("c/cheap".into(), outcome(Verdict::Incompatible)),
            ]),
            ..Default::default()
        };
        let models = [
            listed("a/grok", Some(0.0000277777777778)),
            listed("b/mid", Some(0.000266666666667)),
            listed("c/cheap", Some(0.00000333)),
        ];
        let (new, _) = probe_all(&models, &list(vec![]), &policy(), &prober)
            .await
            .unwrap();
        assert_eq!(new.models[0].usd_per_hour, Some(0.1));
        assert_eq!(new.models[1].usd_per_hour, Some(0.96));
        assert_eq!(new.models[2].usd_per_hour, Some(0.011988));
        // Serialized exactly as `run` writes it, then read back.
        let mut text = serde_json::to_string_pretty(&new).unwrap();
        text.push('\n');
        let parsed = parse_compat(&text).unwrap();
        assert!(same_content(&parsed, &new));
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
        assert_eq!(
            new.models,
            [
                repriced(&previous.models[0], Some(0.002)),
                repriced(&previous.models[1], None)
            ]
        );
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
        assert_eq!(new.models[0], repriced(&previous.models[0], Some(0.0001)));
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
        assert_eq!(new.models[1], repriced(&previous.models[1], Some(0.00005)));
        assert_eq!(new.models[2], repriced(&previous.models[2], Some(0.00005)));
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
        assert_eq!(new.models, [repriced(&previous.models[0], Some(0.0001))]);
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
        let mut no_tags = old.clone();
        no_tags.id = "no/tags".into();
        let previous = list(vec![old.clone(), no_voices, no_tags]);
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
        // endpoint_tags failed: the previous value stays.
        assert_eq!(by_id("no/tags").diarization, old.diarization);
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
        let mut old_a = entry("openrouter", "a/1", CompatStatus::Compatible);
        old_a.diarization = Some(json!({"p": {"diarize": true}}));
        let previous = list(vec![
            old_a.clone(),
            entry("openrouter", "b/2", CompatStatus::Unusable),
        ]);
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
        assert_eq!(new.models[0].diarization, old_a.diarization);
        assert_eq!(new.models[1], repriced(&previous.models[1], Some(0.0001)));
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

    #[tokio::test]
    async fn diarization_is_dropped_when_words_are_gone() {
        let mut old_unusable = entry("openrouter", "a/unusable", CompatStatus::Compatible);
        old_unusable.diarization = Some(json!({"p": {"diarize": true}}));
        // Already on probation: this failure demotes.
        old_unusable.failed_probe = on_probation(CompatStatus::Unusable);
        let mut old_flat = old_unusable.clone();
        old_flat.id = "b/flat".into();
        let mut old_incompatible = old_unusable.clone();
        old_incompatible.id = "c/incompatible".into();
        let prober = FakeProber {
            outcomes: HashMap::from([
                ("a/unusable".into(), outcome(Verdict::Unusable)),
                ("b/flat".into(), usable(false, true, None)),
                ("c/incompatible".into(), outcome(Verdict::Incompatible)),
            ]),
            ..Default::default()
        };
        let previous = list(vec![old_unusable, old_flat, old_incompatible]);
        let models = [
            listed("a/unusable", Some(0.0001)),
            listed("b/flat", Some(0.0001)),
            listed("c/incompatible", Some(0.0001)),
        ];
        let (new, _) = probe_all(&models, &previous, &policy(), &prober)
            .await
            .unwrap();
        assert_eq!(new.models.len(), 3);
        assert!(new.models.iter().all(|m| m.diarization.is_none()));
        assert!(
            new.models
                .iter()
                .all(|m| m.checked_at.as_deref() == Some("2030-05-06"))
        );
    }

    #[test]
    fn limits_must_be_finite_and_non_negative() {
        assert!(check_limits(0.001, 0.5).is_ok());
        assert!(check_limits(0.0, 0.0).is_ok());
        for (max, budget) in [
            (f64::NAN, 0.5),
            (0.001, f64::NAN),
            (f64::INFINITY, 0.5),
            (0.001, f64::INFINITY),
            (-0.001, 0.5),
            (0.001, -1.0),
        ] {
            let err = check_limits(max, budget).unwrap_err();
            assert!(matches!(err, SrttyError::Usage(_)), "{max} {budget}");
            assert_eq!(err.exit_code(), 1);
        }
    }

    #[test]
    fn model_line_marks_estimated_costs() {
        let new = list(vec![]);
        let report = ProbeReport::default();
        assert_eq!(
            model_line("m", &new, &report, Some((0.0015, true))),
            "m: kept (no entry), ~$0.0015"
        );
        assert_eq!(
            model_line("m", &new, &report, Some((0.0015, false))),
            "m: kept (no entry), $0.0015"
        );
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

    #[tokio::test]
    async fn kept_entries_take_the_current_price() {
        let prober = FakeProber {
            outcomes: HashMap::from([(
                "d/inconclusive".into(),
                outcome(Verdict::Inconclusive("HTTP 503".into())),
            )]),
            ..Default::default()
        };
        let ids = [
            "a/dear",
            "b/tokens",
            "c/other",
            "d/inconclusive",
            "e/budget",
        ];
        let previous = list(
            ids.iter()
                .map(|id| entry("openrouter", id, CompatStatus::Compatible))
                .collect(),
        );
        let models = [
            listed("a/dear", Some(0.002)),
            listed("b/tokens", None),
            listed("c/other", Some(0.0001)),
            listed("d/inconclusive", Some(0.0001)),
            listed("e/budget", Some(0.0001)),
        ];
        let policy = ProbePolicy {
            only: vec![
                "a/dear".into(),
                "b/tokens".into(),
                "d/inconclusive".into(),
                "e/budget".into(),
            ],
            budget_usd: 0.004,
            ..policy()
        };
        let (new, report) = probe_all(&models, &previous, &policy, &prober)
            .await
            .unwrap();
        let reasons: Vec<&str> = report.kept.iter().map(|(_, why)| why.as_str()).collect();
        assert_eq!(
            reasons,
            ["price", "price", "not selected", "HTTP 503", "budget"]
        );
        let prices: Vec<Option<f64>> = new.models.iter().map(|m| m.usd_per_hour).collect();
        assert_eq!(
            prices,
            [Some(7.2), None, Some(0.36), Some(0.36), Some(0.36)]
        );
        for (new, old) in new.models.iter().zip(&previous.models) {
            assert_eq!(
                CompatEntry {
                    usd_per_hour: old.usd_per_hour,
                    ..new.clone()
                },
                *old
            );
        }
    }

    /// A run where the `failed` first of `compatible` previously compatible
    /// models failed their probe, and the others passed.
    fn demotions(compatible: usize, failed: usize) -> (CompatList, ProbeReport) {
        let ids: Vec<String> = (0..compatible).map(|i| format!("m/{i}")).collect();
        let previous = list(
            ids.iter()
                .map(|id| entry("openrouter", id, CompatStatus::Compatible))
                .collect(),
        );
        let report = ProbeReport {
            failed: ids[..failed].to_vec(),
            probed: ids,
            ..Default::default()
        };
        (previous, report)
    }

    #[test]
    fn mass_demotion_leaves_the_list_alone() {
        let (previous, report) = demotions(4, 3);
        let err = check_demotions(&previous, &report).unwrap_err();
        assert!(matches!(err, SrttyError::Network(_)), "{err}");
        assert_eq!(
            err.to_string(),
            "3 of 4 compatible models failed this run; compat.json left unchanged"
        );
        assert_eq!(err.exit_code(), 2);

        // Half is not more than half.
        let (previous, report) = demotions(4, 2);
        assert!(check_demotions(&previous, &report).is_ok());
        // Fewer than 4 conclusive probes of compatible models.
        let (previous, report) = demotions(3, 3);
        assert!(check_demotions(&previous, &report).is_ok());
    }

    #[test]
    fn mass_demotion_counts_only_conclusive_probes_of_compatible_models() {
        // 5 failures of 6, but one was inconclusive (kept) and one was not
        // probed: 3 of 4 counted.
        let (previous, mut report) = demotions(6, 5);
        report.kept.push(("m/0".into(), "HTTP 503".into()));
        report.probed.retain(|id| id != "m/1");
        let err = check_demotions(&previous, &report).unwrap_err();
        assert_eq!(
            err.to_string(),
            "3 of 4 compatible models failed this run; compat.json left unchanged"
        );
        // Previously unusable models do not count.
        let (mut previous, report) = demotions(4, 3);
        previous.models[0].status = CompatStatus::Unusable;
        assert!(check_demotions(&previous, &report).is_ok());
    }

    fn on_probation(status: CompatStatus) -> Option<FailedProbe> {
        Some(FailedProbe {
            status,
            checked_at: "2030-04-29".into(),
        })
    }

    #[tokio::test]
    async fn first_failure_puts_a_compatible_model_on_probation() {
        let prober = FakeProber {
            outcomes: HashMap::from([("a/x".into(), outcome(Verdict::Unusable))]),
            tags: tag(&[("a/x", &["xai/fp8"])]),
            diarization: HashMap::from([("a/x".into(), (Some(false), None))]),
            ..Default::default()
        };
        let mut old = entry("openrouter", "a/x", CompatStatus::Compatible);
        old.words = true;
        old.diarization = Some(json!({"xai": {"diarize": true}}));
        let previous = list(vec![old.clone()]);
        let (new, report) = probe_all(
            &[listed("a/x", Some(0.0001))],
            &previous,
            &policy(),
            &prober,
        )
        .await
        .unwrap();
        let e = &new.models[0];
        assert_eq!(e.status, CompatStatus::Compatible);
        assert!(e.words);
        assert_eq!(e.diarization, old.diarization);
        assert_eq!(e.usd_per_hour, Some(0.36));
        assert_eq!(e.checked_at.as_deref(), Some("2030-05-06"));
        assert_eq!(
            e.failed_probe,
            Some(FailedProbe {
                status: CompatStatus::Unusable,
                checked_at: "2030-05-06".into()
            })
        );
        assert_eq!(report.failed, ["a/x"]);
        assert!(report.kept.is_empty());
        assert!(!prober.calls().iter().any(|c| c.starts_with("diarize")));
        assert_eq!(
            model_line("a/x", &new, &report, None),
            "a/x: compatible, on probation (unusable)"
        );
    }

    #[tokio::test]
    async fn second_failure_in_a_row_demotes() {
        let prober = FakeProber {
            outcomes: HashMap::from([("a/x".into(), outcome(Verdict::Unusable))]),
            ..Default::default()
        };
        let mut old = entry("openrouter", "a/x", CompatStatus::Compatible);
        old.words = true;
        old.diarization = Some(json!({"xai": {"diarize": true}}));
        old.failed_probe = on_probation(CompatStatus::Incompatible);
        let (new, report) = probe_all(
            &[listed("a/x", Some(0.0001))],
            &list(vec![old]),
            &policy(),
            &prober,
        )
        .await
        .unwrap();
        let e = &new.models[0];
        assert_eq!(e.status, CompatStatus::Unusable);
        assert_eq!(e.failed_probe, None);
        assert!(!e.words && !e.fine_segments);
        assert_eq!(e.diarization, None);
        assert_eq!(e.checked_at.as_deref(), Some("2030-05-06"));
        assert_eq!(report.failed, ["a/x"]);
        assert_eq!(model_line("a/x", &new, &report, None), "a/x: unusable");
    }

    #[tokio::test]
    async fn usable_probe_clears_probation() {
        let prober = FakeProber {
            outcomes: HashMap::from([("a/x".into(), usable(true, true, None))]),
            ..Default::default()
        };
        let mut old = entry("openrouter", "a/x", CompatStatus::Compatible);
        old.failed_probe = on_probation(CompatStatus::Unusable);
        let (new, report) = probe_all(
            &[listed("a/x", Some(0.0001))],
            &list(vec![old]),
            &policy(),
            &prober,
        )
        .await
        .unwrap();
        assert_eq!(new.models[0].status, CompatStatus::Compatible);
        assert_eq!(new.models[0].failed_probe, None);
        assert!(report.failed.is_empty());
        assert_eq!(model_line("a/x", &new, &report, None), "a/x: compatible");
    }

    #[tokio::test]
    async fn inconclusive_probe_keeps_probation() {
        let prober = FakeProber {
            outcomes: HashMap::from([(
                "a/x".into(),
                outcome(Verdict::Inconclusive("timeout".into())),
            )]),
            ..Default::default()
        };
        let mut old = entry("openrouter", "a/x", CompatStatus::Compatible);
        old.failed_probe = on_probation(CompatStatus::Unusable);
        let (new, report) = probe_all(
            &[listed("a/x", Some(0.0001))],
            &list(vec![old.clone()]),
            &policy(),
            &prober,
        )
        .await
        .unwrap();
        assert_eq!(new.models, [repriced(&old, Some(0.0001))]);
        assert!(new.models[0].failed_probe.is_some());
        assert!(report.failed.is_empty());
        assert_eq!(report.kept, [("a/x".to_string(), "timeout".to_string())]);
    }

    #[tokio::test]
    async fn only_compatible_models_get_probation() {
        let prober = FakeProber {
            outcomes: HashMap::from([
                ("a/new".into(), outcome(Verdict::Unusable)),
                ("b/old".into(), outcome(Verdict::Incompatible)),
            ]),
            ..Default::default()
        };
        let previous = list(vec![entry("openrouter", "b/old", CompatStatus::Unusable)]);
        let models = [listed("a/new", Some(0.0001)), listed("b/old", Some(0.0001))];
        let (new, report) = probe_all(&models, &previous, &policy(), &prober)
            .await
            .unwrap();
        assert_eq!(new.models[0].status, CompatStatus::Unusable);
        assert_eq!(new.models[0].failed_probe, None);
        assert_eq!(new.models[1].status, CompatStatus::Incompatible);
        assert_eq!(new.models[1].failed_probe, None);
        assert_eq!(report.failed, ["a/new", "b/old"]);
    }

    #[tokio::test]
    async fn mass_guard_counts_first_failures() {
        let ids = ["m/0", "m/1", "m/2", "m/3"];
        let prober = FakeProber {
            outcomes: HashMap::from([
                ("m/0".into(), outcome(Verdict::Unusable)),
                ("m/1".into(), outcome(Verdict::Unusable)),
                ("m/2".into(), outcome(Verdict::Unusable)),
                ("m/3".into(), usable(false, true, None)),
            ]),
            ..Default::default()
        };
        let previous = list(
            ids.iter()
                .map(|id| {
                    let mut e = entry("openrouter", id, CompatStatus::Compatible);
                    e.fine_segments = true;
                    e
                })
                .collect(),
        );
        let models: Vec<ListedModel> = ids.iter().map(|id| listed(id, Some(0.0001))).collect();
        let (new, report) = probe_all(&models, &previous, &policy(), &prober)
            .await
            .unwrap();
        assert!(
            new.models
                .iter()
                .all(|m| m.status == CompatStatus::Compatible)
        );
        let err = check_demotions(&previous, &report).unwrap_err();
        assert_eq!(
            err.to_string(),
            "3 of 4 compatible models failed this run; compat.json left unchanged"
        );
    }

    #[test]
    fn model_line_says_skipped_without_a_previous_entry() {
        let report = ProbeReport {
            kept: vec![("m".into(), "price".into())],
            ..Default::default()
        };
        assert_eq!(
            model_line("m", &list(vec![]), &report, None),
            "m: skipped (price)"
        );
        let with_entry = list(vec![entry("openrouter", "m", CompatStatus::Compatible)]);
        assert_eq!(
            model_line("m", &with_entry, &report, None),
            "m: kept (price)"
        );
    }

    #[test]
    fn only_ids_missing_from_the_listing_are_reported() {
        let models = [listed("a/one", Some(0.0001))];
        assert_eq!(
            unlisted_only(&["a/one".into(), "z/gone".into()], &models),
            ["z/gone: not in the OpenRouter listing"]
        );
        assert!(unlisted_only(&[], &models).is_empty());
    }
}
