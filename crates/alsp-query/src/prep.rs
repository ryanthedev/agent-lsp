//! Batched edit/impact brief (`alsp prep <target...>`).
//!
//! Opens the symbol index once and gathers, for each target: its clean declaration (via
//! `crate::outline::format_item_declaration`), same-name collisions elsewhere in the index,
//! grouped callers/references (production callers before `[test]` callers), trait↔impl links,
//! and reaching tests (direct before indirect). Text rendering deterministically enforces a
//! token budget (`approx_tokens`) by trimming lower-priority lists first while always preserving
//! each target's header, declaration, omitted count, and any target-resolution errors.

use crate::answer::{lines, mixed_tag, span, tilde};
use crate::outline::format_item_declaration;
use crate::target::{kind_str, Candidate, Target, TargetError};
use crate::{Freshness, RefGroup, Section, TargetInfo, TestRef, TreeSources, Workspace, SCHEMA};
use alsp_coverage::is_test_support_path;
use alsp_evidence::{
    approx_tokens, escape, incomplete_entry_points_note, link_provenance, reach,
    unlinked_impls_note, Graph, Provenance, Sources, MAX_LINKED,
};
use alsp_index::{Def, SymKind};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Default approximate token budget for `alsp prep` text output.
pub const DEFAULT_PREP_BUDGET: usize = 1500;

/// Maximum same-name collisions kept per target unless `--all` lifts caps.
const MAX_COLLISIONS: usize = 10;

const OUTSIDE: &str = "<outside any symbol>";

/// A trait declaration or concrete impl method linked to the target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PrepLink {
    pub name: String,
    /// `"trait"` for a trait declaration, `"impl"` for a concrete impl method.
    pub kind: String,
    pub file: String,
    pub line: u32,
    pub provenance: Provenance,
}

/// Complete brief for one resolved target.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PrepItem {
    pub target: TargetInfo,
    pub declaration: Option<String>,
    pub collisions: Vec<String>,
    pub reference_count: usize,
    pub refs: Section<RefGroup>,
    pub links: Vec<PrepLink>,
    pub tests: Section<TestRef>,
}

/// A target that failed to resolve (missing or ambiguous).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PrepTargetError {
    pub query: String,
    /// `"no_match"` or `"ambiguous"`.
    pub error: String,
    pub nearest: Vec<String>,
    pub candidates: Vec<Candidate>,
    pub omitted: usize,
    pub hint: String,
}

impl From<TargetError> for PrepTargetError {
    fn from(e: TargetError) -> Self {
        let hint = e.hint();
        match e {
            TargetError::NoMatch { query, nearest } => PrepTargetError {
                query,
                error: "no_match".into(),
                nearest,
                candidates: Vec::new(),
                omitted: 0,
                hint,
            },
            TargetError::Ambiguous { query, candidates, omitted } => PrepTargetError {
                query,
                error: "ambiguous".into(),
                nearest: Vec::new(),
                candidates,
                omitted,
                hint,
            },
        }
    }
}

/// Batched answer returned by [`Workspace::prep`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PrepAnswer {
    pub schema: u32,
    pub command: String,
    pub index: Freshness,
    pub notes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget_tokens: Option<usize>,
    pub items: Vec<PrepItem>,
    pub errors: Vec<PrepTargetError>,
}

impl PrepAnswer {
    pub fn has_errors(&self) -> bool {
        !self.errors.is_empty()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("prep answer serializes") + "\n"
    }

    pub fn to_text(&self) -> String {
        let mut views: Vec<ItemView> = self.items.iter().map(ItemView::full).collect();
        let mut rendered = self.render_views(&views);
        let Some(budget) = self.budget_tokens else {
            return rendered;
        };
        while approx_tokens(&rendered) > budget {
            let tokens = approx_tokens(&rendered);
            if !trim_one_step(&mut views, tokens, budget) {
                break;
            }
            rendered = self.render_views(&views);
        }
        rendered
    }

    fn render_views(&self, views: &[ItemView]) -> String {
        let mut out = Vec::new();
        let f = &self.index;
        let budget_tag = self.budget_tokens.map(|b| format!(" budget={b}")).unwrap_or_default();
        out.push(format!(
            "prep index={}{} @{}{budget_tag}",
            f.status,
            if f.rebuilt { "+rebuilt" } else { "" },
            &f.head[..f.head.len().min(8)]
        ));
        if !f.dirty.is_empty() {
            let shown = f
                .dirty
                .iter()
                .take(3)
                .map(|d| escape(d).into_owned())
                .collect::<Vec<_>>()
                .join(", ");
            out.push(format!(
                "! the working tree has uncommitted changes to {} Rust/Cargo file(s) ({shown}); the index describes the working tree, not {}",
                f.dirty.len(),
                &f.head[..f.head.len().min(8)]
            ));
        }
        out.extend(self.notes.iter().map(|n| format!("! {}", escape(n))));

        let omit_hint = if self.budget_tokens.is_some() {
            "omitted (--all or --budget <n>)"
        } else {
            "more (--all lists everything)"
        };

        for (item, view) in self.items.iter().zip(views.iter()) {
            render_item(&mut out, item, view, omit_hint);
        }
        for err in &self.errors {
            render_error(&mut out, err);
        }
        out.join("\n") + "\n"
    }
}

#[derive(Clone, Debug)]
struct ItemView {
    indirect_tests: usize,
    test_refs: usize,
    prod_refs: usize,
    collisions: usize,
    links: usize,
    direct_tests: usize,
}

impl ItemView {
    fn full(item: &PrepItem) -> Self {
        let prod_refs = item.refs.items.iter().filter(|g| !g.test).count();
        let test_refs = item.refs.items.len() - prod_refs;
        let direct_tests = item.tests.items.iter().filter(|t| t.direct).count();
        let indirect_tests = item.tests.items.len() - direct_tests;
        ItemView {
            indirect_tests,
            test_refs,
            prod_refs,
            collisions: item.collisions.len(),
            links: item.links.len(),
            direct_tests,
        }
    }

    fn get(&self, tier: TrimTier) -> usize {
        match tier {
            TrimTier::IndirectTests => self.indirect_tests,
            TrimTier::TestRefs => self.test_refs,
            TrimTier::ProdRefs => self.prod_refs,
            TrimTier::Collisions => self.collisions,
            TrimTier::Links => self.links,
            TrimTier::DirectTests => self.direct_tests,
        }
    }

    fn get_mut(&mut self, tier: TrimTier) -> &mut usize {
        match tier {
            TrimTier::IndirectTests => &mut self.indirect_tests,
            TrimTier::TestRefs => &mut self.test_refs,
            TrimTier::ProdRefs => &mut self.prod_refs,
            TrimTier::Collisions => &mut self.collisions,
            TrimTier::Links => &mut self.links,
            TrimTier::DirectTests => &mut self.direct_tests,
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum TrimTier {
    IndirectTests,
    TestRefs,
    ProdRefs,
    Collisions,
    Links,
    DirectTests,
}

const TRIM_ORDER: [TrimTier; 6] = [
    TrimTier::IndirectTests,
    TrimTier::TestRefs,
    TrimTier::ProdRefs,
    TrimTier::Collisions,
    TrimTier::Links,
    TrimTier::DirectTests,
];

/// Drop items from the lowest-priority non-empty tier across `views`. Returns `false` once every
/// trimmable list is empty (irreducible floor: headers, declarations, omitted notes, errors).
fn trim_one_step(views: &mut [ItemView], tokens: usize, budget: usize) -> bool {
    for tier in TRIM_ORDER {
        let best = views
            .iter()
            .enumerate()
            .filter(|(_, v)| v.get(tier) > 0)
            .max_by_key(|(i, v)| (v.get(tier), *i));
        if let Some((idx, _)) = best {
            let slot = views[idx].get_mut(tier);
            let step = if tokens > budget.saturating_mul(2) && *slot > 4 {
                *slot / 2
            } else {
                1
            };
            *slot = slot.saturating_sub(step);
            return true;
        }
    }
    false
}

fn render_item(out: &mut Vec<String>, item: &PrepItem, view: &ItemView, omit_hint: &str) {
    let t = &item.target;
    out.push(format!(
        "=== {} {} {}:{} [resolved] ===",
        escape(&t.kind),
        escape(&t.name),
        escape(&t.file),
        span(t.span)
    ));
    if let Some(decl) = &item.declaration {
        out.push("decl [resolved]".into());
        for line in decl.split('\n') {
            out.push(format!("  {}", escape(line)));
        }
    }

    let mut hidden_omitted = 0usize;

    if view.collisions > 0 {
        let shown: Vec<String> = item
            .collisions
            .iter()
            .take(view.collisions)
            .map(|c| escape(c).into_owned())
            .collect();
        let om = item.collisions.len() - view.collisions;
        let suffix = if om > 0 { format!(" (+{om} {omit_hint})") } else { String::new() };
        out.push(format!(
            "collisions({}) [resolved]: {}{suffix}",
            item.collisions.len(),
            shown.join(", ")
        ));
    } else if !item.collisions.is_empty() {
        hidden_omitted += item.collisions.len();
    }

    if view.links > 0 {
        let shown = &item.links[..view.links];
        let tag = mixed_tag(Provenance::Resolved, shown.iter().map(|l| l.provenance));
        out.push(format!("links({}) [{tag}]", item.links.len()));
        for l in shown {
            out.push(format!(
                "  {} {} {}:{}{}",
                escape(&l.kind),
                escape(&l.name),
                escape(&l.file),
                l.line,
                tilde(l.provenance)
            ));
        }
        let om = item.links.len() - view.links;
        if om > 0 {
            out.push(format!("  +{om} {omit_hint}"));
        }
    } else if !item.links.is_empty() {
        hidden_omitted += item.links.len();
    }

    let shown_refs: Vec<&RefGroup> = item
        .refs
        .items
        .iter()
        .filter(|g| !g.test)
        .take(view.prod_refs)
        .chain(item.refs.items.iter().filter(|g| g.test).take(view.test_refs))
        .collect();
    let refs_omitted = item.refs.total.saturating_sub(shown_refs.len());
    if !shown_refs.is_empty() || item.refs.total == 0 {
        let tag = mixed_tag(item.refs.provenance, shown_refs.iter().map(|g| g.provenance));
        out.push(format!(
            "refs({} in {} symbols) [{tag}]",
            item.reference_count, item.refs.total
        ));
        for g in &shown_refs {
            let mark = if g.test { " [test]" } else { "" };
            let via = g
                .via
                .as_ref()
                .map(|v| format!(" via {}", escape(v)))
                .unwrap_or_default();
            out.push(format!(
                "  {} {}:{}{mark}{via}{}",
                escape(&g.symbol),
                escape(&g.file),
                lines(&g.lines),
                tilde(g.provenance)
            ));
        }
        if refs_omitted > 0 {
            out.push(format!("  +{refs_omitted} {omit_hint}"));
        }
    } else {
        hidden_omitted += item.refs.total;
    }

    let shown_tests: Vec<&TestRef> = item
        .tests
        .items
        .iter()
        .filter(|tr| tr.direct)
        .take(view.direct_tests)
        .chain(item.tests.items.iter().filter(|tr| !tr.direct).take(view.indirect_tests))
        .collect();
    let tests_omitted = item.tests.total.saturating_sub(shown_tests.len());
    if !shown_tests.is_empty() || item.tests.total == 0 {
        let tag = mixed_tag(item.tests.provenance, shown_tests.iter().map(|tr| tr.provenance));
        out.push(format!("tests({}) [{tag}]", item.tests.total));
        for tr in &shown_tests {
            out.push(format!(
                "  {} {}:{} {}{}",
                escape(&tr.name),
                escape(&tr.file),
                tr.line,
                if tr.direct { "direct" } else { "indirect" },
                tilde(tr.provenance)
            ));
        }
        if tests_omitted > 0 {
            out.push(format!("  +{tests_omitted} {omit_hint}"));
        }
    } else {
        hidden_omitted += item.tests.total;
    }

    if hidden_omitted > 0 {
        out.push(format!("  +{hidden_omitted} {omit_hint}"));
    }
}

fn render_error(out: &mut Vec<String>, err: &PrepTargetError) {
    if err.error == "no_match" {
        out.push(format!("no match for `{}`", escape(&err.query)));
        if !err.nearest.is_empty() {
            out.push("nearest:".into());
            out.extend(err.nearest.iter().map(|n| format!("  {}", escape(n))));
        }
    } else {
        out.push(format!(
            "`{}` is ambiguous: {} candidates",
            escape(&err.query),
            err.candidates.len() + err.omitted
        ));
        out.extend(
            err.candidates
                .iter()
                .map(|c| format!("  {} {} {}:{}", c.kind, escape(&c.name), escape(&c.file), c.line)),
        );
        if err.omitted > 0 {
            out.push(format!("  +{} more (--all lists everything)", err.omitted));
        }
    }
}

fn short_name(qualified: &str) -> &str {
    qualified.rsplit("::").next().unwrap_or(qualified)
}

fn push_unique(notes: &mut Vec<String>, note: String) {
    if !notes.contains(&note) {
        notes.push(note);
    }
}

impl Workspace {
    /// Gather a batched edit/impact brief for 1..N targets in a single index open.
    ///
    /// Targets that fail to resolve are recorded in [`PrepAnswer::errors`] while all valid
    /// targets are still returned in [`PrepAnswer::items`].
    pub fn prep(&self, targets: &[String], budget_tokens: Option<usize>) -> PrepAnswer {
        let src = TreeSources(&self.tree);
        let mut graph = Graph::new(&self.index, &src);
        let mut file_cache: HashMap<String, Option<String>> = HashMap::new();
        let mut notes = self.notes.clone();
        let mut items = Vec::new();
        let mut errors = Vec::new();

        for query in targets {
            match self.resolve(query) {
                Ok(t) => items.push(self.prep_target(&t, &src, &mut graph, &mut file_cache, &mut notes)),
                Err(e) => errors.push(PrepTargetError::from(e)),
            }
        }

        PrepAnswer {
            schema: SCHEMA,
            command: "prep".into(),
            index: self.freshness(),
            notes,
            budget_tokens,
            items,
            errors,
        }
    }

    fn prep_target(
        &self,
        t: &Target,
        src: &TreeSources<'_>,
        graph: &mut Graph<'_>,
        file_cache: &mut HashMap<String, Option<String>>,
        notes: &mut Vec<String>,
    ) -> PrepItem {
        if t.region && t.defs.is_empty() && !self.index.has_file(&t.file) {
            push_unique(notes, self.unindexed_note(&t.file));
        }

        let declaration = if !t.region {
            let file_src = file_cache
                .entry(t.file.clone())
                .or_insert_with(|| src.head(&t.file));
            file_src
                .as_deref()
                .and_then(|s| format_item_declaration(s, t.span))
        } else {
            None
        };

        let collisions = self.prep_collisions(t);
        let links = self.prep_links(t);
        let (reference_count, refs) = self.prep_refs(t, graph);
        let tests = self.prep_tests(t, graph, notes);
        self.prep_ambiguity_note(t, notes);

        PrepItem {
            target: TargetInfo {
                kind: t.kind.clone(),
                name: t.name.clone(),
                file: t.file.clone(),
                span: t.span,
            },
            declaration,
            collisions,
            reference_count,
            refs,
            links,
            tests,
        }
    }

    fn prep_collisions(&self, t: &Target) -> Vec<String> {
        if t.region {
            return Vec::new();
        }
        let target_short = short_name(&t.name);
        if target_short.is_empty() {
            return Vec::new();
        }
        let target_keys: HashSet<&str> = t.defs.iter().map(|d| d.key.as_str()).collect();
        let mut names: Vec<String> = self
            .index
            .defs()
            .filter(|d| {
                d.kind != SymKind::Module
                    && !target_keys.contains(d.key.as_str())
                    && d.name != t.name
                    && short_name(&d.name) == target_short
            })
            .map(|d| d.name.clone())
            .collect();
        names.sort();
        names.dedup();
        let cap = if self.limit == usize::MAX {
            usize::MAX
        } else {
            MAX_COLLISIONS.min(self.limit)
        };
        names.truncate(cap);
        names
    }

    fn prep_links(&self, t: &Target) -> Vec<PrepLink> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for d in &t.defs {
            for l in self.index.links(d).into_iter().take(MAX_LINKED) {
                if !seen.insert(l.def.key.clone()) {
                    continue;
                }
                out.push(PrepLink {
                    name: l.def.name.clone(),
                    kind: if l.declaration { "trait" } else { "impl" }.into(),
                    file: l.def.file.clone(),
                    line: l.def.name_line,
                    provenance: link_provenance(l.exact, l.declaration),
                });
            }
        }
        out
    }

    fn prep_refs(&self, t: &Target, graph: &mut Graph<'_>) -> (usize, Section<RefGroup>) {
        let mut list: Vec<RefGroup> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        let mut sources: Vec<(&Def, Provenance, Option<String>)> = Vec::new();
        for d in &t.defs {
            sources.push((d, Provenance::Resolved, None));
            for l in self.index.links(d).into_iter().take(MAX_LINKED) {
                sources.push((
                    l.def,
                    link_provenance(l.exact, l.declaration),
                    Some(l.def.name.clone()),
                ));
            }
        }
        for (d, provenance, via) in sources {
            for site in self.index.references(&d.key) {
                if t.region && site.file == t.file && t.span.contains(site.line) {
                    continue;
                }
                let (key, symbol, kind, file, test) =
                    match self.index.innermost(&site.file, site.line) {
                        Some(u) => (
                            u.key.clone(),
                            u.name.clone(),
                            kind_str(u.kind),
                            u.file.clone(),
                            graph.is_test(u) || is_test_support_path(&u.file),
                        ),
                        None => (
                            format!("{}\u{1}", site.file),
                            OUTSIDE.to_string(),
                            "file",
                            site.file.clone(),
                            is_test_support_path(&site.file),
                        ),
                    };
                let key = format!("{key}\u{2}{}", via.as_deref().unwrap_or(""));
                let i = *at.entry(key).or_insert_with(|| {
                    list.push(RefGroup {
                        symbol,
                        kind: kind.into(),
                        file,
                        test,
                        lines: Vec::new(),
                        provenance,
                        via: via.clone(),
                    });
                    list.len() - 1
                });
                list[i].lines.push(site.line);
            }
        }
        for g in &mut list {
            g.lines.sort_unstable();
            g.lines.dedup();
        }
        // Production callers before `[test]` callers, resolved before heuristic.
        list.sort_by(|a, b| {
            (
                a.test,
                a.provenance == Provenance::Heuristic,
                &a.file,
                a.lines.first().copied().unwrap_or(0),
                &a.symbol,
                &a.via,
            )
                .cmp(&(
                    b.test,
                    b.provenance == Provenance::Heuristic,
                    &b.file,
                    b.lines.first().copied().unwrap_or(0),
                    &b.symbol,
                    &b.via,
                ))
        });
        let total_sites: usize = list.iter().map(|g| g.lines.len()).sum();
        (total_sites, Section::capped(Provenance::Resolved, list, self.limit))
    }

    fn prep_tests(
        &self,
        t: &Target,
        graph: &mut Graph<'_>,
        notes: &mut Vec<String>,
    ) -> Section<TestRef> {
        let mut seen = HashSet::new();
        let mut tests = Vec::new();
        let mut direct_users = HashSet::new();
        let mut truncated = Vec::new();
        let mut unlinked = Vec::new();
        let mut raw_eps = Vec::new();

        for d in &t.defs {
            let r = reach(graph, &self.index, d);
            if r.truncated {
                truncated.push(d.name.clone());
            }
            for n in r.notes {
                push_unique(notes, n);
            }
            if d.kind.is_fn() && d.name.contains(" as ") && self.index.links(d).is_empty() {
                unlinked.push(d.name.clone());
            }
            direct_users.extend(r.callers.into_iter().map(|h| h.edge.id));
            raw_eps.extend(r.entry_points);
        }
        if !truncated.is_empty() {
            push_unique(notes, incomplete_entry_points_note(&truncated));
        }
        if !unlinked.is_empty() {
            push_unique(notes, unlinked_impls_note(&unlinked));
        }

        for h in raw_eps {
            let prov = h.provenance();
            let e = h.edge;
            if !seen.insert((e.file.clone(), e.line)) {
                continue;
            }
            let def = self
                .index
                .defs_in(&e.file)
                .find(|x| x.name_line == e.line && x.kind == SymKind::Fn);
            let Some(def) = def else { continue };
            if !graph.is_test(def) {
                continue;
            }
            tests.push(TestRef {
                name: def.name.clone(),
                file: e.file,
                line: e.line,
                direct: direct_users.contains(&def.id),
                provenance: prov,
            });
        }

        // Direct tests before indirect tests, resolved before heuristic.
        tests.sort_by(|a, b| {
            (!a.direct, a.provenance == Provenance::Heuristic, &a.file, a.line, &a.name).cmp(&(
                !b.direct,
                b.provenance == Provenance::Heuristic,
                &b.file,
                b.line,
                &b.name,
            ))
        });

        Section::capped(Provenance::Resolved, tests, self.limit)
    }

    fn prep_ambiguity_note(&self, t: &Target, notes: &mut Vec<String>) {
        let amb: Vec<String> = t
            .defs
            .iter()
            .filter(|d| d.is_ambiguous())
            .map(|d| format!("{} ({} unattributed)", d.name, self.index.unattributed_refs(d)))
            .collect();
        if !amb.is_empty() {
            push_unique(
                notes,
                format!(
                    "the index gives {} the same symbol id as items in other crate roots (tests/*.rs, bins): only same-file references are counted, and references from files defining none of the copies are left out",
                    amb.join(", ")
                ),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(non_snake_case)]
    use super::*;
    use alsp_coverage::testkit::Fx;
    use alsp_evidence::testkit::{index_for, trait_fixture_scip, TRAIT_LIB};

    fn workspace(files: &[(&str, &str)]) -> (Fx, Workspace) {
        let fx = Fx::new(files);
        let ws = Workspace::new(fx.tree.clone(), fx.src.cache_dir.clone(), index_for(files));
        (fx, ws)
    }

    #[test]
    fn test_DW_3_2_prep_returns_decl_collisions_sorted_refs_links_and_tests_for_multiple_targets() {
        let lib = "\
pub fn api() {
    helper();
}
fn helper() {
    leaf();
}
pub fn leaf() -> u32 {
    42
}
";
        let other = "pub fn leaf() -> u32 {\n    0\n}\n";
        let it = "\
#[test]
fn t_api() {
    api();
}
#[test]
fn t_leaf() {
    leaf();
}
";
        let (_fx, ws) = workspace(&[
            ("src/lib.rs", lib),
            ("src/other.rs", other),
            ("tests/it.rs", it),
        ]);

        let ans = ws.prep(&["lib::leaf".into(), "lib::helper".into()], Some(DEFAULT_PREP_BUDGET));
        assert_eq!(ans.schema, SCHEMA);
        assert_eq!(ans.command, "prep");
        assert!(ans.errors.is_empty());
        assert_eq!(ans.items.len(), 2);

        let leaf = &ans.items[0];
        assert_eq!(leaf.target.name, "lib::leaf");
        assert_eq!(leaf.declaration.as_deref(), Some("pub fn leaf() -> u32"));
        assert_eq!(leaf.collisions, vec!["other::leaf"]);
        assert!(leaf.reference_count >= 2);
        // All prod callers (`!g.test`) are sorted BEFORE all test callers (`g.test`).
        let first_test = leaf.refs.items.iter().position(|g| g.test).expect("a test ref");
        assert!(leaf.refs.items[..first_test].iter().all(|g| !g.test), "{:?}", leaf.refs.items);
        assert!(leaf.refs.items[first_test..].iter().all(|g| g.test), "{:?}", leaf.refs.items);
        assert!(leaf.refs.items[..first_test].iter().any(|g| g.symbol == "lib::helper"), "{:?}", leaf.refs.items);
        assert!(leaf.refs.items[first_test..].iter().any(|g| g.symbol == "t_leaf"), "{:?}", leaf.refs.items);
        // Direct test (`t_leaf`) sorted BEFORE indirect test (`t_api`).
        assert_eq!(leaf.tests.items.len(), 2);
        assert_eq!(leaf.tests.items[0].name, "t_leaf");
        assert!(leaf.tests.items[0].direct);
        assert_eq!(leaf.tests.items[1].name, "t_api");
        assert!(!leaf.tests.items[1].direct);

        let helper = &ans.items[1];
        assert_eq!(helper.target.name, "lib::helper");
        assert_eq!(helper.declaration.as_deref(), Some("fn helper()"));
        assert!(helper.collisions.is_empty());
    }

    #[test]
    fn test_DW_3_2_prep_includes_trait_impl_links_with_provenance() {
        let fx = Fx::new(&[("src/lib.rs", TRAIT_LIB)]);
        let idx = alsp_index::index_from_scip(&trait_fixture_scip(false)).unwrap();
        let ws = Workspace::new(fx.tree.clone(), fx.src.cache_dir.clone(), idx);

        let ans = ws.prep(&["T::m".into()], None);
        assert!(ans.errors.is_empty());
        assert_eq!(ans.items.len(), 1);
        let item = &ans.items[0];
        assert!(!item.links.is_empty(), "trait method should link to its impls");
        assert!(item.links.iter().all(|l| l.kind == "impl"));
        assert!(item.links.iter().all(|l| l.provenance == Provenance::Heuristic));

        let text = ans.to_text();
        assert!(text.contains("links(") && text.contains("impl <A as T>::m"), "{text}");
    }

    #[test]
    fn test_DW_3_3_budget_bounds_text_deterministically_and_reports_omitted() {
        let mut lib = String::from("pub fn target() -> u32 {\n    1\n}\n");
        for i in 0..20 {
            lib.push_str(&format!("pub fn caller_{i:02}() -> u32 {{\n    target()\n}}\n"));
        }
        let mut it = String::new();
        for i in 0..10 {
            it.push_str(&format!("#[test]\nfn test_direct_{i:02}() {{\n    target();\n}}\n"));
        }
        for i in 0..10 {
            it.push_str(&format!("#[test]\nfn test_indirect_{i:02}() {{\n    caller_{i:02}();\n}}\n"));
        }
        let (_fx, ws) = workspace(&[("src/lib.rs", &lib), ("tests/it.rs", &it)]);

        let full = ws.prep(&["lib::target".into()], None);
        let full_text = full.to_text();
        let full_tokens = approx_tokens(&full_text);
        assert!(full_tokens > 300, "fixture should be large enough to test trimming: {full_tokens}");

        let budgeted = ws.prep(&["lib::target".into()], Some(180));
        let text1 = budgeted.to_text();
        let text2 = budgeted.to_text();
        assert_eq!(text1, text2, "budgeted rendering must be deterministic");
        assert!(approx_tokens(&text1) <= 180, "tokens={} > 180:\n{text1}", approx_tokens(&text1));
        assert!(text1.contains("omitted (--all or --budget <n>)"), "{text1}");
        assert!(text1.contains("=== fn lib::target src/lib.rs:1-3 [resolved] ==="), "{text1}");
        assert!(text1.contains("pub fn target() -> u32"), "{text1}");
    }

    #[test]
    fn test_DW_3_5_tiny_budget_50_never_panics_or_emits_empty_output() {
        let lib = "pub struct Config {\n    pub host: String,\n    pub port: u16,\n}\npub fn use_cfg(c: Config) -> u16 {\n    c.port\n}\n";
        let (_fx, ws) = workspace(&[("src/lib.rs", lib)]);
        let ans = ws.prep(&["lib::Config".into()], Some(50));
        let text = ans.to_text();
        assert!(!text.trim().is_empty());
        assert!(text.contains("=== struct lib::Config src/lib.rs:1-4 [resolved] ==="), "{text}");
        assert!(text.contains("pub struct Config"), "{text}");
        assert!(text.contains("omitted (--all or --budget <n>)"), "{text}");
    }

    #[test]
    fn test_DW_3_4_and_DW_3_5_json_schema_and_partial_failure() {
        let lib = "pub fn good() {}\n";
        let (_fx, ws) = workspace(&[("src/lib.rs", lib)]);
        let ans = ws.prep(&["lib::good".into(), "missing_sym".into()], Some(DEFAULT_PREP_BUDGET));
        assert!(ans.has_errors());
        assert_eq!(ans.items.len(), 1);
        assert_eq!(ans.errors.len(), 1);
        assert_eq!(ans.errors[0].error, "no_match");
        assert_eq!(ans.errors[0].query, "missing_sym");

        let json_str = ans.to_json();
        assert_eq!(json_str.lines().count(), 1, "JSON must be one line");
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_eq!(v["schema"], 1);
        assert_eq!(v["command"], "prep");
        assert_eq!(v["budget_tokens"], DEFAULT_PREP_BUDGET);
        assert!(v["index"].is_object());
        assert!(v["notes"].is_array());
        assert_eq!(v["items"].as_array().unwrap().len(), 1);
        assert_eq!(v["errors"].as_array().unwrap().len(), 1);

        let text = ans.to_text();
        assert!(text.contains("=== fn lib::good"), "{text}");
        assert!(text.contains("no match for `missing_sym`"), "{text}");
    }

    #[test]
    fn hostile_control_chars_are_escaped_while_multiline_declarations_stay_readable() {
        let mut ans = PrepAnswer {
            schema: SCHEMA,
            command: "prep".into(),
            index: Freshness {
                status: "fresh".into(),
                rebuilt: false,
                head: "0123456789abcdef".into(),
                dirty: vec![],
            },
            notes: vec!["note\nforged".into()],
            budget_tokens: None,
            items: vec![PrepItem {
                target: TargetInfo {
                    kind: "struct".into(),
                    name: "m::Evil\nName".into(),
                    file: "src/e\x1b.rs".into(),
                    span: alsp_index::Span { start: 1, end: 3 },
                },
                declaration: Some("pub struct Evil {\n    pub x: u32,\r\x1b[31m\n}".into()),
                collisions: vec!["other::Evil\nCol".into()],
                reference_count: 0,
                refs: Section::capped(Provenance::Resolved, vec![], 25),
                links: vec![],
                tests: Section::capped(Provenance::Resolved, vec![], 25),
            }],
            errors: vec![],
        };
        let text = ans.to_text();
        assert!(text.contains("! note\\nforged"), "{text}");
        assert!(text.contains("=== struct m::Evil\\nName src/e\\u{1b}.rs:1-3 [resolved] ==="), "{text}");
        // Multiline declaration preserves its 3 lines, while escaping \r and \x1b on each line.
        assert!(text.contains("  pub struct Evil {\n      pub x: u32,\\r\\u{1b}[31m\n  }"), "{text}");
        assert!(text.contains("other::Evil\\nCol"), "{text}");
        // With budget_tokens: None, budget_tokens is omitted from JSON.
        ans.budget_tokens = None;
        let v: serde_json::Value = serde_json::from_str(&ans.to_json()).unwrap();
        assert!(v.get("budget_tokens").is_none());
    }
}
