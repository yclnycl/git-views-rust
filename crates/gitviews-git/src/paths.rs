//! Keep filesystem paths intact while sharing their display and comparison
//! rules. Windows canonicalization adds a verbatim prefix; libgit2 may also
//! return that prefix with forward slashes. Neither spelling is an error.

use std::path::{Path, PathBuf};

/// Convert libgit2's Windows separators into the form understood by Rust's
/// path parser. Retain verbatim prefixes for long-path filesystem operations.
pub fn native_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        use std::{
            ffi::OsString,
            os::windows::ffi::{OsStrExt, OsStringExt},
        };
        let units: Vec<_> = path
            .as_os_str()
            .encode_wide()
            .map(|unit| {
                if unit == b'/' as u16 {
                    b'\\' as u16
                } else {
                    unit
                }
            })
            .collect();
        PathBuf::from(OsString::from_wide(&units))
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

/// Human-readable absolute paths for labels, clipboard text and path inputs.
/// This never replaces the original path used for filesystem operations.
pub fn display_path(path: &Path) -> String {
    #[cfg(windows)]
    {
        let native = native_path(path);
        if native.is_absolute() {
            return ordinary_windows_path(&native)
                .to_string_lossy()
                .into_owned();
        }
    }
    path.to_string_lossy().into_owned()
}

/// Git for Windows' MSYS path handling does not accept Rust's verbatim prefix.
/// Normalize only at the process boundary while preserving OS string data.
pub(crate) fn command_path(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        ordinary_windows_path(&native_path(path))
    }
    #[cfg(not(windows))]
    {
        path.to_owned()
    }
}

/// Compare path spellings without filesystem I/O. Windows drive/UNC verbatim
/// prefixes, separators and ASCII case are equivalent; relative paths and
/// parent traversal are not resolved. Non-Unicode names remain distinct.
/// Validate/canonicalize paths separately before destructive operations.
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    path_relative_to(left, right).is_some_and(|relative| relative.as_os_str().is_empty())
}

/// Strip a component-aligned filesystem prefix using the same Windows spelling
/// rules as `paths_equal`. Unlike canonicalization this also works after a
/// worktree or watched file has disappeared, and performs no filesystem I/O.
pub fn path_relative_to(path: &Path, base: &Path) -> Option<PathBuf> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let path = ordinary_windows_path(&native_path(path));
        let base = ordinary_windows_path(&native_path(base));
        let mut a = path.components();
        for b in base.components() {
            let a = a.next()?;
            let fold = |unit: u16| {
                if (b'A' as u16..=b'Z' as u16).contains(&unit) {
                    unit + 32
                } else {
                    unit
                }
            };
            if !a
                .as_os_str()
                .encode_wide()
                .map(fold)
                .eq(b.as_os_str().encode_wide().map(fold))
            {
                return None;
            }
        }
        Some(a.as_path().to_owned())
    }
    #[cfg(not(windows))]
    {
        path.strip_prefix(base).ok().map(Path::to_owned)
    }
}

#[cfg(windows)]
fn ordinary_windows_path(path: &Path) -> PathBuf {
    use std::{
        ffi::OsString,
        os::windows::ffi::{OsStrExt, OsStringExt},
    };
    let units: Vec<_> = path.as_os_str().encode_wide().collect();
    let prefix = |text: &str| {
        let expected: Vec<_> = text.encode_utf16().collect();
        units.len() >= expected.len()
            && units.iter().zip(&expected).all(|(a, b)| {
                let fold = |unit: u16| {
                    if (65..=90).contains(&unit) {
                        unit + 32
                    } else {
                        unit
                    }
                };
                fold(*a) == fold(*b)
            })
    };
    let ordinary = if prefix("\\\\?\\UNC\\") {
        let mut ordinary = vec![b'\\' as u16, b'\\' as u16];
        ordinary.extend_from_slice(&units[8..]);
        ordinary
    } else if prefix("\\\\?\\")
        && units.len() >= 7
        && ((65..=90).contains(&units[4]) || (97..=122).contains(&units[4]))
        && units[5] == b':' as u16
        && units[6] == b'\\' as u16
    {
        units[4..].to_vec()
    } else {
        return path.to_owned();
    };
    // Verbatim spelling matters for these names. Do not present or compare an
    // ordinary spelling that could address a different file or device.
    let skip = if ordinary.starts_with(&[92, 92]) {
        4
    } else {
        1
    };
    if ordinary
        .split(|unit| *unit == b'\\' as u16)
        .enumerate()
        .any(|(index, part)| {
            if index < skip {
                return false;
            }
            let stem: Vec<_> = part
                .split(|unit| *unit == 46)
                .next()
                .unwrap_or_default()
                .iter()
                .map(|unit| {
                    if (97..=122).contains(unit) {
                        unit - 32
                    } else {
                        *unit
                    }
                })
                .collect();
            let reserved = ["CON", "PRN", "AUX", "NUL", "CONIN$", "CONOUT$"]
                .iter()
                .any(|name| stem.iter().copied().eq(name.encode_utf16()))
                || (stem.len() == 4
                    && (stem.starts_with(&[67, 79, 77]) || stem.starts_with(&[76, 80, 84]))
                    && (48..=57).contains(&stem[3]));
            reserved
                || part.contains(&58)
                || part == [b'.' as u16]
                || part == [b'.' as u16, b'.' as u16]
                || part.last().is_some_and(|unit| matches!(*unit, 32 | 46))
        })
    {
        return path.to_owned();
    }
    PathBuf::from(OsString::from_wide(&ordinary))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_git_paths_keep_their_spelling() {
        assert_eq!(
            display_path(Path::new("src/nested/file.rs")),
            "src/nested/file.rs"
        );
        assert!(!paths_equal(Path::new("repo"), Path::new("repository")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_paths_share_display_and_identity_across_native_and_libgit2_spellings() {
        for alias in [
            r"F:\project\仓库",
            r"\\?\F:\project\仓库",
            "//?/F:/project/仓库",
            r"f:\PROJECT\仓库\",
        ] {
            assert!(
                paths_equal(Path::new(alias), Path::new(r"F:\project\仓库")),
                "{alias}"
            );
            assert!(!display_path(Path::new(alias)).starts_with(r"\\?\"));
        }
        assert_eq!(
            display_path(Path::new(r"\\?\F:\project\仓库")),
            r"F:\project\仓库"
        );
        assert_eq!(
            display_path(Path::new("//?/F:/project/仓库")),
            r"F:\project\仓库"
        );
        assert!(paths_equal(
            Path::new(r"\\?\UNC\server\share\仓库"),
            Path::new(r"\\SERVER\share\仓库")
        ));
        assert_eq!(
            display_path(Path::new(r"\\?\UNC\server\share\仓库")),
            r"\\server\share\仓库"
        );
        assert!(!paths_equal(Path::new(r"C:repo"), Path::new(r"C:\repo")));
        assert!(!paths_equal(
            Path::new(r"C:\repo\..\other"),
            Path::new(r"C:\other")
        ));
        assert!(!paths_equal(
            Path::new(r"\\?\C:\repo."),
            Path::new(r"C:\repo.")
        ));
        assert_eq!(
            path_relative_to(Path::new(r"F:\REPO\src\file.rs"), Path::new(r"\\?\f:\repo")),
            Some(PathBuf::from(r"src\file.rs"))
        );
        assert_eq!(
            path_relative_to(Path::new(r"F:\repository\file.rs"), Path::new(r"F:\repo")),
            None
        );
    }

    #[cfg(windows)]
    #[test]
    fn comparison_never_aliases_non_unicode_names_or_generic_device_namespaces() {
        use std::{ffi::OsString, os::windows::ffi::OsStringExt};
        let a = PathBuf::from(OsString::from_wide(&[b'C' as u16, 58, 92, 0xd800]));
        let b = PathBuf::from(OsString::from_wide(&[b'C' as u16, 58, 92, 0xd801]));
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        assert!(!paths_equal(&a, &b));
        assert!(!paths_equal(&a, Path::new("C:\\�")));
        for special in [
            r"\\.\PhysicalDrive0",
            r"\\?\Volume{123}\repo",
            r"\\?\C:\repo\..\other",
            r"\\?\C:\NUL.txt",
        ] {
            assert_eq!(display_path(Path::new(special)), special);
        }
    }
}
