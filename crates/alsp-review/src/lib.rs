//! Read-only review orchestration, mechanical evidence, proof filtering, and append-only verdicts.

mod agents;
mod export;
mod findings;
mod ledger;
mod report;

pub use agents::{compose_prompt, run_agents, Agent, AgentResult, AgentRun};
pub use findings::{
    allowed_locations, mechanical_findings, merge_agent_findings, merge_findings,
    normalize_severity, parse_agent_output, prove_agent_findings, Finding, ProofResult,
};
pub use ledger::{
    append_findings, import_gaps, last_review, ledger_review, list_findings, mark, read_rows,
    record_review, stats, LedgerRow, Ledgered, ReviewRecord,
};
pub use report::{
    cap_text, changed_symbols, changed_symbols_json, render_report, ChangedSymbol,
    CHANGED_SYMBOLS_JSON_CAP, CHANGED_SYMBOLS_TEXT_CAP,
};
