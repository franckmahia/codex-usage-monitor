use std::collections::BTreeMap;

use crate::model::{InferenceCall, ModelConfidence, TokenUsage};
use crate::pricing::{CostBreakdown, PricingEngine};

/// Agrégat mono-bucket : tokens + valorisation.
#[derive(Debug, Default, Clone)]
pub struct Bucket {
    pub calls: u64,
    pub usage: TokenUsage,
    pub equivalent_cost: f64,
    pub cost_without_cache: f64,
    pub cache_savings: f64,
    pub long_context_calls: u64,
    pub unknown_model_tokens: u64,
    pub unknown_model_calls: u64,
}

impl Bucket {
    pub fn cache_hit_percent(&self) -> Option<f64> {
        if self.usage.input_tokens > 0 {
            Some(self.usage.cached_input_tokens as f64 / self.usage.input_tokens as f64 * 100.0)
        } else {
            None
        }
    }

    pub fn cache_savings_percent(&self) -> Option<f64> {
        if self.cost_without_cache > 0.0 {
            Some(self.cache_savings / self.cost_without_cache * 100.0)
        } else {
            None
        }
    }

    pub fn add(&mut self, call: &InferenceCall, cd: Option<&CostBreakdown>) {
        self.calls += 1;
        self.usage.add(&call.usage);
        match cd {
            Some(cd) => {
                self.equivalent_cost += cd.equivalent_cost;
                self.cost_without_cache += cd.cost_without_cache;
                self.cache_savings += cd.cache_savings;
                if cd.long_context {
                    self.long_context_calls += 1;
                }
            }
            None => {
                self.unknown_model_tokens += call.usage.total_tokens;
                self.unknown_model_calls += 1;
            }
        }
    }
}

pub type Buckets = BTreeMap<String, Bucket>;

/// Somme des usages d'une liste d'appels.
pub fn usage_of(calls: &[InferenceCall]) -> TokenUsage {
    let mut t = TokenUsage::default();
    for c in calls {
        t.add(&c.usage);
    }
    t
}


/// Valorise un appel et l'ajoute au bucket (helper mutualise).
pub fn priced_bucket(bucket: &mut Bucket, call: &InferenceCall, engine: &PricingEngine) {
    let cd = engine.cost_codex(call);
    bucket.add(call, cd.model_known.then_some(&cd));
}


pub fn by_day(calls: &[InferenceCall], engine: &PricingEngine) -> Buckets {
    let mut out = Buckets::new();
    for c in calls {
        let mut b = out.entry(c.day().to_string()).or_default();
        priced_bucket(&mut b, c, engine);
    }
    out
}

pub fn by_model(calls: &[InferenceCall], engine: &PricingEngine) -> Buckets {
    let mut out = Buckets::new();
    for c in calls {
        let key = c.model_slug.clone().unwrap_or_else(|| "<unknown>".into());
        let mut b = out.entry(key).or_default();
        priced_bucket(&mut b, c, engine);
    }
    out
}

pub fn by_project(calls: &[InferenceCall], engine: &PricingEngine) -> Buckets {
    let mut out = Buckets::new();
    for c in calls {
        let key = c
            .project_path
            .as_deref()
            .map(project_name)
            .unwrap_or_else(|| "<unknown>".into());
        let mut b = out.entry(key).or_default();
        priced_bucket(&mut b, c, engine);
    }
    out
}

/// Nom de projet lisible : dernier composant du chemin.
pub fn project_name(path: &str) -> String {
    path.rsplit('/').find(|s| !s.is_empty()).unwrap_or(path).to_string()
}

/// Séparation par type d'activité sans double comptage : priorité à
/// l'enrichissement state db (main / subagent / auto_review / voice / other),
/// sinon heuristique sur le modèle (codex-auto-review). Les appels hors
/// threads connus restent "unknown" afin de ne jamais inventer.
pub fn by_activity(calls: &[InferenceCall], engine: &PricingEngine) -> Buckets {
    let mut out = Buckets::new();
    for c in calls {
        let key = match c.activity.as_deref() {
            Some(a) => a.to_string(),
            None => match c.model_slug.as_deref() {
                Some("codex-auto-review") => "auto_review".to_string(),
                Some(_) => "ordinary".to_string(),
                None => "unknown_model".to_string(),
            },
        };
        let mut b = out.entry(key).or_default();
        priced_bucket(&mut b, c, engine);
    }
    out
}

/// Par thread : ventilation sans double comptage (chaque call appartient
/// a exactement un thread_id).
pub fn by_thread(calls: &[InferenceCall], engine: &PricingEngine) -> Buckets {
    let mut out = Buckets::new();
    for c in calls {
        let key = c
            .thread_id
            .clone()
            .unwrap_or_else(|| "<unknown>".into());
        let mut b = out.entry(key).or_default();
        priced_bucket(&mut b, c, engine);
    }
    out
}

/// Anomalies detectees automatiquement (cahier des charges §19).
pub fn detect_anomalies(
    calls: &[InferenceCall],
    by_day: &Buckets,
    by_activity: &Buckets,
    long_context_calls: u64,
) -> Vec<String> {
    let mut anomalies = Vec::new();

    let total_input: u64 = calls.iter().map(|c| c.usage.input_tokens).sum();
    let auto_review = by_activity
        .get("auto_review")
        .map(|b| b.usage.input_tokens)
        .unwrap_or(0);
    if total_input > 0 && auto_review as f64 / total_input as f64 > 0.10 {
        anomalies.push(format!(
            "Auto Review = {:.1} % de l'usage total (> 10 %)",
            auto_review as f64 / total_input as f64 * 100.0
        ));
    }

    if long_context_calls > 0 {
        anomalies.push(format!("{long_context_calls} appel(s) au-dela de 272K input"));
    }

    let mut low_cache_threads = std::collections::BTreeMap::new();
    let mut thread_input: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    let mut thread_cached: std::collections::BTreeMap<String, u64> =
        std::collections::BTreeMap::new();
    for c in calls {
        if let Some(t) = &c.thread_id {
            *thread_input.entry(t.clone()).or_default() += c.usage.input_tokens;
            *thread_cached.entry(t.clone()).or_default() += c.usage.cached_input_tokens;
        }
    }
    for (t, inp) in &thread_input {
        let hit = *thread_cached.get(t).unwrap_or(&0);
        if *inp > 100_000 && (hit as f64) / (*inp as f64) < 0.70 {
            low_cache_threads.insert(t.clone(), hit as f64 / *inp as f64 * 100.0);
        }
    }
    for (t, pct) in low_cache_threads.iter().take(5) {
        anomalies.push(format!("Thread {t} : cache hit {pct:.1} % (< 70 %)"));
    }
    for (t, inp) in &thread_input {
        if *inp > 10_000_000 {
            anomalies.push(format!("Thread {t} : {inp} tokens d'input (> 10M)"));
        }
    }

    if let (Some(first), Some(last)) = (by_day.values().next(), by_day.values().last()) {
        let avg = by_day
            .values()
            .map(|b| b.usage.input_tokens)
            .sum::<u64>() as f64
            / by_day.len().max(1) as f64;
        for (day, b) in by_day {
            if avg > 0.0 && b.usage.input_tokens as f64 > avg * 2.0 {
                anomalies.push(format!(
                    "{day} : {:.1}x la moyenne quotidienne",
                    b.usage.input_tokens as f64 / avg
                ));
            }
        }
        let _ = (first, last);
    }

    anomalies
}

pub fn confidence_stats(calls: &[InferenceCall]) -> (u64, u64, u64) {
    let mut exact = 0;
    let mut inferred = 0;
    let mut unknown = 0;
    for c in calls {
        match c.model_confidence {
            ModelConfidence::Exact => exact += 1,
            ModelConfidence::Inferred => inferred += 1,
            ModelConfidence::Unknown => unknown += 1,
        }
    }
    (exact, inferred, unknown)
}
