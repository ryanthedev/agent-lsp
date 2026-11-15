//! `SddDecider` against a fake `bun` + sdd tree (alsp_decide::stub::FakeSdd).
#![allow(non_snake_case)]

use alsp_decide::stub::FakeSdd;
use alsp_decide::{DecideError, DecisionModel, Item, Question, SddDecider};
use std::time::Duration;

fn q() -> Question {
    Question { instructions: "Would the tests fail?".into(), criteria: Some("Be strict.".into()) }
}

fn items(n: usize) -> Vec<Item> {
    (1..=n).map(|i| Item { id: format!("m{i}"), input: format!("fn f{i}() {{}}") }).collect()
}

fn decider(f: &FakeSdd) -> SddDecider {
    SddDecider::new(&f.bun, &f.root).with_grace(Duration::ZERO)
}

#[test]
fn test_DW_3_2_success_maps_answers_back_by_id_in_order() {
    let f = FakeSdd::new("ok");
    f.p(0.25);
    let out = decider(&f).score(&q(), &items(3)).unwrap();
    assert_eq!(out.iter().map(|s| s.id.as_str()).collect::<Vec<_>>(), ["m1", "m2", "m3"]);
    assert!(out.iter().all(|s| s.p == Some(0.25) && s.error.is_none() && s.input_tokens == 100));
    assert_eq!(f.calls(), 1, "one review is one sdd job");
}

#[test]
fn request_is_a_noul_batch_with_criteria_folded_and_no_key() {
    let f = FakeSdd::new("ok");
    decider(&f).score(&q(), &items(2)).unwrap();
    let v: serde_json::Value = serde_json::from_str(&f.request(1)).unwrap();
    assert_eq!(v["questions"]["q"]["type"], "noul");
    assert_eq!(v["questions"]["q"]["instructions"], "Would the tests fail?\n\nBe strict.");
    assert_eq!(v["items"].as_array().unwrap().len(), 2);
    assert_eq!(v["items"][0], serde_json::json!({"id":"m1","input":"fn f1() {}"}));
    assert!(v.get("maxItems").is_none(), "the default cap is enough for 2 items");
}

#[test]
fn request_dir_and_file_are_private() {
    let f = FakeSdd::new("ok");
    decider(&f).score(&q(), &items(1)).unwrap();
    assert_eq!(f.perms(), ["600 700"]);
}

#[test]
fn passes_the_timeout_through_to_sdd() {
    let f = FakeSdd::new("ok");
    decider(&f).with_timeout(Duration::from_millis(1234)).score(&q(), &items(1)).unwrap();
    let a = f.args();
    let at = a.iter().position(|x| x == "--timeout-ms").expect("flag passed");
    assert_eq!(a[at + 1], "1234");
}

#[test]
fn test_DW_3_2_budget_refusal_is_an_error_naming_the_budget() {
    let f = FakeSdd::new("budget");
    let e = decider(&f).score(&q(), &items(2)).unwrap_err();
    assert!(matches!(e, DecideError::Failed(_)), "{e:?}");
    let t = e.to_string();
    assert_eq!(t, "decision budget exceeded: $5.10 in 24h of $5 daily");
    }

#[test]
fn test_DW_3_2_partial_results_use_answered_items_and_mark_the_rest() {
    let f = FakeSdd::new("partial");
    let out = decider(&f).score(&q(), &items(4)).unwrap();
    assert_eq!(out.len(), 4, "the unknown id `ghost` is ignored");
    assert_eq!(out[0].p, Some(0.7));
    assert_eq!(out[1].error.as_deref(), Some("skipped: maxCostUsd reached"));
    for s in &out[2..] {
        assert_eq!((s.p, s.error.as_deref()), (None, Some("budget exceeded mid-run")));
    }
}

#[test]
fn per_item_errors_and_malformed_answers_stay_per_item() {
    let f = FakeSdd::new("peritem");
    let out = decider(&f).score(&q(), &items(3)).unwrap();
    assert_eq!(out[0].p, Some(0.7));
    assert_eq!(out[1].error.as_deref(), Some("safety block"));
    assert!(out[2].p.is_none() && out[2].error.as_deref().unwrap().contains("probability"), "{:?}", out[2]);
}

#[test]
fn test_DW_3_2_missing_bun_is_unavailable_with_the_path() {
    let f = FakeSdd::new("ok");
    let d = SddDecider::new(f.dir().join("no-such-bun"), &f.root);
    let e = d.score(&q(), &items(1)).unwrap_err();
    assert!(matches!(e, DecideError::Unavailable(_)), "{e:?}");
    assert!(e.to_string().contains("bun not found") && e.to_string().contains("no-such-bun"), "{e}");
}

#[test]
fn failed_job_without_results_is_failed_with_the_reason() {
    let f = FakeSdd::new("fail");
    assert_eq!(decider(&f).score(&q(), &items(1)).unwrap_err(), DecideError::Failed("boom".into()));
}

#[test]
fn clean_exit_with_unreadable_results_is_an_error() {
    let f = FakeSdd::new("garbage");
    let e = decider(&f).score(&q(), &items(1)).unwrap_err();
    assert!(e.to_string().contains("sdd results unusable"), "{e}");
}

#[test]
fn a_wedged_sdd_is_killed_after_the_timeout() {
    let f = FakeSdd::new("sleep");
    let t = std::time::Instant::now();
    let e = decider(&f).with_timeout(Duration::from_millis(200)).score(&q(), &items(1)).unwrap_err();
    assert!(e.to_string().contains("timed out"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(10));
}

#[test]
fn a_timeout_sends_sigterm_first_so_sdd_can_settle_the_job() {
    let f = FakeSdd::new("trap");
    let d = decider(&f).with_timeout(Duration::from_millis(300)).with_term_wait(Duration::from_secs(5));
    let t = std::time::Instant::now();
    let e = d.score(&q(), &items(1)).unwrap_err();
    assert!(e.to_string().contains("timed out"), "{e}");
    assert!(f.got_sigterm(), "the fake bun must have caught SIGTERM");
    assert!(t.elapsed() < Duration::from_secs(4), "exited on TERM, no wait for SIGKILL");
}

#[test]
fn a_child_that_ignores_sigterm_is_killed_after_the_term_wait() {
    let f = FakeSdd::new("deaf");
    let d = decider(&f).with_timeout(Duration::from_millis(200)).with_term_wait(Duration::from_millis(300));
    let t = std::time::Instant::now();
    let e = d.score(&q(), &items(1)).unwrap_err();
    assert!(e.to_string().contains("timed out"), "{e}");
    assert!(t.elapsed() < Duration::from_secs(10));
}

#[test]
fn test_stopped_budget_on_a_clean_exit_leaves_one_partial_note() {
    let f = FakeSdd::new("stopped");
    let d = decider(&f);
    let out = d.score(&q(), &items(4)).unwrap();
    assert_eq!(out.iter().filter(|s| s.p.is_some()).count(), 1);
    assert_eq!(d.take_notes(), ["predicted gaps partial: budget reached after 1 of 4 items"]);
    assert!(d.take_notes().is_empty(), "notes are drained on read");
    f.mode("ok");
    d.score(&q(), &items(2)).unwrap();
    assert!(d.take_notes().is_empty(), "a complete run leaves no note");
}

#[test]
fn a_batch_over_the_cap_is_split_into_jobs_and_reassembled_in_order() {
    let f = FakeSdd::new("ok");
    let out = decider(&f).with_max_items(2).score(&q(), &items(5)).unwrap();
    assert_eq!(f.calls(), 3);
    assert_eq!(out.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), ["m1", "m2", "m3", "m4", "m5"]);
    assert!(out.iter().all(|s| s.p.is_some()));
}

#[test]
fn a_failing_job_stops_later_jobs_and_marks_their_items() {
    let f = FakeSdd::new("ok_then_budget");
    let out = decider(&f).with_max_items(2).score(&q(), &items(6)).unwrap();
    assert_eq!(f.calls(), 2, "the third job is never started");
    assert!(out[..2].iter().all(|s| s.p.is_some()));
    assert!(out[2..].iter().all(|s| s.p.is_none() && s.error.as_deref().unwrap().starts_with("decision budget exceeded")));
}

#[test]
fn a_job_over_500_items_raises_sdds_cap() {
    let f = FakeSdd::new("ok");
    let out = decider(&f).with_max_items(600).score(&q(), &items(501)).unwrap();
    assert_eq!(out.len(), 501);
    let v: serde_json::Value = serde_json::from_str(&f.request(1)).unwrap();
    assert_eq!(v["maxItems"], 501);
}

#[test]
fn empty_inputs_are_never_sent_and_an_all_empty_batch_spawns_nothing() {
    let f = FakeSdd::new("ok");
    let mixed = [Item { id: "a".into(), input: "  ".into() }, Item { id: "b".into(), input: "x".into() }];
    let out = decider(&f).score(&q(), &mixed).unwrap();
    assert_eq!(out[0].error.as_deref(), Some("empty input"));
    assert_eq!(out[1].p, Some(0.7));
    assert!(!f.request(1).contains("\"a\""));
    let only_empty = [Item { id: "a".into(), input: "".into() }];
    assert_eq!(decider(&f).score(&q(), &only_empty).unwrap()[0].error.as_deref(), Some("empty input"));
    assert_eq!(f.calls(), 1);
}

#[test]
fn bad_calls_are_refused_before_anything_is_spawned() {
    let f = FakeSdd::new("ok");
    let d = decider(&f);
    let blank = Question { instructions: " ".into(), criteria: None };
    assert!(matches!(d.score(&blank, &items(1)), Err(DecideError::InvalidInput(_))));
    let dup = [Item { id: "a".into(), input: "x".into() }, Item { id: "a".into(), input: "y".into() }];
    assert!(matches!(d.score(&q(), &dup), Err(DecideError::InvalidInput(_))));
    let noid = [Item { id: "".into(), input: "x".into() }];
    assert!(matches!(d.score(&q(), &noid), Err(DecideError::InvalidInput(_))));
    assert_eq!(d.score(&q(), &[]).unwrap(), vec![]);
    assert_eq!(f.calls(), 0);
}

#[test]
fn test_DW_3_1_no_source_reads_an_api_key_or_the_sdd_config_file() {
    // The needles are built at run time so this file does not contain them.
    let needles = [["GEMINI", "_API_KEY"].concat(), [".config/", "sdd"].concat(), ["x-goog", "-api-key"].concat()];
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let mut stack = vec![crates.to_path_buf()];
    let mut checked = 0;
    while let Some(d) = stack.pop() {
        for e in std::fs::read_dir(&d).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x == "rs") && p.file_name().unwrap() != "sdd_decide.rs" {
                let text = std::fs::read_to_string(&p).unwrap();
                for n in &needles {
                    assert!(!text.contains(n.as_str()), "{} mentions {n}", p.display());
                }
                checked += 1;
            }
        }
    }
    assert!(checked > 20, "walked {checked} source files");
}
