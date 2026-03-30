// =============================================================================
// NKR Pull — Utiliza Docker (extractor) para construir discos ext4 desde OCI
// =============================================================================

use std::error::Error;
use std::process::Command;
use std::fs;

pub fn pull_image(image: &str, dest: &str, size_mb: u32) -> Result<(), Box<dyn Error>> {
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ NKR Pull — Construyendo disco ext4 desde Docker Hub          ║");
    eprintln!("╠══════════════════════════════════════════════════════════════╣");
    eprintln!("║ Imagen:  {:<51} ║", image);
    eprintln!("║ Destino: {:<51} ║", dest);
    eprintln!("║ Tamaño:  {} MB{:<49} ║", size_mb, "");
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // 1. Verificar si Docker está instalado
    if !Command::new("docker").arg("--version").output().is_ok() {
        return Err("Docker no está instalado. nkr pull usa 'docker create/export' como motor unicamente para generar los ext4 de las imagenes OCI.".into());
    }

    // 2. Extraer contenedor a tar local
    eprintln!("[NKR-PULL] 1/5 Descargando e instanciando contenedor...");
    let create_output = Command::new("docker")
        .args(["create", image])
        .output()
        .map_err(|e| format!("Fallo docker create: {}", e))?;

    if !create_output.status.success() {
        let err = String::from_utf8_lossy(&create_output.stderr);
        return Err(format!("No se pudo instanciar la imagen '{}': {}", image, err).into());
    }

    let container_id = String::from_utf8_lossy(&create_output.stdout).trim().to_string();
    let tar_path = format!("/tmp/nkr_{}.tar", container_id);

    eprintln!("[NKR-PULL] 2/5 Exportando filesystem a {}...", tar_path);
    let export_status = Command::new("docker")
        .args(["export", "-o", &tar_path, &container_id])
        .status()?;

    if !export_status.success() {
        // Limpiamos
        let _ = Command::new("docker").args(["rm", &container_id]).status();
        return Err("Fallo docker export".into());
    }

    // Limpiar contenedor
    let _ = Command::new("docker").args(["rm", &container_id]).status();

    // 3. Crear archivo de disco vacío
    eprintln!("[NKR-PULL] 3/5 Creando disco ext4 '{}' ({} MB)...", dest, size_mb);
    let size_bytes = (size_mb as u64) * 1024 * 1024;
    
    // Fallback manual a fs::File si truncate falla
    if Command::new("truncate").args(["-s", &format!("{}M", size_mb), dest]).status().is_err() {
        let file = fs::File::create(dest)?;
        file.set_len(size_bytes)?;
    }

    // 4. Formatear y montar
    let mkfs_status = Command::new("mkfs.ext4")
        .args(["-q", "-F", dest])
        .status()
        .map_err(|_| "mkfs.ext4 no disponible")?;

    if !mkfs_status.success() {
        let _ = fs::remove_file(&tar_path);
        return Err("mkfs.ext4 devolvió un error.".into());
    }

    let mount_dir = format!("/tmp/nkr_mnt_{}", container_id);
    fs::create_dir_all(&mount_dir)?;

    eprintln!("[NKR-PULL] 4/5 Montando disco para transferir archivos...");
    let mount_status = Command::new("mount")
        .args(["-o", "loop", dest, &mount_dir])
        .status()?;

    if !mount_status.success() {
        let _ = fs::remove_file(&tar_path);
        return Err("No se pudo montar el disco (¿ejecutando con sudo?).".into());
    }

    // 5. Extraer el tar al disco montado
    eprintln!("[NKR-PULL] 5/5 Extrayendo contenido (puede tomar un minuto)...");
    let tar_status = Command::new("tar")
        .args(["-xf", &tar_path, "-C", &mount_dir])
        .status()?;

    // Limpieza incondicional
    let _ = Command::new("umount").arg(&mount_dir).status();
    let _ = fs::remove_dir(&mount_dir);
    let _ = fs::remove_file(&tar_path);

    if !tar_status.success() {
        return Err("Error extrayendo el contenido tar al disco.".into());
    }

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ ✅ Disco creado exitosamente                                 ║");
    eprintln!("║ Archivo: {:<51} ║", dest);
    eprintln!("╚══════════════════════════════════════════════════════════════╝");
    
    Ok(())
}

/// Extrae ENTRYPOINT y CMD de una imagen Docker, retorna la combinación
fn extract_docker_cmd(image: &str) -> Option<Vec<String>> {
    let output = Command::new("docker")
        .args(["inspect", "--format", "ENTRY={{json .Config.Entrypoint}}|||CMD={{json .Config.Cmd}}", image])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let raw = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let parts: Vec<&str> = raw.splitn(2, "|||").collect();
    if parts.len() != 2 {
        return None;
    }

    let entrypoint = parse_json_array(parts[0].trim_start_matches("ENTRY="));
    let cmd = parse_json_array(parts[1].trim_start_matches("CMD="));

    let mut full = Vec::new();
    full.extend(entrypoint);
    full.extend(cmd);
    if full.is_empty() { None } else { Some(full) }
}

/// Parsea un array JSON simple como ["a","b"]
fn parse_json_array(s: &str) -> Vec<String> {
    let s = s.trim();
    if s == "null" || s.is_empty() {
        return Vec::new();
    }
    let inner = s.trim_start_matches('[').trim_end_matches(']');
    if inner.is_empty() {
        return Vec::new();
    }
    inner
        .split(',')
        .map(|v| v.trim().trim_matches('"').to_string())
        .filter(|v| !v.is_empty())
        .collect()
}

/// Deposita el disco en /mnt/nkr/images/ y genera initramfs automáticamente.
/// Retorna (ruta_disco, ruta_initramfs).
pub fn pull_and_generate(
    image: &str,
    size_mb: u32,
) -> Result<(String, String), Box<dyn Error>> {
    use crate::initramfs;

    let data_dir = std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string());
    let images_dir = format!("{}/images", data_dir);
    fs::create_dir_all(&images_dir)?;

    // Derivar nombre del disco desde la imagen: "edoburu/pgbouncer:latest" → "pgbouncer"
    let raw_name = image
        .rsplit('/')
        .next()
        .unwrap_or(image)
        .split(':')
        .next()
        .unwrap_or(image);
    let name = raw_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect::<String>();

    let disk_path = format!("{}/{}.ext4", images_dir, name);

    // 1. Pull -> disco
    pull_image(image, &disk_path, size_mb)?;

    // 2. Extraer CMD completo de Docker (ENTRYPOINT + CMD)
    let docker_cmd = extract_docker_cmd(image);
    if let Some(ref cmd) = docker_cmd {
        eprintln!("[NKR-PULL] Docker CMD detectado: {:?}", cmd);
    }

    // 3. Generar initramfs
    let initramfs_path = initramfs::generate_initramfs(&name, &disk_path, docker_cmd.as_deref())?;

    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║ 🚀 NVM listo para usar                                      ║");
    eprintln!("║ Disco:     {:<49} ║", disk_path);
    eprintln!("║ Initramfs: {:<49} ║", initramfs_path);
    eprintln!("║                                                              ║");
    eprintln!("║ En tu compose, simplemente usa:                              ║");
    eprintln!("║   disks: [{}.ext4]  (auto-resuelve todo)  {:<17} ║", name, "");
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    Ok((disk_path, initramfs_path))
}
