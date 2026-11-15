//! Callers and entry points, walked over the index's references.

use crate::changed::Sources;
use crate::syntax::{analyze, FileSyntax, ItemKind};
use crate::types::{Edge, Provenance};
use alsp_index::{Def, SymKind, SymbolIndex};
use std::collections::{HashMap, HashSet, VecDeque};

/// How far up the call graph entry points are searched, and the most symbols visited doing it.
pub const MAX_DEPTH: u32 = 6;
pub const MAX_VISITED: usize = 5000;
/// Entry points kept per symbol; the rest are counted in the pack's notes.
pub const MAX_ENTRY_POINTS: usize = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum EntryKind {
    Test,
    Main,
    Pub,
}

pub struct Graph<'a> {
    idx: &'a SymbolIndex,
    src: &'a dyn Sources,
    syntax: HashMap<String, Option<FileSyntax>>,
    /// Memo: definition key -> (caller key, edge).
    callers: HashMap<String, Vec<(String, Edge)>>,
}

pub struct Reach {
    pub entry_points: Vec<Edge>,
    /// Entry points found beyond `MAX_ENTRY_POINTS`, or the walk stopped at its limits.
    pub truncated: bool,
}

fn is_main_file(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    name == "main.rs" || name == "build.rs" || file.contains("/bin/") || file.starts_with("bin/") || file.contains("examples/")
}

impl<'a> Graph<'a> {
    pub fn new(idx: &'a SymbolIndex, src: &'a dyn Sources) -> Self {
        Graph { idx, src, syntax: HashMap::new(), callers: HashMap::new() }
    }

    fn syntax(&mut self, file: &str) -> Option<&FileSyntax> {
        let src = self.src;
        self.syntax.entry(file.to_string()).or_insert_with(|| src.head(file).as_deref().and_then(analyze)).as_ref()
    }

    /// Direct users of `id`: the innermost definition around each reference, once per user, at its
    /// first reference line. Imports, uses by the symbol itself and module-level mentions are not
    /// users; for functions and constants only code counts.
    pub fn callers(&mut self, key: &str) -> Vec<Edge> {
        self.caller_nodes(key).into_iter().map(|(_, e)| e).collect()
    }

    fn caller_nodes(&mut self, key: &str) -> Vec<(String, Edge)> {
        if let Some(c) = self.callers.get(key) {
            return c.clone();
        }
        let Some(target) = self.idx.def_by_key(key) else { return Vec::new() };
        let target_is_code = target.kind.is_code();
        let mut by_caller: HashMap<String, (String, Edge)> = HashMap::new();
        for site in self.idx.references(key) {
            if self.syntax(&site.file).is_some_and(|s| s.in_use(site.line)) {
                continue;
            }
            let Some(user) = self.idx.innermost(&site.file, site.line) else { continue };
            if user.key == key || user.kind == SymKind::Module || (target_is_code && !user.kind.is_code()) {
                continue;
            }
            by_caller
                .entry(user.key.clone())
                .and_modify(|(_, e)| {
                    if (&site.file, site.line) < (&e.file, e.line) {
                        (e.file, e.line) = (site.file.clone(), site.line);
                    }
                })
                .or_insert((user.key.clone(), Edge { id: user.id.clone(), file: site.file.clone(), line: site.line, provenance: Provenance::Resolved }));
        }
        let mut v: Vec<(String, Edge)> = by_caller.into_values().collect();
        v.sort_by(|a, b| (&a.1.file, a.1.line, &a.1.id).cmp(&(&b.1.file, b.1.line, &b.1.id)));
        self.callers.insert(key.to_string(), v.clone());
        v
    }

    /// Whether `def` is a `#[test]` function (read from the file's syntax, so not a guess).
    pub fn is_test(&mut self, def: &Def) -> bool {
        def.kind == SymKind::Fn && self.syntax(&def.file).and_then(|s| s.item_at_name_line(def.name_line)).is_some_and(|i| i.is_test)
    }

    fn entry_kind(&mut self, def: &Def) -> Option<(EntryKind, Provenance)> {
        if def.kind != SymKind::Fn {
            return None;
        }
        let main_file = is_main_file(&def.file);
        let item = self.syntax(&def.file)?.item_at_name_line(def.name_line).filter(|i| i.kind == ItemKind::Fn && i.name == def.name.rsplit("::").next().unwrap_or(""))?.clone();
        if item.is_test {
            Some((EntryKind::Test, Provenance::Resolved))
        } else if item.name == "main" && item.top_level && main_file {
            Some((EntryKind::Main, Provenance::Resolved))
        } else if item.is_pub {
            // Visible as `pub` does not mean reachable from outside the crate.
            Some((EntryKind::Pub, Provenance::Heuristic))
        } else {
            None
        }
    }

    /// Entry points (tests, `main`, `pub` functions) that can reach `start`, `start` included when
    /// it is one. Nearest first; tests before `main` before `pub` at equal distance.
    pub fn entry_points(&mut self, start: &Def) -> Reach {
        let mut seen: HashSet<String> = HashSet::from([start.key.clone()]);
        let mut queue: VecDeque<(String, u32)> = VecDeque::from([(start.key.clone(), 0)]);
        let mut found: Vec<(u32, EntryKind, Edge)> = Vec::new();
        let mut truncated = false;
        while let Some((key, depth)) = queue.pop_front() {
            if let Some(def) = self.idx.def_by_key(&key).cloned() {
                if let Some((kind, prov)) = self.entry_kind(&def) {
                    found.push((depth, kind, Edge { id: def.id.clone(), file: def.file.clone(), line: def.name_line, provenance: prov }));
                }
            }
            if depth >= MAX_DEPTH {
                continue;
            }
            for (ck, _) in self.caller_nodes(&key) {
                if seen.len() >= MAX_VISITED {
                    truncated = true;
                    break;
                }
                if seen.insert(ck.clone()) {
                    queue.push_back((ck, depth + 1));
                }
            }
        }
        found.sort_by(|a, b| (a.0, a.1, &a.2.file, a.2.line).cmp(&(b.0, b.1, &b.2.file, b.2.line)));
        if found.len() > MAX_ENTRY_POINTS {
            found.truncate(MAX_ENTRY_POINTS);
            truncated = true;
        }
        Reach { entry_points: found.into_iter().map(|f| f.2).collect(), truncated }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{id_of, index_for};

    struct Mem(HashMap<String, String>);
    impl Sources for Mem {
        fn head(&self, p: &str) -> Option<String> {
            self.0.get(p).cloned()
        }
        fn base(&self, _: &str) -> Option<String> {
            None
        }
    }
    fn setup(files: &[(&str, &str)]) -> (SymbolIndex, Mem) {
        (index_for(files), Mem(files.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()))
    }
    #[test]
    fn is_test_reads_the_attribute_and_only_for_functions() {
        let (idx, src) = setup(&[("src/lib.rs", "fn leaf() {}\n#[test]\nfn t() {\n    leaf();\n}\nstruct S;\n")]);
        let mut g = Graph::new(&idx, &src);
        let find = |n: &str| idx.defs_in("src/lib.rs").find(|d| d.name.ends_with(n)).unwrap().clone();
        assert!(g.is_test(&find("::t")));
        assert!(!g.is_test(&find("::leaf")) && !g.is_test(&find("::S")));
    }

    fn names(idx: &SymbolIndex, edges: &[Edge]) -> Vec<String> {
        edges.iter().map(|e| idx.def(&e.id).unwrap().name.clone()).collect()
    }

    const LIB: &str = "use crate::helper;\npub fn api() {\n    helper();\n}\nfn helper() {\n    leaf();\n    leaf();\n}\nfn leaf() {\n    leaf();\n}\nconst K: u8 = 1;\nfn uses_k() -> u8 {\n    K\n}\n";
    const TESTS: &str = "#[test]\nfn t_api() {\n    api();\n}\n#[test]\nfn t_leaf() {\n    leaf();\n}\nfn not_a_test() {\n    leaf();\n}\n";

    #[test]
    fn callers_are_enclosing_functions_once_each_without_imports_or_self() {
        let (idx, src) = setup(&[("src/lib.rs", LIB), ("src/t.rs", TESTS)]);
        let mut g = Graph::new(&idx, &src);
        let leaf = id_of(&idx, "src/lib.rs", "leaf");
        let c = g.callers(&leaf);
        assert_eq!(names(&idx, &c), vec!["lib::helper", "t::t_leaf", "t::not_a_test"], "helper once despite two calls; leaf's own recursion excluded");
        assert_eq!(c[0].line, 6, "first reference line");
        assert!(c.iter().all(|e| e.provenance == Provenance::Resolved));
        // the `use crate::helper;` line is an import, not a caller (module-level, and in a use range)
        let helper = id_of(&idx, "src/lib.rs", "helper");
        assert_eq!(names(&idx, &g.callers(&helper)), vec!["lib::api"]);
        // a constant is used by the function that reads it
        assert_eq!(names(&idx, &g.callers(&id_of(&idx, "src/lib.rs", "K"))), vec!["lib::uses_k"]);
        assert!(g.callers("nonexistent").is_empty());
        assert_eq!(g.callers(&leaf), c, "second call is served from the memo");
    }

    #[test]
    fn use_inside_a_function_body_is_not_a_call() {
        let src = "fn f() {\n    use crate::g;\n    other();\n}\nfn g() {}\nfn other() {}\n";
        let (idx, mem) = setup(&[("src/m.rs", src)]);
        let mut g = Graph::new(&idx, &mem);
        assert!(g.callers(&id_of(&idx, "src/m.rs", "g")).is_empty());
        assert_eq!(g.callers(&id_of(&idx, "src/m.rs", "other")).len(), 1);
    }

    #[test]
    fn entry_points_are_tests_main_and_pub_fns_nearest_first() {
        let main = "fn main() {\n    api();\n}\n";
        let (idx, src) = setup(&[("src/lib.rs", LIB), ("src/t.rs", TESTS), ("src/main.rs", main)]);
        let mut g = Graph::new(&idx, &src);
        let leaf = idx.def(&id_of(&idx, "src/lib.rs", "leaf")).unwrap().clone();
        let r = g.entry_points(&leaf);
        let got: Vec<(String, Provenance)> = r.entry_points.iter().map(|e| (idx.def(&e.id).unwrap().name.clone(), e.provenance)).collect();
        assert_eq!(
            got,
            vec![
                ("t::t_leaf".to_string(), Provenance::Resolved),  // depth 1, test
                ("lib::api".to_string(), Provenance::Heuristic),  // depth 3 via helper, pub
                ("t::t_api".to_string(), Provenance::Resolved),   // depth 4
                ("main::main".to_string(), Provenance::Resolved), // depth 4
            ]
        );
        assert!(!r.truncated);
        assert_eq!(r.entry_points[0].line, 6, "entry edges point at the definition's name line");
    }

    #[test]
    fn the_changed_symbol_counts_when_it_is_itself_an_entry_point() {
        let (idx, src) = setup(&[("src/lib.rs", LIB)]);
        let mut g = Graph::new(&idx, &src);
        let api = idx.def(&id_of(&idx, "src/lib.rs", "api")).unwrap().clone();
        let r = g.entry_points(&api);
        assert_eq!(names(&idx, &r.entry_points), vec!["lib::api"]);
        // a private leaf with no pub caller has none
        let k = idx.def(&id_of(&idx, "src/lib.rs", "K")).unwrap().clone();
        assert!(g.entry_points(&k).entry_points.is_empty(), "K is not a function; uses_k is private");
    }

    #[test]
    fn walk_depth_is_bounded() {
        let mut src = String::from("pub fn top() {\n    f0();\n}\n");
        for i in 0..=(MAX_DEPTH + 2) {
            src.push_str(&format!("fn f{i}() {{\n    f{}();\n}}\n", i + 1));
        }
        src.push_str(&format!("fn f{}() {{}}\n", MAX_DEPTH + 3));
        let (idx, mem) = setup(&[("src/lib.rs", &src)]);
        let mut g = Graph::new(&idx, &mem);
        let deep = idx.def(&id_of(&idx, "src/lib.rs", &format!("f{}", MAX_DEPTH + 3))).unwrap().clone();
        assert!(g.entry_points(&deep).entry_points.is_empty(), "top is {} hops away, past MAX_DEPTH", MAX_DEPTH + 5);
        let near = idx.def(&id_of(&idx, "src/lib.rs", "f3")).unwrap().clone();
        assert_eq!(g.entry_points(&near).entry_points.len(), 1);
    }

    #[test]
    fn entry_points_are_capped_and_flagged() {
        let mut src = String::from("fn target() {}\n");
        for i in 0..(MAX_ENTRY_POINTS + 5) {
            src.push_str(&format!("#[test]\nfn t{i:03}() {{\n    target();\n}}\n"));
        }
        let (idx, mem) = setup(&[("src/lib.rs", &src)]);
        let mut g = Graph::new(&idx, &mem);
        let t = idx.def(&id_of(&idx, "src/lib.rs", "target")).unwrap().clone();
        let r = g.entry_points(&t);
        assert_eq!(r.entry_points.len(), MAX_ENTRY_POINTS);
        assert!(r.truncated);
    }

    #[test]
    fn cyclic_calls_terminate() {
        let src = "pub fn a() {\n    b();\n}\nfn b() {\n    a();\n}\n";
        let (idx, mem) = setup(&[("src/lib.rs", src)]);
        let mut g = Graph::new(&idx, &mem);
        let b = idx.def(&id_of(&idx, "src/lib.rs", "b")).unwrap().clone();
        assert_eq!(names(&idx, &g.entry_points(&b).entry_points), vec!["lib::a"]);
    }

    #[test]
    fn main_only_counts_in_a_binary_root() {
        let (idx, src) = setup(&[("src/lib.rs", "fn main() {}\nfn x() {}\n")]);
        let mut g = Graph::new(&idx, &src);
        let m = idx.def(&id_of(&idx, "src/lib.rs", "main")).unwrap().clone();
        assert!(g.entry_points(&m).entry_points.is_empty());
        assert!(is_main_file("src/bin/tool.rs") && is_main_file("main.rs") && is_main_file("crates/x/src/main.rs") && !is_main_file("src/lib.rs"));
    }

    #[test]
    fn unparsable_file_yields_no_entry_classification() {
        // Index knows the file, but its text cannot be parsed: no syntactic facts, no crash.
        let good = "pub fn f() {}\n";
        let idx = index_for(&[("src/lib.rs", good)]);
        let broken = Mem(HashMap::from([("src/lib.rs".to_string(), "pub fn f( {".to_string())]));
        let mut g = Graph::new(&idx, &broken);
        let f = idx.def(&id_of(&idx, "src/lib.rs", "f")).unwrap().clone();
        assert!(g.entry_points(&f).entry_points.is_empty());
    }
}
