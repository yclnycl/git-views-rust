//! Run synchronous repository/CPU work on GPUI's shared background executor.
//! The tracing span is entered only inside the synchronous job, never across await.
use gpui_kit::{BackgroundExecutor, Priority, Task};
use std::time::Instant;

pub(super) fn spawn<T: Send + 'static>(
    executor: &BackgroundExecutor,
    operation: &'static str,
    request_id: u64,
    work: impl FnOnce() -> T + Send + 'static,
) -> Task<T> {
    let queued = Instant::now();
    let parent = tracing::Span::current();
    let priority = match operation {
        "submodule.read"
        | "submodule.metadata"
        | "submodule.overview"
        | "submodule.tree"
        | "repository.metadata"
        | "repository.open"
        | "repository.details"
        | "repository.tree"
        | "preview.read"
        | "file_list.rows"
        | "files.search" => Priority::High,
        "repository.scan" | "repository.watcher" | "submodule.watcher" => Priority::Low,
        _ => Priority::default(),
    };
    executor.spawn_with_priority(priority, async move {
        let queue_ms = queued.elapsed().as_millis() as u64;
        let span = tracing::info_span!(target: "gitviews::tasks", parent: &parent, "background", operation, request_id, queue_ms, ?priority);
        let _entered = span.enter();
        tracing::debug!(target: "gitviews::tasks", "background_started");
        let started = Instant::now();
        let value = work();
        tracing::info!(target: "gitviews::tasks", elapsed_ms = started.elapsed().as_millis() as u64, "background_complete");
        value
    })
}
