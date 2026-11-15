//! The evidence pack: what `alsp pack` returns and later phases extend.

use alsp_index::{IndexState, Span};
use serde::{Deserialize, Serialize};

/// How a fact was obtained.
/// - `Resolved`: from the semantic index, or from source syntax that leaves no doubt (a `#[test]`
///   attribute, `fn main` in a binary root).
/// - `Observed`: measured by running something (coverage).
/// - `Predicted`: scored by a model.
/// - `Heuristic`: inferred (pub-ness standing in for "reachable from outside", symbols removed in
///   the diff that the index of the head tree cannot know).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provenance {
    Resolved,
    Heuristic,
    Observed,
    Predicted,
}

impl Provenance {
    pub fn as_str(self) -> &'static str {
        match self {
            Provenance::Resolved => "resolved",
            Provenance::Heuristic => "heuristic",
            Provenance::Observed => "observed",
            Provenance::Predicted => "predicted",
        }
    }
}

/// `Removed` symbols exist only in the base tree: they are found by reading the base file's syntax,
/// so they are always heuristic, have no callers, and carry a synthetic `id` and base-side span.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Removed,
}

impl ChangeKind {
    pub fn letter(self) -> char {
        match self {
            ChangeKind::Added => 'A',
            ChangeKind::Modified => 'M',
            ChangeKind::Removed => 'R',
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Edge {
    /// SCIP symbol string of the other end.
    pub id: String,
    pub file: String,
    pub line: u32,
    pub provenance: Provenance,
}

/// Observed line and branch coverage of a changed symbol. Filled by the coverage step.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Coverage {
    pub lines_total: u32,
    pub lines_hit: u32,
    pub branches_total: Option<u32>,
    pub branches_hit: Option<u32>,
    pub uncovered_changed_lines: Vec<u32>,
} // provenance Observed

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum GapReason {
    Uncovered,
    PredictedSurvivor,
}

/// A mutation the tests are predicted not to catch. Filled by the gaps step.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PredictedGap {
    pub file: String,
    pub line: u32,
    pub mutation: String,
    pub p_tests_catch: f64,
    pub reason: GapReason,
} // provenance Predicted

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SymbolEvidence {
    pub id: String,
    pub name: String,
    pub file: String,
    pub span: Span,
    pub kind: ChangeKind,
    pub callers: Vec<Edge>,
    pub entry_points: Vec<Edge>,
    pub coverage: Option<Coverage>,
    pub gaps: Vec<PredictedGap>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidencePack {
    pub base: String,
    pub head: String,
    pub index: IndexState,
    pub symbols: Vec<SymbolEvidence>,
    /// Caveats a reader needs to weigh the facts: where the index came from, what was left out.
    #[serde(default)]
    pub notes: Vec<String>,
}

impl EvidencePack {
    /// False when the index could not serve (stale under `--no-build`, every root failed): an
    /// empty `symbols` then means "unknown", not "nothing changed".
    pub fn facts_served(&self) -> bool {
        self.index.status == alsp_index::IndexStatus::Skipped || self.index.units.iter().any(|u| u.ok)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("pack serializes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alsp_index::{IndexStatus, UnitReport};

    fn pack(units: Vec<UnitReport>, status: IndexStatus) -> EvidencePack {
        EvidencePack {
            base: "b".into(),
            head: "h".into(),
            index: IndexState { status, rebuilt: false, units, millis: 0 },
            symbols: vec![],
            notes: vec![],
        }
    }
    fn unit(ok: bool) -> UnitReport {
        UnitReport { path: ".".into(), ok, detail: None }
    }

    #[test]
    fn facts_served_distinguishes_unknown_from_nothing_changed() {
        assert!(pack(vec![], IndexStatus::Skipped).facts_served());
        assert!(pack(vec![unit(true)], IndexStatus::Fresh).facts_served());
        assert!(pack(vec![unit(true), unit(false)], IndexStatus::Missing).facts_served());
        assert!(!pack(vec![unit(false)], IndexStatus::Stale).facts_served());
    }

    #[test]
    fn json_round_trips_and_uses_lowercase_provenance() {
        let mut p = pack(vec![unit(true)], IndexStatus::Fresh);
        p.symbols.push(SymbolEvidence {
            id: "x".into(),
            name: "x".into(),
            file: "a.rs".into(),
            span: Span { start: 1, end: 2 },
            kind: ChangeKind::Modified,
            callers: vec![Edge { id: "y".into(), file: "b.rs".into(), line: 3, provenance: Provenance::Resolved }],
            entry_points: vec![],
            coverage: None,
            gaps: vec![],
        });
        let json = p.to_json();
        assert!(json.contains("\"provenance\":\"resolved\"") && json.contains("\"status\":\"fresh\""), "{json}");
        assert_eq!(serde_json::from_str::<EvidencePack>(&json).unwrap(), p);
        // a pack written before `notes` existed still reads
        let old = json.replace(",\"notes\":[]", "");
        assert_eq!(serde_json::from_str::<EvidencePack>(&old).unwrap(), p);
    }

    #[test]
    fn labels() {
        assert_eq!(Provenance::Heuristic.as_str(), "heuristic");
        assert_eq!(Provenance::Observed.as_str(), "observed");
        assert_eq!(Provenance::Predicted.as_str(), "predicted");
        assert_eq!(Provenance::Resolved.as_str(), "resolved");
        assert_eq!((ChangeKind::Added.letter(), ChangeKind::Modified.letter(), ChangeKind::Removed.letter()), ('A', 'M', 'R'));
    }
}
