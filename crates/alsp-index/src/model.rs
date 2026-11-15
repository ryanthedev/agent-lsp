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

/// The item kinds the index keeps. Locals, parameters, fields and variants are not kept: a change
/// inside them belongs to the item that owns them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SymKind {
    Fn,
    Const,
    Static,
    Struct,
    Enum,
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
        SymbolIndex { defs, by_id, by_key, by_file, refs: keyed, unattributed }
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
        Def { id: id.into(), key: String::new(), name: id.into(), kind, file: file.into(), name_line: start, span: Span { start, end } }
    }

    #[test]
    fn readable_names() {
        let p = "rust-analyzer cargo proto 0.0.1 ";
        assert_eq!(readable_name(&format!("{p}frame/impl#[Frame]fin().")), "frame::Frame::fin");
        assert_eq!(readable_name(&format!("{p}frame/encode().")), "frame::encode");
        assert_eq!(readable_name(&format!("{p}frame/Kind#")), "frame::Kind");
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

    #[test]
    fn span_helpers() {
        let s = Span { start: 3, end: 5 };
        assert!(s.contains(3) && s.contains(5) && !s.contains(6) && !s.contains(2));
        assert_eq!(s.len(), 2);
        assert!(SymKind::Fn.is_fn() && SymKind::Fn.is_code() && SymKind::Const.is_code());
        assert!(!SymKind::Struct.is_code() && !SymKind::Struct.is_fn());
    }
}
