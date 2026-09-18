use serde::{Deserialize, Serialize};

use crate::model::{ModelConfidence, SourceFormat, TokenUsage};

#[derive(Debug, Deserialize)]
pub struct PricingCatalog {
    pub catalog: String,
    pub profile: String,
    pub version: String,
    pub currency: String,
    #[serde(default = "default_unit")]
    pub unit: String,
    pub source_url: String,
    pub last_verified: String,
    #[serde(default)]
    pub notes: Vec<String>,
    pub rules: Vec<PricingRule>,
}

fn default_unit() -> String {
    "per_1m_tokens".to_string()
}

#[derive(Debug, Deserialize, Serialize)]
pub struct PricingRule {
    pub model: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub effective_from: Option<String>,
    #[serde(default)]
    pub effective_until: Option<String>,
    pub input_rate: f64,
    pub cached_rate: f64,
    /// None = non facture dans ce profil (ex: ecritures de cache dans Codex).
    #[serde(default)]
    pub cache_write_rate: Option<f64>,
    pub output_rate: f64,
    #[serde(default)]
    pub long_context: bool,
    #[serde(default = "default_threshold")]
    pub long_context_threshold: u64,
    #[serde(default = "default_input_mult")]
    pub long_context_input_multiplier: f64,
    #[serde(default = "default_cached_mult")]
    pub long_context_cached_multiplier: f64,
    #[serde(default = "default_output_mult")]
    pub long_context_output_multiplier: f64,
    #[serde(default)]
    pub fast_multiplier: Option<f64>,
    #[serde(default)]
    pub regional_multiplier: Option<f64>,
}

fn default_threshold() -> u64 {
    272_000
}
fn default_input_mult() -> f64 {
    2.0
}
fn default_cached_mult() -> f64 {
    2.0
}
fn default_output_mult() -> f64 {
    1.5
}

/// Resultat de valorisation d'un appel. Le cout est toujours une ESTIMATION
/// (equivalent), jamais une facture (invariant 11).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostBreakdown {
    pub model_known: bool,
    pub long_context: bool,
    pub fast_applied: bool,
    pub equivalent_cost: f64,
    pub cost_without_cache: f64,
    pub cache_savings: f64,
}

impl CostBreakdown {
    pub fn cache_savings_percent(&self) -> Option<f64> {
        if self.cost_without_cache > 0.0 {
            Some(self.cache_savings / self.cost_without_cache * 100.0)
        } else {
            None
        }
    }
}

pub struct PricingEngine {
    pub codex: PricingCatalog,
    pub api: PricingCatalog,
}

impl PricingEngine {
    /// Charge les catalogues embarques (profile Codex + API).
    pub fn embedded() -> Self {
        PricingEngine {
            codex: serde_json::from_str(include_str!(
                "../pricing/openai-codex-2026-09-18.json"
            ))
            .expect("embedded codex pricing catalog is valid JSON"),
            api: serde_json::from_str(include_str!("../pricing/openai-api-2026-09-18.json"))
                .expect("embedded api pricing catalog is valid JSON"),
        }
    }

    pub fn load(path_codex: &str, path_api: Option<&str>) -> Result<Self, String> {
        let raw = std::fs::read_to_string(path_codex)
            .map_err(|e| format!("cannot read pricing catalog {path_codex}: {e}"))?;
        let codex: PricingCatalog =
            serde_json::from_str(&raw).map_err(|e| format!("invalid pricing catalog: {e}"))?;
        let api = match path_api {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .map_err(|e| format!("cannot read pricing catalog {p}: {e}"))?;
                serde_json::from_str::<PricingCatalog>(&raw)
                    .map_err(|e| format!("invalid pricing catalog: {e}"))?
            }
            None => {
                let raw = include_str!("../pricing/openai-api-2026-09-18.json");
                serde_json::from_str(raw).expect("embedded api pricing catalog is valid JSON")
            }
        };
        Ok(PricingEngine { codex, api })
    }

    /// Resolve une regle par slug ou alias, avec periode de validite.
    fn resolve<'a>(rules: &'a [PricingRule], model: &str, day: &str) -> Option<&'a PricingRule> {
        rules.iter().find(|r| {
            (r.model == model || r.aliases.iter().any(|a| a == model))
                && r.effective_from.as_deref().map_or(true, |from| day >= from)
                && r.effective_until.as_deref().map_or(true, |until| day <= until)
        })
    }

    /// Valorise un appel selon le profil voulu. Decision long context par
    /// appel (input > seuil), jamais sur des totaux agreges.
    pub fn cost(&self, profile: &PricingCatalog, call: &crate::model::InferenceCall) -> CostBreakdown {
        let day = call.day();
        let Some(rule) = Self::resolve(&profile.rules, call.model_slug.as_deref().unwrap_or(""), day)
        else {
            // Invariant 9 : jamais de prix pour un modele inconnu.
            return CostBreakdown::default();
        };
        let u = &call.usage;
        let long = rule.long_context && u.input_tokens > rule.long_context_threshold;
        let in_mult = if long { rule.long_context_input_multiplier } else { 1.0 };
        let cached_mult = if long { rule.long_context_cached_multiplier } else { 1.0 };
        let out_mult = if long { rule.long_context_output_multiplier } else { 1.0 };

        let ordinary = u.ordinary_input() as f64;
        let cached = u.cached_input_tokens as f64;
        let cache_write = u.cache_write_input_tokens as f64;
        let output = u.output_tokens as f64;

        // Service tier : fast applique explicitement (multiplier du catalogue),
        // regional reserve a une future source de donnees (jamais suppose).
        let fast_applied =
            call.service_tier == crate::model::ServiceTier::Fast && rule.fast_multiplier.is_some();
        let fast_mult = if fast_applied {
            rule.fast_multiplier.unwrap_or(1.0)
        } else {
            1.0
        };
        let regional_mult = rule.regional_multiplier.unwrap_or(1.0);

        let input_rate = rule.input_rate * in_mult * fast_mult * regional_mult;
        let cached_rate = rule.cached_rate * cached_mult * fast_mult * regional_mult;
        let output_rate = rule.output_rate * out_mult * fast_mult * regional_mult;

        // Ecritures de cache :
        // - profil Codex : non facturees separement, valorisees au tarif cached
        //   (hypothese documentee, jamais le multiplicateur API 1.25x).
        // - profil API : facturees au cache_write_rate (1.25x entree ordinaire).
        let cache_write_cost = if let Some(cw_rate) = rule.cache_write_rate {
            cache_write * cw_rate * in_mult
        } else {
            cache_write * cached_rate
        };

        // Catalogue exprime en USD par million de tokens.
        let per_m = 1_000_000.0;
        let equivalent_cost = (ordinary * input_rate
            + cached * cached_rate
            + cache_write_cost
            + output * output_rate)
            / per_m;

        let cost_without_cache =
            ((u.input_tokens as f64) * input_rate + output * output_rate) / per_m;

        CostBreakdown {
            model_known: true,
            long_context: long,
            fast_applied,
            equivalent_cost,
            cost_without_cache,
            cache_savings: cost_without_cache - equivalent_cost,
        }
    }

    pub fn cost_codex(&self, call: &crate::model::InferenceCall) -> CostBreakdown {
        self.cost(&self.codex, call)
    }

    pub fn cost_api(&self, call: &crate::model::InferenceCall) -> CostBreakdown {
        self.cost(&self.api, call)
    }
}

/// Cout "equivalent" du profil Codex ; None si le modele est inconnu
/// (invariant 9 : les tokens restent comptes, le cout est N/A).
pub fn priced(
    engine: &PricingEngine,
    call: &crate::model::InferenceCall,
) -> (Option<CostBreakdown>, ModelConfidence, SourceFormat) {
    let cd = engine.cost_codex(call);
    let priceable = cd.model_known;
    (
        if priceable { Some(cd) } else { None },
        call.model_confidence,
        call.source_format,
    )
}

/// Confiance tarifaire (cahier des charges §13) : part des appels valorises
/// avec un modele connu ET un service tier determine (pas de supposition
/// silencieuse du standard).
pub fn pricing_confidence(calls: &[crate::model::InferenceCall]) -> f64 {
    if calls.is_empty() {
        return 0.0;
    }
    let confident = calls
        .iter()
        .filter(|c| {
            c.model_slug.is_some() && c.service_tier != crate::model::ServiceTier::Unknown
        })
        .count();
    confident as f64 / calls.len() as f64 * 100.0
}

/// (fast, standard, unknown) sur une liste d'appels.
pub fn tier_stats(calls: &[crate::model::InferenceCall]) -> (u64, u64, u64) {
    let mut fast = 0;
    let mut standard = 0;
    let mut unknown = 0;
    for c in calls {
        match c.service_tier {
            crate::model::ServiceTier::Fast => fast += 1,
            crate::model::ServiceTier::Standard => standard += 1,
            crate::model::ServiceTier::Unknown => unknown += 1,
        }
    }
    (fast, standard, unknown)
}

/// Somme helper utilisee par les agregats.
pub fn usage_of(calls: &[crate::model::InferenceCall]) -> TokenUsage {
    let mut t = TokenUsage::default();
    for c in calls {
        t.add(&c.usage);
    }
    t
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(model: &str, input: u64) -> crate::model::InferenceCall {
        crate::model::InferenceCall {
            event_uid: format!("test-{model}-{input}"),
            response_id: None,
            timestamp_utc: Some("2026-09-15T12:00:00Z".into()),
            session_id: None,
            thread_id: None,
            turn_id: None,
            root_turn_id: None,
            model_slug: Some(model.into()),
            model_confidence: ModelConfidence::Exact,
            service_tier: crate::model::ServiceTier::Unknown,
            project_path: None,
            activity: None,
            parent_thread_id: None,
            thread_title: None,
            usage: TokenUsage {
                input_tokens: input,
                cached_input_tokens: 0,
                cache_write_input_tokens: 0,
                output_tokens: 1000,
                reasoning_output_tokens: 0,
                total_tokens: input + 1000,
            },
            source_format: SourceFormat::TokenUsageRecord,
            source_file: "test".into(),
            source_ordinal: 0,
            archived: false,
        }
    }

    #[test]
    fn long_context_boundary() {
        let engine = PricingEngine::embedded();
        let short = engine.cost_codex(&call("gpt-5.6-sol", 272_000));
        let long = engine.cost_codex(&call("gpt-5.6-sol", 272_001));
        assert!(!short.long_context);
        assert!(long.long_context);
    }

    #[test]
    fn astra_has_no_long_context_multiplier() {
        let engine = PricingEngine::embedded();
        let long = engine.cost_codex(&call("gpt-6-astra", 300_000));
        assert!(!long.long_context);
    }

    #[test]
    fn auto_review_alias_resolves_to_gpt54() {
        let engine = PricingEngine::embedded();
        let c = engine.cost_codex(&call("codex-auto-review", 1000));
        assert!(c.model_known);
    }

    #[test]
    fn unknown_model_never_priced() {
        let engine = PricingEngine::embedded();
        let c = engine.cost_codex(&call("totally-new-model", 1000));
        assert!(!c.model_known);
        assert_eq!(c.equivalent_cost, 0.0);
    }
}
