//! Synthetic, local-only service benchmark. See docs/PERFORMANCE_BASELINE.md.
use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs,
    hint::black_box,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};

use gitviews_git::Git2Backend;
use gitviews_service::{ContentSearchIndex, ContentSearchLimits, RepositoryService, SearchIndex};
use serde::{Deserialize, Serialize};
use serde_json::json;

// Increment when fixture contents, layout or history generation change.
const FIXTURE_VERSION: u32 = 1;
const SAMPLE_FILE: &str = "src/group000/file000000.txt";
const CONTENT_QUERY: &str = "baseline_absent_needle_729cf1";
const FULL_SEARCH_BUDGET: Duration = Duration::from_secs(30);

#[derive(Serialize, Deserialize)]
struct Fixture {
    version: u32,
    name: String,
    tracked_files: usize,
    commits: usize,
    modified_files: usize,
    untracked_files: usize,
    working_text_bytes: usize,
    head: String,
}

fn git(root: &Path, args: &[&str], commit: usize) -> Result<String, Box<dyn Error>> {
    // Ignore ambient repository overrides and user/system config. Never touch
    // the user's repositories, identity, hooks, signing keys or remotes.
    let mut command = Command::new("git");
    for variable in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_INDEX_FILE",
        "GIT_COMMON_DIR",
        "GIT_CONFIG_PARAMETERS",
        "GIT_CONFIG_COUNT",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_TEMPLATE_DIR",
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
        "GIT_DEFAULT_HASH",
    ] {
        command.env_remove(variable);
    }
    let date = format!("{} +0000", 1_700_000_000 + commit);
    let output = command
        .args(["-C", &root.to_string_lossy()])
        .args([
            "-c",
            "user.name=GitViews Baseline",
            "-c",
            "user.email=baseline@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.filemode=false",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "gc.auto=0",
            "-c",
            "maintenance.auto=false",
        ])
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join(".absent-global-config"))
        .env("GIT_AUTHOR_DATE", &date)
        .env("GIT_COMMITTER_DATE", date)
        .output()?;
    if !output.status.success() {
        return Err(format!(
            "fixture Git failed: {}",
            String::from_utf8_lossy(&output.stderr)
        )
        .into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn file_name(index: usize) -> String {
    format!("src/group{:03}/file{index:06}.txt", index / 100)
}

fn content(index: usize, revision: usize) -> String {
    let mut text = format!("fixture-v{FIXTURE_VERSION} file={index:06} revision={revision:04}\n");
    for line in 0..32 {
        text.push_str(&format!(
            "line {line:03} deterministic repository text payload {index:06} abcdefghijklmnop\n"
        ));
    }
    text
}

fn prepare(parent: &Path) -> Result<(), Box<dyn Error>> {
    // CreateNew semantics: an existing fixture/output directory is never reused
    // or deleted. Fixture generation is outside all timed/memory measurements.
    fs::create_dir(parent)?;
    for (name, tracked_files, commits) in [
        ("small", 100, 5),
        ("medium", 1_000, 20),
        ("large", 10_000, 50),
    ] {
        let root = parent.join(name);
        fs::create_dir(&root)?;
        git(
            &root,
            &[
                "init",
                "--template=",
                "--initial-branch=main",
                "--object-format=sha1",
            ],
            0,
        )?;
        // Retain these fixture-local settings for native libgit2 reads as well.
        git(&root, &["config", "core.autocrlf", "false"], 0)?;
        git(&root, &["config", "core.filemode", "false"], 0)?;
        git(
            &root,
            &["config", "core.excludesfile", ".git/disabled-excludes"],
            0,
        )?;
        git(&root, &["config", "gc.auto", "0"], 0)?;
        git(&root, &["config", "maintenance.auto", "false"], 0)?;
        git(
            &root,
            &["config", "core.hooksPath", ".git/disabled-hooks"],
            0,
        )?;
        let mut working_text_bytes = 0;
        for index in 0..tracked_files {
            let path = root.join(file_name(index));
            fs::create_dir_all(path.parent().ok_or("file has no parent")?)?;
            let text = content(index, 0);
            working_text_bytes += text.len();
            fs::write(path, text)?;
        }
        git(&root, &["add", "--all"], 0)?;
        git(&root, &["commit", "-m", "fixture initial"], 0)?;
        for revision in 1..commits {
            fs::write(root.join(SAMPLE_FILE), content(0, revision))?;
            git(&root, &["add", "--", SAMPLE_FILE], revision)?;
            git(
                &root,
                &["commit", "-m", &format!("fixture revision {revision:04}")],
                revision,
            )?;
        }
        let modified_files = tracked_files / 20;
        for index in 0..modified_files {
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(root.join(file_name(index)))?;
            file.write_all(b"unstaged baseline change\n")?;
            working_text_bytes += 25;
        }
        let untracked_files = tracked_files / 100;
        fs::create_dir(root.join("untracked"))?;
        for index in 0..untracked_files {
            let text = content(index, 0);
            working_text_bytes += text.len();
            fs::write(root.join(format!("untracked/new{index:06}.txt")), text)?;
        }
        let fixture = Fixture {
            version: FIXTURE_VERSION,
            name: name.into(),
            tracked_files,
            commits,
            modified_files,
            untracked_files,
            working_text_bytes,
            head: git(&root, &["rev-parse", "HEAD"], 0)?,
        };
        // Keep benchmark metadata outside the repository so it is not indexed.
        fs::write(
            parent.join(format!("{name}.json")),
            serde_json::to_vec_pretty(&fixture)?,
        )?;
    }
    Ok(())
}

#[derive(Default, Serialize)]
struct Stats {
    samples_ms: Vec<f64>,
    median_ms: f64,
    p95_ms: f64,
    min_ms: f64,
    max_ms: f64,
}

fn timed<T>(
    samples: &mut BTreeMap<&'static str, Stats>,
    key: &'static str,
    call: impl FnOnce() -> T,
) -> T {
    let start = Instant::now();
    let value = black_box(call());
    samples
        .entry(key)
        .or_default()
        .samples_ms
        .push(start.elapsed().as_secs_f64() * 1_000.0);
    value
}

fn stats_json(stats: gitviews_service::PreviewCacheStats) -> serde_json::Value {
    json!({"entries": stats.entries, "estimated_bytes": stats.bytes, "hits": stats.hits, "misses": stats.misses})
}

fn run(root: &Path, iterations: usize, output: &Path) -> Result<(), Box<dyn Error>> {
    if !(3..=100).contains(&iterations) {
        return Err("iterations must be 3..=100".into());
    }
    let parent = root.parent().ok_or("fixture has no parent")?;
    let name = root
        .file_name()
        .ok_or("fixture has no name")?
        .to_string_lossy();
    let fixture: Fixture = serde_json::from_slice(&fs::read(parent.join(format!("{name}.json")))?)?;
    if fixture.version != FIXTURE_VERSION {
        return Err("unsupported fixture version".into());
    }
    let mut samples = BTreeMap::new();
    let cancel = AtomicBool::new(false);
    let mut observations = Vec::new();
    let workload_start = Instant::now();
    for iteration in 0..iterations {
        let service = RepositoryService::new(Git2Backend);
        timed(&mut samples, "metadata_fresh_service", || {
            service.open_metadata(root)
        })?;
        let scan = timed(&mut samples, "load_scan_fresh_service", || {
            service.scan(root)
        })?;
        timed(&mut samples, "metadata_repeat", || {
            service.open_metadata(root)
        })?;
        timed(&mut samples, "status_history_repeat", || service.open(root))?;
        timed(&mut samples, "tree_reusing_status", || {
            service.tree_with_changes(root, &scan.snapshot.changes)
        })?;
        timed(&mut samples, "load_scan_repeat", || service.scan(root))?;
        timed(&mut samples, "source_cache_miss", || {
            service.cached_source(root, SAMPLE_FILE, None)
        })?;
        timed(&mut samples, "source_cache_hit", || {
            service.cached_source(root, SAMPLE_FILE, None)
        })?;
        timed(&mut samples, "diff_cache_miss", || {
            service.cached_diff(root, SAMPLE_FILE, false)
        })?;
        timed(&mut samples, "diff_cache_hit", || {
            service.cached_diff(root, SAMPLE_FILE, false)
        })?;
        let paths = timed(&mut samples, "path_index_build", || {
            SearchIndex::from_tree(&scan.tree)
        })?;
        let path_result = timed(&mut samples, "path_search", || {
            paths.query("file000", 50, &cancel)
        })?;
        let default_search = timed(&mut samples, "content_default_index_build", || {
            ContentSearchIndex::from_tree(root, &scan.tree)
        })?;
        let default_content = timed(&mut samples, "content_search_default_budget", || {
            default_search.query(CONTENT_QUERY, 50, &cancel)
        })?;
        // Do not retain two independent content caches at the same time.
        drop(default_search);
        let search = timed(&mut samples, "content_full_index_build", || {
            ContentSearchIndex::with_limits(
                root,
                &scan.tree,
                ContentSearchLimits {
                    time_budget: FULL_SEARCH_BUDGET,
                    ..ContentSearchLimits::default()
                },
                &cancel,
            )
        })?;
        // An absent literal forces the full candidate set (no early result cap).
        let cold = timed(&mut samples, "content_search_full_cache_miss", || {
            search.query(CONTENT_QUERY, 50, &cancel)
        })?;
        let warm = timed(&mut samples, "content_search_full_cache_hit", || {
            search.query(CONTENT_QUERY, 50, &cancel)
        })?;
        if cold.timed_out
            || warm.timed_out
            || cold.truncated
            || warm.truncated
            || path_result.timed_out
        {
            return Err(format!(
                "full traversal was partial: path_timeout={} cold_timeout={} warm_timeout={} cold_scanned={} warm_scanned={}",
                path_result.timed_out, cold.timed_out, warm.timed_out, cold.scanned_files, warm.scanned_files,
            ).into());
        }
        if scan.snapshot.changes.len() != fixture.modified_files + fixture.untracked_files
            || scan
                .snapshot
                .recent_commits
                .first()
                .map(|commit| commit.id.as_str())
                != Some(fixture.head.as_str())
            || search.len() != fixture.tracked_files + fixture.untracked_files
            || cold.scanned_files != search.len()
            || warm.scanned_files != search.len()
            || cold.read_bytes != fixture.working_text_bytes as u64
            || cold.skipped_files != 0
            || warm.skipped_files != 0
            || !scan.tree.warnings.is_empty()
            || !cold.matches.is_empty()
            || !warm.matches.is_empty()
            || !default_content.matches.is_empty()
        {
            return Err(
                "fixture workload did not match manifest; refusing misleading measurements".into(),
            );
        }
        let before_pressure = service.cache_stats();
        timed(
            &mut samples,
            "preview_cache_pressure_96_files",
            || -> Result<(), Box<dyn Error>> {
                for index in 0..96 {
                    black_box(service.cached_source(root, &file_name(index), None)?);
                }
                Ok(())
            },
        )?;
        let after_pressure = service.cache_stats();
        service.invalidate(root);
        let after_invalidation = service.cache_stats();
        observations.push(json!({
            "iteration": iteration + 1, "first_in_process": iteration == 0,
            "changes": scan.snapshot.changes.len(), "tree_entries": scan.tree.entries.len(),
            "path_index_estimated_bytes": paths.estimated_bytes(),
            "content_index_estimated_bytes": search.estimated_bytes(),
            "content_default": {
                "time_budget_ms": ContentSearchLimits::default().time_budget.as_millis(),
                "indexed_files": default_content.indexed_files, "scanned_files": default_content.scanned_files,
                "read_bytes": default_content.read_bytes, "cached_files": default_content.cached_files,
                "skipped_files": default_content.skipped_files, "matches": default_content.matches.len(),
                "timed_out": default_content.timed_out, "truncated": default_content.truncated,
                "cancelled": cancel.load(Ordering::Acquire),
                "complete": !default_content.timed_out && !default_content.truncated && default_content.scanned_files == search.len() && default_content.skipped_files == 0,
            },
            "content_miss": {"scanned_files": cold.scanned_files, "read_bytes": cold.read_bytes, "cached_files": cold.cached_files},
            "content_hit": {"scanned_files": warm.scanned_files, "read_bytes": warm.read_bytes, "cached_files": warm.cached_files},
            "preview_before_pressure": stats_json(before_pressure),
            "preview_after_pressure": stats_json(after_pressure),
            "preview_after_invalidation": stats_json(after_invalidation),
        }));
    }
    let workload_elapsed_ms = workload_start.elapsed().as_secs_f64() * 1_000.0;
    for stats in samples.values_mut() {
        let mut sorted = stats.samples_ms.clone();
        sorted.sort_by(f64::total_cmp);
        let n = sorted.len();
        stats.median_ms = if n.is_multiple_of(2) {
            (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
        } else {
            sorted[n / 2]
        };
        stats.p95_ms = sorted[(n * 95).div_ceil(100) - 1];
        stats.min_ms = sorted[0];
        stats.max_ms = sorted[n - 1];
    }
    let report = json!({
        "schema_version": 1, "fixture": fixture, "iterations": iterations,
        "backend": "built-in libgit2", "arch": env::consts::ARCH, "os": env::consts::OS,
        "package_version": env!("CARGO_PKG_VERSION"),
        "cache_state": "fresh service per iteration; same OS filesystem cache; first sample is first process access",
        "search_budgets": {
            "product_default_ms": ContentSearchLimits::default().time_budget.as_millis(),
            "full_traversal_benchmark_only_ms": FULL_SEARCH_BUDGET.as_millis(),
            "memory_scope": "Default-budget query cache is dropped before full-budget cache construction; process memory includes these workloads serially and harness allocations.",
        },
        "workload_elapsed_ms": workload_elapsed_ms, "metrics": samples, "observations": observations,
    });
    fs::write(output, serde_json::to_vec_pretty(&report)?)?;
    // Give the parent sampler time to observe the OS-maintained peak working
    // set even for the smallest fixture. Excluded from all workload timings.
    thread::sleep(Duration::from_millis(200));
    Ok(())
}

fn main() -> Result<(), Box<dyn Error>> {
    let args: Vec<_> = env::args_os().skip(1).collect();
    match args.first().and_then(|value| value.to_str()) {
        Some("prepare") if args.len() == 2 => prepare(&PathBuf::from(&args[1])),
        Some("run") if args.len() == 4 => run(
            &PathBuf::from(&args[1]), args[2].to_str().ok_or("invalid iterations")?.parse()?, &PathBuf::from(&args[3]),
        ),
        _ => Err("Usage: performance_baseline prepare <new-directory> | run <fixture> <iterations> <report.json>".into()),
    }
}
