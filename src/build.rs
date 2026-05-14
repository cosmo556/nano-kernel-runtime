// =============================================================================
// NKR Build — Builds ext4 disks from Nkrfile (Dockerfile compatible)
// =============================================================================
//
// Uses Docker as the build engine to build a custom image, then exports it
// to an ext4 disk ready for nkr run.
//
// Nkrfile example:
//   FROM odoo:17.0
//   RUN pip3 install phonenumbers
//   COPY ./odoo.conf /etc/odoo/odoo.conf
//
// Usage: sudo nkr build -f Nkrfile -o odoo.ext4 --size-gb 4
// =============================================================================

use std::error::Error;
use std::process::Command;
use std::fs;
use std::path::Path;

/// Returns true if `output` lives under the central /mnt/nkr/images/ tree —
/// the convention for "shared master ext4 images" that get reflinked to each
/// cell via `cell::provision_cell_root_disks`. Outputs to other paths
/// (developer scratch in /tmp, ad-hoc tests) are not subject to the
/// chattr +i lifecycle.
fn is_master_image_path(output: &str) -> bool {
    let data_dir = std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string());
    let images_prefix = format!("{}/images/", data_dir.trim_end_matches('/'));
    output.starts_with(&images_prefix)
}

/// Best-effort `chattr -i` on a master image. Silent if the file doesn't
/// exist yet (first build) or if the flag wasn't set. Returning an error
/// here would block re-builds on hosts where chattr is unavailable; treat
/// the lifecycle as best-effort and surface only stderr from the tool.
fn unlock_master(output: &str) {
    if !is_master_image_path(output) {
        return;
    }
    if !Path::new(output).exists() {
        return;
    }
    let _ = Command::new("chattr").args(["-i", output]).status();
}

/// Best-effort `chattr +i` on a master image. Logs a WARN if it fails
/// because that means the master remains writable — risky if any cell
/// later maps it directly. Operator can re-apply manually with
/// `sudo chattr +i <path>`.
fn lock_master(output: &str) {
    if !is_master_image_path(output) {
        return;
    }
    match Command::new("chattr").args(["+i", output]).status() {
        Ok(s) if s.success() => {
            eprintln!("[NKR-BUILD] chattr +i aplicado al master {}", output);
        }
        _ => {
            eprintln!("[NKR-BUILD] WARN: chattr +i falló sobre {}. \
                       Master queda escribible. Reaplicar manualmente: \
                       sudo chattr +i {}", output, output);
        }
    }
}

/// Builds an ext4 disk from an Nkrfile (Dockerfile compatible)
pub fn build_disk(
    nkrfile: &str,
    output: &str,
    size_mb: u32,
    context_dir: &str,
) -> Result<(), Box<dyn Error>> {
    // Validate that the Nkrfile exists
    if !Path::new(nkrfile).exists() {
        return Err(format!("Nkrfile no encontrado: '{}'", nkrfile).into());
    }

    // Validate that Docker is available
    if !Command::new("docker").arg("--version").output().is_ok() {
        return Err("Docker no está instalado. nkr build usa Docker como motor de build.".into());
    }

    // If the output is a master image under /mnt/nkr/images/, the previous
    // build may have set `chattr +i` to prevent accidental writes from VMs.
    // Unlock it here so the build can overwrite the file. Re-locked at the
    // end of the build on success.
    unlock_master(output);

    let tag = format!("nkr-build-{}", std::process::id());

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ NKR Build — Construyendo disco ext4 desde Nkrfile           ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║ Nkrfile: {:<51} ║", nkrfile);
    eprintln!("║ Output:  {:<51} ║", output);
    eprintln!("║ Tamaño:  {} MB{:<49} ║", size_mb, "");
    eprintln!("║ Context: {:<51} ║", context_dir);
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // ── 1. Docker build ──
    eprintln!("[NKR-BUILD] 1/5 Construyendo imagen desde {}...", nkrfile);
    let build_status = Command::new("docker")
        .args(["build", "-f", nkrfile, "-t", &tag, context_dir])
        .status()
        .map_err(|e| format!("Fallo docker build: {}", e))?;

    if !build_status.success() {
        return Err(format!("docker build falló. Revisa tu Nkrfile: {}", nkrfile).into());
    }

    // ── 2. Docker create + export ──
    eprintln!("[NKR-BUILD] 2/5 Exportando filesystem...");
    let create_output = Command::new("docker")
        .args(["create", &tag])
        .output()
        .map_err(|e| format!("Fallo docker create: {}", e))?;

    if !create_output.status.success() {
        let _ = cleanup_image(&tag);
        let err = String::from_utf8_lossy(&create_output.stderr);
        return Err(format!("docker create falló: {}", err).into());
    }

    let container_id = String::from_utf8_lossy(&create_output.stdout).trim().to_string();
    let tar_path = format!("/tmp/nkr_build_{}.tar", container_id);

    let export_status = Command::new("docker")
        .args(["export", "-o", &tar_path, &container_id])
        .status()?;

    // Clean up container
    let _ = Command::new("docker").args(["rm", &container_id]).status();

    if !export_status.success() {
        let _ = cleanup_image(&tag);
        let _ = fs::remove_file(&tar_path);
        return Err("docker export falló".into());
    }

    // ── 3. Create ext4 disk (+C on btrfs before allocate) ──
    eprintln!("[NKR-BUILD] 3/5 Creando disco ext4 ({} MB)...", size_mb);
    crate::fsutil::create_ext4_disk(output, size_mb)?;

    let mkfs_status = Command::new("mkfs.ext4")
        .args(["-q", "-F", output])
        .status()
        .map_err(|_| "mkfs.ext4 no disponible")?;

    if !mkfs_status.success() {
        let _ = fs::remove_file(&tar_path);
        let _ = cleanup_image(&tag);
        return Err("mkfs.ext4 falló".into());
    }

    // ── 4. Mount and extract ──
    let mount_dir = format!("/tmp/nkr_build_mnt_{}", std::process::id());
    fs::create_dir_all(&mount_dir)?;

    eprintln!("[NKR-BUILD] 4/5 Montando disco y extrayendo filesystem...");
    let mount_status = Command::new("mount")
        .args(["-o", "loop", output, &mount_dir])
        .status()?;

    if !mount_status.success() {
        let _ = fs::remove_file(&tar_path);
        let _ = fs::remove_dir(&mount_dir);
        let _ = cleanup_image(&tag);
        return Err("mount falló (¿ejecutando con sudo?)".into());
    }

    let tar_status = Command::new("tar")
        .args(["-xf", &tar_path, "-C", &mount_dir])
        .status()?;

    // Always unmount
    let _ = Command::new("umount").arg(&mount_dir).status();
    let _ = fs::remove_dir(&mount_dir);
    let _ = fs::remove_file(&tar_path);

    if !tar_status.success() {
        let _ = cleanup_image(&tag);
        return Err("tar extract falló".into());
    }

    // ── 5. Clean up temporary image ──
    eprintln!("[NKR-BUILD] 5/5 Limpiando...");
    let _ = cleanup_image(&tag);

    let disk_size = fs::metadata(output).map(|m| m.len() / (1024 * 1024)).unwrap_or(0);
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ ✅ Disco construido exitosamente                            ║");
    eprintln!("║ Archivo: {:<51} ║", output);
    eprintln!("║ Tamaño:  {} MB{:<49} ║", disk_size, "");
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // Re-lock master images so VMs cannot accidentally open them RW.
    // Per-cell rootfs copies are produced by `cell::provision_cell_root_disks`
    // via `cp --reflink=auto`, which still works against an immutable source.
    lock_master(output);

    Ok(())
}

/// Builds disk and generates initramfs automatically.
/// Drops both in /mnt/nkr/.
/// Returns (disk_path, initramfs_path).
pub fn build_and_generate(
    nkrfile: &str,
    name: &str,
    size_mb: u32,
    context_dir: &str,
) -> Result<(String, String), Box<dyn Error>> {
    use crate::initramfs;

    let data_dir = std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string());
    let images_dir = format!("{}/images", data_dir);
    fs::create_dir_all(&images_dir)?;

    let disk_path = format!("{}/{}.ext4", images_dir, name);

    // 1. Build → disk
    build_disk(nkrfile, &disk_path, size_mb, context_dir)?;

    // 2. Generate initramfs (no Docker CMD, detects from disk)
    let initramfs_path = initramfs::generate_initramfs(name, &disk_path, None, None)?;

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ 🚀 NVM listo para usar                                      ║");
    eprintln!("║ Disco:     {:<49} ║", disk_path);
    eprintln!("║ Initramfs: {:<49} ║", initramfs_path);
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    Ok((disk_path, initramfs_path))
}

/// Cleans up the temporary Docker image created during the build
fn cleanup_image(tag: &str) -> Result<(), Box<dyn Error>> {
    let _ = Command::new("docker").args(["rmi", tag]).status();
    Ok(())
}
