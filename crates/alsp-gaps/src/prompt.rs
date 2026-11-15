//! What the decision model reads for one mutant: the enclosing function with the mutated region
//! marked, then the test functions the index says reach it. This is the prompt variant (the
//! stronger one in an offline evaluation) with tests chosen through the index, never by name.

use crate::mutants::Mutant;
use alsp_decide::Question;

/// Characters of test code sent per mutant (about 6k tokens): the nearest tests come first.
pub const TEST_CHARS_CAP: usize = 24_000;
/// Characters of the enclosing function sent per mutant.
pub const FUNCTION_CHARS_CAP: usize = 24_000;

pub fn question() -> Question {
    Question {
        instructions: "Would the existing tests fail if this change were made?".into(),
        criteria: Some(
            "Answer yes if at least one existing test in this package would fail (or hang) with the mutated line in place of the original. \
             Answer no if every existing test in this package would still pass with the mutated line in place of the original."
                .into(),
        ),
    }
}

/// A test function the index says reaches the symbol.
pub struct TestFn {
    pub file: String,
    pub line: u32,
    pub text: String,
}

/// Byte offset of 1-based `(line, column)` in `text`; the column counts characters. A column one
/// past the last character is the end of that line; further is outside the text.
fn offset(text: &str, (line, col): (u32, u32)) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line {
        start += text[start..].find('\n')? + 1;
    }
    let row = text[start..].split('\n').next()?;
    let want = col as usize - 1;
    if want > row.chars().count() {
        return None;
    }
    Some(start + row.char_indices().nth(want).map_or(row.len(), |(i, _)| i))
}

/// The function's lines `first..=last`, the mutated region shown as `-` original and `+` new lines.
/// `None` when the mutant's span does not fit the file or function (a stale listing): the caller
/// falls back to cargo-mutants' own diff.
fn marked_function(file_text: &str, first: u32, last: u32, m: &Mutant) -> Option<String> {
    let (s, e) = (offset(file_text, m.start)?, offset(file_text, m.end)?);
    if s > e || m.start.0 < first || m.end.0 > last {
        return None;
    }
    let mutated = format!("{}{}{}", &file_text[..s], m.replacement, &file_text[e..]);
    let (o, n): (Vec<&str>, Vec<&str>) = (file_text.split('\n').collect(), mutated.split('\n').collect());
    let shift = n.len() as i64 - o.len() as i64;
    let at = |v: &[&str], l: i64| usize::try_from(l - 1).ok().and_then(|i| v.get(i)).copied().unwrap_or("").to_string();
    let mut out = Vec::new();
    for ln in first..=last {
        if ln < m.start.0 || ln > m.end.0 {
            out.push(format!("  {}", at(&o, ln as i64)));
        } else if ln == m.start.0 {
            out.extend((m.start.0..=m.end.0).map(|k| format!("- {}", at(&o, k as i64))));
            out.extend((m.start.0 as i64..=m.end.0 as i64 + shift).map(|k| format!("+ {}", at(&n, k))));
        }
    }
    Some(out.join("\n"))
}

fn cap(s: String, max: usize) -> String {
    match s.char_indices().nth(max) {
        None => s,
        Some((cut, _)) => format!("{}\n[function truncated]", &s[..cut]),
    }
}

/// The model's input for `m`, inside the function `name` spanning `first..=last` of `file_text`,
/// with `tests` (nearest first; the ones that do not fit `TEST_CHARS_CAP` are counted, not sent).
pub fn build(m: &Mutant, name: &str, file_text: &str, first: u32, last: u32, tests: &[TestFn]) -> String {
    let body = match marked_function(file_text, first, last, m) {
        Some(b) => cap(b, FUNCTION_CHARS_CAP),
        None => cap(m.diff.clone(), FUNCTION_CHARS_CAP),
    };
    let mut s = format!(
        "Rust package: {}\nFile: {}\nItem: {} (lines {}-{})\nProposed change (mutation) at line {}: {}. Lines marked \"-\" are the original code; lines marked \"+\" replace them.\n\n```rust\n{}\n```\n",
        m.package.as_deref().unwrap_or("unknown"),
        m.file,
        name,
        first,
        last,
        m.start.0,
        m.description,
        body
    );
    s += &format!("\nExisting tests that reach `{name}` (found through the symbol index):\n");
    if tests.is_empty() {
        return s + "No test function reaches this function through the index.\n";
    }
    let (mut used, mut included) = (0usize, 0usize);
    for t in tests {
        let block = format!("\n// {}:{}\n```rust\n{}\n```\n", t.file, t.line, t.text);
        let len = block.chars().count();
        if used + len > TEST_CHARS_CAP {
            break;
        }
        s += &block;
        used += len;
        included += 1;
    }
    if included < tests.len() {
        s += &format!("\n[truncated: {} more test functions omitted]\n", tests.len() - included);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = "fn f(a: u32) -> bool {\n    if a < 3 {\n        true\n    } else {\n        false\n    }\n}\n";

    fn m(start: (u32, u32), end: (u32, u32), replacement: &str) -> Mutant {
        Mutant {
            file: "src/a.rs".into(),
            package: Some("p".into()),
            genre: "BinaryOperator".into(),
            description: "replace < with <= in f".into(),
            replacement: replacement.into(),
            start,
            end,
            diff: "THE DIFF".into(),
        }
    }

    #[test]
    fn offsets_count_characters_not_bytes_and_accept_end_of_line() {
        let t = "ab\nçd = 1;\nlast";
        assert_eq!(offset(t, (1, 1)), Some(0));
        assert_eq!(offset(t, (2, 3)), Some(3 + 3), "ç is two bytes, one column");
        assert_eq!(offset(t, (1, 3)), Some(2), "one past the last char is the line end");
        assert_eq!(offset(t, (1, 4)), None, "two past is outside");
        assert_eq!(offset(t, (9, 1)), None);
        assert_eq!(offset(t, (3, 5)), Some(t.len()));
    }

    #[test]
    fn marks_original_and_mutated_lines_and_keeps_context() {
        let b = build(&m((2, 10), (2, 11), "<="), "p::f", SRC, 1, 7, &[]);
        assert!(b.contains("```rust\n  fn f(a: u32) -> bool {\n-     if a < 3 {\n+     if a <= 3 {\n          true\n"), "{b}");
        assert!(b.starts_with("Rust package: p\nFile: src/a.rs\nItem: p::f (lines 1-7)\nProposed change (mutation) at line 2: replace < with <= in f."), "{b}");
        assert!(b.ends_with("No test function reaches this function through the index.\n"));
    }

    #[test]
    fn a_multi_line_replacement_shifts_the_plus_block() {
        let b = build(&m((2, 5), (6, 6), "return true;"), "p::f", SRC, 1, 7, &[]);
        assert!(b.contains("-     if a < 3 {\n-         true\n-     } else {\n-         false\n-     }\n+     return true;\n  }\n"), "{b}");
    }

    #[test]
    fn a_span_outside_the_file_or_function_falls_back_to_the_tool_diff() {
        assert!(build(&m((40, 1), (40, 2), "x"), "p::f", SRC, 1, 7, &[]).contains("```rust\nTHE DIFF\n```"));
        assert!(build(&m((2, 10), (2, 11), "<="), "p::f", SRC, 3, 7, &[]).contains("THE DIFF"), "mutant starts before the function");
    }

    #[test]
    fn tests_are_listed_with_their_location_and_truncated_at_the_cap() {
        let t = |n: u32, len: usize| TestFn { file: "tests/t.rs".into(), line: n, text: "x".repeat(len) };
        let b = build(&m((2, 10), (2, 11), "<="), "p::f", SRC, 1, 7, &[t(1, 10), t(2, TEST_CHARS_CAP), t(3, 5)]);
        assert!(b.contains("\n// tests/t.rs:1\n```rust\nxxxxxxxxxx\n```\n"));
        assert!(!b.contains("tests/t.rs:2") && !b.contains("tests/t.rs:3"), "stops at the first block over the cap");
        assert!(b.ends_with("\n[truncated: 2 more test functions omitted]\n"), "{b}");
    }

    #[test]
    fn a_huge_function_is_cut_with_a_marker() {
        let big = format!("fn f() {{\n{}}}\n", "    let x = 1;\n".repeat(3000));
        let mu = m((2, 5), (2, 10), "y");
        let b = build(&mu, "p::f", &big, 1, 3002, &[]);
        assert!(b.contains("[function truncated]") && b.chars().count() < FUNCTION_CHARS_CAP + 1000);
    }

    #[test]
    fn question_is_phase_ones_question() {
        let q = question();
        assert_eq!(q.instructions, "Would the existing tests fail if this change were made?");
        assert!(q.criteria.unwrap().contains("at least one existing test"));
    }
}
