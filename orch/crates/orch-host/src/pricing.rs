//! 计价表：未命中一律 unknown，subscription 不产生增量金额。

use serde::Deserialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CostBasis {
    Known,
    Estimated,
    Subscription,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TokenUsage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostEstimate {
    pub usd: Option<f64>,
    pub basis: CostBasis,
}

#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    models: BTreeMap<String, PricingModel>,
}

#[derive(Debug, Clone, Deserialize)]
struct PricingDocument {
    #[serde(default)]
    models: BTreeMap<String, PricingModel>,
}

#[derive(Debug, Clone, Deserialize)]
struct PricingModel {
    billing: String,
    #[serde(rename = "inputPerMTok", default)]
    input_per_mtok: Option<f64>,
    #[serde(rename = "outputPerMTok", default)]
    output_per_mtok: Option<f64>,
    #[serde(rename = "cacheReadPerMTok", default)]
    cache_read_per_mtok: Option<f64>,
    #[serde(rename = "cacheWritePerMTok", default)]
    cache_write_per_mtok: Option<f64>,
}

impl PricingTable {
    pub fn from_yaml_str(text: &str) -> anyhow::Result<Self> {
        if text.trim().is_empty() {
            return Ok(Self::default());
        }
        let doc: PricingDocument = serde_yaml::from_str(text)?;
        Ok(Self { models: doc.models })
    }
}

pub fn estimate_cost(table: &PricingTable, model: &str, usage: &TokenUsage) -> CostEstimate {
    let Some(entry) = table.models.get(model) else {
        return CostEstimate {
            usd: None,
            basis: CostBasis::Unknown,
        };
    };
    if entry.billing.eq_ignore_ascii_case("subscription") {
        return CostEstimate {
            usd: None,
            basis: CostBasis::Subscription,
        };
    }
    if !entry.billing.eq_ignore_ascii_case("usage") {
        return CostEstimate {
            usd: None,
            basis: CostBasis::Unknown,
        };
    }
    let (Some(input), Some(output)) = (entry.input_per_mtok, entry.output_per_mtok) else {
        return CostEstimate {
            usd: None,
            basis: CostBasis::Unknown,
        };
    };
    let usd = (usage.input as f64 * input
        + usage.output as f64 * output
        + usage.cache_read as f64 * entry.cache_read_per_mtok.unwrap_or(0.0)
        + usage.cache_write as f64 * entry.cache_write_per_mtok.unwrap_or(0.0))
        / 1_000_000.0;
    CostEstimate {
        usd: Some(usd),
        basis: CostBasis::Known,
    }
}
