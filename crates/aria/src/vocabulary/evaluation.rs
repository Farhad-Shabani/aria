use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use aria_kit::cancel::CancelToken;
use serde_json::{Value, json};

use super::catalog::Catalog;
use super::extract::{Extractor, LocalExtractor};
use super::{PersonalRules, Vocabulary, discover, pipeline_key, select};

const FILES: &[(&str, &str)] = &[
    ("java/Main.java", "class LedgerClient {} // ledger-api\n"),
    (
        "swift/Client.swift",
        "let client = URLSession.shared // watch-link\n",
    ),
    ("rust/lib.rs", "// aria-kit\npub struct VoiceBridge;\n"),
    ("python/task.py", "# batch_job\nclass QueueWorker: pass\n"),
    ("web/page.html", "<p>design.tokens</p>\n"),
    (
        "notes/report.txt",
        "Harbor-Plan Persephone URLSession sync-id sync_id\n",
    ),
];
const STRUCTURED: &[(&str, &str)] = &[
    ("ledger dash api", "ledger-api"),
    ("watch dash link", "watch-link"),
    ("aria dash kit", "aria-kit"),
    ("batch underscore job", "batch_job"),
    ("design dot tokens", "design.tokens"),
    ("harbor dash plan", "Harbor-Plan"),
];
const UNSUPPORTED: &[(&str, &str)] = &[("persefony", "Persephone"), ("url session", "URLSession")];
const NEGATIVES: &[&str] = &[
    "ordinary prose stays ordinary",
    "sync i d",
    "sync dash id",
    "sync underscore id",
    "aria dot kit",
    "ledger-api",
];

fn fixture(parent: &Path) -> std::path::PathBuf {
    let root = parent.join("fixture");
    for (relative, text) in FILES {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
    root
}

fn corrected(matcher: &Vocabulary, input: &str) -> String {
    matcher.correct(input).unwrap_or_else(|| input.to_owned())
}

fn case_counts(matcher: &Vocabulary) -> Value {
    json!({
        "structured": {
            "denominator": STRUCTURED.len(),
            "exact_target": STRUCTURED.iter().filter(|(input, target)| corrected(matcher, input) == *target).count(),
        },
        "unsupported_plain_name_or_spoken_camel_case": {
            "denominator": UNSUPPORTED.len(),
            "exact_target": UNSUPPORTED.iter().filter(|(input, target)| corrected(matcher, input) == *target).count(),
        },
        "negative_or_confusable": {
            "denominator": NEGATIVES.len(),
            "changed": NEGATIVES.iter().filter(|input| matcher.correct(input).is_some()).count(),
        },
        "split_segments": {
            "denominator": 2,
            "changed": (["open aria dash", "kit"].iter().filter(|input| matcher.correct(input).is_some()).count()),
        },
    })
}

fn measure(
    root: &Path,
    directory: &Path,
    repetitions: usize,
    fixed_cases: bool,
) -> Result<Value, Value> {
    let cancel = CancelToken::never();
    let key = pipeline_key(LocalExtractor.cache_key())
        .ok_or_else(|| failure("identity", "invalid_key"))?;
    let mut catalog = Catalog::open(directory, cancel.clone())
        .map_err(|error| failure("catalog_open", catalog_category(&error)))?;
    let mut samples = Vec::new();
    let mut counts = Value::Null;
    let mut cases = Value::Null;
    for _ in 0..repetitions {
        let started = Instant::now();
        let discovery = discover::scan_cached(root, &cancel, &[], &mut LocalExtractor)
            .map_err(|error| failure("cold_scan", scan_category(&error)))?;
        let cold_scan_us = started.elapsed().as_secs_f64() * 1e6;
        let started = Instant::now();
        let terms = select::terms(&discovery.sources, &cancel)
            .map_err(|error| failure("selection", selection_category(&error)))?;
        let select_us = started.elapsed().as_secs_f64() * 1e6;
        let started = Instant::now();
        let matcher = Vocabulary::compile_with(&terms, PersonalRules::default(), &cancel).map_err(
            |error| {
                failure(
                    "compile",
                    match error {
                        super::MatchError::Cancelled => "cancelled",
                        super::MatchError::TooLarge => "too_large",
                        super::MatchError::InvalidName => "invalid_name",
                    },
                )
            },
        )?;
        let compile_us = started.elapsed().as_secs_f64() * 1e6;
        let started = Instant::now();
        catalog
            .refresh_cancellable(&discovery, &key, &cancel)
            .map_err(|error| failure("catalog_commit", catalog_category(&error)))?;
        let catalog_commit_us = started.elapsed().as_secs_f64() * 1e6;
        let started = Instant::now();
        let cached = catalog
            .load(&discovery.root, &key)
            .map_err(|error| failure("catalog_load", catalog_category(&error)))?
            .ok_or_else(|| failure("catalog_load", "missing_scope"))?;
        let catalog_load_us = started.elapsed().as_secs_f64() * 1e6;
        let started = Instant::now();
        let warm = discover::scan_cached(root, &cancel, &cached.sources, &mut LocalExtractor)
            .map_err(|error| failure("cached_scan", scan_category(&error)))?;
        let cached_scan_us = started.elapsed().as_secs_f64() * 1e6;
        if select::terms(&warm.sources, &cancel)
            .map_err(|error| failure("cached_selection", selection_category(&error)))?
            != terms
        {
            return Err(failure("cached_selection", "selection_changed"));
        }
        counts = json!({
            "sources": discovery.sources.len(),
            "candidates": discovery.sources.iter().map(|source| source.candidates.len()).sum::<usize>(),
            "selected": terms.len(),
            "entries": discovery.coverage.entries,
            "bytes_read": discovery.coverage.bytes,
            "hidden": discovery.coverage.hidden,
            "symlinks": discovery.coverage.symlinks,
            "unsupported": discovery.coverage.unsupported,
            "oversized": discovery.coverage.oversized,
        });
        if fixed_cases {
            cases = case_counts(&matcher);
        }
        samples.push(
            json!({ "cold_scan_us": cold_scan_us, "select_us": select_us,
            "compile_us": compile_us, "catalog_commit_us": catalog_commit_us,
            "catalog_load_us": catalog_load_us, "cached_scan_us": cached_scan_us }),
        );
    }
    Ok(json!({ "repetitions": repetitions, "counts": counts, "cases": cases, "timings": samples }))
}

fn failure(stage: &str, category: &str) -> Value {
    json!({ "status": "failed", "stage": stage, "category": category })
}

fn scan_category(error: &discover::Error<super::extract::Error>) -> &'static str {
    match error {
        discover::Error::TooLarge => "too_large",
        discover::Error::Cancelled => "cancelled",
        discover::Error::InvalidDirectory => "invalid_directory",
        discover::Error::Unstable => "unstable",
        discover::Error::InvalidOutput => "invalid_output",
        discover::Error::Extractor(_) => "extractor_error",
        discover::Error::Io(_) => "io_error",
    }
}

fn catalog_category(error: &super::catalog::Error) -> &'static str {
    match error {
        super::catalog::Error::TooLarge => "too_large",
        super::catalog::Error::Cancelled => "cancelled",
        _ => "catalog_error",
    }
}

fn selection_category(error: &select::Error) -> &'static str {
    match error {
        select::Error::TooLarge => "too_large",
        select::Error::Cancelled => "cancelled",
    }
}

fn cooperative_cancellation(parent: &Path) -> Value {
    let root = parent.join("cancellation");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("large.txt"), "plainword ".repeat(6500)).unwrap();
    let mut samples = Vec::new();
    for _ in 0..100 {
        let signalled = Arc::new(Mutex::new(None));
        let observed = Arc::clone(&signalled);
        let checks = AtomicUsize::new(0);
        let cancel = CancelToken::from_fn(move || {
            if checks.fetch_add(1, Ordering::Relaxed) >= 1000 {
                observed.lock().unwrap().get_or_insert_with(Instant::now);
                true
            } else {
                false
            }
        });
        assert!(discover::scan_cached(&root, &cancel, &[], &mut LocalExtractor).is_err());
        samples.push(
            signalled
                .lock()
                .unwrap()
                .expect("signal observed")
                .elapsed()
                .as_secs_f64()
                * 1e6,
        );
    }
    samples.sort_by(f64::total_cmp);
    json!({
        "metric": "return_after_first_observed_cooperative_signal_us",
        "limitation": "same-thread injected signal; excludes external scheduling, worker wakeup, and active recording contention",
        "samples": samples.len(), "p99_us": samples[98], "sorted_us": samples,
    })
}

#[test]
fn folder_scope_and_conflicts_survive_catalog_reuse() {
    let temporary = tempfile::tempdir().unwrap();
    let root = fixture(temporary.path());
    let cancel = CancelToken::never();
    let discovery = discover::scan_cached(&root, &cancel, &[], &mut LocalExtractor).unwrap();
    let terms = select::terms(&discovery.sources, &cancel).unwrap();
    let matcher = Vocabulary::compile(&terms, PersonalRules::default()).unwrap();
    for (input, target) in STRUCTURED {
        assert_eq!(corrected(&matcher, input), *target);
    }
    assert!(!terms.iter().any(|term| matches!(
        term.canonical.as_str(),
        "sync-id" | "sync_id" | "Persephone"
    )));
    for input in NEGATIVES.iter().copied().chain(["open aria dash", "kit"]) {
        assert_eq!(matcher.correct(input), None);
    }
    for (input, target) in UNSUPPORTED {
        assert_ne!(corrected(&matcher, input), *target);
    }
    let key = pipeline_key(LocalExtractor.cache_key()).unwrap();
    let mut catalog = Catalog::open(&temporary.path().join("catalog"), cancel.clone()).unwrap();
    catalog
        .refresh_cancellable(&discovery, &key, &cancel)
        .unwrap();
    let cached = catalog.load(&discovery.root, &key).unwrap().unwrap();
    let warm = discover::scan_cached(&root, &cancel, &cached.sources, &mut LocalExtractor).unwrap();
    assert_eq!(select::terms(&warm.sources, &cancel).unwrap(), terms);
    let child =
        discover::scan_cached(&root.join("rust"), &cancel, &[], &mut LocalExtractor).unwrap();
    let child_terms = select::terms(&child.sources, &cancel).unwrap();
    assert!(child_terms.iter().any(|term| term.canonical == "aria-kit"));
    assert!(
        !child_terms
            .iter()
            .any(|term| term.canonical == "ledger-api")
    );
}

#[test]
fn selected_folder_budget_failure_reports_only_stage_and_category() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("scope");
    let mut nested = root.clone();
    for _ in 0..18 {
        nested.push("nested");
    }
    fs::create_dir_all(nested).unwrap();
    assert_eq!(
        measure(&root, &temporary.path().join("catalog"), 1, false).unwrap_err(),
        json!({ "status": "failed", "stage": "cold_scan", "category": "too_large" })
    );
}

#[test]
#[ignore = "release diagnostic; writes only to ARIA_VOCABULARY_COLLECTION_RESULTS when set"]
fn vocabulary_collection_benchmark() {
    if cfg!(debug_assertions) {
        panic!("run this diagnostic with --release");
    }
    let temporary = tempfile::tempdir().unwrap();
    let root = fixture(temporary.path());
    let fixed = measure(&root, &temporary.path().join("catalog"), 20, true)
        .expect("fixed fixture diagnostic failed");
    let selected = std::env::var_os("ARIA_VOCABULARY_SCAN_DIR").map(|root| {
        measure(
            Path::new(&root),
            &temporary.path().join("selected-catalog"),
            3,
            false,
        )
        .unwrap_or_else(|failure| failure)
    });
    let report = json!({
        "diagnostic_only": true,
        "fixture_version": 1,
        "pipeline_key": pipeline_key(LocalExtractor.cache_key()).unwrap(),
        "corpus": "six fixed Java, Swift, Rust, Python, web, and non-code fixtures; not held out",
        "limitations": "raw fixed-case counts, not population accuracy; cold means no extraction cache, not an OS cold filesystem; no real dictation latency or deployment acceptance claim",
        "fixed_fixtures": fixed,
        "selected_folder": selected,
        "cooperative_cancellation": cooperative_cancellation(temporary.path()),
    });
    if let Some(path) = std::env::var_os("ARIA_VOCABULARY_COLLECTION_RESULTS") {
        let path = Path::new(&path);
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .expect("output requires a parent directory");
        assert_eq!(
            fs::metadata(parent).unwrap().permissions().mode() & 0o777,
            0o700,
            "output directory must be private"
        );
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap();
        serde_json::to_writer_pretty(&mut file, &report).unwrap();
        writeln!(file).unwrap();
    }
    println!("{report}");
}
