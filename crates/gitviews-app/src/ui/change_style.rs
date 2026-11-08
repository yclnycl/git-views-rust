//! Git status semantics shared by the virtual file list and design-backed tree.
//! Keep conflict detection on the original XY pair: AA and DD contain no U.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ChangeKind {
    Clean,
    Added,
    Untracked,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Conflict,
    Ignored,
    Unreadable,
}

impl ChangeKind {
    pub(super) fn from_status(index: char, worktree: char) -> Self {
        if is_conflict(index, worktree) {
            return Self::Conflict;
        }
        if (index, worktree) == ('!', '!') {
            return Self::Ignored;
        }
        // Structural changes stay visible in the tree even if the other side
        // is also modified. Grouped rows classify each side independently.
        for (codes, kind) in [
            ("!", Self::Unreadable),
            ("?", Self::Untracked),
            ("D", Self::Deleted),
            ("R", Self::Renamed),
            ("C", Self::Copied),
            ("T", Self::TypeChanged),
            ("A", Self::Added),
        ] {
            if codes.contains(index) || codes.contains(worktree) {
                return kind;
            }
        }
        if index == ' ' && worktree == ' ' {
            Self::Clean
        } else {
            Self::Modified
        }
    }

    /// Colors remain legible on both the panel and the selected-row background.
    pub(super) fn rgb(self, light: bool) -> u32 {
        match (self, light) {
            (Self::Clean, true) => 0x1f2328,
            (Self::Clean, false) => 0xf0f6fc,
            (Self::Added | Self::Untracked, true) => 0x1a7d37,
            (Self::Added | Self::Untracked, false) => 0x3fb950,
            (Self::Modified, true) => 0x0969da,
            (Self::Modified, false) => 0x79c0ff,
            (Self::Deleted | Self::Conflict, true) => 0xcf222e,
            (Self::Deleted | Self::Conflict, false) => 0xff7b72,
            (Self::Renamed | Self::Copied, true) => 0x7b47d5,
            (Self::Renamed | Self::Copied, false) => 0xd2a8ff,
            (Self::TypeChanged | Self::Unreadable, true) => 0x926000,
            (Self::TypeChanged | Self::Unreadable, false) => 0xe3b341,
            (Self::Ignored, true) => 0x59636e,
            (Self::Ignored, false) => 0x9da7b3,
        }
    }

    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Clean => "未修改",
            Self::Added => "新增",
            Self::Untracked => "未跟踪",
            Self::Modified => "修改",
            Self::Deleted => "删除",
            Self::Renamed => "重命名",
            Self::Copied => "复制",
            Self::TypeChanged => "类型变化",
            Self::Conflict => "冲突",
            Self::Ignored => "已忽略",
            Self::Unreadable => "无法读取",
        }
    }
}

pub(super) fn is_conflict(index: char, worktree: char) -> bool {
    matches!(
        (index, worktree),
        ('D', 'D') | ('A', 'U') | ('U', 'D') | ('U', 'A') | ('D', 'U') | ('A', 'A') | ('U', 'U')
    )
}

#[cfg(test)]
mod tests {
    use super::{ChangeKind, is_conflict};

    #[test]
    fn porcelain_types_and_compound_statuses_keep_their_meaning() {
        for (status, expected) in [
            ("  ", ChangeKind::Clean),
            ("A ", ChangeKind::Added),
            ("??", ChangeKind::Untracked),
            (" M", ChangeKind::Modified),
            ("MM", ChangeKind::Modified),
            (" D", ChangeKind::Deleted),
            ("AD", ChangeKind::Deleted),
            ("RM", ChangeKind::Renamed),
            ("C ", ChangeKind::Copied),
            ("TM", ChangeKind::TypeChanged),
            ("!!", ChangeKind::Ignored),
            (" !", ChangeKind::Unreadable),
        ] {
            let mut chars = status.chars();
            assert_eq!(
                ChangeKind::from_status(chars.next().unwrap(), chars.next().unwrap()),
                expected,
                "{status:?}"
            );
        }
        for status in ["DD", "AU", "UD", "UA", "DU", "AA", "UU"] {
            let mut chars = status.chars();
            let (index, worktree) = (chars.next().unwrap(), chars.next().unwrap());
            assert!(is_conflict(index, worktree), "{status}");
            assert_eq!(
                ChangeKind::from_status(index, worktree),
                ChangeKind::Conflict
            );
        }
        assert!(!is_conflict('A', 'D'));
        assert!(!is_conflict('D', 'M'));
    }

    #[test]
    fn colors_distinguish_types_and_have_readable_contrast_in_both_themes() {
        fn luminance(rgb: u32) -> f64 {
            let linear = |value: u32| {
                let value = f64::from(value) / 255.;
                if value <= 0.04045 {
                    value / 12.92
                } else {
                    ((value + 0.055) / 1.055).powf(2.4)
                }
            };
            0.2126 * linear((rgb >> 16) & 255)
                + 0.7152 * linear((rgb >> 8) & 255)
                + 0.0722 * linear(rgb & 255)
        }
        for light in [false, true] {
            let backgrounds = if light {
                [0xffffff, 0xddf4ff]
            } else {
                [0x0d1117, 0x1a3453]
            };
            for kind in [
                ChangeKind::Clean,
                ChangeKind::Added,
                ChangeKind::Untracked,
                ChangeKind::Modified,
                ChangeKind::Deleted,
                ChangeKind::Renamed,
                ChangeKind::Copied,
                ChangeKind::TypeChanged,
                ChangeKind::Conflict,
                ChangeKind::Ignored,
                ChangeKind::Unreadable,
            ] {
                for background in backgrounds {
                    let a = luminance(kind.rgb(light));
                    let b = luminance(background);
                    let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
                    assert!(
                        contrast >= 4.5,
                        "{kind:?} light={light} contrast={contrast}"
                    );
                }
            }
            assert_ne!(
                ChangeKind::Modified.rgb(light),
                ChangeKind::Renamed.rgb(light)
            );
            assert_ne!(
                ChangeKind::TypeChanged.rgb(light),
                ChangeKind::Deleted.rgb(light)
            );
        }
    }
}
