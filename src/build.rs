// =============================================================================
// NKR Build — Construye discos ext4 desde Nkrfile (Dockerfile compatible)
// =============================================================================
//
// Usa Docker como motor de build para construir una imagen custom,
// luego la exporta a un disco ext4 listo para nkr run.
//
// Ejemplo de Nkrfile:
//   FROM odoo:17.0
//   RUN pip3 install phonenumbers
//   COPY ./odoo.conf /etc/odoo/odoo.conf
//
// Uso: sudo nkr build -f Nkrfile -o odoo.ext4 --size-gb 4
// =============================================================================

use std::error::Error;
use std::process::Command;
use std::fs;
use std::path::Path;

/// Construye un disco ext4 desde un Nkrfile (Dockerfile compatible)
pub fn build_disk(
    nkrfile: &str,
    output: &str,
    size_mb: u32,
    context_dir: &str,
) -> Result<(), Box<dyn Error>> {
    // Validar que el Nkrfile existe
    if !Path::new(nkrfile).exists() {
        return Err(format!("Nkrfile no encontrado: '{}'", nkrfile).into());
    }

    // Validar que Docker está disponible
    if !Command::new("docker").arg("--version").output().is_ok() {
        return Err("Docker no está instalado. nkr build usa Docker como motor de build.".into());
    }

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

    // Limpiar contenedor
    let _ = Command::new("docker").args(["rm", &container_id]).status();

    if !export_status.success() {
        let _ = cleanup_image(&tag);
        let _ = fs::remove_file(&tar_path);
        return Err("docker export falló".into());
    }

    // ── 3. Crear disco ext4 ──
    eprintln!("[NKR-BUILD] 3/5 Creando disco ext4 ({} MB)...", size_mb);

    if Command::new("truncate")
        .args(["-s", &format!("{}M", size_mb), output])
        .status()
        .is_err()
    {
        let file = fs::File::create(output)?;
        file.set_len((size_mb as u64) * 1024 * 1024)?;
    }

    let mkfs_status = Command::new("mkfs.ext4")
        .args(["-q", "-F", output])
        .status()
        .map_err(|_| "mkfs.ext4 no disponible")?;

    if !mkfs_status.success() {
        let _ = fs::remove_file(&tar_path);
        let _ = cleanup_image(&tag);
        return Err("mkfs.ext4 falló".into());
    }

    // ── 4. Montar y extraer ──
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

    // Desmontar siempre
    let _ = Command::new("umount").arg(&mount_dir).status();
    let _ = fs::remove_dir(&mount_dir);
    let _ = fs::remove_file(&tar_path);

    if !tar_status.success() {
        let _ = cleanup_image(&tag);
        return Err("tar extract falló".into());
    }

    // ── 5. Limpiar imagen temporal ──
    eprintln!("[NKR-BUILD] 5/5 Limpiando...");
    let _ = cleanup_image(&tag);

    let disk_size = fs::metadata(output).map(|m| m.len() / (1024 * 1024)).unwrap_or(0);
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ ✅ Disco construido exitosamente                            ║");
    eprintln!("║ Archivo: {:<51} ║", output);
    eprintln!("║ Tamaño:  {} MB{:<49} ║", disk_size, "");
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    Ok(())
}

/// Construye disco y genera initramfs automáticamente.
/// Deposita ambos en /mnt/nkr/.
/// Retorna (ruta_disco, ruta_initramfs).
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

    // 1. Build → disco
    build_disk(nkrfile, &disk_path, size_mb, context_dir)?;

    // 2. Generar initramfs (sin Docker CMD, detecta del disco)
    let initramfs_path = initramfs::generate_initramfs(name, &disk_path, None)?;

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ 🚀 NVM listo para usar                                      ║");
    eprintln!("║ Disco:     {:<49} ║", disk_path);
    eprintln!("║ Initramfs: {:<49} ║", initramfs_path);
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    Ok((disk_path, initramfs_path))
}

/// Limpia la imagen Docker temporal creada durante el build
fn cleanup_image(tag: &str) -> Result<(), Box<dyn Error>> {
    let _ = Command::new("docker").args(["rmi", tag]).status();
    Ok(())
}
