//! SCIP file -> `SymbolIndex`. SCIP positions are 0-based; the index speaks 1-based lines.

use crate::model::{readable_name, Def, RefSite, Span, SymKind, SymbolIndex};
use protobuf::Message;
use scip::types::{symbol_information::Kind, Index};
use std::collections::HashMap;
use std::path::Path;

const ROLE_DEFINITION: i32 = 1;
const ROLE_IMPORT: i32 = 2;

fn kind_of(k: Kind) -> Option<SymKind> {
    Some(match k {
        Kind::Function | Kind::Method | Kind::StaticMethod | Kind::TraitMethod | Kind::AbstractMethod | Kind::Constructor => SymKind::Fn,
        Kind::Constant => SymKind::Const,
        Kind::StaticVariable => SymKind::Static,
        Kind::Struct => SymKind::Struct,
        Kind::Enum => SymKind::Enum,
        Kind::Trait => SymKind::Trait,
        Kind::TypeAlias => SymKind::TypeAlias,
        Kind::Macro => SymKind::Macro,
        Kind::Module => SymKind::Module,
        _ => return None,
    })
}

/// Last 1-based line covered by a SCIP range whose end is `(end_line, end_col)`, 0-based.
/// A range that ends at column 0 of a later line does not include that line.
fn last_line(start_line: i32, end_line: i32, end_col: i32) -> u32 {
    let l = if end_col == 0 && end_line > start_line { end_line } else { end_line + 1 };
    l.max(1) as u32
}

/// Ranges are `[line, col, end_col]` or `[line, col, end_line, end_col]`.
fn range_lines(r: &[i32]) -> Option<(u32, u32)> {
    match r {
        [l, _, ec] => Some((*l as u32 + 1, last_line(*l, *l, *ec))),
        [l, _, el, ec] => Some((*l as u32 + 1, last_line(*l, *el, *ec))),
        _ => None,
    }
}

/// Parse SCIP bytes. Paths in the result are prefixed with `prefix` (the unit's directory relative
/// to the tree root, empty for the root itself) so that all units share one coordinate system.
pub fn read_scip(bytes: &[u8], prefix: &str) -> Result<(Vec<Def>, HashMap<String, Vec<RefSite>>), String> {
    let idx = Index::parse_from_bytes(bytes).map_err(|e| format!("not a SCIP index: {e}"))?;
    let mut defs = Vec::new();
    let mut refs: HashMap<String, Vec<RefSite>> = HashMap::new();
    for doc in &idx.documents {
        // Generated code (OUT_DIR) is indexed under absolute or `..` paths; it is not in the tree.
        if doc.relative_path.starts_with('/') || doc.relative_path.starts_with("..") || !doc.relative_path.ends_with(".rs") {
            continue;
        }
        let file = if prefix.is_empty() { doc.relative_path.clone() } else { format!("{prefix}/{}", doc.relative_path) };
        let kinds: HashMap<&str, Kind> = doc
            .symbols
            .iter()
            .filter_map(|s| s.kind.enum_value().ok().map(|k| (s.symbol.as_str(), k)))
            .collect();
        for occ in &doc.occurrences {
            if occ.symbol.is_empty() || occ.symbol.starts_with("local ") {
                continue;
            }
            let Some((line, _)) = range_lines(&occ.range) else { continue };
            if occ.symbol_roles & ROLE_DEFINITION != 0 {
                let Some(kind) = kinds.get(occ.symbol.as_str()).and_then(|k| kind_of(*k)) else { continue };
                let Some((start, end)) = range_lines(if occ.enclosing_range.is_empty() { &occ.range } else { &occ.enclosing_range }) else { continue };
                defs.push(Def {
                    id: occ.symbol.clone(),
                    key: String::new(),
                    name: readable_name(&occ.symbol),
                    kind,
                    file: file.clone(),
                    name_line: line,
                    span: Span { start, end: end.max(start) },
                });
            } else if occ.symbol_roles & ROLE_IMPORT == 0 {
                refs.entry(occ.symbol.clone()).or_default().push(RefSite { file: file.clone(), line });
            }
        }
    }
    Ok((defs, refs))
}

/// Merge several units' parsed output into one index.
pub fn merge(parts: Vec<(Vec<Def>, HashMap<String, Vec<RefSite>>)>) -> SymbolIndex {
    let mut defs = Vec::new();
    let mut refs: HashMap<String, Vec<RefSite>> = HashMap::new();
    for (d, r) in parts {
        defs.extend(d);
        for (k, v) in r {
            refs.entry(k).or_default().extend(v);
        }
    }
    for v in refs.values_mut() {
        v.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
        v.dedup();
    }
    SymbolIndex::from_parts(defs, refs)
}

pub fn read_scip_file(path: &Path, prefix: &str) -> Result<(Vec<Def>, HashMap<String, Vec<RefSite>>), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    read_scip(&bytes, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use scip::types::{Document, Occurrence, SymbolInformation};

    pub(crate) fn occ(range: Vec<i32>, sym: &str, roles: i32, encl: Vec<i32>) -> Occurrence {
        Occurrence { range, symbol: sym.into(), symbol_roles: roles, enclosing_range: encl, ..Default::default() }
    }
    pub(crate) fn info(sym: &str, kind: Kind) -> SymbolInformation {
        let mut s = SymbolInformation::new();
        s.symbol = sym.into();
        s.kind = kind.into();
        s
    }
    pub(crate) fn encode(docs: Vec<Document>) -> Vec<u8> {
        let mut idx = Index::new();
        idx.documents = docs;
        idx.write_to_bytes().unwrap()
    }

    const P: &str = "rust-analyzer cargo c 0.0.1 ";

    #[test]
    fn ranges_become_one_based_inclusive_lines() {
        assert_eq!(range_lines(&[4, 3, 9]), Some((5, 5)));
        assert_eq!(range_lines(&[4, 0, 8, 1]), Some((5, 9)));
        // ends at column 0 of line 342 (0-based): the last covered line is 342 (1-based)
        assert_eq!(range_lines(&[0, 0, 342, 0]), Some((1, 342)));
        assert_eq!(range_lines(&[1, 2]), None);
    }

    #[test]
    fn defs_refs_locals_imports_and_generated_files() {
        let f = format!("{P}m/f().");
        let g = format!("{P}m/g().");
        let mut doc = Document::new();
        doc.relative_path = "src/m.rs".into();
        doc.occurrences = vec![
            occ(vec![2, 3, 4], &f, 1, vec![1, 0, 6, 1]),
            occ(vec![9, 3, 4], &g, 1, vec![8, 0, 11, 1]),
            occ(vec![9, 8, 9], &f, 0, vec![]),            // call to f inside g
            occ(vec![0, 4, 5], &f, 2, vec![]),            // `use` import: skipped
            occ(vec![3, 4, 5], "local 7", 1, vec![3, 4, 5]),
            occ(vec![4, 4, 5], &format!("{P}m/Field."), 1, vec![4, 4, 5]), // kind unknown/Field: not kept
        ];
        doc.symbols = vec![info(&f, Kind::Function), info(&g, Kind::Method), info(&format!("{P}m/Field."), Kind::Field)];
        let mut gen = Document::new();
        gen.relative_path = "/abs/out/gen.rs".into();
        gen.occurrences = vec![occ(vec![0, 0, 1], &f, 0, vec![])];
        let mut other = Document::new();
        other.relative_path = "README.md".into();
        let bytes = encode(vec![doc, gen, other]);
        let (defs, refs) = read_scip(&bytes, "unit").unwrap();
        assert_eq!(defs.len(), 2);
        let df = defs.iter().find(|d| d.id == f).unwrap();
        assert_eq!((df.file.as_str(), df.name_line, df.span, df.kind), ("unit/src/m.rs", 3, Span { start: 2, end: 7 }, SymKind::Fn));
        assert_eq!(df.name, "m::f");
        assert_eq!(refs[&f], vec![RefSite { file: "unit/src/m.rs".into(), line: 10 }]);
        let idx = merge(vec![(defs, refs)]);
        assert_eq!(idx.references(&f).len(), 1);
    }

    #[test]
    fn garbage_is_an_error_not_a_panic() {
        assert!(read_scip(&[0xff, 0xff, 0xff], "").is_err());
    }
}

#[cfg(test)]
pub(crate) use tests::{encode, info, occ};
