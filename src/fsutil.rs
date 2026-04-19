// =============================================================================
// NKR fsutil — Detección de filesystem host y helpers btrfs
// =============================================================================
//
// btrfs + archivos ext4 mmap'eados vía virtio-pmem o escritos random (PG data)
// genera fragmentación catastrófica por CoW. La defensa obligatoria es
// `chattr +C` (NODATACOW) aplicado a un archivo VACÍO antes de cualquier write.
// Si el flag se aplica después de truncate/allocate, no surte efecto sobre los
// extents ya reservados.
//
// Este módulo expone:
//   - detect_fs(path): identifica el fs del path (btrfs, ext4/xfs, otro)
//   - create_ext4_disk(path, size_mb): crea el archivo con +C si está en btrfs,
//     luego trunca al tamaño deseado. El mkfs.ext4 lo hace el caller.
//   - apply_nocow_dir(path): +C recursivo sobre un dir vacío (para PG data).
//   - try_btrfs_snapshot(src, dst): si src es subvolumen btrfs, hace snapshot;
//     devuelve Ok(true) si se usó snapshot, Ok(false) si hay que caer al cp.
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

/// Detecta el filesystem en el que vive `path`. Si el path no existe, usa su
/// directorio padre. Cae a Other si no puede determinar.
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

/// Crea un archivo de disco (destinado a ext4/pmem/virtio-blk backing) con el
/// tratamiento correcto según el fs host:
///   - En btrfs: touch → chattr +C → truncate (flag debe aplicar antes de
///     reservar extents).
///   - En otros: truncate directo.
///
/// El `mkfs.ext4` posterior debe correrlo el caller.
pub fn create_ext4_disk(path: &str, size_mb: u32) -> Result<(), Box<dyn Error>> {
    let p = Path::new(path);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent)?;
    }

    // Si ya existe, borrar para que +C aplique limpio.
    if p.exists() {
        fs::remove_file(p)?;
    }

    let fs_kind = detect_fs(p);

    if fs_kind == FsKind::Btrfs {
        // touch: crear archivo vacío
        fs::File::create(path)?;
        // chattr +C sobre archivo VACÍO (obligatorio antes de reservar extents)
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

    // Ahora sí: truncate al tamaño deseado.
    let truncate_ok = Command::new("truncate")
        .args(["-s", &format!("{}M", size_mb), path])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);

    if !truncate_ok {
        // Fallback: crear+set_len manualmente (sobrescribe el archivo vacío).
        let file = fs::File::create(path)?;
        file.set_len((size_mb as u64) * 1024 * 1024)?;
    }

    Ok(())
}

/// Aplica `chattr +C` a un directorio existente. Debe llamarse ANTES de
/// escribir datos dentro (si ya hay archivos, el flag solo aplicará a los
/// nuevos). Pensado para dirs de PG data y filestore.
#[allow(dead_code)]
pub fn apply_nocow_dir(dir: &str) -> Result<(), Box<dyn Error>> {
    let p = Path::new(dir);
    if !p.exists() {
        fs::create_dir_all(p)?;
    }

    if detect_fs(p) != FsKind::Btrfs {
        return Ok(()); // No-op fuera de btrfs
    }

    let status = Command::new("chattr").args(["+C", dir]).status();
    match status {
        Ok(s) if s.success() => {
            eprintln!("[NKR-FS] btrfs: chattr +C aplicado al dir {}", dir);
            Ok(())
        }
        _ => {
            eprintln!("[NKR-FS] WARN: chattr +C falló en dir {}", dir);
            Ok(()) // No fatal
        }
    }
}

/// Re-aplica `chattr +C` a un `.ext4` ya poblado (ej. cloned via cp/snapshot).
///
/// `chattr +C` sólo surte efecto sobre extents futuros. Para que los extents
/// existentes del archivo sean NODATACOW, hay que:
///   1. renombrar el archivo (preserva datos),
///   2. `touch` uno nuevo vacío + `chattr +C`,
///   3. `cp --sparse=always` del viejo al nuevo (extents nuevos heredan el flag),
///   4. borrar el viejo.
///
/// Costo: reescribe el contenido entero (~5-15s por cada 4 GB en NVMe btrfs).
/// Idempotente: fast-path si el archivo ya tiene `+C`.
///
/// Llamado desde `cell::clone_instance_with_opts` para cada `.ext4` del dst,
/// ya que `cp --reflink` y `btrfs subvolume snapshot` NO heredan `+C`.
pub fn preserve_nocow(path: &Path) -> Result<(), Box<dyn Error>> {
    if detect_fs(path) != FsKind::Btrfs { return Ok(()); }
    if !path.exists() { return Ok(()); }

    // Fast path: ya tiene +C
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

/// Intenta snapshot btrfs de src → dst. Requiere que ambos estén en btrfs y
/// que src sea un subvolumen. Si falla por cualquier razón, devuelve Ok(false)
/// para que el caller use cp --reflink como fallback.
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

/// Verifica si un path es un subvolumen btrfs (inode number == 256).
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

        // Ejecutar
        create_ext4_disk(&path_str, 10).expect("create_ext4_disk failed");

        // Verificar atributos vía lsattr
        let out = Command::new("lsattr")
            .arg(&path_str)
            .output()
            .expect("lsattr");
        let attrs = String::from_utf8_lossy(&out.stdout);
        assert!(attrs.contains('C'),
            "Esperaba +C en attrs tras create_ext4_disk en btrfs, obtuve: {}", attrs);

        // Verificar tamaño
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
