//! Keep the real index locked while libgit2 writes a private index. This is
//! separate from ref transactions: Git's index lock must cover the file-write
//! and recovery phases too, so a concurrent `git add` cannot be overwritten.

use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::PathBuf,
    sync::atomic::{AtomicU64, Ordering},
};

use git2::{ErrorCode, Repository, RepositoryInitOptions};

use super::{CheckoutError, CheckoutErrorKind, concurrent_change, map_git_error};

static NEXT: AtomicU64 = AtomicU64::new(0);

pub(crate) struct IndexTransaction {
    path: PathBuf,
    lock: PathBuf,
    marker: Vec<u8>,
    directory: PathBuf,
    parent: PathBuf,
    staging: PathBuf,
    original: Option<Vec<u8>>,
    permissions: Option<fs::Permissions>,
    // Both repositories cache the staged index. Keep its initializer alive
    // through checkout/recovery: dropping it clears that index's owner.
    _shadow: Option<Repository>,
    _lease: File,
}

impl IndexTransaction {
    pub(crate) fn prepare(repository: &Repository) -> Result<Self, CheckoutError> {
        let parent = fs::canonicalize(repository.path()).map_err(|_| index_error())?;
        let path = parent.join("index");
        let lock = parent.join("index.lock");
        let marker = format!(
            "gitviews-index-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
        .into_bytes();
        let mut lease = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lock)
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    CheckoutError::new(
                        CheckoutErrorKind::ConcurrentChange,
                        "暂存区已被其他 Git 操作锁定（index.lock）；请等待完成后重试。",
                    )
                } else {
                    index_error()
                }
            })?;
        if lease
            .write_all(&marker)
            .and_then(|()| lease.sync_all())
            .is_err()
        {
            drop(lease);
            let _ = fs::remove_file(&lock);
            return Err(index_error());
        }
        let mut transaction = Self {
            path,
            lock,
            marker,
            directory: PathBuf::new(),
            parent,
            staging: PathBuf::new(),
            original: None,
            permissions: None,
            _shadow: None,
            _lease: lease,
        };
        transaction.original = transaction.read_original()?;
        for _ in 0..32 {
            let path = transaction.parent.join(format!(
                ".gitviews-index-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match fs::create_dir(&path) {
                Ok(()) => {
                    transaction.directory = path;
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(_) => return Err(index_error()),
            }
        }
        if transaction.directory.as_os_str().is_empty() {
            return Err(index_error());
        }
        let mut owner = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(transaction.directory.join(".gitviews-owner"))
            .map_err(|_| index_error())?;
        owner
            .write_all(&transaction.marker)
            .and_then(|()| owner.sync_all())
            .map_err(|_| index_error())?;
        drop(owner);
        transaction.staging = transaction.directory.join("index");
        let mut options = RepositoryInitOptions::new();
        options.bare(true).external_template(false);
        let shadow =
            Repository::init_opts(&transaction.directory, &options).map_err(map_git_error)?;
        let config = repository.config().map_err(map_git_error)?;
        let mut shadow_config = shadow.config().map_err(map_git_error)?;
        // Index::open alone has no repository capabilities, and git2 exposes
        // no set_caps. Initializing via a private repository preserves the
        // actual repository's case, filemode and symlink semantics safely.
        for (key, fallback) in [
            ("core.ignorecase", false),
            ("core.filemode", true),
            ("core.symlinks", true),
        ] {
            let value = match config.get_bool(key) {
                Ok(value) => value,
                Err(error) if error.code() == ErrorCode::NotFound => fallback,
                Err(error) => return Err(map_git_error(error)),
            };
            shadow_config.set_bool(key, value).map_err(map_git_error)?;
        }
        drop(shadow_config);
        drop(shadow);
        if let Some(original) = &transaction.original {
            let mut file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&transaction.staging)
                .map_err(|_| index_error())?;
            file.write_all(original)
                .and_then(|()| file.sync_all())
                .map_err(|_| index_error())?;
        }
        let shadow = Repository::open_bare(&transaction.directory).map_err(map_git_error)?;
        let mut index = shadow.index().map_err(map_git_error)?;
        repository.set_index(&mut index).map_err(map_git_error)?;
        transaction._shadow = Some(shadow);
        Ok(transaction)
    }

    fn read_original(&mut self) -> Result<Option<Vec<u8>>, CheckoutError> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) => {
                if !plain_file(&metadata) || metadata.permissions().readonly() {
                    return Err(index_error());
                }
                self.permissions = Some(metadata.permissions());
                fs::read(&self.path).map(Some).map_err(|_| index_error())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(index_error()),
        }
    }

    pub(crate) fn commit(&mut self) -> Result<(), CheckoutError> {
        self.verify()?;
        if let Some(permissions) = &self.permissions {
            fs::set_permissions(&self.staging, permissions.clone()).map_err(|_| index_error())?;
        }
        OpenOptions::new()
            .write(true)
            .open(&self.staging)
            .and_then(|file| file.sync_all())
            .map_err(|_| index_error())?;
        // This is a same-filesystem atomic replace; the real index.lock
        // remains present until this guard is dropped after all Git work.
        fs::rename(&self.staging, &self.path).map_err(|_| index_error())
    }

    /// Check the lock and original bytes even when an operation only updates
    /// refs and must leave the physical index completely unchanged.
    pub(crate) fn verify(&self) -> Result<(), CheckoutError> {
        if !self.owns_lock() || self.read_current()? != self.original {
            return Err(concurrent_change());
        }
        Ok(())
    }

    fn read_current(&self) -> Result<Option<Vec<u8>>, CheckoutError> {
        match fs::symlink_metadata(&self.path) {
            Ok(metadata) if plain_file(&metadata) => {
                fs::read(&self.path).map(Some).map_err(|_| index_error())
            }
            Ok(_) => Err(index_error()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(index_error()),
        }
    }

    fn owns_lock(&self) -> bool {
        fs::symlink_metadata(&self.lock).is_ok_and(|metadata| {
            plain_file(&metadata) && metadata.len() == self.marker.len() as u64
        }) && fs::read(&self.lock).ok().as_deref() == Some(self.marker.as_slice())
    }
}

impl Drop for IndexTransaction {
    fn drop(&mut self) {
        // Verify the absolute owned directory before recursive cleanup; never
        // follow a replacement symlink or Windows reparse point.
        if !self.directory.as_os_str().is_empty()
            && fs::symlink_metadata(&self.directory)
                .is_ok_and(|metadata| metadata.is_dir() && !is_link(&metadata))
            && fs::canonicalize(&self.directory).ok().is_some_and(|path| {
                path == self.directory && path.parent() == Some(self.parent.as_path())
            })
            && fs::read(self.directory.join(".gitviews-owner"))
                .ok()
                .as_deref()
                == Some(self.marker.as_slice())
        {
            let _ = fs::remove_dir_all(&self.directory);
        }
        if self.owns_lock() {
            let _ = fs::remove_file(&self.lock);
        }
    }
}

fn plain_file(metadata: &fs::Metadata) -> bool {
    metadata.is_file() && !is_link(metadata)
}

fn is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        metadata.file_type().is_symlink() || metadata.file_attributes() & 0x400 != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn index_error() -> CheckoutError {
    CheckoutError::new(
        CheckoutErrorKind::CheckoutFailed,
        "无法安全锁定或更新暂存区；请检查 index 文件、权限和磁盘空间后重试。",
    )
}
