// =============================================================================
// NKR fsutil — Host filesystem detection and btrfs helpers
// =============================================================================
//
// btrfs + ext4 files mmap'ed via virtio-pmem or random-written (PG data)
// cause catastrophic CoW fragmentation. The mandatory defense is
// `chattr +C` (NODATACOW) applied to an EMPTY file before any write.
// If the flag is applied after truncate/allocate, it doesn't take effect on
// the already-reserved extents.
//
// This module exposes:
//   - detect_fs(path): identifies the path's fs (btrfs, ext4/xfs, other)
//   - create_ext4_disk(path, size_mb): creates the file with +C if on btrfs,
//     then truncates to the desired size. mkfs.ext4 is the caller's job.
//   - apply_nocow_dir(path): +C recursive on an empty dir (for PG data).
//   - try_btrfs_snapshot(src, dst): if src is a btrfs subvolume, snapshots it;
//     returns Ok(true) if snapshot was used, Ok(false) if fallback to cp.
// =============================================================================

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsKind {
    Btrfs,
    Other,
}

/// Detects the filesystem that `path` lives on. If the path does not exist,
/// uses its parent directory. Falls back to Other if it can't determine.
pub fn detect_fs(path: &Path) -> FsKind {
    let target = if path.exists() {
        path.to_path_buf()
    } else {
        path.parent().unwrap_or(Path::new("/")).to_path_buf()
    };

    let out = Command::new("stat")
        .args(["-f", "-c", "%T", &target.to_string_lossy()])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let kind = String::from_utf8_lossy(&o.stdout).trim().to_lowercase();
            if kind == "btrfs" {
                return FsKind::Btrfs;
            }
        }
    }
    FsKind::Other
}

/// Creates a disk file (intended for ext4/pmem/virtio-blk backing) with the
/// proper treatment for the host fs:
///   - On btrfs: touch → chattr +C → truncate (flag must apply before
///     extents are reserved).
///   - Otherwise: direct truncate.
///
/// The subsequent `mkfs.ext4` must be run by the caller.
pub fn create_ext4_disk(path: &str, size_mb: u32) -> Result<(), Box<dyn Error>> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }

    // If it already exists, delete so +C applies cleanly.
    if p.exists() {
        fs::remove_file(p)?;
    }

    let fs_kind = detect_fs(p);

    if fs_kind == FsKind::Btrfs {
        // touch: create empty file
        fs::File::create(path)?;
        // chattr +C on an EMPTY file (mandatory before extents are reserved)
        let status = Command::new("chattr").args(["+C", path]).status();
        match status {
            Ok(s) if s.success() => {
                eprintln!("[NKR-FS] btrfs detectado → chattr +C aplicado a {}", path);
            }
            _ => {
                eprintln!("[NKR-FS] WARN: chattr +C falló en {}. El disco sufrirá fragmentación CoW en btrfs.", path);
            }
        }
    }

    // Now truncate to the desired size.
    let truncate_ok = Command::new("truncate")
        .args(["-s", &format!("{}M", size_mb), path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !truncate_ok {
        // Fallback: manually create+set_len (overwrites the empty file).
        let file = fs::File::create(path)?;
        file.set_len((size_mb as u64) * 1024 * 1024)?;
    }

    Ok(())
}

/// Applies `chattr +C` to an existing directory. Must be called BEFORE
/// writing data inside (if files already exist, the flag will only apply to
/// new ones). Intended for PG data and filestore dirs.
#[allow(dead_code)]
pub fn apply_nocow_dir(dir: &str) -> Result<(), Box<dyn Error>> {
    let p = Path::new(dir);
    if !p.exists() {
        fs::create_dir_all(p)?;
    }

    if detect_fs(p) != FsKind::Btrfs {
        return Ok(()); // No-op outside btrfs
    }

    let status = Command::new("chattr").args(["+C", dir]).status();
    match status {
        Ok(s) if s.success() => {
            eprintln!("[NKR-FS] btrfs: chattr +C aplicado al dir {}", dir);
            Ok(())
        }
        _ => {
            eprintln!("[NKR-FS] WARN: chattr +C falló en dir {}", dir);
            Ok(()) // Not fatal
        }
    }
}

/// Re-applies `chattr +C` to an already-populated `.ext4` (e.g. cloned via cp/snapshot).
///
/// `chattr +C` only takes effect on future extents. For the file's existing
/// extents to become NODATACOW, you must:
///   1. rename the file (preserves data),
///   2. `touch` a new empty one + `chattr +C`,
///   3. `cp --sparse=always` from old to new (new extents inherit the flag),
///   4. delete the old one.
///
/// Cost: rewrites the full content (~5-15s per 4 GB on NVMe btrfs).
/// Idempotent: fast-path if the file already has `+C`.
///
/// Called from `cell::clone_instance_with_opts` for each `.ext4` of dst,
/// since `cp --reflink` and `btrfs subvolume snapshot` do NOT inherit `+C`.
pub fn preserve_nocow(path: &Path) -> Result<(), Box<dyn Error>> {
    if detect_fs(path) != FsKind::Btrfs { return Ok(()); }
    if !path.exists() { return Ok(()); }

    // Fast path: already has +C
    if let Ok(o) = Command::new("lsattr").arg(path).output() {
        if let Some(attrs) = String::from_utf8_lossy(&o.stdout).split_whitespace().next() {
            if attrs.contains('C') { return Ok(()); }
        }
    }

    let parent = path.parent().ok_or_else(|| format!("{} sin parent", path.display()))?;
    let fname = path.file_name().ok_or_else(|| format!("{} sin filename", path.display()))?;
    let tmp = parent.join(format!(".{}.premigrate", fname.to_string_lossy()));

    fs::rename(path, &tmp)?;
    fs::File::create(path)?;
    let chattr_ok = Command::new("chattr").args(["+C", &*path.to_string_lossy()])
        .status().map(|s| s.success()).unwrap_or(false);
    if !chattr_ok {
        // rollback
        let _ = fs::remove_file(path);
        let _ = fs::rename(&tmp, path);
        return Err(format!("chattr +C falló en {}", path.display()).into());
    }
    let cp_ok = Command::new("cp")
        .args(["--sparse=always", &*tmp.to_string_lossy(), &*path.to_string_lossy()])
        .status().map(|s| s.success()).unwrap_or(false);
    if !cp_ok {
        let _ = fs::remove_file(path);
        let _ = fs::rename(&tmp, path);
        return Err(format!("cp --sparse falló durante preserve_nocow de {}", path.display()).into());
    }
    let _ = fs::remove_file(&tmp);
    eprintln!("[NKR-FS] +C re-aplicado a {} (clone)", path.display());
    Ok(())
}

/// Attempts a btrfs snapshot of src → dst. Requires both to be on btrfs and
/// src to be a subvolume. If it fails for any reason, returns Ok(false)
/// so the caller can use cp --reflink as fallback.
#[allow(dead_code)]
pub fn try_btrfs_snapshot(src: &Path, dst: &Path) -> Result<bool, Box<dyn Error>> {
    if detect_fs(src) != FsKind::Btrfs || detect_fs(dst.parent().unwrap_or(Path::new("/"))) != FsKind::Btrfs {
        return Ok(false);
    }
    if !is_btrfs_subvolume(src) {
        return Ok(false);
    }

    let status = Command::new("btrfs")
        .args([
            "subvolume", "snapshot",
            &src.to_string_lossy(),
            &dst.to_string_lossy(),
        ])
        .status();

    match status {
        Ok(s) if s.success() => {
            eprintln!("[NKR-FS] btrfs subvolume snapshot {} → {}", src.display(), dst.display());
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// Checks if a path is a btrfs subvolume (inode number == 256).
#[allow(dead_code)]
pub fn is_btrfs_subvolume(path: &Path) -> bool {
    let out = Command::new("stat")
        .args(["-c", "%i", &path.to_string_lossy()])
        .output();
    if let Ok(o) = out {
        if o.status.success() {
            let inode = String::from_utf8_lossy(&o.stdout).trim().to_string();
            return inode == "256";
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn btrfs_test_dir() -> Option<PathBuf> {
        let candidates = ["/mnt/nkr/_nkr_bench", "/mnt/nkr"];
        for c in &candidates {
            let p = PathBuf::from(c);
            if p.exists() && detect_fs(&p) == FsKind::Btrfs {
                return Some(p);
            }
        }
        None
    }

    #[test]
    fn detect_fs_returns_btrfs_on_btrfs_mount() {
        let dir = match btrfs_test_dir() {
            Some(d) => d,
            None => { eprintln!("SKIP: sin btrfs mount disponible"); return; }
        };
        assert_eq!(detect_fs(&dir), FsKind::Btrfs);
    }

    #[test]
    fn detect_fs_returns_other_on_non_btrfs() {
        let p = PathBuf::from("/tmp");
        assert_eq!(detect_fs(&p), FsKind::Other);
    }

    #[test]
    fn create_ext4_disk_applies_nocow_on_btrfs() {
        let dir = match btrfs_test_dir() {
            Some(d) => d,
            None => { eprintln!("SKIP: sin btrfs mount disponible"); return; }
        };
        let test_path = dir.join(format!("fsutil_unit_{}.bin", std::process::id()));
        let path_str = test_path.to_string_lossy().to_string();

        // Execute
        create_ext4_disk(&path_str, 10).expect("create_ext4_disk failed");

        // Verify attributes via lsattr
        let out = Command::new("lsattr")
            .arg(&path_str)
            .output()
            .expect("lsattr");
        let attrs = String::from_utf8_lossy(&out.stdout);
        assert!(attrs.contains('C'),
            "Esperaba +C en attrs tras create_ext4_disk en btrfs, obtuve: {}", attrs);

        // Verify size
        let meta = fs::metadata(&test_path).expect("metadata");
        assert_eq!(meta.len(), 10 * 1024 * 1024);

        let _ = fs::remove_file(&test_path);
    }

    #[test]
    fn create_ext4_disk_works_on_non_btrfs() {
        let test_path = PathBuf::from(format!("/tmp/fsutil_unit_{}.bin", std::process::id()));
        let path_str = test_path.to_string_lossy().to_string();

        create_ext4_disk(&path_str, 5).expect("create_ext4_disk failed");

        let meta = fs::metadata(&test_path).expect("metadata");
        assert_eq!(meta.len(), 5 * 1024 * 1024);

        let _ = fs::remove_file(&test_path);
    }
}
