//! Test doubles: a fixture tree on disk, a fake `cargo` that answers `metadata`, `llvm-cov` and
//! `mutants` from files in a fixture directory (logging every call), and builders for packs.

use alsp_evidence::{ChangeKind, EvidencePack, PackSource, SymbolEvidence};
use alsp_index::{IndexState, IndexStatus, Span, UnitReport};
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const SCRIPT: &str = r#"#!/bin/sh
echo "$@" >> "$ALSP_FX/calls.log"
case "$1" in
  metadata) cat "$ALSP_FX/metadata.json" ;;
  mutants)
    if [ -f "$ALSP_FX/mutants.fail" ]; then echo "error: cannot list mutants" >&2; exit 1; fi
    cat "$ALSP_FX/mutants.json" ;;
  llvm-cov)
    if [ "$2" = --version ]; then
      if [ -f "$ALSP_FX/no_llvm_cov" ]; then echo "error: no such command: llvm-cov" >&2; exit 101; fi
      echo "cargo-llvm-cov 9.9.9"; exit 0
    fi
    case "$*" in *--branch*) if [ -f "$ALSP_FX/no_branch" ]; then echo "error: --branch requires nightly compiler" >&2; exit 1; fi ;; esac
    if [ -f "$ALSP_FX/fail" ]; then echo "test t::bad ... FAILED" >&2; echo "error: test failed, to rerun pass --lib" >&2; exit 101; fi
    if [ -f "$ALSP_FX/lock_wait" ]; then echo "    Blocking waiting for file lock on build directory" >&2; sleep 0.3; fi
    while [ $# -gt 0 ]; do [ "$1" = --output-path ] && out="$2"; shift; done
    sed "s#@TREE@#$ALSP_TREE#g" "$ALSP_FX/out.lcov" > "$out" ;;
esac
"#;

/// Install the fake `cargo` into `fx_dir` and return its path.
pub fn install_fake_cargo(fx_dir: &Path) -> PathBuf {
    std::fs::create_dir_all(fx_dir).unwrap();
    let cargo = fx_dir.join("cargo");
    std::fs::write(&cargo, SCRIPT).unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    cargo
}

/// `cargo metadata` output for a single package `fx` at the root of `tree`.
pub fn metadata_json(tree: &Path) -> String {
    format!(r#"{{"workspace_root":"{t}","packages":[{{"name":"fx","manifest_path":"{t}/Cargo.toml"}}]}}"#, t = tree.display())
}

pub struct Fx {
    pub root: tempfile::TempDir,
    pub tree: PathBuf,
    pub fx: PathBuf,
    pub cargo: PathBuf,
    pub src: PackSource,
}

impl Fx {
    /// A package `fx` at the root of a fresh tree holding `files` (path, content).
    pub fn new(files: &[(&str, &str)]) -> Fx {
        let root = tempfile::tempdir().unwrap();
        let tree = std::fs::canonicalize(root.path()).unwrap().join("tree");
        let fx = tree.parent().unwrap().join("fx");
        std::fs::create_dir_all(&fx).unwrap();
        write(&tree, "Cargo.toml", "[package]\nname = \"fx\"\nversion = \"0.1.0\"\n");
        for (p, c) in files {
            write(&tree, p, c);
        }
        std::fs::write(fx.join("metadata.json"), metadata_json(&tree)).unwrap();
        let cargo = install_fake_cargo(&fx);
        std::fs::write(fx.join("out.lcov"), "").unwrap();
        std::fs::write(fx.join("mutants.json"), "[]").unwrap();
        let src = PackSource {
            tree: tree.clone(),
            cache_dir: tree.parent().unwrap().join("cache"),
            build_env: vec![("ALSP_FX".into(), fx.to_string_lossy().into()), ("ALSP_TREE".into(), tree.to_string_lossy().into())],
            changed_lines: BTreeMap::new(),
        };
        Fx { root, tree, fx, cargo, src }
    }

    pub fn set_lcov(&self, lcov: &str) {
        std::fs::write(self.fx.join("out.lcov"), lcov).unwrap();
    }

    pub fn set_mutants(&self, json: &str) {
        std::fs::write(self.fx.join("mutants.json"), json).unwrap();
    }

    /// Switch a fake behaviour on: `fail`, `no_branch`, `no_llvm_cov`, `lock_wait`, `mutants.fail`.
    pub fn flag(&self, name: &str, on: bool) {
        let p = self.fx.join(name);
        if on {
            std::fs::write(p, "").unwrap();
        } else {
            let _ = std::fs::remove_file(p);
        }
    }

    /// Every call the fake cargo received, one argument line each.
    pub fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.fx.join("calls.log")).unwrap_or_default().lines().map(String::from).collect()
    }

    pub fn write_source(&self, rel: &str, content: &str) {
        write(&self.tree, rel, content);
    }
}

fn write(dir: &Path, rel: &str, body: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

/// lcov text for `(file, [(line, count)], [(line, [branch taken])])`, paths under the fixture tree.
pub fn lcov(files: &[(&str, &[(u32, u64)], &[(u32, &[bool])])]) -> String {
    let mut s = String::new();
    for (file, lines, branches) in files {
        s.push_str(&format!("SF:@TREE@/{file}\n"));
        for (l, c) in *lines {
            s.push_str(&format!("DA:{l},{c}\n"));
        }
        for (l, bs) in *branches {
            for (i, b) in bs.iter().enumerate() {
                s.push_str(&format!("BRDA:{l},0,{i},{}\n", if *b { "1" } else { "-" }));
            }
        }
        s.push_str("end_of_record\n");
    }
    s
}

/// A symbol with no edges; `id` ends in `().` for a function.
pub fn sym(file: &str, name: &str, id: &str, kind: ChangeKind, start: u32, end: u32) -> SymbolEvidence {
    SymbolEvidence {
        id: id.into(),
        name: name.into(),
        file: file.into(),
        span: Span { start, end },
        kind,
        callers: vec![],
        entry_points: vec![],
        coverage: None,
        gaps: vec![],
    }
}

pub fn pack(symbols: Vec<SymbolEvidence>) -> EvidencePack {
    EvidencePack {
        base: "b".into(),
        head: "h".into(),
        index: IndexState { status: IndexStatus::Fresh, rebuilt: false, units: vec![UnitReport { path: ".".into(), ok: true, detail: None }], millis: 0, notes: vec![] },
        symbols,
        notes: vec![],
    }
}
