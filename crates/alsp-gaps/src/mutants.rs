//! `cargo mutants --list`: the mutations cargo-mutants would try, as data. Nothing is executed.
//! The listing is the only way this crate talks to cargo-mutants, and it always passes `--list`.

use alsp_coverage::exec;
use serde_json::Value;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct MutantsTool {
    /// `cargo`; looked up on `PATH` when not a path (`$ALSP_CARGO` overrides the default).
    pub program: std::path::PathBuf,
    pub timeout: Duration,
}

impl Default for MutantsTool {
    fn default() -> Self {
        MutantsTool { program: std::env::var_os("ALSP_CARGO").map(Into::into).unwrap_or_else(|| "cargo".into()), timeout: Duration::from_secs(5 * 60) }
    }
}

/// One mutation, with 1-based line and column ends as cargo-mutants reports them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Mutant {
    pub file: String,
    pub package: Option<String>,
    pub genre: String,
    /// What the mutation does and where on its line, e.g. `replace < with <= in f (col 7)`.
    pub description: String,
    pub replacement: String,
    pub start: (u32, u32),
    pub end: (u32, u32),
    /// cargo-mutants' own unified diff of the mutation.
    pub diff: String,
}

/// The command that lists mutants for `files` of `tree`. Always `--list`.
pub fn list_command(tool: &MutantsTool, tree: &Path, files: &[&str]) -> Command {
    let mut cmd = Command::new(&tool.program);
    cmd.args(["mutants", "--list", "--json", "--diff", "--dir"]).arg(tree);
    for f in files {
        cmd.arg(format!("--file={f}"));
    }
    cmd.current_dir(tree);
    cmd
}

/// Refuse any cargo-mutants command that could execute mutants: a run without `--list` builds and
/// tests every mutant, which is exactly what this tool exists to avoid.
pub fn ensure_list_only(cmd: &Command) -> Result<(), String> {
    if cmd.get_args().any(|a| a == "--list") {
        Ok(())
    } else {
        Err("refusing to run cargo-mutants without --list".into())
    }
}

/// Parse the JSON listing. Entries that lack a usable file, span or name are counted, not guessed at.
pub fn parse(json: &str) -> Result<(Vec<Mutant>, usize), String> {
    let v: Value = serde_json::from_str(json).map_err(|e| format!("cargo mutants --list output is not JSON: {e}"))?;
    let arr = v.as_array().ok_or("cargo mutants --list output is not a list")?;
    let (mut out, mut skipped) = (Vec::new(), 0);
    for m in arr {
        match entry(m) {
            Some(x) => out.push(x),
            None => skipped += 1,
        }
    }
    Ok((out, skipped))
}

fn entry(m: &Value) -> Option<Mutant> {
    let s = |k: &str| m.get(k).and_then(Value::as_str);
    let pos = |p: &Value| -> Option<(u32, u32)> {
        let (l, c) = (p.get("line")?.as_u64()?, p.get("column")?.as_u64()?);
        (l >= 1 && c >= 1).then_some((u32::try_from(l).ok()?, u32::try_from(c).ok()?))
    };
    let span = m.get("span")?;
    let (start, end) = (pos(span.get("start")?)?, pos(span.get("end")?)?);
    if end < start {
        return None;
    }
    let file = s("file")?;
    if file.is_empty() || file.starts_with('/') || file.split('/').any(|c| c == "..") {
        return None;
    }
    let name = s("name")?;
    // The column stays: two operators on one line (`a + b + c`) are different mutants.
    let what = name.strip_prefix(&format!("{file}:{}:{}: ", start.0, start.1)).unwrap_or(name);
    let description = format!("{what} (col {})", start.1);
    Some(Mutant {
        file: file.to_string(),
        package: s("package").map(String::from),
        genre: s("genre").unwrap_or("").to_string(),
        description,
        replacement: s("replacement").unwrap_or("").to_string(),
        start,
        end,
        diff: s("diff").unwrap_or("").to_string(),
    })
}

/// List the mutants of `files`. `Err` is a one-line reason (cargo-mutants missing, broken manifest,
/// timeout, unreadable output).
pub fn list(tool: &MutantsTool, tree: &Path, files: &[&str], env: &[(String, String)]) -> Result<(Vec<Mutant>, usize), String> {
    let mut cmd = list_command(tool, tree, files);
    cmd.envs(env.iter().map(|(k, v)| (k, v)));
    ensure_list_only(&cmd)?;
    let out = exec::run(&mut cmd, tool.timeout, 600)?;
    if !out.success() {
        return Err(format!("`cargo mutants --list` exited {}: {}", out.code.map_or("by signal".into(), |c| c.to_string()), out.stderr));
    }
    parse(&out.stdout)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE: &str = r#"[{"file":"src/a.rs","package":"p","genre":"BinaryOperator","name":"src/a.rs:3:7: replace < with <= in f","replacement":"<=","span":{"start":{"line":3,"column":7},"end":{"line":3,"column":8}},"diff":"--- x\n+++ y\n"}]"#;

    #[test]
    fn parses_a_listing_and_replaces_the_position_prefix_with_a_column_suffix() {
        let (m, skipped) = parse(ONE).unwrap();
        assert_eq!(skipped, 0);
        assert_eq!((m[0].file.as_str(), m[0].description.as_str(), m[0].genre.as_str(), m[0].start, m[0].end), ("src/a.rs", "replace < with <= in f (col 7)", "BinaryOperator", (3, 7), (3, 8)));
        assert_eq!(m[0].package.as_deref(), Some("p"));
    }

    #[test]
    fn malformed_entries_are_counted_and_unsafe_paths_refused() {
        let bad = r#"[{"file":"/etc/passwd","name":"n","span":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}},
            {"file":"../x.rs","name":"n","span":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}},
            {"file":"a.rs","name":"n","span":{"start":{"line":0,"column":1},"end":{"line":1,"column":2}}},
            {"file":"a.rs","name":"n","span":{"start":{"line":5,"column":1},"end":{"line":4,"column":2}}},
            {"file":"a.rs","span":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}},
            {"file":"a.rs","name":"n"}, 7]"#;
        let (m, skipped) = parse(bad).unwrap();
        assert!(m.is_empty() && skipped == 7);
        assert!(parse("nope").unwrap_err().contains("not JSON"));
        assert!(parse("{}").unwrap_err().contains("not a list"));
        assert_eq!(parse("[]").unwrap(), (vec![], 0));
    }

    #[test]
    fn test_DW_3_2_the_listing_command_always_says_list_and_the_guard_refuses_anything_else() {
        let t = MutantsTool::default();
        let cmd = list_command(&t, Path::new("/t"), &["a.rs", "b.rs"]);
        let args: Vec<String> = cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
        assert_eq!(args[..3], ["mutants", "--list", "--json"]);
        assert!(args.contains(&"--file=a.rs".to_string()) && args.contains(&"--file=b.rs".to_string()));
        assert!(ensure_list_only(&cmd).is_ok());
        let mut run = Command::new("cargo");
        run.args(["mutants", "--dir", "/t"]);
        assert!(ensure_list_only(&run).unwrap_err().contains("without --list"));
        let mut other = Command::new("cargo");
        other.args(["mutants", "--list-files"]);
        assert!(ensure_list_only(&other).is_err(), "a lookalike flag is not --list");
    }

    #[test]
    fn a_failing_or_missing_lister_is_an_error_with_the_reason() {
        let d = tempfile::tempdir().unwrap();
        let t = MutantsTool { program: "/no/such/cargo".into(), ..Default::default() };
        assert!(list(&t, d.path(), &["a.rs"], &[]).unwrap_err().contains("cannot run"));
    }
}
