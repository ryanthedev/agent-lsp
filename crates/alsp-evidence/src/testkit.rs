//! Test double for rust-analyzer: a name-based indexer over in-memory sources. It is exact only
//! when the fixture's names are unique, which fixtures keep to. Real rust-analyzer output is
//! exercised by the `#[ignore]`d real-tool tests in alsp-cli/tests/real_tools.rs.

use crate::syntax::{analyze, ItemKind};
use alsp_index::{index_from_scip, SymbolIndex};
use protobuf::Message;
use scip::types::{symbol_information::Kind, Document, Index, Occurrence, Relationship, SymbolInformation};

const PREFIX: &str = "rust-analyzer cargo fx 0.0.1 ";

/// Files that are the root of their own crate (integration tests, bins): rust-analyzer gives their
/// items no module prefix, which is why two such files can define the same SCIP string.
fn is_crate_root(file: &str) -> bool {
    file.starts_with("tests/") || file.contains("/tests/") || file.contains("/bin/")
}

fn stem(file: &str) -> &str {
    file.rsplit('/').next().unwrap_or(file).trim_end_matches(".rs")
}

/// `<A for B>::name` -> `impl#[B][A]name`, `<B>::name` -> `impl#[B]name`, `m::f` -> `m/f`.
fn descriptor(path: &str, kind: ItemKind) -> String {
    let mut out = String::new();
    let segs: Vec<&str> = path.split("::").collect();
    for (i, s) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        if let Some(inner) = s.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
            match inner.split_once(" for ") {
                Some((tr, ty)) => out.push_str(&format!("impl#[{ty}][{tr}]")),
                None => out.push_str(&format!("impl#[{inner}]")),
            }
        } else if last {
            out.push_str(s);
            out.push_str(match kind {
                ItemKind::Fn => "().",
                ItemKind::Mod => "/",
                ItemKind::Type => "#",
                ItemKind::Const => ".",
                ItemKind::Macro => "!",
            });
        } else {
            out.push_str(s);
            out.push('/');
        }
    }
    out
}

fn idents(line: &str) -> Vec<&str> {
    line.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty()).collect()
}

/// SCIP bytes for the given `(path, source)` files.
pub fn scip_for(files: &[(&str, &str)]) -> Vec<u8> {
    struct D {
        id: String,
        name: String,
        file: String,
        name_line: u32,
    }
    let mut docs: Vec<Document> = Vec::new();
    let mut all: Vec<D> = Vec::new();
    for (file, src) in files {
        let syn = analyze(src).unwrap_or_else(|| panic!("fixture {file} must parse"));
        let total = src.lines().count().max(1) as i32;
        let mut doc = Document::new();
        doc.relative_path = (*file).to_string();
        let module = if is_crate_root(file) { "crate".to_string() } else { stem(file).to_string() };
        let qualify = |d: &str| if is_crate_root(file) { format!("{PREFIX}{d}") } else { format!("{PREFIX}{}/{d}", stem(file)) };
        let mut defs = vec![(format!("{PREFIX}{module}/"), Kind::Module, 1u32, 1i32, total, "".to_string())];
        for it in &syn.items {
            let (kind, d) = match it.kind {
                ItemKind::Fn => (Kind::Function, descriptor(&it.path, it.kind)),
                ItemKind::Mod => (Kind::Module, descriptor(&it.path, it.kind)),
                ItemKind::Type => (Kind::Struct, descriptor(&it.path, it.kind)),
                ItemKind::Const => (Kind::Constant, descriptor(&it.path, it.kind)),
                ItemKind::Macro => (Kind::Macro, descriptor(&it.path, it.kind)),
            };
            defs.push((qualify(&d), kind, it.name_line, it.start as i32, it.end as i32, it.name.clone()));
        }
        for (id, kind, name_line, start, end, name) in defs {
            let mut o = Occurrence::new();
            o.range = vec![name_line as i32 - 1, 0, 1];
            o.symbol = id.clone();
            o.symbol_roles = 1;
            o.enclosing_range = vec![start - 1, 0, end - 1, 1];
            doc.occurrences.push(o);
            let mut s = SymbolInformation::new();
            s.symbol = id.clone();
            s.kind = kind.into();
            doc.symbols.push(s);
            if !name.is_empty() {
                all.push(D { id, name, file: (*file).to_string(), name_line });
            }
        }
        docs.push(doc);
    }
    // References: every identifier equal to some definition's simple name, except on that
    // definition's own name line. Unique names make this exact.
    for (doc, (file, src)) in docs.iter_mut().zip(files) {
        let _ = file;
        for (n, line) in src.lines().enumerate() {
            let ln = n as u32 + 1;
            let mut seen = std::collections::HashSet::new();
            for w in idents(line) {
                for d in all.iter().filter(|d| d.name == w) {
                    if all.iter().any(|o| o.id == d.id && o.file == doc.relative_path && o.name_line == ln) {
                        continue;
                    }
                    if seen.insert(d.id.clone()) {
                        let mut o = Occurrence::new();
                        o.range = vec![n as i32, 0, 1];
                        o.symbol = d.id.clone();
                        doc.occurrences.push(o);
                    }
                }
            }
        }
    }
    let mut idx = Index::new();
    idx.documents = docs;
    idx.write_to_bytes().expect("SCIP encodes")
}

/// A trait with two impls, a `dyn` call, direct calls on each concrete type, a std-trait impl and
/// two tests (one calls the impl directly, one goes through `via_dyn`). Line numbers matter:
/// [`trait_fixture_scip`] is written by hand against them.
pub const TRAIT_LIB: &str = "\
pub trait T {
    fn m(&self) -> u32;
    fn d(&self) -> u32 { 7 }
}
pub struct A;
pub struct B;
impl T for A {
    fn m(&self) -> u32 { 1 }
}
impl T for B {
    fn m(&self) -> u32 { 2 }
}
impl std::fmt::Display for A {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result { write!(f, \"A\") }
}
pub fn via_dyn(x: &dyn T) -> u32 {
    x.m()
}
pub fn direct_a(a: &A) -> u32 {
    a.m()
}
pub fn direct_b(b: &B) -> u32 {
    b.m()
}
pub fn show(a: &A) -> String {
    format!(\"{a}\")
}
#[test]
fn t_direct_a() {
    assert_eq!(A.m(), 1);
}
#[test]
fn t_dyn() {
    via_dyn(&A);
}
";

/// SCIP for [`TRAIT_LIB`] (as `src/lib.rs`) the way rust-analyzer writes it: calls on a concrete
/// type resolve to the impl's method, calls on `dyn T` to the trait's, the std impl's trait is an
/// external symbol, and no relationships. With `relationships`, the impl methods also carry the
/// `is_implementation` relationship to the trait method that an indexer could write.
pub fn trait_fixture_scip(relationships: bool) -> Vec<u8> {
    let sym = |d: &str| format!("{PREFIX}{d}");
    let defs: [(&str, Kind, i32, i32, i32); 14] = [
        ("T#", Kind::Trait, 1, 1, 4),
        ("T#m().", Kind::TraitMethod, 2, 2, 2),
        ("T#d().", Kind::Method, 3, 3, 3),
        ("A#", Kind::Struct, 5, 5, 5),
        ("B#", Kind::Struct, 6, 6, 6),
        ("impl#[A][T]m().", Kind::Method, 8, 8, 8),
        ("impl#[B][T]m().", Kind::Method, 11, 11, 11),
        ("impl#[A][Display]fmt().", Kind::Method, 14, 14, 14),
        ("via_dyn().", Kind::Function, 16, 16, 18),
        ("direct_a().", Kind::Function, 19, 19, 21),
        ("direct_b().", Kind::Function, 22, 22, 24),
        ("show().", Kind::Function, 25, 25, 27),
        ("t_direct_a().", Kind::Function, 29, 28, 31),
        ("t_dyn().", Kind::Function, 33, 32, 35),
    ];
    let core = "rust-analyzer cargo core https://github.com/rust-lang/rust/library/core ";
    let refs: [(i32, String); 9] = [
        (7, sym("T#")),
        (10, sym("T#")),
        (13, format!("{core}fmt/Display#")),
        (17, sym("T#m().")),
        (20, sym("impl#[A][T]m().")),
        (23, sym("impl#[B][T]m().")),
        (26, format!("{core}fmt/Display#fmt().")), // `format!("{a}")` calls std's Display::fmt
        (30, sym("impl#[A][T]m().")),
        (34, sym("via_dyn().")),
    ];
    let mut doc = Document::new();
    doc.relative_path = "src/lib.rs".into();
    for (d, kind, name_line, start, end) in defs {
        let mut o = Occurrence::new();
        o.range = vec![name_line - 1, 0, 1];
        o.symbol = sym(d);
        o.symbol_roles = 1;
        o.enclosing_range = vec![start - 1, 0, end - 1, 1];
        doc.occurrences.push(o);
        let mut si = SymbolInformation::new();
        si.symbol = sym(d);
        si.kind = kind.into();
        if relationships && d.starts_with("impl#[") && d.contains("][T]m") {
            let mut r = Relationship::new();
            r.symbol = sym("T#m().");
            r.is_implementation = true;
            si.relationships.push(r);
        }
        doc.symbols.push(si);
    }
    for (line, symbol) in refs {
        let mut o = Occurrence::new();
        o.range = vec![line - 1, 4, 5];
        o.symbol = symbol;
        doc.occurrences.push(o);
    }
    let mut idx = Index::new();
    idx.documents = vec![doc];
    idx.write_to_bytes().expect("SCIP encodes")
}

/// SCIP written by hand for one file `src/lib.rs`: each def is `(descriptor, 0-based line)` and spans
/// that one line; each reference is `(descriptor, 0-based line)`. Every def is a method (a
/// descriptor ending in `().`) or a trait. For fixtures the name-based double cannot express.
pub fn scip_by_hand(defs: &[(&str, i32)], refs: &[(&str, i32)]) -> Vec<u8> {
    let mut doc = Document::new();
    doc.relative_path = "src/lib.rs".into();
    for (d, line) in defs {
        let id = format!("{PREFIX}{d}");
        let mut o = Occurrence::new();
        (o.range, o.symbol, o.symbol_roles, o.enclosing_range) = (vec![*line, 0, 1], id.clone(), 1, vec![*line, 0, *line, 1]);
        doc.occurrences.push(o);
        let mut si = SymbolInformation::new();
        si.symbol = id;
        si.kind = if d.ends_with("().") { Kind::Method } else { Kind::Trait }.into();
        doc.symbols.push(si);
    }
    for (d, line) in refs {
        let mut o = Occurrence::new();
        (o.range, o.symbol) = (vec![*line, 4, 5], format!("{PREFIX}{d}"));
        doc.occurrences.push(o);
    }
    let mut idx = Index::new();
    idx.documents = vec![doc];
    idx.write_to_bytes().expect("SCIP encodes")
}

pub fn index_for(files: &[(&str, &str)]) -> SymbolIndex {
    index_from_scip(&scip_for(files)).expect("testkit SCIP parses")
}

/// Like `index_for`, but the files named in `spanless` lose their file-level module definition,
/// so no definition encloses any line of them: how rust-analyzer's output looks for generated or
/// macro-expanded code that carries no spans.
pub fn index_for_spanless(files: &[(&str, &str)], spanless: &[&str]) -> SymbolIndex {
    let mut idx = Index::parse_from_bytes(&scip_for(files)).expect("testkit SCIP parses back");
    for doc in idx.documents.iter_mut().filter(|d| spanless.contains(&d.relative_path.as_str())) {
        // `scip_for` writes the module definition first in both lists.
        doc.occurrences.remove(0);
        doc.symbols.remove(0);
    }
    index_from_scip(&idx.write_to_bytes().expect("SCIP encodes")).expect("testkit SCIP parses")
}

/// Referencing identifier -> its symbol id, for tests that need an id: the unique definition
/// named `name` (optionally restricted to `file`).
pub fn id_of(idx: &SymbolIndex, file: &str, name: &str) -> String {
    let mut found: Vec<String> = idx.defs_in(file).filter(|d| d.name.rsplit("::").next() == Some(name)).map(|d| d.id.clone()).collect();
    found.dedup();
    assert_eq!(found.len(), 1, "expected one `{name}` in {file}, got {found:?}");
    found.remove(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_double_produces_defs_spans_and_references() {
        let src = "fn callee() {}\nfn caller() {\n    callee();\n}\n";
        let idx = index_for(&[("src/m.rs", src)]);
        let callee = id_of(&idx, "src/m.rs", "callee");
        let d = idx.def(&callee).unwrap();
        assert_eq!((d.name.as_str(), d.name_line, d.span.start, d.span.end), ("m::callee", 1, 1, 1));
        assert_eq!(idx.references(&callee).iter().map(|r| r.line).collect::<Vec<_>>(), vec![3]);
        assert_eq!(idx.innermost("src/m.rs", 3).unwrap().name, "m::caller");
    }

    #[test]
    fn crate_root_files_share_ids_like_rust_analyzer_does() {
        let a = "fn helper() {}\nfn use_a() {\n    helper();\n}\n";
        let b = "fn helper() {}\nfn use_b() {\n    helper();\n}\n";
        let idx = index_for(&[("tests/a.rs", a), ("tests/b.rs", b)]);
        let ha = idx.defs_in("tests/a.rs").find(|d| d.name == "helper").unwrap();
        let hb = idx.defs_in("tests/b.rs").find(|d| d.name == "helper").unwrap();
        assert_eq!(ha.id, hb.id);
        assert_ne!(ha.key, hb.key);
        assert_eq!(idx.references(&ha.key).iter().map(|r| r.file.as_str()).collect::<Vec<_>>(), vec!["tests/a.rs"]);
    }

    #[test]
    fn impl_and_trait_impl_descriptors_read_back() {
        let src = "struct S;\nimpl S {\n    fn go(&self) {}\n}\nimpl Show for S {\n    fn show(&self) {}\n}\n";
        let idx = index_for(&[("src/m.rs", src)]);
        let names: Vec<_> = idx.defs_in("src/m.rs").map(|d| d.name.clone()).collect();
        assert!(names.contains(&"m::S::go".to_string()), "{names:?}");
        assert!(names.contains(&"m::<S as Show>::show".to_string()), "{names:?}");
    }
}
