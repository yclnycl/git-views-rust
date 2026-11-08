//! Bounded retained preview controls, independent of the opened-tab model.
//!
//! Evicting a pane never closes its tab. The caller keeps the identity and
//! rereads it on a later activation. Only the active pane needs to be rendered;
//! GPUI Kit's editor/diff virtualize their visible rows while retained entities
//! preserve each tab's reading, selection, search and display-mode state.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use gpui_kit::{App, AppContext as _, Entity};

use super::preview::PreviewPane;

const MAX_RETAINED_PANES: usize = 8;
const MAX_ESTIMATED_BYTES: usize = 32 * 1024 * 1024;

/// Root comes from the backend's canonical repository identity. `path` is the
/// complete raw repository-relative path: do not lowercase it or use basename.
/// Revision, when present, must be the resolved immutable commit's full OID.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct PreviewKey {
    pub root: PathBuf,
    pub path: String,
    pub staged: bool,
    pub revision: Option<String>,
}

impl PreviewKey {
    fn immutable(&self) -> bool {
        self.revision.as_ref().is_some_and(|revision| {
            matches!(revision.len(), 40 | 64)
                && revision.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    }
}

/// Eight retained panes and a 32 MiB conservative content/derived-state budget.
/// The active pane is protected; if it alone exceeds the estimate, all inactive
/// panes are evicted and it becomes evictable as soon as another tab activates.
/// This bounds reusable UI cache state, not process RSS or service-cache memory.
pub struct PreviewDeck {
    panes: RetainedPanes<Entity<PreviewPane>>,
}

impl Default for PreviewDeck {
    fn default() -> Self {
        Self {
            panes: RetainedPanes::new(MAX_RETAINED_PANES, MAX_ESTIMATED_BYTES),
        }
    }
}

impl PreviewDeck {
    pub fn set_limits(&mut self, max_panes: usize, max_bytes: usize, cx: &mut App) {
        self.panes.max_panes = max_panes.max(1);
        self.panes.max_bytes = max_bytes;
        self.enforce_limits(cx);
    }
    /// No Window is required. Input controls are deferred until the pane renders.
    /// A cache hit returns the exact same Entity without resetting any content.
    pub fn activate(&mut self, key: PreviewKey, cx: &mut App) -> Entity<PreviewPane> {
        let pane = self
            .panes
            .activate_with(key, || cx.new(PreviewPane::new_deferred));
        self.enforce_limits(cx);
        pane
    }

    /// Background completions may only update a still-retained matching key.
    /// Looking up an inactive pane does not make it recently used or active.
    pub fn get(&self, key: &PreviewKey) -> Option<Entity<PreviewPane>> {
        self.panes.entries.get(key).map(|entry| entry.value.clone())
    }

    pub fn active_key(&self) -> Option<&PreviewKey> {
        self.panes.active.as_ref()
    }

    pub fn deactivate(&mut self) {
        self.panes.active = None;
    }

    pub fn is_stale(&self, key: &PreviewKey) -> bool {
        self.panes.entries.get(key).is_none_or(|entry| entry.stale)
    }

    /// Call after accepting a read for this exact identity, not when queuing it.
    pub fn mark_fresh(&mut self, key: &PreviewKey) {
        if let Some(entry) = self.panes.entries.get_mut(key) {
            entry.stale = false;
        }
    }

    /// HEAD/index/worktree changes invalidate mutable previews, while a cached
    /// resolved commit remains valid. An unresolved name such as HEAD is mutable.
    pub fn invalidate_root(&mut self, root: &Path) {
        self.panes.invalidate_root(root);
    }

    pub fn close(&mut self, key: &PreviewKey, cx: &mut App) {
        if self.panes.active.as_ref() == Some(key) {
            self.panes.active = None;
        }
        if let Some(entry) = self.panes.entries.remove(key) {
            entry.value.update(cx, |pane, cx| pane.clear(cx));
        }
    }

    pub fn clear(&mut self, cx: &mut App) {
        self.panes.active = None;
        for (_, entry) in self.panes.entries.drain() {
            entry.value.update(cx, |pane, cx| pane.clear(cx));
        }
    }

    /// Call after any accepted source/diff update as well as activation. Payload
    /// estimates reserve rope/parsed-diff/search copies before deferred rendering
    /// and asynchronous parsing, so those preparations cannot hide cache growth.
    pub fn enforce_limits(&mut self, cx: &mut App) -> Vec<PreviewKey> {
        for entry in self.panes.entries.values_mut() {
            entry.bytes = entry.value.read(cx).estimated_bytes();
        }
        self.panes
            .trim()
            .into_iter()
            .map(|(key, entry)| {
                // Cancel pending diff preparation and release payload/control
                // state even if a caller briefly still holds an Entity clone.
                entry.value.update(cx, |pane, cx| pane.clear(cx));
                key
            })
            .collect()
    }
}

struct CachedPane<V> {
    value: V,
    bytes: usize,
    last_used: u64,
    stale: bool,
}

/// Pure cache policy, kept separate from GPUI so identity, budget and invalidation
/// behavior can be tested without a native window or rendering a document.
struct RetainedPanes<V> {
    entries: HashMap<PreviewKey, CachedPane<V>>,
    active: Option<PreviewKey>,
    clock: u64,
    max_panes: usize,
    max_bytes: usize,
}

impl<V> RetainedPanes<V> {
    fn new(max_panes: usize, max_bytes: usize) -> Self {
        Self {
            entries: HashMap::new(),
            active: None,
            clock: 0,
            max_panes: max_panes.max(1),
            max_bytes,
        }
    }

    fn activate_with(&mut self, key: PreviewKey, create: impl FnOnce() -> V) -> V
    where
        V: Clone,
    {
        self.clock = self.clock.saturating_add(1);
        self.active = Some(key.clone());
        let entry = self.entries.entry(key).or_insert_with(|| CachedPane {
            value: create(),
            bytes: 0,
            last_used: self.clock,
            stale: true,
        });
        entry.last_used = self.clock;
        entry.value.clone()
    }

    fn invalidate_root(&mut self, root: &Path) {
        for (key, entry) in &mut self.entries {
            if key.root == root && !key.immutable() {
                entry.stale = true;
            }
        }
    }

    fn trim(&mut self) -> Vec<(PreviewKey, CachedPane<V>)> {
        let mut evicted = Vec::new();
        loop {
            let bytes = self
                .entries
                .values()
                .fold(0usize, |sum, entry| sum.saturating_add(entry.bytes));
            if self.entries.len() <= self.max_panes && bytes <= self.max_bytes {
                break;
            }
            let victim = self
                .entries
                .iter()
                .filter(|(key, _)| Some(*key) != self.active.as_ref())
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else {
                break;
            };
            if let Some(entry) = self.entries.remove(&victim) {
                evicted.push((victim, entry));
            }
        }
        evicted
    }
}

#[cfg(test)]
mod tests {
    use super::{PreviewKey, RetainedPanes};
    use std::{cell::Cell, path::PathBuf};

    fn key(path: &str) -> PreviewKey {
        PreviewKey {
            root: PathBuf::from("repository"),
            path: path.into(),
            staged: false,
            revision: None,
        }
    }

    #[test]
    fn keys_isolate_roots_full_raw_paths_index_and_immutable_revisions() {
        let base = key("src/View.rs");
        let alternatives = [
            PreviewKey {
                root: "other-repository".into(),
                ..base.clone()
            },
            PreviewKey {
                path: "tests/View.rs".into(),
                ..base.clone()
            },
            PreviewKey {
                path: "src/view.rs".into(),
                ..base.clone()
            },
            PreviewKey {
                staged: true,
                ..base.clone()
            },
            PreviewKey {
                revision: Some("a".repeat(40)),
                ..base.clone()
            },
            PreviewKey {
                revision: Some("b".repeat(40)),
                ..base.clone()
            },
        ];
        let mut panes = RetainedPanes::new(8, usize::MAX);
        panes.activate_with(base.clone(), || 0);
        for (index, alternative) in alternatives.into_iter().enumerate() {
            panes.activate_with(alternative, || index + 1);
        }
        assert_eq!(panes.entries.len(), 7);
        assert_eq!(
            panes.activate_with(base, || panic!("cache hit must not reconstruct a pane")),
            0
        );
    }

    #[test]
    fn count_budget_evicts_least_recent_inactive_pane_and_preserves_reactivated_state() {
        let mut panes = RetainedPanes::new(2, usize::MAX);
        let builds = Cell::new(0);
        for (path, value) in [("a", 11), ("b", 22)] {
            panes.activate_with(key(path), || {
                builds.set(builds.get() + 1);
                value
            });
        }
        assert_eq!(
            panes.activate_with(key("a"), || panic!("a should remain retained")),
            11
        );
        panes.activate_with(key("c"), || 33);
        let evicted = panes.trim();
        assert_eq!(evicted.len(), 1);
        assert_eq!(evicted[0].0.path, "b");
        assert_eq!(panes.active.as_ref().unwrap().path, "c");
        assert_eq!(builds.get(), 2);
        assert_eq!(
            panes.activate_with(key("b"), || 222),
            222,
            "an evicted tab identity can be rebuilt"
        );
    }

    #[test]
    fn byte_budget_reclaims_inactive_derived_state_and_oversized_active_is_not_retained_later() {
        let mut panes = RetainedPanes::new(8, 100);
        for (path, bytes) in [("old", 60), ("recent", 60), ("active", 30)] {
            panes.activate_with(key(path), || ());
            panes.entries.get_mut(&key(path)).unwrap().bytes = bytes;
        }
        assert_eq!(
            panes
                .trim()
                .into_iter()
                .map(|(key, _)| key.path)
                .collect::<Vec<_>>(),
            ["old"]
        );
        panes.entries.get_mut(&key("active")).unwrap().bytes = 150;
        assert_eq!(
            panes
                .trim()
                .into_iter()
                .map(|(key, _)| key.path)
                .collect::<Vec<_>>(),
            ["recent"]
        );
        assert_eq!(
            panes.entries.len(),
            1,
            "the visible active pane is protected"
        );
        panes.activate_with(key("new"), || ());
        assert_eq!(
            panes
                .trim()
                .into_iter()
                .map(|(key, _)| key.path)
                .collect::<Vec<_>>(),
            ["active"]
        );
    }

    #[test]
    fn worktree_invalidation_does_not_expire_resolved_commit_or_other_repository() {
        let workspace = key("view.rs");
        let staged = PreviewKey {
            staged: true,
            ..workspace.clone()
        };
        let commit = PreviewKey {
            revision: Some("a".repeat(40)),
            ..workspace.clone()
        };
        let unresolved = PreviewKey {
            revision: Some("HEAD".into()),
            ..workspace.clone()
        };
        let other = PreviewKey {
            root: "other".into(),
            ..workspace.clone()
        };
        let mut panes = RetainedPanes::new(8, usize::MAX);
        for key in [&workspace, &staged, &commit, &unresolved, &other] {
            panes.activate_with(key.clone(), || ());
            panes.entries.get_mut(key).unwrap().stale = false;
        }
        panes.invalidate_root(&workspace.root);
        assert!(panes.entries[&workspace].stale);
        assert!(panes.entries[&staged].stale);
        assert!(panes.entries[&unresolved].stale);
        assert!(!panes.entries[&commit].stale);
        assert!(!panes.entries[&other].stale);
    }
}
