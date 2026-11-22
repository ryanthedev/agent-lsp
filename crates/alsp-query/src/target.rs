//! From what the user typed to the thing a query is about.
//!
//! A target is a symbol name (exact, or a unique `::`-suffix of the qualified name, so `net::f`
//! finds `app::net::f`) or `file:line`. A name that matches several definitions is an error that
//! lists them, never a guess; except for `sym`, whose answer is that list. A `file:line` names the
//! innermost definition around the line, or the whole file when none surrounds it. Modules are
//! regions: their target is the span of the module and the definitions inside it.

use crate::Workspace;
use alsp_evidence::escape;
use alsp_index::{Def, Span, SymKind, SymbolIndex};
use serde::Serialize;
use std::path::{Component, Path};

/// What a query answers about: one symbol, or a region of a file with the definitions in it.
#[derive(Clone, Debug)]
pub struct Target {
    /// `fn`, `struct`, ..., or `module` / `file` for a region.
    pub kind: String,
    pub name: String,
    pub file: String,
    pub span: Span,
    /// The symbol itself, or the definitions inside the region (modules left out).
    pub defs: Vec<Def>,
    pub region: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct Candidate {
    pub name: String,
    pub kind: String,
    pub file: String,
    pub line: u32,
}

/// The target did not name exactly one thing. The CLI exits 4.
#[derive(Clone, Debug, PartialEq)]
pub enum TargetError {
    NoMatch { query: String, nearest: Vec<String> },
    Ambiguous { query: String, candidates: Vec<Candidate>, omitted: usize },
}

impl TargetError {
    /// What to do next, for stderr.
    pub fn hint(&self) -> String {
        match self {
            TargetError::NoMatch { query, .. } => format!("nothing matches `{query}`: search with `alsp sym <part of the name>`, or pass file:line"),
            TargetError::Ambiguous { query, candidates, .. } => {
                let first = candidates.first().map(|c| c.name.as_str()).unwrap_or("<name>");
                format!("`{query}` names {} things: repeat the command with a longer name such as `{first}`, or with file:line", candidates.len() + self_omitted(self))
            }
        }
    }

    pub fn to_text(&self) -> String {
        let mut out = Vec::new();
        match self {
            TargetError::NoMatch { query, nearest } => {
                out.push(format!("no match for `{}`", escape(query)));
                if !nearest.is_empty() {
                    out.push("nearest:".into());
                    out.extend(nearest.iter().map(|n| format!("  {}", escape(n))));
                }
            }
            TargetError::Ambiguous { query, candidates, omitted } => {
                out.push(format!("`{}` is ambiguous: {} candidates", escape(query), candidates.len() + omitted));
                out.extend(candidates.iter().map(|c| format!("  {} {} {}:{}", c.kind, escape(&c.name), escape(&c.file), c.line)));
                if *omitted > 0 {
                    out.push(format!("  +{omitted} more (--all lists everything)"));
                }
            }
        }
        out.join("\n") + "\n"
    }

    pub fn to_json(&self) -> String {
        let v = match self {
            TargetError::NoMatch { query, nearest } => serde_json::json!({"schema": crate::SCHEMA, "error": "no_match", "query": query, "nearest": nearest, "hint": self.hint()}),
            TargetError::Ambiguous { query, candidates, omitted } => {
                serde_json::json!({"schema": crate::SCHEMA, "error": "ambiguous", "query": query, "candidates": candidates, "omitted": omitted, "hint": self.hint()})
            }
        };
        v.to_string() + "\n"
    }
}

fn self_omitted(e: &TargetError) -> usize {
    match e {
        TargetError::Ambiguous { omitted, .. } => *omitted,
        TargetError::NoMatch { .. } => 0,
    }
}

pub fn kind_str(k: SymKind) -> &'static str {
    match k {
        SymKind::Fn => "fn",
        SymKind::Const => "const",
        SymKind::Static => "static",
        SymKind::Struct => "struct",
        SymKind::Enum => "enum",
        SymKind::Variant => "variant",
        SymKind::Trait => "trait",
        SymKind::TypeAlias => "type",
        SymKind::Macro => "macro",
        SymKind::Module => "mod",
    }
}

/// `src/net.rs:120` -> (`src/net.rs`, 120). `net::f` is a name: the part after its last `:` is not
/// a number.
fn split_file_line(t: &str) -> Option<(&str, u32)> {
    let (file, line) = t.rsplit_once(':')?;
    if file.is_empty() || file.ends_with(':') {
        return None;
    }
    line.parse().ok().map(|n| (file, n))
}

fn levenshtein(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = std::mem::replace(&mut row[0], i + 1);
        for (j, cb) in b.iter().enumerate() {
            let cur = row[j + 1];
            row[j + 1] = (prev + usize::from(ca != *cb)).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

fn last_segment(s: &str) -> &str {
    s.rsplit(['/', ':']).next().unwrap_or(s)
}

/// Up to `n` names close to `query`: the last path segment contains or is contained in the
/// query's, or is within a few edits of it. Closest first.
fn nearest<'a>(query: &str, names: impl Iterator<Item = &'a str>, n: usize) -> Vec<String> {
    let q = last_segment(query).to_lowercase();
    let max_edits = (q.chars().count() / 3).max(2);
    let mut scored: Vec<(usize, &str)> = names
        .filter_map(|name| {
            let l = last_segment(name).to_lowercase();
            let near = !q.is_empty() && (l.contains(&q) || (l.chars().count() >= 3 && q.contains(&l)));
            let d = if near { 0 } else { levenshtein(&l, &q) };
            (near || d <= max_edits).then_some((d, name))
        })
        .collect();
    scored.sort_by(|a, b| (a.0, a.1.len(), a.1).cmp(&(b.0, b.1.len(), b.1)));
    scored.dedup_by(|a, b| a.1 == b.1);
    scored.into_iter().take(n).map(|(_, s)| s.to_string()).collect()
}

fn safe_rel(rel: &str) -> bool {
    !rel.is_empty() && Path::new(rel).components().all(|c| matches!(c, Component::Normal(_)))
}

fn region(idx: &SymbolIndex, kind: &str, name: String, file: &str, span: Span) -> Target {
    let defs = idx.defs_in(file).filter(|d| d.kind != SymKind::Module && span.start <= d.span.start && d.span.end <= span.end).cloned().collect();
    Target { kind: kind.into(), name, file: file.into(), span, defs, region: true }
}

fn of_def(idx: &SymbolIndex, d: &Def) -> Target {
    if d.kind == SymKind::Module {
        return region(idx, "module", d.name.clone(), &d.file, d.span);
    }
    Target { kind: kind_str(d.kind).into(), name: d.name.clone(), file: d.file.clone(), span: d.span, defs: vec![d.clone()], region: false }
}

fn by_name<'a>(idx: &'a SymbolIndex, q: &str) -> Vec<&'a Def> {
    let exact: Vec<&Def> = idx.defs().filter(|d| d.name == q).collect();
    if !exact.is_empty() {
        return exact;
    }
    let suffix = format!("::{q}");
    idx.defs().filter(|d| d.name.ends_with(&suffix)).collect()
}

fn candidate(d: &Def) -> Candidate {
    Candidate { name: d.name.clone(), kind: kind_str(d.kind).into(), file: d.file.clone(), line: d.name_line }
}

/// What a typed target found: one place (`file:line`), or the definitions a name matches.
pub(crate) enum Found<'a> {
    Place(Target),
    Defs(Vec<&'a Def>),
}

impl Workspace {
    /// The indexed (or on-disk) file the user means by `f`: repo-relative, absolute, relative to
    /// the current directory, or a unique path suffix such as `net.rs`.
    fn resolve_file(&self, f: &str, query: &str) -> Result<String, TargetError> {
        let known = |rel: &str| safe_rel(rel) && (self.index.has_file(rel) || self.tree.join(rel).is_file());
        let rel_of = |p: &Path| std::fs::canonicalize(p).ok().and_then(|c| c.strip_prefix(&self.tree).ok().map(|r| r.to_string_lossy().replace('\\', "/")));
        let direct = f.strip_prefix("./").unwrap_or(f);
        let cwd = std::env::current_dir().ok();
        let tries: Vec<Option<String>> = if Path::new(f).is_absolute() {
            vec![rel_of(Path::new(f))]
        } else {
            vec![Some(direct.to_string()), cwd.and_then(|c| rel_of(&c.join(f)))]
        };
        if let Some(found) = tries.into_iter().flatten().find(|c| known(c)) {
            return Ok(found);
        }
        let mut files: Vec<&str> = self.index.defs().map(|d| d.file.as_str()).collect();
        files.dedup();
        files.sort_unstable();
        files.dedup();
        let tail = format!("/{direct}");
        let hits: Vec<&str> = files.iter().copied().filter(|p| p.ends_with(&tail)).collect();
        match hits.as_slice() {
            [one] => Ok(one.to_string()),
            [] => Err(TargetError::NoMatch { query: query.into(), nearest: nearest(f, files.iter().copied(), 5) }),
            many => Err(TargetError::Ambiguous {
                query: query.into(),
                candidates: many.iter().take(self.limit).map(|p| Candidate { name: p.to_string(), kind: "file".into(), file: p.to_string(), line: 1 }).collect(),
                omitted: many.len().saturating_sub(self.limit),
            }),
        }
    }

    pub(crate) fn lookup(&self, query: &str) -> Result<Found<'_>, TargetError> {
        let q = query.trim();
        if let Some((f, line)) = split_file_line(q) {
            let rel = self.resolve_file(f, query)?;
            let inner = self.index.defs_in(&rel).filter(|d| d.kind != SymKind::Module && d.span.contains(line)).min_by_key(|d| d.span.len());
            return Ok(Found::Place(match inner {
                Some(d) => of_def(&self.index, d),
                None => region(&self.index, "file", rel.clone(), &rel, Span { start: 1, end: u32::MAX }),
            }));
        }
        let found = if q.is_empty() { Vec::new() } else { by_name(&self.index, q) };
        if found.is_empty() {
            let names = self.index.defs().map(|d| d.name.as_str());
            return Err(TargetError::NoMatch { query: query.into(), nearest: nearest(q, names, 5) });
        }
        Ok(Found::Defs(found))
    }

    /// The one thing `query` names. A name matching several definitions is
    /// [`TargetError::Ambiguous`] with the candidates listed (capped like every list).
    pub fn resolve(&self, query: &str) -> Result<Target, TargetError> {
        match self.lookup(query)? {
            Found::Place(t) => Ok(t),
            Found::Defs(ds) if ds.len() == 1 => Ok(of_def(&self.index, ds[0])),
            Found::Defs(ds) => Err(TargetError::Ambiguous {
                query: query.into(),
                candidates: ds.iter().take(self.limit).map(|d| candidate(d)).collect(),
                omitted: ds.len().saturating_sub(self.limit),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_line_is_told_from_a_qualified_name() {
        assert_eq!(split_file_line("src/net.rs:120"), Some(("src/net.rs", 120)));
        assert_eq!(split_file_line("net::send_frame"), None);
        assert_eq!(split_file_line("net::7"), None);
        assert_eq!(split_file_line(":12"), None);
        assert_eq!(split_file_line("name"), None);
    }

    #[test]
    fn levenshtein_counts_edits() {
        assert_eq!((levenshtein("kitten", "sitting"), levenshtein("", "ab"), levenshtein("same", "same")), (3, 2, 0));
    }

    #[test]
    fn nearest_prefers_containment_then_few_edits_and_drops_the_far() {
        let names = ["net::send_frame", "net::send_frames", "ui::draw", "net::send_fame", "zzz::unrelated_name"];
        let got = nearest("send_frame", names.iter().copied(), 5);
        assert_eq!(got[0], "net::send_frame");
        assert!(got.contains(&"net::send_frames".to_string()) && got.contains(&"net::send_fame".to_string()), "{got:?}");
        assert!(!got.contains(&"ui::draw".to_string()) && !got.contains(&"zzz::unrelated_name".to_string()), "{got:?}");
        assert_eq!(nearest("send_frame", names.iter().copied(), 1).len(), 1);
        assert!(nearest("", names.iter().copied(), 5).is_empty());
    }

    #[test]
    fn unsafe_relative_paths_are_refused() {
        assert!(safe_rel("src/a.rs") && !safe_rel("../a.rs") && !safe_rel("/a.rs") && !safe_rel(""));
    }

    #[test]
    fn kind_str_formats_variant() {
        assert_eq!(kind_str(SymKind::Variant), "variant");
    }

    #[test]
    fn errors_render_with_a_next_step() {
        let e = TargetError::NoMatch { query: "x".into(), nearest: vec!["a::x".into()] };
        assert_eq!(e.to_text(), "no match for `x`\nnearest:\n  a::x\n");
        assert!(e.hint().contains("alsp sym"));
        assert!(e.to_json().contains("\"error\":\"no_match\"") && e.to_json().contains("\"nearest\":[\"a::x\"]"));
        let c = |n: &str| Candidate { name: n.into(), kind: "fn".into(), file: "f.rs".into(), line: 3 };
        let e = TargetError::Ambiguous { query: "f".into(), candidates: vec![c("a::f"), c("b::f")], omitted: 3 };
        let t = e.to_text();
        assert!(t.starts_with("`f` is ambiguous: 5 candidates\n  fn a::f f.rs:3\n  fn b::f f.rs:3\n  +3 more"), "{t}");
        assert!(e.hint().contains("names 5 things") && e.hint().contains("`a::f`"), "{}", e.hint());
        assert!(e.to_json().contains("\"error\":\"ambiguous\"") && e.to_json().contains("\"omitted\":3"));
    }
}
