//! Diff hunks -> changed symbols.
//!
//! Head-side lines are attributed to the innermost definition that contains them (so an edit in a
//! nested function belongs to that function, not to the module around it). Lines outside every
//! item but a module (imports, loose comments) go to the module; blank lines go nowhere. Symbols
//! deleted by the diff exist only in the base tree, so they are read from the base file's syntax.

use crate::diff::FileDiff;
use crate::syntax::{analyze, FileSyntax, Item};
use crate::types::ChangeKind;
use alsp_index::{Def, Span, SymKind, SymbolIndex};
use std::collections::BTreeMap;

/// Where file contents come from: the head tree (the one the index describes) and the base commit.
pub trait Sources {
    fn head(&self, path: &str) -> Option<String>;
    fn base(&self, path: &str) -> Option<String>;
}

#[derive(Clone, Debug)]
pub struct Changed {
    pub kind: ChangeKind,
    /// The head-tree definition; `None` for `Removed`.
    pub def: Option<Def>,
    pub id: String,
    pub name: String,
    pub file: String,
    pub span: Span,
}

#[derive(Debug, Default)]
pub struct Changes {
    pub symbols: Vec<Changed>,
    pub notes: Vec<String>,
}

fn blank(text: &str, line: u32) -> bool {
    text.lines().nth(line as usize - 1).is_none_or(|l| l.trim().is_empty())
}

/// Smallest non-variant definition covering `first..=last`: edits on an enum variant line belong
/// to the enclosing `Enum` rather than reporting individual variants as top-level changed symbols.
fn innermost_item<'a>(idx: &'a SymbolIndex, file: &str, first: u32, last: u32) -> Option<&'a Def> {
    idx.defs_in(file)
        .filter(|d| d.kind != SymKind::Variant && d.span.start <= first && last <= d.span.end)
        .min_by_key(|d| (d.span.len(), d.kind == SymKind::Module, std::cmp::Reverse(d.span.start)))
}

/// Base-side items removed by the diff: overlapping a removed line, absent from the head file.
fn removed_items<'a>(base: &'a FileSyntax, head: Option<&FileSyntax>, removed: std::ops::Range<u32>) -> Vec<&'a Item> {
    base.items
        .iter()
        .filter(|i| i.start < removed.end && removed.start <= i.end && head.is_none_or(|h| !h.has_key(&i.key)))
        .collect()
}

pub fn changed_symbols(diffs: &[FileDiff], idx: &SymbolIndex, src: &dyn Sources) -> Changes {
    let mut out = Changes::default();
    // Keyed so each definition appears once however many hunks touch it.
    let mut hits: BTreeMap<(String, u32, String), Changed> = BTreeMap::new();
    let mut removed: BTreeMap<(String, u32, String), Changed> = BTreeMap::new();
    let mut unindexed = Vec::new();
    let mut unparsed = Vec::new();

    for fd in diffs.iter().filter(|d| d.is_rust()) {
        let base_text = fd.old_path.as_deref().and_then(|p| src.base(p));
        let base_syn = base_text.as_deref().and_then(analyze);
        let head_text = fd.new_path.as_deref().and_then(|p| src.head(p));
        let head_syn = head_text.as_deref().and_then(analyze);
        let new_file = fd.old_path.is_none();

        if let (Some(p), None) = (&fd.new_path, &head_syn) {
            if head_text.is_some() {
                unparsed.push(p.clone());
            }
        }

        // Removed symbols: syntactic, from the base side.
        // Without a parsed head file (and unless the file is deleted) nothing can be called removed.
        let head_known = fd.new_path.is_none() || head_syn.is_some();
        if let (Some(op), Some(bs), true) = (&fd.old_path, &base_syn, head_known) {
            for h in &fd.hunks {
                for it in removed_items(bs, head_syn.as_ref(), h.old_lines()) {
                    removed.entry((op.clone(), it.start, it.key.clone())).or_insert_with(|| Changed {
                        kind: ChangeKind::Removed,
                        def: None,
                        id: format!("removed {op} {}", it.key),
                        name: it.path.clone(),
                        file: op.clone(),
                        span: Span { start: it.start, end: it.end },
                    });
                }
            }
        }

        let (Some(np), Some(text)) = (&fd.new_path, &head_text) else { continue };
        if !idx.has_file(np) {
            unindexed.push(np.clone());
            continue;
        }
        let mut attribute = |def: &Def| {
            let kind = match (&base_syn, &head_syn) {
                _ if new_file => ChangeKind::Added,
                (Some(b), Some(h)) => match h.item_at_name_line(def.name_line) {
                    Some(it) if def.kind != SymKind::Module || it.name == def.name.rsplit("::").next().unwrap_or("") => {
                        if b.has_key(&it.key) { ChangeKind::Modified } else { ChangeKind::Added }
                    }
                    _ => ChangeKind::Modified,
                },
                _ => ChangeKind::Modified,
            };
            hits.entry((np.clone(), def.span.start, def.id.clone())).or_insert_with(|| Changed {
                kind,
                def: Some(def.clone()),
                id: def.id.clone(),
                name: def.name.clone(),
                file: np.clone(),
                span: def.span,
            });
        };

        for h in &fd.hunks {
            if h.new_len > 0 {
                for line in h.new_lines() {
                    if let Some(d) = innermost_item(idx, np, line, line) {
                        if d.kind == SymKind::Module && blank(text, line) {
                            continue;
                        }
                        attribute(d);
                    }
                }
            } else {
                // Pure deletion between head lines `new_start` and `new_start + 1`.
                let Some(d) = innermost_item(idx, np, h.new_start.max(1), h.new_start + 1) else { continue };
                if d.kind == SymKind::Module {
                    // Deleting whole items (reported as Removed) or blank lines is not a module change.
                    let leftover = base_text.as_deref().is_some_and(|bt| {
                        let gone = base_syn.as_ref().map(|bs| removed_items(bs, head_syn.as_ref(), h.old_lines())).unwrap_or_default();
                        h.old_lines().any(|l| !blank(bt, l) && !gone.iter().any(|i| i.start <= l && l <= i.end))
                    });
                    if !leftover {
                        continue;
                    }
                }
                attribute(d);
            }
        }
    }

    out.symbols = hits.into_values().chain(removed.into_values()).collect();
    unindexed.sort();
    unindexed.dedup();
    if !unindexed.is_empty() {
        out.notes.push(format!("not in the index (outside the workspace, or not compiled): {}", unindexed.join(", ")));
    }
    if !unparsed.is_empty() {
        out.notes.push(format!("could not parse, so added/modified is a guess: {}", unparsed.join(", ")));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::Hunk;
    use crate::testkit::index_for;
    use std::collections::HashMap;

    struct Mem {
        head: HashMap<String, String>,
        base: HashMap<String, String>,
    }
    impl Sources for Mem {
        fn head(&self, p: &str) -> Option<String> {
            self.head.get(p).cloned()
        }
        fn base(&self, p: &str) -> Option<String> {
            self.base.get(p).cloned()
        }
    }
    fn mem(head: &[(&str, &str)], base: &[(&str, &str)]) -> Mem {
        let c = |v: &[(&str, &str)]| v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect();
        Mem { head: c(head), base: c(base) }
    }
    fn fd(old: Option<&str>, new: Option<&str>, hunks: &[(u32, u32, u32, u32)]) -> FileDiff {
        FileDiff {
            old_path: old.map(Into::into),
            new_path: new.map(Into::into),
            hunks: hunks.iter().map(|&(a, b, c, d)| Hunk { old_start: a, old_len: b, new_start: c, new_len: d }).collect(),
        }
    }
    fn summary(c: &Changes) -> Vec<(char, String)> {
        c.symbols.iter().map(|s| (s.kind.letter(), s.name.clone())).collect()
    }

    // base: lines 1-3 a, 5-7 b (with a blank line 4)
    const BASE: &str = "fn a() {\n    1;\n}\n\nfn b() {\n    2;\n}\n";
    // head: a edited (line 2), new fn c appended after a blank line
    const HEAD: &str = "fn a() {\n    11;\n}\n\nfn b() {\n    2;\n}\n\nfn c() {\n}\n";

    #[test]
    fn modified_added_and_untouched_functions() {
        let m = mem(&[("src/m.rs", HEAD)], &[("src/m.rs", BASE)]);
        let idx = index_for(&[("src/m.rs", HEAD)]);
        // line 2 changed; lines 8-10 added (blank line 8 + fn c)
        let d = [fd(Some("src/m.rs"), Some("src/m.rs"), &[(2, 1, 2, 1), (7, 0, 8, 3)])];
        let c = changed_symbols(&d, &idx, &m);
        assert_eq!(summary(&c), vec![('M', "m::a".to_string()), ('A', "m::c".to_string())], "b is untouched; the blank line is not a module change");
        assert!(c.notes.is_empty());
    }

    #[test]
    fn hunk_outside_any_function_goes_to_the_module() {
        let base = "use a;\nfn f() {}\n";
        let head = "use a;\nuse b;\nfn f() {}\n";
        let m = mem(&[("src/m.rs", head)], &[("src/m.rs", base)]);
        let idx = index_for(&[("src/m.rs", head)]);
        let c = changed_symbols(&[fd(Some("src/m.rs"), Some("src/m.rs"), &[(1, 0, 2, 1)])], &idx, &m);
        assert_eq!(summary(&c), vec![('M', "m".to_string())]);
        assert_eq!(c.symbols[0].span, Span { start: 1, end: 3 });
    }

    #[test]
    fn deleted_function_is_removed_and_does_not_dirty_the_module() {
        let base = "fn keep() {}\n\nfn gone() {\n    1;\n}\n";
        let head = "fn keep() {}\n";
        let m = mem(&[("src/m.rs", head)], &[("src/m.rs", base)]);
        let idx = index_for(&[("src/m.rs", head)]);
        // lines 2-5 removed (blank + gone)
        let c = changed_symbols(&[fd(Some("src/m.rs"), Some("src/m.rs"), &[(2, 4, 1, 0)])], &idx, &m);
        assert_eq!(summary(&c), vec![('R', "gone".to_string())]);
        let r = &c.symbols[0];
        assert!(r.def.is_none() && r.id.starts_with("removed src/m.rs") && r.span == Span { start: 3, end: 5 });
    }

    #[test]
    fn deleting_lines_inside_a_function_modifies_it() {
        let base = "fn f() {\n    1;\n    2;\n}\n";
        let head = "fn f() {\n    1;\n}\n";
        let m = mem(&[("src/m.rs", head)], &[("src/m.rs", base)]);
        let idx = index_for(&[("src/m.rs", head)]);
        let c = changed_symbols(&[fd(Some("src/m.rs"), Some("src/m.rs"), &[(3, 1, 2, 0)])], &idx, &m);
        assert_eq!(summary(&c), vec![('M', "m::f".to_string())]);
    }

    #[test]
    fn deleting_a_loose_line_between_items_is_a_module_change() {
        let base = "fn a() {}\n// note\nfn b() {}\n";
        let head = "fn a() {}\nfn b() {}\n";
        let m = mem(&[("src/m.rs", head)], &[("src/m.rs", base)]);
        let idx = index_for(&[("src/m.rs", head)]);
        let c = changed_symbols(&[fd(Some("src/m.rs"), Some("src/m.rs"), &[(2, 1, 1, 0)])], &idx, &m);
        assert_eq!(summary(&c), vec![('M', "m".to_string())]);
    }

    #[test]
    fn renamed_file_with_an_edit_keeps_identity() {
        let base = "fn a() {\n    1;\n}\nfn b() {}\n";
        let head = "fn a() {\n    2;\n}\nfn b() {}\n";
        let m = mem(&[("src/new.rs", head)], &[("src/old.rs", base)]);
        let idx = index_for(&[("src/new.rs", head)]);
        let c = changed_symbols(&[fd(Some("src/old.rs"), Some("src/new.rs"), &[(2, 1, 2, 1)])], &idx, &m);
        assert_eq!(summary(&c), vec![('M', "new::a".to_string())], "a is modified, not added, and nothing is removed");
        assert_eq!(c.symbols[0].file, "src/new.rs");
    }

    #[test]
    fn pure_rename_changes_nothing() {
        let src = "fn a() {}\n";
        let m = mem(&[("src/new.rs", src)], &[("src/old.rs", src)]);
        let idx = index_for(&[("src/new.rs", src)]);
        let c = changed_symbols(&[fd(Some("src/old.rs"), Some("src/new.rs"), &[])], &idx, &m);
        assert!(c.symbols.is_empty());
    }

    #[test]
    fn added_file_adds_everything_in_it() {
        let head = "use x;\nfn a() {}\nfn b() {}\n";
        let m = mem(&[("src/n.rs", head)], &[]);
        let idx = index_for(&[("src/n.rs", head)]);
        let c = changed_symbols(&[fd(None, Some("src/n.rs"), &[(0, 0, 1, 3)])], &idx, &m);
        assert_eq!(summary(&c), vec![('A', "n".to_string()), ('A', "n::a".to_string()), ('A', "n::b".to_string())]);
    }

    #[test]
    fn deleted_file_removes_everything_in_it() {
        let base = "fn a() {}\nfn b() {}\n";
        let m = mem(&[], &[("src/d.rs", base)]);
        let idx = index_for(&[("src/other.rs", "fn z() {}\n")]);
        let c = changed_symbols(&[fd(Some("src/d.rs"), None, &[(1, 2, 0, 0)])], &idx, &m);
        assert_eq!(summary(&c), vec![('R', "a".to_string()), ('R', "b".to_string())]);
    }

    #[test]
    fn renamed_function_is_removed_plus_added() {
        let base = "fn old_name() {}\n";
        let head = "fn new_name() {}\n";
        let m = mem(&[("src/m.rs", head)], &[("src/m.rs", base)]);
        let idx = index_for(&[("src/m.rs", head)]);
        let c = changed_symbols(&[fd(Some("src/m.rs"), Some("src/m.rs"), &[(1, 1, 1, 1)])], &idx, &m);
        assert_eq!(summary(&c), vec![('A', "m::new_name".to_string()), ('R', "old_name".to_string())]);
    }

    #[test]
    fn nested_function_edit_belongs_to_the_inner_function_only() {
        let src = "fn outer() {\n    fn inner() {\n        1;\n    }\n    inner();\n}\n";
        let m = mem(&[("src/m.rs", src)], &[("src/m.rs", src)]);
        let idx = index_for(&[("src/m.rs", src)]);
        let c = changed_symbols(&[fd(Some("src/m.rs"), Some("src/m.rs"), &[(3, 1, 3, 1)])], &idx, &m);
        assert_eq!(summary(&c), vec![('M', "m::outer::inner".to_string())]);
    }

    #[test]
    fn file_missing_from_the_index_is_noted_not_guessed() {
        let src = "fn a() {}\n";
        let m = mem(&[("spikes/x.rs", src)], &[("spikes/x.rs", "fn a() { 1; }\n")]);
        let idx = index_for(&[("src/m.rs", "fn z() {}\n")]);
        let c = changed_symbols(&[fd(Some("spikes/x.rs"), Some("spikes/x.rs"), &[(1, 1, 1, 1)])], &idx, &m);
        assert!(c.symbols.is_empty());
        assert!(c.notes[0].contains("not in the index") && c.notes[0].contains("spikes/x.rs"));
    }

    #[test]
    fn non_rust_files_are_ignored_and_unparsable_head_is_noted() {
        let broken = "fn a( {\n";
        let m = mem(&[("src/m.rs", broken)], &[("src/m.rs", "fn a() {}\n")]);
        let idx = index_for(&[("src/m.rs", "fn a() {}\n")]);
        let d = [fd(Some("README.md"), Some("README.md"), &[(1, 1, 1, 1)]), fd(Some("src/m.rs"), Some("src/m.rs"), &[(1, 1, 1, 1)])];
        let c = changed_symbols(&d, &idx, &m);
        assert_eq!(summary(&c), vec![('M', "m::a".to_string())], "index spans still attribute the hunk");
        assert!(c.notes.iter().any(|n| n.contains("could not parse")));
    }

    #[test]
    fn test_dw_1_4_editing_enum_variant_attributes_to_enclosing_enum() {
        use protobuf::Message;
        use scip::types::{symbol_information::Kind, Document, Index, Occurrence, SymbolInformation};

        let base = "pub enum Outcome {\n    Applied,\n    Failed {\n        code: u32,\n        msg: String,\n    },\n}\n";
        let head = "pub enum Outcome {\n    Applied(u32),\n    Failed {\n        code: u32,\n    },\n}\n";

        let p = "rust-analyzer cargo fx 0.0.1 ";
        let mut doc = Document::new();
        doc.relative_path = "src/m.rs".into();
        let defs = [
            (format!("{p}m/"), Kind::Module, 1, 1, 6),
            (format!("{p}m/Outcome#"), Kind::Enum, 1, 1, 6),
            (format!("{p}m/Outcome#Applied."), Kind::EnumMember, 2, 2, 2),
            (format!("{p}m/Outcome#Failed."), Kind::EnumMember, 3, 3, 5),
        ];
        for (sym, kind, name_line, start, end) in defs {
            let mut o = Occurrence::new();
            o.range = vec![name_line - 1, 0, 1];
            o.symbol = sym.clone();
            o.symbol_roles = 1;
            o.enclosing_range = vec![start - 1, 0, end - 1, 1];
            doc.occurrences.push(o);
            let mut s = SymbolInformation::new();
            s.symbol = sym;
            s.kind = kind.into();
            doc.symbols.push(s);
        }
        let mut scip_idx = Index::new();
        scip_idx.documents = vec![doc];
        let idx = alsp_index::index_from_scip(&scip_idx.write_to_bytes().unwrap()).unwrap();
        // Sanity check: the raw index resolves line 2 to the Variant itself.
        assert_eq!(idx.innermost("src/m.rs", 2).unwrap().kind, SymKind::Variant);

        let m = mem(&[("src/m.rs", head)], &[("src/m.rs", base)]);
        // Hunk 1: edit on line 2 (Applied variant); Hunk 2: pure deletion inside Failed struct variant (between head lines 4 and 5).
        let d = [fd(Some("src/m.rs"), Some("src/m.rs"), &[(2, 1, 2, 1), (5, 1, 4, 0)])];
        let c = changed_symbols(&d, &idx, &m);
        assert_eq!(summary(&c), vec![('M', "m::Outcome".to_string())]);
        assert_eq!(c.symbols[0].def.as_ref().unwrap().kind, SymKind::Enum);
    }
}
