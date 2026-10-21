//! A local repository's content store (`<repo>/blobs/`): file contents by hash, used as
//! the read cache and as the source working copies clone from.
//!
//! ```text
//! <repo>/blobs/<hh>/<blake3>   one read-only copy per distinct file content
//! ```
//!
//! Files are materialized as copy-on-write clones (APFS `clonefile`, Linux `FICLONE`), so every
//! working copy shares the store's blocks until a file is edited. A clone never aliases the
//! store copy (unlike a hardlink), so editing in place is safe and the store is a plain cache:
//! deleting from it never affects a working copy. Clones only work within one volume; a
//! working copy on another volume than its repository (or a filesystem without clones) gets
//! copies.

use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use anyhow::{Context, Result};

use crate::hash::Hash;

#[derive(Clone)]
pub struct Blobs {
    pub dir: PathBuf,
}

/// How materialized bytes reached the disk.
#[derive(Default, Debug)]
pub struct CloneStats {
    pub cloned: AtomicU64,
    pub cloned_bytes: AtomicU64,
    pub copied: AtomicU64,
    pub copied_bytes: AtomicU64,
}

impl CloneStats {
    pub fn add(&self, cloned: bool, bytes: u64) {
        let (n, b) = if cloned {
            (&self.cloned, &self.cloned_bytes)
        } else {
            (&self.copied, &self.copied_bytes)
        };
        n.fetch_add(1, Ordering::Relaxed);
        b.fetch_add(bytes, Ordering::Relaxed);
    }
}

/// Clone `src` to `dst` (which must not exist). Directories are cloned recursively.
/// Returns false when the filesystem can't clone (e.g. ext4, or across volumes).
pub fn clone_path(src: &Path, dst: &Path) -> Result<bool> {
    if std::env::var("PAIL_CLONE").as_deref() == Ok("off") {
        return Ok(false);
    }
    imp::clone_path(src, dst)
}

#[cfg(target_vendor = "apple")]
mod imp {
    use super::*;

    pub fn clone_path(src: &Path, dst: &Path) -> Result<bool> {
        let s = CString::new(src.as_os_str().as_bytes())?;
        let d = CString::new(dst.as_os_str().as_bytes())?;
        // clonefile(2) clones a whole directory tree in one call. CLONE_NOFOLLOW = 1.
        if unsafe { libc::clonefile(s.as_ptr(), d.as_ptr(), 1) } == 0 {
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        match err.raw_os_error() {
            Some(libc::ENOTSUP) | Some(libc::EXDEV) => Ok(false),
            _ => {
                Err(err).with_context(|| format!("clonefile {} → {}", src.display(), dst.display()))
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use super::*;
    use std::os::fd::AsRawFd;

    pub fn clone_path(src: &Path, dst: &Path) -> Result<bool> {
        let md = std::fs::symlink_metadata(src)?;
        if md.is_dir() {
            std::fs::create_dir(dst)?;
            std::fs::set_permissions(dst, md.permissions())?;
            for ent in std::fs::read_dir(src)? {
                let ent = ent?;
                if !clone_path(&ent.path(), &dst.join(ent.file_name()))? {
                    std::fs::remove_dir_all(dst)?;
                    return Ok(false);
                }
            }
            return Ok(true);
        }
        if md.file_type().is_symlink() {
            std::os::unix::fs::symlink(std::fs::read_link(src)?, dst)?;
            return Ok(true);
        }
        let s = std::fs::File::open(src)?;
        let d = std::fs::File::create(dst)?;
        if unsafe { libc::ioctl(d.as_raw_fd(), libc::FICLONE, s.as_raw_fd()) } == 0 {
            std::fs::set_permissions(dst, md.permissions())?;
            let _ = d.set_modified(md.modified()?);
            return Ok(true);
        }
        let err = std::io::Error::last_os_error();
        drop(d);
        let _ = std::fs::remove_file(dst);
        match err.raw_os_error() {
            Some(libc::EOPNOTSUPP)
            | Some(libc::EXDEV)
            | Some(libc::EINVAL)
            | Some(libc::ENOTTY) => Ok(false),
            _ => Err(err).with_context(|| format!("FICLONE {} → {}", src.display(), dst.display())),
        }
    }
}

#[cfg(not(any(target_vendor = "apple", target_os = "linux")))]
mod imp {
    use super::*;
    pub fn clone_path(_: &Path, _: &Path) -> Result<bool> {
        Ok(false)
    }
}

impl Blobs {
    /// Cached content, if present. Marks it used (at most daily) so pruning keeps it.
    pub fn read(&self, h: &Hash) -> Option<bytes::Bytes> {
        use std::io::Read;
        let mut f = std::fs::File::open(self.path(h)).ok()?;
        let md = f.metadata().ok()?;
        let mut data = Vec::with_capacity(md.len() as usize);
        f.read_to_end(&mut data).ok()?;
        let day = std::time::Duration::from_secs(86400);
        if md
            .accessed()
            .ok()
            .and_then(|a| a.elapsed().ok())
            .is_none_or(|age| age > day)
        {
            let _ =
                f.set_times(std::fs::FileTimes::new().set_accessed(std::time::SystemTime::now()));
        }
        Some(data.into())
    }

    /// Every cached blob with its size and last use, oldest first.
    pub fn list(&self) -> Vec<(PathBuf, u64, std::time::SystemTime)> {
        let mut out = vec![];
        for d in std::fs::read_dir(&self.dir).into_iter().flatten().flatten() {
            for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
                if let Ok(md) = f.metadata() {
                    let used = md
                        .accessed()
                        .or_else(|_| md.modified())
                        .unwrap_or(std::time::UNIX_EPOCH);
                    if md.is_file() && !f.file_name().to_string_lossy().contains(".tmp") {
                        out.push((f.path(), md.len(), used));
                    }
                }
            }
        }
        out.sort_by_key(|x| x.2);
        out
    }

    pub fn path(&self, h: &Hash) -> PathBuf {
        let hex = h.hex();
        self.dir.join(&hex[..2]).join(hex)
    }

    pub fn has(&self, h: &Hash) -> bool {
        self.path(h).exists()
    }

    /// Store content (already verified to hash to `h`). Concurrent writers are safe: each writes
    /// a private temp file and renames it into place.
    pub fn put(&self, h: &Hash, data: &[u8]) -> Result<()> {
        let p = self.path(h);
        if p.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        let tmp = p.with_extension(format!(
            "tmp{}.{}",
            std::process::id(),
            rand::random::<u32>()
        ));
        std::fs::write(&tmp, data)?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o444))?;
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Move a file already on this volume into the store (used for committed stage snapshots).
    pub fn adopt(&self, h: &Hash, file: &Path) -> Result<()> {
        let p = self.path(h);
        if p.exists() {
            return Ok(());
        }
        std::fs::create_dir_all(p.parent().unwrap())?;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o444))?;
        if std::fs::rename(file, &p).is_err() {
            self.put(h, &std::fs::read(file)?)?;
        }
        Ok(())
    }

    /// Put content `h` at `dst` (which must not exist) as a clone, or a copy if cloning is
    /// unsupported. The caller sets mode and mtime.
    pub fn checkout(&self, h: &Hash, dst: &Path, stats: &CloneStats) -> Result<()> {
        let src = self.path(h);
        let size = std::fs::metadata(&src)?.len();
        if clone_path(&src, dst)? {
            stats.add(true, size);
        } else {
            std::fs::copy(&src, dst)
                .with_context(|| format!("copying blob to {}", dst.display()))?;
            stats.add(false, size);
        }
        Ok(())
    }
}
