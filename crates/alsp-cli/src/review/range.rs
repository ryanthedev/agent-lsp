//! Which commits an `alsp review` covers, and the one line that says so (docs/review.md, "Ad-hoc use").

use alsp_evidence::Git;
use alsp_review::{last_review, ReviewRecord};

/// The span of history one review covers.
#[derive(Debug)]
pub struct Range {
    /// Commit id the diff starts from.
    pub base: String,
    /// Why `base` was chosen, for the first output line.
    pub base_reason: String,
    /// Commit id the diff ends at: the checked-out commit, a work-tree snapshot commit, or `--head`.
    pub head: String,
    /// `head` is a snapshot that includes uncommitted work.
    pub uncommitted: bool,
    /// The checked-out commit (`HEAD`), whatever `head` is.
    pub checked_out: String,
    /// Branch key under which the review is recorded.
    pub branch: String,
    /// Whether finishing this review should be recorded as "reviewed up to here" for the branch:
    /// only when `head` is the checked-out commit or a snapshot of the work tree on top of it.
    pub record: bool,
    /// Untracked, non-ignored files left out of the review because `--untracked` was not given.
    pub untracked_left_out: Vec<String>,
    /// Untracked files `--untracked` would have included but did not: `path (reason)`.
    pub untracked_skipped: Vec<String>,
}

impl Range {
    /// Nothing to look at: same commit, or two commits with identical trees.
    pub fn is_empty(&self, git: &Git) -> Result<bool, String> {
        Ok(self.base == self.head || git.diff(&self.base, &self.head)?.trim().is_empty())
    }
}

fn short(sha: &str) -> &str { &sha[..8.min(sha.len())] }

/// Picks base and head.
/// - `head`: `--head` when given (committed work only); otherwise `HEAD`, plus a snapshot of uncommitted
///   work unless `committed`. The snapshot holds tracked edits and staged additions; untracked files only
///   with `untracked` (and never secrets or large files, see `Git::snapshot_worktree`).
/// - `base`: `--base` when given; else the commit the branch's last recorded review covered, if it is still
///   in this branch's history; else the merge-base with the default branch.
/// `Err` is a message for the user (exit 2).
pub fn resolve(git: &Git, base: Option<&str>, head: Option<&str>, committed: bool, untracked: bool) -> Result<Range, String> {
    let checked_out = git.resolve("HEAD")?;
    let branch = git.branch().unwrap_or_else(|| "(detached)".into());
    let (mut untracked_left_out, mut untracked_skipped) = (Vec::new(), Vec::new());
    let (head, uncommitted) = match head {
        Some(h) => (git.resolve(h)?, false),
        None if committed => (checked_out.clone(), false),
        None => {
            let (snap, skipped) = git.snapshot_worktree(untracked).map_err(|e| format!("cannot snapshot uncommitted changes ({e}); use --committed to review commits only"))?;
            untracked_skipped = skipped;
            if !untracked { untracked_left_out = git.untracked_files()?; }
            match snap { Some(s) => (s, true), None => (checked_out.clone(), false) }
        }
    };
    let (base, base_reason) = match base {
        Some(b) => (git.resolve(b)?, "--base".to_string()),
        None => default_base(git, &branch, &checked_out, last_recorded(git, &branch))?,
    };
    let record = head == checked_out || uncommitted;
    Ok(Range { base, base_reason, head, uncommitted, checked_out, branch, record, untracked_left_out, untracked_skipped })
}

/// The branch's last review record. `Err(why)` when records cannot be trusted: `.alsp` is tracked by git
/// (repository-provided, and a committed record could hide changes from review) or unreadable.
fn last_recorded(git: &Git, branch: &str) -> Result<Option<ReviewRecord>, String> {
    match crate::settings::enabled_is_tracked(&git.root) {
        Ok(false) => last_review(&git.root, branch).map_err(|e| format!("cannot read .alsp/reviews.jsonl: {e}")),
        Ok(true) => Err("`.alsp` is tracked by git, so its review records are ignored".into()),
        Err(e) => Err(e),
    }
}

/// Base for an ad-hoc review: the end of the last review when its commit is still an ancestor of
/// `checked_out`, else the merge-base with the default branch.
fn default_base(git: &Git, branch: &str, checked_out: &str, last: Result<Option<ReviewRecord>, String>) -> Result<(String, String), String> {
    let why_not = match last {
        Err(e) => Some(e),
        Ok(None) => None,
        Ok(Some(rec)) => match git.resolve(&rec.head).and_then(|h| git.is_ancestor(&h, checked_out).map(|ok| (h, ok))) {
            Ok((head, true)) => {
                let snapshot = if rec.uncommitted { git.resolve(&rec.reviewed).ok() } else { None };
                let (base, what) = match snapshot {
                    Some(s) => (s, " with its uncommitted changes"),
                    None if rec.uncommitted => (head, " (its uncommitted changes were not kept, so they are reviewed again)"),
                    None => (head, ""),
                };
                return Ok((base, format!("last review of {branch}, at {}{what}", short(&rec.head))));
            }
            _ => Some(format!("last reviewed {} is no longer in this branch's history (rebased?)", short(&rec.head))),
        },
    };
    let Some((name, sha)) = git.default_branch() else {
        let prefix = why_not.map(|w| format!("{w}; ")).unwrap_or_default();
        return Err(format!("{prefix}no earlier review of {branch} and no default branch (origin/HEAD, main, master): pass --base <rev>"));
    };
    let Some(base) = git.merge_base(&sha, checked_out) else {
        return Err(format!("{branch} shares no history with {name}: pass --base <rev>"));
    };
    let mut reason = format!("merge-base with {name}");
    if base == checked_out { reason.push_str("; HEAD is that commit and nothing was reviewed before, so only uncommitted changes show: pass --base to look further back"); }
    if let Some(w) = why_not { reason.push_str(&format!("; {w}")); }
    Ok((base, reason))
}

/// How long a review with these choices is expected to take and what it costs.
pub fn estimate(agents: &[String], coverage: bool, gaps: bool) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if agents.is_empty() && !coverage && !gaps { parts.push("seconds on a warm index, free"); }
    if !agents.is_empty() { parts.push("~2-10 min, uses agent quota"); }
    if coverage || gaps { parts.push("tests run"); }
    if gaps { parts.push("~$0.02-0.10 Gemini via sdd"); }
    parts.join(", ")
}

/// The first output line: base and why, head, tier, agents, estimate.
pub fn banner(r: &Range, tier: &str, agents: &[String], coverage: bool, gaps: bool) -> String {
    let head = if r.uncommitted { format!("{} + uncommitted changes", short(&r.checked_out)) } else { short(&r.head).to_string() };
    let agent_list = if agents.is_empty() { "none".to_string() } else { agents.join(",") };
    let mut line = format!("alsp review: base {} ({}); head {head}; {tier} tier, agents {agent_list}: {}", short(&r.base), r.base_reason, estimate(agents, coverage, gaps));
    let n = r.untracked_left_out.len();
    if n > 0 {
        let shown: Vec<&str> = r.untracked_left_out.iter().take(5).map(String::as_str).collect();
        let more = if n > 5 { format!(", +{} more", n - 5) } else { String::new() };
        line.push_str(&format!("; {n} untracked file(s) not reviewed (pass --untracked): {}{more}", shown.join(", ")));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

    fn range(uncommitted: bool) -> Range {
        Range { base: "0123456789abcdef".into(), base_reason: "why".into(), head: "fedcba9876543210".into(), uncommitted, checked_out: "aaaaaaaabbbbbbbb".into(), branch: "b".into(), record: true, untracked_left_out: vec![], untracked_skipped: vec![] }
    }
    fn names(v: &[&str]) -> Vec<String> { v.iter().map(|s| s.to_string()).collect() }

    #[test]
    fn estimate_states_time_and_cost_for_each_combination() {
        assert_eq!(estimate(&[], false, false), "seconds on a warm index, free");
        assert_eq!(estimate(&names(&["claude"]), false, false), "~2-10 min, uses agent quota");
        assert_eq!(estimate(&[], true, false), "tests run");
        assert_eq!(estimate(&[], false, true), "tests run, ~$0.02-0.10 Gemini via sdd");
        assert_eq!(estimate(&names(&["claude", "codex"]), true, true), "~2-10 min, uses agent quota, tests run, ~$0.02-0.10 Gemini via sdd");
    }

    #[test]
    fn banner_shortens_ids_and_names_the_checked_out_commit_when_uncommitted_work_is_included() {
        assert_eq!(banner(&range(false), "quick", &[], false, false), "alsp review: base 01234567 (why); head fedcba98; quick tier, agents none: seconds on a warm index, free");
        let b = banner(&range(true), "default", &names(&["agy"]), false, false);
        assert!(b.contains("head aaaaaaaa + uncommitted changes") && b.contains("agents agy"), "{b}");
    }

    #[test]
    fn banner_lists_up_to_five_untracked_files_left_out() {
        let mut r = range(false);
        r.untracked_left_out = vec!["a".into(), "b".into()];
        assert!(banner(&r, "quick", &[], false, false).ends_with("; 2 untracked file(s) not reviewed (pass --untracked): a, b"));
        r.untracked_left_out = (1..=7).map(|i| format!("f{i}")).collect();
        assert!(banner(&r, "quick", &[], false, false).ends_with("; 7 untracked file(s) not reviewed (pass --untracked): f1, f2, f3, f4, f5, +2 more"));
    }

    #[test]
    fn short_never_panics_on_short_ids() {
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
    }
}
