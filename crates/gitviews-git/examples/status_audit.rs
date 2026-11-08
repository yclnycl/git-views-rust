//! Read-only diagnostic: compare every displayed change/side without printing file contents.
use gitviews_git::{Git2Backend, GitBackend, paths_equal};
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(
        std::env::args_os()
            .nth(1)
            .ok_or("provide a repository path")?,
    );
    let backend = Git2Backend;
    let repository = backend.inspect(&root)?;
    if !paths_equal(&repository.root, &std::fs::canonicalize(&root)?) {
        return Err("provide an exact repository root".into());
    }
    let tree = backend.tree(&root)?;
    let mut roots = vec![repository.root];
    roots.extend(
        tree.submodules
            .iter()
            .filter(|module| module.initialized)
            .map(|module| root.join(&module.path)),
    );
    for root in roots {
        let snapshot = backend.inspect(&root)?;
        println!("REPOSITORY\t{}\t{}", root.display(), snapshot.changes.len());
        for change in &snapshot.changes {
            print!(
                "CHANGE\t{}{}\t{}",
                change.index_status, change.worktree_status, change.path
            );
            if change.is_conflicted() {
                match backend.conflict_versions(&root, &change.path) {
                    Ok(versions) => println!(
                        "\tCONFLICT\tbase={} ours={} theirs={}",
                        versions.base.is_some(),
                        versions.ours.is_some(),
                        versions.theirs.is_some()
                    ),
                    Err(error) => println!("\tERROR\t{error}"),
                }
            } else {
                for (staged, code) in [(true, change.index_status), (false, change.worktree_status)]
                {
                    if code == ' ' || staged && code == '?' {
                        continue;
                    }
                    match backend.diff(&root, &change.path, staged) {
                        Ok(diff) => print!(
                            "\t{}:OK(old={} new={} patch={})",
                            if staged { "index" } else { "worktree" },
                            diff.old_text.len(),
                            diff.new_text.len(),
                            diff.patch.len()
                        ),
                        Err(error) => {
                            print!("\t{}:{error}", if staged { "index" } else { "worktree" })
                        }
                    }
                }
                println!();
            }
        }
    }
    Ok(())
}
