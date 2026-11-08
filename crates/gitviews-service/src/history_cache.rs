use crate::{HistoryPage, RepositoryService, ServiceError};
use gitviews_git::GitBackend;
use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_PAGES: usize = 64;
const MAX_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq)]
struct Key {
    root: PathBuf,
    anchor: String,
    skip: usize,
    limit: usize,
}
#[derive(Debug)]
struct Page {
    key: Key,
    value: Arc<HistoryPage>,
    bytes: usize,
}
#[derive(Debug, Default)]
pub(crate) struct HistoryCache {
    pages: VecDeque<Page>,
    bytes: usize,
}

impl HistoryCache {
    fn get(&mut self, key: &Key) -> Option<Arc<HistoryPage>> {
        let index = self.pages.iter().position(|page| page.key == *key)?;
        let page = self.pages.remove(index)?;
        let value = page.value.clone();
        self.pages.push_back(page);
        Some(value)
    }
    fn insert(&mut self, key: Key, value: Arc<HistoryPage>) {
        let bytes = std::mem::size_of::<Page>()
            + key.root.as_os_str().len() * 2
            + key.anchor.capacity()
            + value.commits.capacity() * std::mem::size_of::<crate::CommitSummary>()
            + value
                .commits
                .iter()
                .map(|c| c.id.capacity() + c.subject.capacity() + c.author.capacity())
                .sum::<usize>();
        if bytes > MAX_BYTES {
            return;
        }
        if let Some(index) = self.pages.iter().position(|page| page.key == key) {
            self.bytes -= self.pages.remove(index).expect("existing page").bytes;
        }
        while self.pages.len() >= MAX_PAGES || self.bytes.saturating_add(bytes) > MAX_BYTES {
            if let Some(page) = self.pages.pop_front() {
                self.bytes -= page.bytes;
            } else {
                break;
            }
        }
        self.bytes += bytes;
        self.pages.push_back(Page { key, value, bytes });
    }
}

impl<B: GitBackend> RepositoryService<B> {
    /// Immutable pages survive working-file invalidation, within a separate 4 MiB budget.
    pub fn history_at(
        &self,
        root: &Path,
        anchor: &str,
        skip: usize,
        limit: usize,
    ) -> Result<HistoryPage, ServiceError> {
        let key = Key {
            root: crate::cache::normalized(root),
            anchor: anchor.to_owned(),
            skip,
            limit,
        };
        if let Some(page) = self
            .history_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .get(&key)
        {
            return Ok((*page).clone());
        }
        let page = Arc::new(
            self.backend
                .history_at(root, anchor, skip, limit)
                .map_err(ServiceError::Git)?,
        );
        self.history_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(key, page.clone());
        Ok((*page).clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{CommitSummary, GitError, RepositorySnapshot};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Backend(AtomicUsize);
    impl GitBackend for Backend {
        fn inspect(&self, root: &Path) -> Result<RepositorySnapshot, GitError> {
            Err(GitError::NotRepository {
                path: root.to_owned(),
            })
        }
        fn history_at(
            &self,
            _: &Path,
            anchor: &str,
            _: usize,
            _: usize,
        ) -> Result<HistoryPage, GitError> {
            self.0.fetch_add(1, Ordering::Relaxed);
            Ok(HistoryPage {
                commits: vec![CommitSummary {
                    id: anchor.into(),
                    subject: "提交".into(),
                    author: "作者".into(),
                }],
                has_more: false,
            })
        }
    }
    #[test]
    fn immutable_history_pages_reuse_cache_across_worktree_invalidation_and_isolate_anchors() {
        let service = RepositoryService::new(Backend(AtomicUsize::new(0)));
        let root = Path::new("history-cache-fixture");
        service.history_at(root, "anchor-one", 0, 100).unwrap();
        service.invalidate(root);
        assert_eq!(
            service
                .history_at(root, "anchor-one", 0, 100)
                .unwrap()
                .commits[0]
                .id,
            "anchor-one"
        );
        assert_eq!(service.backend.0.load(Ordering::Relaxed), 1);
        service.history_at(root, "anchor-two", 0, 100).unwrap();
        service.history_at(root, "anchor-one", 100, 100).unwrap();
        assert_eq!(service.backend.0.load(Ordering::Relaxed), 3);
    }
    #[test]
    fn history_cache_caps_page_count_and_retains_recent_pages() {
        let mut cache = HistoryCache::default();
        for skip in 0..(MAX_PAGES + 20) {
            cache.insert(
                Key {
                    root: "repo".into(),
                    anchor: "anchor".into(),
                    skip,
                    limit: 1,
                },
                Arc::new(HistoryPage::default()),
            );
        }
        assert_eq!(cache.pages.len(), MAX_PAGES);
        assert!(cache.bytes <= MAX_BYTES);
        assert!(
            cache
                .get(&Key {
                    root: "repo".into(),
                    anchor: "anchor".into(),
                    skip: 0,
                    limit: 1
                })
                .is_none()
        );
    }

    #[test]
    fn history_cache_evicts_for_byte_budget_and_does_not_cache_oversized_page() {
        let mut cache = HistoryCache::default();
        let key = |skip| Key {
            root: "repo".into(),
            anchor: "a".repeat(40),
            skip,
            limit: 100,
        };
        let page = |size| {
            Arc::new(HistoryPage {
                commits: vec![CommitSummary {
                    id: "a".repeat(40),
                    subject: "x".repeat(size),
                    author: "作者".into(),
                }],
                has_more: true,
            })
        };
        cache.insert(key(0), page(MAX_BYTES / 2));
        cache.insert(key(100), page(MAX_BYTES / 2));
        assert!(cache.get(&key(0)).is_none());
        assert!(cache.get(&key(100)).is_some());
        assert!(cache.bytes <= MAX_BYTES);
        cache.insert(key(200), page(MAX_BYTES));
        assert!(cache.get(&key(200)).is_none());
        assert!(cache.get(&key(100)).is_some());
    }
}
