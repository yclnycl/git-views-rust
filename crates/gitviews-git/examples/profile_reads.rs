//! Read-only, timing-only repository diagnostics. Repository contents and
//! configuration values are deliberately omitted from the output.

use std::{env, path::Path, process::ExitCode, time::Instant};

use gitviews_git::{Git2Backend, GitBackend};

fn profile(index: usize, path: &Path) -> bool {
    let backend = Git2Backend;
    let total = Instant::now();
    let started = Instant::now();
    match backend.inspect_metadata(path) {
        Ok(snapshot) => println!(
            "repository={index} phase=metadata elapsed_ms={} commits={}",
            started.elapsed().as_millis(),
            snapshot.recent_commits.len()
        ),
        Err(_) => {
            eprintln!("repository={index} phase=metadata failed=true");
            return false;
        }
    }
    let started = Instant::now();
    let snapshot = match backend.inspect(path) {
        Ok(snapshot) => {
            println!(
                "repository={index} phase=inspect elapsed_ms={} changes={} commits={}",
                started.elapsed().as_millis(),
                snapshot.changes.len(),
                snapshot.recent_commits.len()
            );
            snapshot
        }
        Err(_) => {
            eprintln!("repository={index} phase=inspect failed=true");
            return false;
        }
    };
    let started = Instant::now();
    match backend.details(path) {
        Ok(details) => println!(
            "repository={index} phase=details elapsed_ms={} branches={} worktrees={}",
            started.elapsed().as_millis(),
            details.branches.len(),
            details.worktrees.len()
        ),
        Err(_) => {
            eprintln!("repository={index} phase=details failed=true");
            return false;
        }
    }
    let started = Instant::now();
    match backend.tree_with_changes(path, &snapshot.changes) {
        Ok(tree) => println!(
            "repository={index} phase=tree elapsed_ms={} entries={} submodules={} warnings={}",
            started.elapsed().as_millis(),
            tree.entries.len(),
            tree.submodules.len(),
            tree.warnings.len()
        ),
        Err(_) => {
            eprintln!("repository={index} phase=tree failed=true");
            return false;
        }
    }
    println!(
        "repository={index} phase=total elapsed_ms={}",
        total.elapsed().as_millis()
    );
    true
}

fn main() -> ExitCode {
    let mut paths = env::args_os().skip(1).peekable();
    if paths.peek().is_none() {
        eprintln!("Usage: profile_reads <repository> [repository ...]");
        return ExitCode::FAILURE;
    }
    let mut success = true;
    for (index, path) in paths.enumerate() {
        success &= profile(index + 1, Path::new(&path));
    }
    if success {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
