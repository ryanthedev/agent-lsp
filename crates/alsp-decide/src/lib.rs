//! Decision-model client: ask a model one yes/no question about many items
//! and get back P(yes) for each. Every call goes through sdd's `decide`
//! command, so sdd's budget sees the spend; alsp holds no API key.

mod error;
mod sdd;
#[cfg(feature = "testkit")]
pub mod stub;

pub use error::DecideError;
pub use sdd::{default_model, default_model_or_unavailable, SddDecider, Unavailable, TRUNCATION_MARKER};

/// Gemini Decisions list price, USD per million input tokens (output is free).
pub const PRICE_PER_M_INPUT_USD: f64 = 0.10;

/// Gaps with P(tests catch) below this are reported. Chosen from an offline
/// evaluation of the model on labelled mutants: roughly the smallest p at which
/// the model's "tests catch it" skip precision stayed at or above 0.90.
pub const DEFAULT_GAP_THRESHOLD: f64 = 0.50;

pub trait DecisionModel {
    /// One yes/no question per item; returns P(yes) in [0,1] or a per-item error.
    ///
    /// The result has one entry per item, in the same order. Per-item failures
    /// (a safety block, a malformed reply, an item sdd never answered) are
    /// recorded in that entry and do not abort the batch; only systematic
    /// failures return `Err`.
    fn score(&self, question: &Question, items: &[Item]) -> Result<Vec<ItemScore>, DecideError>;

    /// Run-level notes from the last `score` calls (e.g. a partial run), drained on read.
    fn take_notes(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Sent as a yes/no (`noul`) question.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub instructions: String,
    pub criteria: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub id: String,
    pub input: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemScore {
    pub id: String,
    pub p: Option<f64>,
    pub error: Option<String>,
    pub input_tokens: u64,
}

/// USD cost of `input_tokens` at list price.
pub fn cost_usd(input_tokens: u64) -> f64 {
    input_tokens as f64 / 1_000_000.0 * PRICE_PER_M_INPUT_USD
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cost_matches_list_price() {
        assert_eq!(cost_usd(0), 0.0);
        assert!((cost_usd(1_000_000) - 0.10).abs() < 1e-12);
        assert!((cost_usd(2_500_000) - 0.25).abs() < 1e-12);
    }

    #[test]
    #[allow(non_snake_case)]
    fn test_DW_1_4_threshold_in_unit_interval() {
        assert!(DEFAULT_GAP_THRESHOLD > 0.0 && DEFAULT_GAP_THRESHOLD < 1.0);
    }
}
