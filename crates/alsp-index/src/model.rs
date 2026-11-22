//! In-memory symbol index: definitions with their enclosing spans, and the places that refer to them.
//! Everything here is "resolved": it comes from the compiler-grade index, not from guessing.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

/// Inclusive 1-based line range.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    pub fn contains(&self, line: u32) -> bool {
        self.start <= line && line <= self.end
    }
    pub fn len(&self) -> u32 {
        self.end - self.start
    }
}

/// The item kinds the index keeps. Locals, parameters and fields are not kept: a change inside
/// them belongs to the item that owns them. Enum variants are kept so queries can resolve them;
/// diff attribution maps variant-line edits back to their enclosing enum.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymKind {
    Fn,
    Const,
    Static,
    Struct,
    Enum,
    Variant,
    Trait,
    TypeAlias,
    Macro,
    Module,
}

impl SymKind {
    /// Functions and methods (free, associated, trait).
    pub fn is_fn(self) -> bool {
        self == SymKind::Fn
    }
    /// Whether code runs "inside" this item, so that a reference inside it is a use by that item.
    pub fn is_code(self) -> bool {
        matches!(self, SymKind::Fn | SymKind::Const | SymKind::Static)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Def {
    /// The SCIP symbol string. Not always unique: rust-analyzer gives items in the root file of
    /// different crates of one package (`tests/a.rs`, `tests/b.rs`, bins) the same string.
    pub id: String,
    /// Unique per definition: `id`, or `id` plus the file when `id` is defined in several files.
    /// References and graph walks are keyed by it.
    pub key: String,
    /// Readable qualified name without the crate version, e.g. `frame::Frame::fin`.
    pub name: String,
    pub kind: SymKind,
    /// Path relative to the indexed tree root, `/`-separated.
    pub file: String,
    /// Line of the name identifier (1-based).
    pub name_line: u32,
    /// The whole item, attributes and doc comments included.
    pub span: Span,
    /// SCIP symbols this definition implements, as the indexer recorded them (`is_implementation`
    /// relationships): an impl method's trait method. Empty when the indexer says nothing, which
    /// is what rust-analyzer does today; [`SymbolIndex::links`] then falls back to names.
    pub implements: Vec<String>,
}

impl Def {
    /// The same SCIP string is defined in other files too, so references to it can only be
    /// attributed within this definition's own file.
    pub fn is_ambiguous(&self) -> bool {
        self.key != self.id
    }
}

/// A place that refers to a symbol (not its definition).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefSite {
    pub file: String,
    pub line: u32,
}

/// A trait method and an impl's method that stand for each other.
#[derive(Clone, Copy, Debug)]
pub struct Linked<'a> {
    pub def: &'a Def,
    /// The index recorded the link (an `is_implementation` relationship). `false`: the impl was
    /// matched to a trait method of the same trait and method name, which is a guess.
    pub exact: bool,
    /// `def` is the trait's declaration, so a call that resolves to it may dispatch to any impl.
    /// `false`: `def` is an impl's method, which a call on its own type reaches for certain.
    pub declaration: bool,
}

/// Impl methods per trait method and back, as `(def index, exact)`.
type LinkMap = HashMap<usize, Vec<(usize, bool)>>;

#[derive(Default)]
pub struct SymbolIndex {
    defs: Vec<Def>,
    by_id: HashMap<String, usize>,
    /// file -> def indices, sorted by span start.
    by_file: HashMap<String, Vec<usize>>,
    by_key: HashMap<String, usize>,
    /// By `Def::key`.
    refs: HashMap<String, Vec<RefSite>>,
    /// Ambiguous id -> references to it from files that define none of its copies.
    unattributed: HashMap<String, usize>,
    /// Trait method -> its impls' methods.
    implementations: LinkMap,
    /// Impl method -> the trait methods it implements.
    declarations: LinkMap,
    /// (trait, method) -> impl methods left unlinked because several traits carry that name.
    unmatched: HashMap<(String, String), usize>,
}

impl SymbolIndex {
    pub(crate) fn from_parts(mut defs: Vec<Def>, refs: HashMap<String, Vec<RefSite>>) -> Self {
        // Duplicate-symbol records (crate roots, a module reachable twice) collapse to one.
        defs.sort_by(|a, b| (&a.file, a.span.start, a.span.end, &a.id).cmp(&(&b.file, b.span.start, b.span.end, &b.id)));
        defs.dedup_by(|a, b| a.file == b.file && a.span == b.span && a.id == b.id);
        let mut files_of: HashMap<&str, HashSet<&str>> = HashMap::new();
        for d in &defs {
            files_of.entry(&d.id).or_default().insert(&d.file);
        }
        let ambiguous: HashSet<String> = files_of.iter().filter(|(_, f)| f.len() > 1).map(|(id, _)| id.to_string()).collect();
        let known: HashSet<(String, String)> = defs.iter().map(|d| (d.id.clone(), d.file.clone())).collect();
        for d in &mut defs {
            d.key = if ambiguous.contains(&d.id) { format!("{}\u{1}{}", d.id, d.file) } else { d.id.clone() };
        }
        let mut by_id = HashMap::new();
        let mut by_key = HashMap::new();
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, d) in defs.iter().enumerate() {
            by_id.entry(d.id.clone()).or_insert(i);
            by_key.insert(d.key.clone(), i);
            by_file.entry(d.file.clone()).or_default().push(i);
        }
        let mut keyed: HashMap<String, Vec<RefSite>> = HashMap::new();
        let mut unattributed: HashMap<String, usize> = HashMap::new();
        for (id, sites) in refs {
            if !by_id.contains_key(&id) {
                continue;
            }
            if !ambiguous.contains(&id) {
                keyed.insert(id, sites);
                continue;
            }
            // Same string, several files: a reference belongs to the definition in its own file.
            for s in sites {
                if known.contains(&(id.clone(), s.file.clone())) {
                    keyed.entry(format!("{id}\u{1}{}", s.file)).or_default().push(s);
                } else {
                    *unattributed.entry(id.clone()).or_default() += 1;
                }
            }
        }
        let (implementations, declarations, unmatched) = link_impls(&defs, &by_id);
        SymbolIndex { defs, by_id, by_key, by_file, refs: keyed, unattributed, implementations, declarations, unmatched }
    }

    pub fn def(&self, id: &str) -> Option<&Def> {
        self.by_id.get(id).map(|&i| &self.defs[i])
    }

    /// Definitions in a file, ordered by where they start.
    pub fn defs_in(&self, file: &str) -> impl Iterator<Item = &Def> {
        self.by_file.get(file).into_iter().flatten().map(|&i| &self.defs[i])
    }

    /// Every definition, ordered by file and start line.
    pub fn defs(&self) -> impl Iterator<Item = &Def> {
        self.defs.iter()
    }

    pub fn has_file(&self, file: &str) -> bool {
        self.by_file.contains_key(file)
    }

    /// The definition with this `Def::key`.
    pub fn def_by_key(&self, key: &str) -> Option<&Def> {
        self.by_key.get(key).map(|&i| &self.defs[i])
    }

    /// Every reference to the definition with this `Def::key` (its `id` when unambiguous), in file
    /// and line order. Empty for unknown keys.
    pub fn references(&self, key: &str) -> &[RefSite] {
        self.refs.get(key).map(Vec::as_slice).unwrap_or(&[])
    }

    /// References to this definition's (ambiguous) id from files that define none of its copies:
    /// they could belong to any copy, so none is credited with them. Zero for unambiguous ids.
    pub fn unattributed_refs(&self, def: &Def) -> usize {
        self.unattributed.get(&def.id).copied().unwrap_or(0)
    }

    /// The trait/impl counterparts of `def`: for a trait method, the methods of every impl; for an
    /// impl method, the trait method it implements. Empty for anything else, and for an impl of a
    /// trait that is not defined in this index (`Display`, `Drop`: calls through those are not
    /// attributed, which keeps std's call sites out of the answer).
    ///
    /// Links come from the indexer's `is_implementation` relationships when it writes them
    /// (`exact`). rust-analyzer does not, so otherwise an impl method is matched to the one trait
    /// method with the same trait and method name, preferring the same package; two candidates
    /// mean no link (counted by [`SymbolIndex::unmatched_impls`]).
    pub fn links(&self, def: &Def) -> Vec<Linked<'_>> {
        let Some(&i) = self.by_key.get(&def.key) else { return Vec::new() };
        let pick = |m: &LinkMap, declaration: bool| -> Vec<Linked<'_>> {
            m.get(&i).into_iter().flatten().map(|&(j, exact)| Linked { def: &self.defs[j], exact, declaration }).collect()
        };
        let mut v = pick(&self.implementations, false);
        v.extend(pick(&self.declarations, true));
        v.sort_by(|a, b| (&a.def.file, a.def.name_line).cmp(&(&b.def.file, b.def.name_line)));
        v
    }

    /// Impl methods of trait method `decl` that could not be linked because several traits in the
    /// index share the trait's name and have this method.
    pub fn unmatched_impls(&self, decl: &Def) -> usize {
        trait_method_of(&decl.id).and_then(|k| self.unmatched.get(&k)).copied().unwrap_or(0)
    }

    /// The smallest definition whose span contains `line`.
    pub fn innermost(&self, file: &str, line: u32) -> Option<&Def> {
        self.innermost_covering(file, line, line)
    }

    /// The smallest definition whose span contains both lines (`first <= last`).
    pub fn innermost_covering(&self, file: &str, first: u32, last: u32) -> Option<&Def> {
        self.defs_in(file)
            .filter(|d| d.span.start <= first && last <= d.span.end)
            .min_by_key(|d| (d.span.len(), d.kind == SymKind::Module, std::cmp::Reverse(d.span.start)))
    }

    pub fn len(&self) -> usize {
        self.defs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.defs.is_empty()
    }
}

/// The descriptor part of a SCIP symbol (`impl#[A][T]m().`), without scheme, manager and package.
fn descriptor(symbol: &str) -> Option<&str> {
    symbol.splitn(5, ' ').nth(4)
}

/// `rust-analyzer cargo fx 0.1.0` of a symbol: which package it belongs to.
fn package(symbol: &str) -> &str {
    let end = symbol.match_indices(' ').nth(3).map_or(symbol.len(), |(i, _)| i);
    &symbol[..end]
}

/// `` `PartialEq<i32>` `` -> `PartialEq`.
fn trait_name(group: &str) -> &str {
    group.trim_matches('`').split('<').next().unwrap_or("")
}

/// `fx 0.1.0 m/impl#[A][T]run().` -> (`T`, `run`): the trait and method an impl method implements.
fn impl_method_of(symbol: &str) -> Option<(String, String)> {
    let desc = descriptor(symbol)?;
    let rest = &desc[desc.find("impl#[")? + "impl#".len()..];
    let (groups, name) = split_brackets(rest);
    let [_, tr] = groups.as_slice() else { return None };
    let method = name.strip_suffix("().")?;
    (!method.is_empty() && !method.contains(['/', '#'])).then(|| (trait_name(tr).to_string(), method.to_string()))
}

/// `fx 0.1.0 m/T#run().` -> (`T`, `run`): the trait and method a trait method declares. An impl
/// method (`impl#[..]`) is not one.
fn trait_method_of(symbol: &str) -> Option<(String, String)> {
    let desc = descriptor(symbol)?;
    if desc.contains("impl#") {
        return None;
    }
    let (owner, method) = desc.strip_suffix("().")?.rsplit_once('#')?;
    let tr = trait_name(owner.rsplit('/').next()?);
    (!tr.is_empty() && !method.is_empty() && !method.contains(['/', '#'])).then(|| (tr.to_string(), method.to_string()))
}

/// Pair impl methods with trait methods: by the indexer's relationships where it wrote them, else
/// by trait and method name.
fn link_impls(defs: &[Def], by_id: &HashMap<String, usize>) -> (LinkMap, LinkMap, HashMap<(String, String), usize>) {
    let mut decls: HashMap<(String, String), Vec<usize>> = HashMap::new();
    for (i, d) in defs.iter().enumerate().filter(|(_, d)| d.kind.is_fn()) {
        if let Some(k) = trait_method_of(&d.id) {
            decls.entry(k).or_default().push(i);
        }
    }
    let (mut implementations, mut declarations): (LinkMap, LinkMap) = Default::default();
    let mut unmatched: HashMap<(String, String), usize> = HashMap::new();
    let mut link = |decl: usize, imp: usize, exact: bool| {
        implementations.entry(decl).or_default().push((imp, exact));
        declarations.entry(imp).or_default().push((decl, exact));
    };
    for (i, d) in defs.iter().enumerate().filter(|(_, d)| d.kind.is_fn()) {
        if !d.implements.is_empty() {
            // The indexer spoke: use what it says, and do not second-guess a target outside the index.
            d.implements.iter().filter_map(|t| by_id.get(t)).filter(|&&j| j != i).for_each(|&j| link(j, i, true));
            continue;
        }
        let Some(key) = impl_method_of(&d.id) else { continue };
        let all = decls.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        let near: Vec<usize> = all.iter().copied().filter(|&j| package(&defs[j].id) == package(&d.id)).collect();
        match (near.as_slice(), all) {
            ([j], _) | ([], [j]) => link(*j, i, false),
            ([], []) => {}
            _ => *unmatched.entry(key).or_default() += 1,
        }
    }
    (implementations, declarations, unmatched)
}

/// `rust-analyzer cargo proto 0.0.1 frame/impl#[Frame]fin().` -> `frame::Frame::fin`.
/// Falls back to the raw descriptor text when the shape is unfamiliar.
pub fn readable_name(symbol: &str) -> String {
    let desc = match symbol.splitn(5, ' ').nth(4) {
        Some(d) => d,
        None => return symbol.to_string(),
    };
    let mut out = String::new();
    let mut chars = desc.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '/' | '#' => {
                // `Type#member` and `module/item` both read as a path; a trailing one is dropped.
                if chars.peek().is_some() {
                    out.push_str("::");
                }
            }
            '`' => {
                for q in chars.by_ref() {
                    if q == '`' {
                        break;
                    }
                    out.push(q);
                }
            }
            '(' => {
                // Method parameter marker `()` or `().(name)`: stop at the end of the name.
                break;
            }
            '.' if chars.peek().is_none() => {}
            _ => out.push(c),
        }
    }
    // `impl#[Type]name` / `impl#[Type][Trait]name` were flattened to `impl::[Type][Trait]name`.
    if let Some(at) = out.find("impl::[") {
        let (head, rest) = out.split_at(at);
        let rest = &rest["impl::".len()..];
        let (types, name) = split_brackets(rest);
        let owner = match types.as_slice() {
            [t] => t.clone(),
            [t, tr] => format!("<{t} as {tr}>"),
            _ => return desc.to_string(),
        };
        return format!("{head}{owner}::{name}");
    }
    out
}

/// `[A][B]name` -> (["A", "B"], "name").
fn split_brackets(s: &str) -> (Vec<String>, String) {
    let mut groups = Vec::new();
    let mut rest = s;
    while let Some(r) = rest.strip_prefix('[') {
        match r.find(']') {
            Some(end) => {
                groups.push(r[..end].to_string());
                rest = &r[end + 1..];
            }
            None => break,
        }
    }
    (groups, rest.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn def(id: &str, file: &str, start: u32, end: u32, kind: SymKind) -> Def {
        Def { id: id.into(), key: String::new(), name: id.into(), kind, file: file.into(), name_line: start, span: Span { start, end }, implements: Vec::new() }
    }

    #[test]
    fn readable_names() {
        let p = "rust-analyzer cargo proto 0.0.1 ";
        assert_eq!(readable_name(&format!("{p}frame/impl#[Frame]fin().")), "frame::Frame::fin");
        assert_eq!(readable_name(&format!("{p}frame/encode().")), "frame::encode");
        assert_eq!(readable_name(&format!("{p}frame/Kind#")), "frame::Kind");
        assert_eq!(readable_name(&format!("{p}frame/Kind#Ping.")), "frame::Kind::Ping");
        assert_eq!(readable_name(&format!("{p}frame/MAX_LEN.")), "frame::MAX_LEN");
        assert_eq!(readable_name(&format!("{p}frame/")), "frame");
        assert_eq!(readable_name(&format!("{p}impl#[Mcp][Drop]drop().")), "<Mcp as Drop>::drop");
        assert_eq!(readable_name(&format!("{p}value/partial_eq/impl#[Value][`PartialEq<i32>`]eq().")), "value::partial_eq::<Value as PartialEq<i32>>::eq");
        assert_eq!(readable_name(&format!("{p}Kind#from_u8().")), "Kind::from_u8");
        assert_eq!(readable_name("local 3"), "local 3");
    }

    #[test]
    fn innermost_picks_the_smallest_span_and_junction_needs_both_lines() {
        let idx = SymbolIndex::from_parts(
            vec![
                def("m", "a.rs", 1, 100, SymKind::Module),
                def("f", "a.rs", 10, 20, SymKind::Fn),
                def("g", "a.rs", 12, 14, SymKind::Fn),
            ],
            HashMap::new(),
        );
        assert_eq!(idx.innermost("a.rs", 13).unwrap().id, "g");
        assert_eq!(idx.innermost("a.rs", 11).unwrap().id, "f");
        assert_eq!(idx.innermost("a.rs", 50).unwrap().id, "m");
        assert!(idx.innermost("a.rs", 101).is_none());
        assert!(idx.innermost("b.rs", 1).is_none());
        // a deletion between lines 20 and 21 is inside the module only
        assert_eq!(idx.innermost_covering("a.rs", 20, 21).unwrap().id, "m");
        assert_eq!(idx.innermost_covering("a.rs", 15, 16).unwrap().id, "f");
    }

    #[test]
    fn an_item_spanning_the_whole_file_beats_the_file_module() {
        let idx = SymbolIndex::from_parts(vec![def("m", "a.rs", 1, 3, SymKind::Module), def("f", "a.rs", 1, 3, SymKind::Fn)], HashMap::new());
        assert_eq!(idx.innermost("a.rs", 2).unwrap().id, "f");
    }

    #[test]
    fn duplicates_collapse_and_refs_to_unknown_symbols_are_dropped() {
        let mut refs = HashMap::new();
        refs.insert("f".to_string(), vec![RefSite { file: "a.rs".into(), line: 3 }]);
        refs.insert("std".to_string(), vec![RefSite { file: "a.rs".into(), line: 4 }]);
        let idx = SymbolIndex::from_parts(
            vec![def("f", "a.rs", 10, 20, SymKind::Fn), def("f", "a.rs", 10, 20, SymKind::Fn)],
            refs,
        );
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.references("f").len(), 1);
        assert!(idx.references("std").is_empty());
        assert!(idx.references("nope").is_empty());
        assert!(idx.has_file("a.rs") && !idx.has_file("b.rs"));
        assert_eq!(idx.defs_in("a.rs").count(), 1);
    }

    #[test]
    fn an_id_defined_in_several_files_keeps_references_with_their_own_file() {
        // `BG` in tests/a.rs and tests/b.rs: two crates, one SCIP string.
        let mut refs = HashMap::new();
        refs.insert(
            "BG".to_string(),
            vec![
                RefSite { file: "tests/a.rs".into(), line: 5 },
                RefSite { file: "tests/b.rs".into(), line: 9 },
                RefSite { file: "tests/common.rs".into(), line: 2 }, // defines none of them
            ],
        );
        let idx = SymbolIndex::from_parts(vec![def("BG", "tests/a.rs", 1, 1, SymKind::Const), def("BG", "tests/b.rs", 1, 1, SymKind::Const), def("u", "x.rs", 1, 1, SymKind::Fn)], refs);
        let a = idx.defs_in("tests/a.rs").next().unwrap();
        let b = idx.defs_in("tests/b.rs").next().unwrap();
        assert!(a.is_ambiguous() && b.is_ambiguous() && a.key != b.key && a.id == b.id);
        assert!(!idx.defs_in("x.rs").next().unwrap().is_ambiguous());
        assert_eq!(idx.references(&a.key), &[RefSite { file: "tests/a.rs".into(), line: 5 }]);
        assert_eq!(idx.references(&b.key), &[RefSite { file: "tests/b.rs".into(), line: 9 }]);
        assert_eq!(idx.unattributed_refs(a), 1);
        assert_eq!(idx.unattributed_refs(b), 1, "the count is per id: it could belong to either copy");
        assert_eq!(idx.unattributed_refs(idx.defs_in("x.rs").next().unwrap()), 0);
        assert_eq!(idx.def_by_key(&b.key).unwrap().file, "tests/b.rs");
        assert_eq!(idx.def_by_key("u").unwrap().file, "x.rs");
        assert!(idx.def_by_key("nope").is_none());
        assert_eq!(idx.defs().count(), idx.len(), "defs() visits every definition once");
    }

    const P: &str = "rust-analyzer cargo fx 0.1.0 ";

    fn fn_def(id: &str, line: u32, implements: &[&str]) -> Def {
        Def { implements: implements.iter().map(|s| s.to_string()).collect(), ..def(id, "src/lib.rs", line, line, SymKind::Fn) }
    }
    fn linked(idx: &SymbolIndex, id: &str) -> Vec<(String, bool, bool)> {
        let d = idx.def(id).unwrap();
        idx.links(d).iter().map(|l| (l.def.id.clone(), l.exact, l.declaration)).collect()
    }

    #[test]
    fn impl_and_trait_methods_are_matched_by_trait_and_method_name_and_are_not_exact() {
        let (t_m, a_m, b_m) = (format!("{P}T#m()."), format!("{P}impl#[A][T]m()."), format!("{P}impl#[B][T]m()."));
        let idx = SymbolIndex::from_parts(vec![fn_def(&t_m, 2, &[]), fn_def(&a_m, 8, &[]), fn_def(&b_m, 11, &[]), fn_def(&format!("{P}impl#[A]m()."), 20, &[])], HashMap::new());
        assert_eq!(linked(&idx, &t_m), vec![(a_m.clone(), false, false), (b_m, false, false)], "an inherent `A::m` is not an impl of T");
        assert_eq!(linked(&idx, &a_m), vec![(t_m, false, true)]);
        assert!(linked(&idx, &format!("{P}impl#[A]m().")).is_empty());
    }

    #[test]
    fn test_DW_3_4_a_relationship_from_the_index_links_exactly_and_beats_the_names() {
        // The impl's trait name matches nothing, so only the recorded relationship can link it.
        let (decl, imp) = (format!("{P}Shape#area()."), format!("{P}impl#[Sq][Other]area()."));
        let idx = SymbolIndex::from_parts(vec![fn_def(&decl, 2, &[]), fn_def(&imp, 8, &[&decl])], HashMap::new());
        assert_eq!(linked(&idx, &decl), vec![(imp.clone(), true, false)]);
        assert_eq!(linked(&idx, &imp), vec![(decl, true, true)]);
        // A def with a relationship is never name-matched as well: `Shape` here would also match by name.
        let (d2, i2) = (format!("{P}Shape#perimeter()."), format!("{P}impl#[Sq][Shape]perimeter()."));
        let idx = SymbolIndex::from_parts(vec![fn_def(&d2, 2, &[]), fn_def(&i2, 8, &["some external trait method"])], HashMap::new());
        assert!(linked(&idx, &d2).is_empty() && linked(&idx, &i2).is_empty(), "the indexer pointed elsewhere: trust it, do not guess");
    }

    #[test]
    fn an_impl_of_a_trait_outside_the_index_has_no_links() {
        // `Display::fmt` is not defined here: nothing to link, so no std call site can join in.
        let imp = format!("{P}impl#[A][Display]fmt().");
        let idx = SymbolIndex::from_parts(vec![fn_def(&imp, 14, &[]), fn_def(&format!("{P}impl#[B][`PartialEq<i32>`]eq()."), 20, &[])], HashMap::new());
        assert!(linked(&idx, &imp).is_empty());
    }

    #[test]
    fn same_named_traits_in_one_package_leave_the_impl_unlinked_and_counted_but_a_near_package_wins() {
        let (a_t, b_t, imp) = (format!("{P}a/T#m()."), format!("{P}b/T#m()."), format!("{P}impl#[X][T]m()."));
        let idx = SymbolIndex::from_parts(vec![fn_def(&a_t, 2, &[]), fn_def(&b_t, 4, &[]), fn_def(&imp, 8, &[])], HashMap::new());
        assert!(linked(&idx, &a_t).is_empty() && linked(&idx, &imp).is_empty());
        assert_eq!(idx.unmatched_impls(idx.def(&a_t).unwrap()), 1);
        assert_eq!(idx.unmatched_impls(idx.def(&format!("{P}impl#[X][T]m().")).unwrap()), 0, "only trait methods report it");
        // another package's `T` does not compete with the one in the impl's own package
        let other = "rust-analyzer cargo other 0.1.0 T#m().";
        let idx = SymbolIndex::from_parts(vec![fn_def(&format!("{P}T#m()."), 2, &[]), fn_def(other, 4, &[]), fn_def(&imp, 8, &[])], HashMap::new());
        assert_eq!(linked(&idx, &imp), vec![(format!("{P}T#m()."), false, true)]);
        // an impl in a package that has no `T` of its own links to the single `T` elsewhere
        let imp2 = "rust-analyzer cargo third 0.1.0 impl#[X][T]m().";
        let idx = SymbolIndex::from_parts(vec![fn_def(other, 4, &[]), fn_def(imp2, 8, &[])], HashMap::new());
        assert_eq!(linked(&idx, imp2), vec![(other.to_string(), false, true)]);
    }

    #[test]
    fn default_methods_and_blanket_impls_link_by_name() {
        let (d, over, blanket) = (format!("{P}T#d()."), format!("{P}impl#[A][T]d()."), format!("{P}impl#[Z][T]d()."));
        let idx = SymbolIndex::from_parts(vec![fn_def(&d, 3, &[]), fn_def(&over, 8, &[]), fn_def(&blanket, 12, &[])], HashMap::new());
        assert_eq!(linked(&idx, &d).len(), 2, "an override and a blanket impl (`impl<Z: Bound> T for Z`)");
    }

    #[test]
    fn span_helpers() {
        let s = Span { start: 3, end: 5 };
        assert!(s.contains(3) && s.contains(5) && !s.contains(6) && !s.contains(2));
        assert_eq!(s.len(), 2);
        assert!(SymKind::Fn.is_fn() && SymKind::Fn.is_code() && SymKind::Const.is_code());
        assert!(!SymKind::Struct.is_code() && !SymKind::Struct.is_fn());
        assert!(!SymKind::Variant.is_code() && !SymKind::Variant.is_fn());
    }
}
