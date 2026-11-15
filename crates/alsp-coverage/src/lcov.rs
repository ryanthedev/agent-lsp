//! The lcov text `cargo llvm-cov --lcov` writes: per source file, hit counts of the lines the
//! compiler instrumented (`DA:line,count`) and of every branch (`BRDA:line,block,branch,taken`).
//! This is llvm-cov's own line attribution, so line numbers agree with `cargo llvm-cov report`.

use std::collections::{BTreeMap, HashMap};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileCov {
    /// Instrumented line -> execution count (0 = never run). Lines absent are not code.
    pub lines: BTreeMap<u32, u64>,
    /// Line -> one entry per branch: whether it was taken.
    pub branches: BTreeMap<u32, Vec<bool>>,
}

/// Absolute source path -> coverage. Records for the same file (one per instantiation group) are
/// merged by taking the larger count per line, which is what a union of runs means.
pub fn parse(text: &str) -> HashMap<String, FileCov> {
    let mut out: HashMap<String, FileCov> = HashMap::new();
    let mut current: Option<String> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if let Some(path) = line.strip_prefix("SF:") {
            current = Some(path.to_string());
            out.entry(path.to_string()).or_default();
        } else if line == "end_of_record" {
            current = None;
        } else if let (Some(file), Some(rest)) = (&current, line.strip_prefix("DA:")) {
            let mut it = rest.split(',');
            if let (Some(Ok(l)), Some(Ok(c))) = (it.next().map(str::parse::<u32>), it.next().map(str::parse::<u64>)) {
                let slot = out.get_mut(file).expect("entry exists").lines.entry(l).or_insert(0);
                *slot = (*slot).max(c);
            }
        } else if let (Some(file), Some(rest)) = (&current, line.strip_prefix("BRDA:")) {
            // line,block,branch,taken  (taken is `-` when the branch was never evaluated)
            let f: Vec<&str> = rest.split(',').collect();
            if let (Some(Ok(l)), Some(taken)) = (f.first().map(|s| s.parse::<u32>()), f.get(3)) {
                let hit = taken.parse::<u64>().is_ok_and(|n| n > 0);
                out.get_mut(file).expect("entry exists").branches.entry(l).or_default().push(hit);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_lines_and_branches_per_file_and_ignores_the_rest() {
        let t = "TN:\nSF:/t/a.rs\nFN:1,f\nFNDA:3,f\nDA:1,3\nDA:2,0\nDA:bad\nDA:3,x\nBRDA:2,0,0,-\nBRDA:2,0,1,4\nBRF:2\nend_of_record\nSF:/t/b.rs\nDA:9,1\nend_of_record\n";
        let m = parse(t);
        let a = &m["/t/a.rs"];
        assert_eq!(a.lines, BTreeMap::from([(1, 3), (2, 0)]));
        assert_eq!(a.branches[&2], vec![false, true]);
        assert_eq!(m["/t/b.rs"].lines[&9], 1);
        assert!(m["/t/b.rs"].branches.is_empty());
    }

    #[test]
    fn repeated_records_for_one_file_keep_the_larger_count() {
        let m = parse("SF:/a.rs\nDA:1,0\nDA:2,5\nend_of_record\nSF:/a.rs\nDA:1,2\nDA:2,1\nend_of_record\n");
        assert_eq!(m["/a.rs"].lines, BTreeMap::from([(1, 2), (2, 5)]));
    }

    #[test]
    fn empty_and_garbage_input_give_nothing() {
        assert!(parse("").is_empty());
        assert!(parse("DA:1,1\nhello\n").is_empty(), "records outside a file are ignored");
    }
}
