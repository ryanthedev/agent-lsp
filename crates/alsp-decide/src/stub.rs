//! A fake `bun` and sdd tree for tests (feature `testkit`). The fake `bun` is a shell script that
//! behaves like `bun run <sdd>/src/cli/decide.ts ...` as chosen by a `mode` file next to it, so it
//! works both in-process (`SddDecider::new(fake.bun, &fake.root)`) and as a child of the alsp
//! binary (`ALSP_BUN`, `ALSP_SDD_ROOT`).
//!
//! Modes: `ok` (every item answered with the `p` file's value), `budget` (exit 1, no results),
//! `partial` (exit 1; item 1 answered, item 2 skipped, an unknown id answered, the rest missing),
//! `peritem` (exit 0; item 1 answered, item 2 an error, item 3 a malformed answer),
//! `stopped` (exit 0, results carry top-level `stopped:"budget"`: item 1 answered, item 2 skipped), `trap` (sleeps; on SIGTERM writes `term.marker` and exits), `deaf` (ignores SIGTERM), `ok_then_budget` (first call ok, later calls refused), `fail`, `garbage` (exit 0, results not
//! JSON), `sleep` (never finishes).

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

const SCRIPT: &str = r#"#!/bin/sh
D=$(dirname "$0")
while [ $# -gt 2 ]; do echo "arg: $1" >> "$D/args.log"; shift; done
req=$1; res=$2
n=$(( $(cat "$D/calls" 2>/dev/null || echo 0) + 1 )); echo $n > "$D/calls"
cp "$req" "$D/req.$n.json"
perm() { stat -f %Lp "$1" 2>/dev/null || stat -c %a "$1"; }
echo "$(perm "$req") $(perm "$(dirname "$req")")" >> "$D/perms.log"
mode=$(cat "$D/mode"); p=$(cat "$D/p" 2>/dev/null || echo 0.7)
[ "$mode" = ok_then_budget ] && { [ $n = 1 ] && mode=ok || mode=budget; }
ids=$(grep -o '"id":"[^"]*"' "$req" | sed 's/"id":"\(.*\)"/\1/')
tok=$(cat "$D/tokens" 2>/dev/null || echo 100)
ans() { printf '{"id":"%s","answers":{"q":{"noul":%s,"p":%s}},"inputTokens":%s,"costUsd":0.00001,"seconds":0.1}' "$1" "$2" "$2" "$tok"; }
emit() { # emit <kind>
  printf '{"model":"m",%s"results":[' "$([ "$1" = stopped ] && echo '"stopped":"budget",')" > "$res"; i=0; sep=
  for id in $ids; do i=$((i+1))
    case "$1:$i" in
      ok:*) printf '%s' "$sep" >> "$res"; ans "$id" "$p" >> "$res" ;;
      partial:1) ans "$id" "$p" >> "$res" ;;
      partial:2) printf ',{"id":"%s","error":"skipped: maxCostUsd reached"}' "$id" >> "$res" ;;
      stopped:1) ans "$id" "$p" >> "$res" ;;
      stopped:2) printf ',{"id":"%s","error":"skipped: maxCostUsd reached"}' "$id" >> "$res" ;;
      peritem:1) ans "$id" "$p" >> "$res" ;;
      peritem:2) printf ',{"id":"%s","error":"safety block"}' "$id" >> "$res" ;;
      peritem:3) printf ',{"id":"%s","answers":{"q":{"noul":1.5}}}' "$id" >> "$res" ;;
    esac
    sep=,
  done
  [ "$1" = partial ] && printf ',' >> "$res" && ans ghost 0.1 >> "$res"
  printf ']}' >> "$res"
}
case "$mode" in
  ok) emit ok; echo "$res"; exit 0 ;;
  peritem) emit peritem; echo "$res"; exit 0 ;;
  stopped) emit stopped; echo "$res"; exit 0 ;;
  trap) trap 'echo term > "$D/term.marker"; kill $sp; exit 143' TERM; sleep 30 & sp=$!; wait ;;
  deaf) trap '' TERM; sleep 30 & wait ;;
  partial) emit partial; echo "decide: gemini: exited with code 1: gemini-decide: budget exceeded mid-run (log: /tmp/x/job.log)" >&2; exit 1 ;;
  budget) echo "decide: gemini: exited with code 1: gemini-decide: decision budget exceeded: \$5.10 in 24h of \$5 daily (log: /tmp/x/job.log)" >&2; exit 1 ;;
  fail) echo "decide: boom" >&2; exit 1 ;;
  garbage) echo "not json" > "$res"; echo "$res"; exit 0 ;;
  sleep) exec sleep 30 ;;
esac
exit 3
"#;

pub struct FakeSdd {
    dir: TempDir,
    /// Pass to `SddDecider::new`, or as `ALSP_SDD_ROOT`.
    pub root: PathBuf,
    /// Pass to `SddDecider::new`, or as `ALSP_BUN`.
    pub bun: PathBuf,
}

impl FakeSdd {
    pub fn new(mode: &str) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let base = fs::canonicalize(dir.path()).expect("canonical temp dir");
        let root = base.join("sdd");
        fs::create_dir_all(root.join("src/cli")).unwrap();
        fs::write(root.join("src/cli/decide.ts"), "// fake\n").unwrap();
        let bun = base.join("bun");
        fs::write(&bun, SCRIPT).unwrap();
        fs::set_permissions(&bun, fs::Permissions::from_mode(0o755)).unwrap();
        let f = Self { dir, root, bun };
        f.mode(mode);
        f
    }

    pub fn mode(&self, mode: &str) {
        fs::write(self.path("mode"), mode).unwrap();
    }

    /// P(yes) answered for every item (default 0.7).
    pub fn p(&self, p: f64) {
        fs::write(self.path("p"), p.to_string()).unwrap();
    }

    /// Input tokens reported per answered item (default 100).
    pub fn tokens(&self, n: u64) {
        fs::write(self.path("tokens"), n.to_string()).unwrap();
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// How many times the fake `bun` ran.
    pub fn calls(&self) -> usize {
        fs::read_to_string(self.path("calls")).ok().and_then(|s| s.trim().parse().ok()).unwrap_or(0)
    }

    /// Items across every request sent so far.
    pub fn items_sent(&self) -> usize {
        (1..=self.calls()).map(|n| self.request(n).matches("\"id\":").count()).sum()
    }

    /// The request file of the n-th call (from 1), as sdd received it.
    pub fn request(&self, n: usize) -> String {
        fs::read_to_string(self.path(&format!("req.{n}.json"))).expect("request recorded")
    }

    /// `"<file perms> <dir perms>"` of each request, in octal, one per call.
    pub fn perms(&self) -> Vec<String> {
        fs::read_to_string(self.path("perms.log")).unwrap_or_default().lines().map(str::to_string).collect()
    }

    /// Flags the fake saw before the request path (e.g. `--timeout-ms`).
    pub fn args(&self) -> Vec<String> {
        fs::read_to_string(self.path("args.log")).unwrap_or_default().lines().filter_map(|l| l.strip_prefix("arg: ")).map(str::to_string).collect()
    }

    /// Whether the fake saw SIGTERM (mode `trap`).
    pub fn got_sigterm(&self) -> bool {
        self.path("term.marker").exists()
    }

    pub fn dir(&self) -> &Path {
        self.dir.path()
    }
}
