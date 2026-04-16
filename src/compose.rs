// =============================================================================
// NKR Compose — Orquestador de multi-servicio con YAML
// =============================================================================

use std::collections::HashMap;
use std::fs;
use std::net::TcpStream;
use std::thread;
use std::io::{Write, BufRead, BufReader};
use std::time::Duration;
use std::process::Stdio;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::collections::HashSet;

use libc;
use serde::Deserialize;

use crate::cli::VmConfig;
use crate::state;
use crate::build;
use crate::registry;
use crate::initramfs;
use crate::cell;

// =============================================================================
// Modelo YAML
// =============================================================================

#[derive(Deserialize)]
pub struct ComposeFile {
    pub services: HashMap<String, ServiceConfig>,
}

#[derive(Deserialize)]
pub struct ServiceConfig {
    #[serde(default)]
    pub disks: Vec<String>,
    /// Directorio del host a montar como rootfs (/) vía VirtIO-FS compartido.
    /// Reemplaza 'disks': la VM arranca sin disco de bloque propio.
    /// Permite que 100+ VMs compartan el mismo código (ej. Odoo) con 1 copia en page cache.
    pub rootfs: Option<String>,
    #[serde(default = "default_ram")]
    pub ram: u32,
    #[serde(default = "default_chrs")]
    pub chrs: u32,
    /// ID explícito de la VM (opcional, default: índice+1)
    pub id: Option<u8>,
    /// Nombre amigable del contenedor (equivale a --name)
    pub nvm_name: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Directorios del host compartidos vía VirtIO-FS (sin loop-mount, sin copia).
    /// Formato: "host_path:guest_mountpoint"
    /// Usar para datos grandes :rw (PG data, filestore) — evita los ~5s de mount/cp/umount.
    #[serde(default)]
    pub shares: Vec<String>,
    pub tap: Option<String>,
    pub kernel: Option<String>,
    pub initramfs: Option<String>,
    pub healthcheck: Option<HealthCheck>,
    pub build: Option<BuildConfig>,
    /// Variables de entorno inyectadas al guest
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Activar VirtIO-PMEM+DAX para el disco principal (default: true).
    /// Reduce ~150-200 MB de page cache duplicada por instancia.
    #[serde(default = "default_pmem")]
    pub pmem: bool,
    /// Activar burst de CPU (default: true): permite usar CPU sobrante de otros CHRs.
    #[serde(default = "default_burst")]
    pub burst: bool,
}

// PMEM desactivado por defecto: con ≥20 VMs, mapear el disco entero (ej. 6 GB) por VM
// agota la RAM del host antes que la RAM anónima de los guests.
// Activar solo con pocos VMs y discos < 2 GB: pmem: true en el compose.
fn default_pmem() -> bool { false }
fn default_burst() -> bool { true }

/// Configuración de build para un servicio (Nkrfile)
#[derive(Deserialize, Clone)]
pub struct BuildConfig {
    /// Ruta al Nkrfile
    pub nkrfile: String,
    /// Directorio de contexto (default: .)
    #[serde(default = "default_context")]
    pub context: String,
    /// Tamaño del disco en GB (default: 4)
    #[serde(default = "default_build_size")]
    pub size_mb: u32,
}

fn default_context() -> String { ".".to_string() }
fn default_build_size() -> u32 { 4 }

/// Configuración de health check para un servicio
#[derive(Deserialize, Clone)]
pub struct HealthCheck {
    /// Puerto TCP a verificar (en el guest)
    pub port: u16,
    /// Segundos de espera antes del primer check (default: 10)
    #[serde(default = "default_initial_delay")]
    pub initial_delay: u64,
    /// Segundos entre reintentos (default: 5)
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Número máximo de reintentos (default: 12)
    #[serde(default = "default_retries")]
    pub retries: u32,
}

fn default_initial_delay() -> u64 { 10 }
fn default_interval() -> u64 { 5 }
fn default_retries() -> u32 { 40 }  // 40 × 5s = 200s — cubre crash-recovery de postgres

fn default_ram() -> u32 { 512 }
fn default_chrs() -> u32 { 1 }

const PID_FILE: &str = "/tmp/nkr-compose.pid";
const LOG_DIR: &str = "logs";
const LOG_NAME: &str = "nkr-compose.log";
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
const LOG_KEEP: u32 = 3;                      // mantener 3 rotados (.1 .2 .3)
const SNAPSHOT_DIR: &str = ".nkr/snapshots";

// =============================================================================
// Log rotation — Rota logs cuando superan LOG_MAX_BYTES
// =============================================================================

fn resolve_log_dir(yaml_path: &str) -> std::path::PathBuf {
    let yaml = std::path::Path::new(yaml_path);
    let base = yaml.parent().unwrap_or_else(|| std::path::Path::new("."));
    base.join(LOG_DIR)
}

fn rotate_logs(log_path: &std::path::Path) {
    if !log_path.exists() { return; }
    if let Ok(meta) = fs::metadata(log_path) {
        if meta.len() < LOG_MAX_BYTES { return; }
    }
    let base = log_path.to_string_lossy().to_string();
    // Eliminar el más antiguo
    let oldest = format!("{}.{}", base, LOG_KEEP);
    let _ = fs::remove_file(&oldest);
    // Rotar .2→.3, .1→.2, etc.
    for i in (1..LOG_KEEP).rev() {
        let from = format!("{}.{}", base, i);
        let to = format!("{}.{}", base, i + 1);
        let _ = fs::rename(&from, &to);
    }
    // Actual → .1
    let _ = fs::rename(log_path, format!("{}.1", base));
}

fn canonical_or_self(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn canonical_string(path: &Path) -> String {
    canonical_or_self(path).to_string_lossy().to_string()
}

fn is_within_dir(path: &Path, dir: &Path) -> bool {
    let path = canonical_or_self(path);
    let dir = canonical_or_self(dir);
    path.starts_with(dir)
}

fn is_ext4_disk(path: &Path) -> bool {
    path.extension()
        .map(|ext| ext.to_string_lossy().eq_ignore_ascii_case("ext4"))
        .unwrap_or(false)
}

fn sanitize_component(input: &str) -> String {
    input
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

fn ensure_snapshot(base_disk: &Path, snapshot_disk: &Path) -> Result<(), Box<dyn std::error::Error>> {
    if snapshot_disk.exists() {
        return Ok(());
    }

    if !base_disk.exists() {
        return Err(format!("Disco base no existe: {}", base_disk.display()).into());
    }

    if let Some(parent) = snapshot_disk.parent() {
        fs::create_dir_all(parent)?;
    }

    let reflink_status = std::process::Command::new("cp")
        .args([
            "--reflink=auto",
            "--sparse=always",
            base_disk.to_string_lossy().as_ref(),
            snapshot_disk.to_string_lossy().as_ref(),
        ])
        .status();

    if matches!(reflink_status, Ok(status) if status.success()) {
        return Ok(());
    }

    fs::copy(base_disk, snapshot_disk)
        .map_err(|e| format!("No se pudo crear snapshot '{}': {}", snapshot_disk.display(), e))?;
    Ok(())
}

// =============================================================================
// NKR Data Directory — Almacén central de recursos compartidos
// =============================================================================
// Resolución automática estilo Docker: si un recurso no existe localmente,
// se busca en NKR_DATA_DIR (default: /mnt/nkr).
//
//   /mnt/nkr/
//   ├── images/       ← discos ext4 base
//   ├── initramfs/    ← archivos .cpio.gz
//   ├── kernel/       ← bzImage
//   └── snapshots/    ← snapshots CoW por stack
// =============================================================================

const NKR_DATA_DIR_DEFAULT: &str = "/mnt/nkr";

fn nkr_data_dir() -> PathBuf {
    std::env::var("NKR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(NKR_DATA_DIR_DEFAULT))
}

/// Resuelve ruta de disco: local → central images/
fn resolve_disk(disk: &str, yaml_dir: &Path) -> String {
    let disk_path = Path::new(disk);

    // 1. Ruta absoluta que existe → usar tal cual
    if disk_path.is_absolute() {
        if disk_path.exists() || disk_path.read_link().is_ok() {
            return disk.to_string();
        }
    }

    // 2. Ruta relativa: resolver contra yaml_dir
    if !disk_path.is_absolute() {
        let local = yaml_dir.join(disk);
        if local.exists() || local.read_link().is_ok() {
            return local.to_string_lossy().to_string();
        }
    }

    // 3. Buscar en almacén central por nombre de archivo
    let filename = disk_path.file_name().unwrap_or(disk_path.as_os_str());
    let central = nkr_data_dir().join("images").join(filename);
    if central.exists() {
        eprintln!("[NKR-COMPOSE] Disco '{}' → {}", disk, central.display());
        return central.to_string_lossy().to_string();
    }

    // 4. Fallback: resolver contra yaml_dir (build scenario, disco aún no existe)
    if disk_path.is_absolute() {
        disk.to_string()
    } else {
        yaml_dir.join(disk).to_string_lossy().to_string()
    }
}

/// Resuelve initramfs: explícito → local → central → auto-detección por nombre de servicio
fn resolve_initramfs(explicit: Option<&str>, service_name: &str, disks: &[String], yaml_dir: &Path) -> Option<String> {
    let data_dir = nkr_data_dir();
    let initramfs_dir = data_dir.join("initramfs");

    // Si se especificó explícitamente, resolver su ruta
    if let Some(path_str) = explicit {
        let path = Path::new(path_str);
        if path.is_absolute() && path.exists() {
            return Some(path_str.to_string());
        }
        if !path.is_absolute() {
            let local = yaml_dir.join(path_str);
            if local.exists() {
                return Some(local.to_string_lossy().to_string());
            }
        }
        // Buscar por nombre en central
        if let Some(filename) = path.file_name() {
            let central = initramfs_dir.join(filename);
            if central.exists() {
                return Some(central.to_string_lossy().to_string());
            }
        }
        // Devolver ruta resuelta (puede no existir, pero es explícita)
        return if path.is_absolute() {
            Some(path_str.to_string())
        } else {
            Some(yaml_dir.join(path_str).to_string_lossy().to_string())
        };
    }

    // Auto-detección genérica desde almacén central
    // Prioridad: nombre exacto → nombre de disco → heurística por keywords
    if !initramfs_dir.exists() {
        return None;
    }

    let name_lower = service_name.to_lowercase();
    let disk_stems: Vec<String> = disks.iter()
        .filter_map(|d| Path::new(d).file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .collect();

    // 1) Nombre exacto del servicio: <service_name>.cpio.gz
    let by_name = initramfs_dir.join(format!("{}.cpio.gz", name_lower));
    if by_name.exists() {
        eprintln!("[NKR-COMPOSE] Initramfs auto-detectado (por servicio): {}", by_name.display());
        return Some(by_name.to_string_lossy().to_string());
    }

    // 2) Nombre del disco: <disk_stem>.cpio.gz
    for stem in &disk_stems {
        let by_disk = initramfs_dir.join(format!("{}.cpio.gz", stem));
        if by_disk.exists() {
            eprintln!("[NKR-COMPOSE] Initramfs auto-detectado (por disco): {}", by_disk.display());
            return Some(by_disk.to_string_lossy().to_string());
        }
    }

    // 3) Heurística por keywords (fallback para nombres genéricos como "db")
    //    Mapa de keywords → initramfs. Se evalúan en orden; el primero que
    //    matchea nombre de servicio o de disco gana.
    let keyword_map: &[(&[&str], &str)] = &[
        (&["postgres", "pg", "db"],  "pg.cpio.gz"),
        (&["odoo"],                  "odoo.cpio.gz"),
        (&["nginx", "proxy", "web"], "nginx.cpio.gz"),
        (&["redis"],                 "redis.cpio.gz"),
    ];

    for (keywords, initramfs_file) in keyword_map {
        let matched = keywords.iter().any(|kw| {
            name_lower == *kw
                || disk_stems.iter().any(|d| d == *kw)
        });
        if matched {
            let path = initramfs_dir.join(initramfs_file);
            if path.exists() {
                eprintln!("[NKR-COMPOSE] Initramfs auto-detectado (por keyword): {}", path.display());
                return Some(path.to_string_lossy().to_string());
            }
        }
    }

    None
}

/// Resuelve kernel: explícito → local → central kernel/ → junto al ejecutable nkr
///
/// Prefiere nanolinux (ELF ultrarrápido) como default absoluto.
fn resolve_kernel(explicit: Option<&str>, yaml_dir: &Path) -> String {
    if let Some(path_str) = explicit {
        let path = Path::new(path_str);
        if path.is_absolute() && path.exists() {
            return path_str.to_string();
        }
        if !path.is_absolute() {
            let local = yaml_dir.join(path_str);
            if local.exists() {
                return local.to_string_lossy().to_string();
            }
        }
    }

    let kernel_dir = nkr_data_dir().join("kernel");

    // Preferir nanolinux ELF (carga directa, sin descompresión gzip en guest)
    let nanolinux = kernel_dir.join("nanolinux");
    if nanolinux.exists() {
        eprintln!("[NKR-COMPOSE] Kernel: nanolinux ELF (−20ms arranque) — {}", nanolinux.display());
        return nanolinux.to_string_lossy().to_string();
    }

    // Buscar junto al ejecutable nkr
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let nanolinux_sibling = exe_dir.join("nanolinux");
            if nanolinux_sibling.exists() {
                return nanolinux_sibling.to_string_lossy().to_string();
            }
        }
    }

    // Fallback absoluto
    if let Some(path_str) = explicit {
        if Path::new(path_str).is_absolute() {
            path_str.to_string()
        } else {
            yaml_dir.join(path_str).to_string_lossy().to_string()
        }
    } else {
        "nanolinux".to_string()
    }
}

/// Resuelve directorio de snapshots: central btrfs si existe, si no .nkr/snapshots local
fn resolve_snapshot_dir(yaml_dir: &Path) -> PathBuf {
    let data_dir = nkr_data_dir();
    let snapshots_base = data_dir.join("snapshots");

    if snapshots_base.exists() {
        let abs_dir = if yaml_dir.is_absolute() {
            yaml_dir.to_path_buf()
        } else {
            yaml_dir.canonicalize()
                .or_else(|_| std::env::current_dir())
                .unwrap_or_else(|_| yaml_dir.to_path_buf())
        };
        let stack_name = abs_dir.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "default".to_string());
        snapshots_base.join(stack_name)
    } else {
        yaml_dir.join(SNAPSHOT_DIR)
    }
}

// =============================================================================
// compose up — Lanzar stack
// =============================================================================

pub fn compose_up(yaml_path: &str, detach: bool) -> Result<(), Box<dyn std::error::Error>> {
    let log_dir = resolve_log_dir(yaml_path);
    let log_path = log_dir.join(LOG_NAME);

    // ── Modo daemon: re-lanzarse como proceso background ──
    if detach {
        fs::create_dir_all(&log_dir)
            .map_err(|e| format!("No se pudo crear '{}': {}", log_dir.display(), e))?;
        rotate_logs(&log_path);

        let exe = std::env::current_exe().unwrap_or_else(|_| "nkr".into());
        let log = fs::File::create(&log_path)
            .map_err(|e| format!("No se pudo crear log '{}': {}", log_path.display(), e))?;
        let log_err = log.try_clone()?;

        // setsid() en el hijo: crea nueva sesión, se desconecta del terminal
        // controlador para que SIGHUP al cerrar el terminal no mate los VMs.
        let child = unsafe {
            std::process::Command::new(exe)
                .args(["compose", "up", "-f", yaml_path])
                .stdout(log)
                .stderr(log_err)
                .stdin(Stdio::null())
                .pre_exec(|| {
                    if libc::setsid() < 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                })
                .spawn()
                .map_err(|e| format!("No se pudo demonizar: {}", e))?
        };

        eprintln!("[NKR-COMPOSE] Lanzando stack en background (PID {})...", child.id());

        // Parsear YAML para saber qué servicios tienen healthcheck
        let content = fs::read_to_string(yaml_path)
            .map_err(|e| format!("No se pudo leer '{}': {}", yaml_path, e))?;
        let compose: ComposeFile = serde_yaml::from_str(&content)
            .map_err(|e| format!("Error parseando YAML: {}", e))?;
        let services_with_hc: Vec<String> = compose.services.iter()
            .filter(|(_, svc)| svc.healthcheck.is_some())
            .map(|(name, _)| name.clone())
            .collect();
        let total_services = compose.services.len();

        if services_with_hc.is_empty() {
            // Sin healthchecks: no podemos esperar readiness, informar y salir
            let log_display = log_path.display();
            eprintln!("╔══════════════════════════════════════════════════════════════╗");
            eprintln!("║  NKR Compose — {} servicio(s) en background                  ║", total_services);
            eprintln!("╠══════════════════════════════════════════════════════════════╣");
            eprintln!("║  Logs  : tail -f {:<43}║", log_display);
            eprintln!("║  Parar : nkr compose down                                    ║");
            eprintln!("╚══════════════════════════════════════════════════════════════╝");
            return Ok(());
        }

        // Esperar readiness: tail del log buscando [NKR-READY]
        // Timeout generoso: initial_delay + retries*interval + margen
        let max_wait: u64 = compose.services.values()
            .filter_map(|svc| svc.healthcheck.as_ref())
            .map(|hc| hc.initial_delay + (hc.retries as u64) * hc.interval + 10)
            .max()
            .unwrap_or(120);

        eprintln!("[NKR-COMPOSE] Esperando a que los servicios estén listos (timeout {}s)...", max_wait);

        let start = std::time::Instant::now();
        let mut ready = false;

        // Esperar a que el log exista y tenga contenido
        while start.elapsed().as_secs() < 5 {
            if log_path.exists() && fs::metadata(&log_path).map(|m| m.len() > 0).unwrap_or(false) {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        // Tail del log buscando marcadores de readiness
        let mut last_pos: u64 = 0;
        while start.elapsed().as_secs() < max_wait {
            if let Ok(f) = fs::File::open(&log_path) {
                use std::io::{Seek, SeekFrom};
                let mut f = BufReader::new(f);
                let _ = f.seek(SeekFrom::Start(last_pos));
                let mut line = String::new();
                while f.read_line(&mut line).unwrap_or(0) > 0 {
                    let trimmed = line.trim();
                    // Mostrar health checks en tiempo real
                    if trimmed.contains("[NKR-HEALTH]") {
                        eprintln!("{}", trimmed);
                    }
                    if trimmed.contains("[NKR-READY]") {
                        eprintln!("{}", trimmed);
                        ready = true;
                    }
                    line.clear();
                }
                last_pos = f.seek(SeekFrom::Current(0)).unwrap_or(last_pos);
            }
            if ready { break; }
            thread::sleep(Duration::from_millis(500));
        }

        let log_display = log_path.display();
        if ready {
            eprintln!("╔══════════════════════════════════════════════════════════════╗");
            eprintln!("║  NKR Compose — {} servicio(s) listos ✅                       ║", total_services);
            eprintln!("╠══════════════════════════════════════════════════════════════╣");
            eprintln!("║  Logs  : tail -f {:<43}║", log_display);
            eprintln!("║  Parar : nkr compose down                                    ║");
            eprintln!("╚══════════════════════════════════════════════════════════════╝");
        } else {
            eprintln!("╔══════════════════════════════════════════════════════════════╗");
            eprintln!("║  NKR Compose — timeout esperando readiness ⚠️                ║");
            eprintln!("╠══════════════════════════════════════════════════════════════╣");
            eprintln!("║  Los servicios podrían seguir arrancando.                     ║");
            eprintln!("║  Logs  : tail -f {:<43}║", log_display);
            eprintln!("║  Parar : nkr compose down                                    ║");
            eprintln!("╚══════════════════════════════════════════════════════════════╝");
        }
        return Ok(());
    }

    let content = fs::read_to_string(yaml_path)
        .map_err(|e| format!("No se pudo leer '{}': {}", yaml_path, e))?;
    let compose: ComposeFile = serde_yaml::from_str(&content)
        .map_err(|e| format!("Error parseando YAML: {}", e))?;

    // Directorio base del YAML para resolver paths relativos
    let yaml_parent = std::path::Path::new(yaml_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let yaml_dir = if yaml_parent.as_os_str().is_empty() {
        std::path::Path::new(".").canonicalize()
    } else {
        yaml_parent.canonicalize()
    }.unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));

    let stack_name = yaml_dir.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "default".to_string());

    // Auto-detectar cell.yml en yaml_dir: si existe, usar su cell_id.
    // Backward-compat: sin cell.yml → cell_id=0 (bridge nkr0 legacy).
    let cell_config = cell::load_cell_from_dir(&yaml_dir);
    let (cell_id, cell_name): (u8, Option<String>) = match &cell_config {
        Some(c) => {
            eprintln!("[NKR-COMPOSE] Célula detectada: '{}' (cell_id={}, subnet=10.0.{}.0/24)",
                c.name, c.cell_id, c.cell_id);
            // Asegurar bridge de la célula antes de lanzar VMs
            if let Err(e) = cell::ensure_cell_bridge(c.cell_id) {
                eprintln!("[NKR-COMPOSE] WARN: bridge celda no creado: {}", e);
            }
            (c.cell_id, Some(c.name.clone()))
        }
        None => (0, None),
    };

    let total = compose.services.len();
    eprintln!("╔══════════════════════════════════════════════════════════════╗");
    eprintln!("║  NKR Compose — {} servicio(s)                                ║", total);
    eprintln!("╚══════════════════════════════════════════════════════════════╝");

    // Ordenar servicios por nombre para IDs determinísticos
    let mut services: Vec<(String, ServiceConfig)> = compose.services.into_iter().collect();
    services.sort_by(|a, b| a.0.cmp(&b.0));

    // Resolver paths: buscar primero local, luego almacén central (NKR_DATA_DIR)
    for (name, svc) in &mut services {
        // Resolver discos (local → central images/)
        for disk in &mut svc.disks {
            *disk = resolve_disk(disk, &yaml_dir);
        }

        // Resolver rootfs: local → central rootfs/
        if let Some(ref mut rootfs_path) = svc.rootfs {
            let p = Path::new(rootfs_path.as_str());
            if !p.is_absolute() {
                let local = yaml_dir.join(&*rootfs_path);
                if local.exists() {
                    *rootfs_path = local.to_string_lossy().to_string();
                } else {
                    // Buscar en almacén central: /mnt/nkr/rootfs/<name>
                    let central = nkr_data_dir().join("rootfs").join(&*rootfs_path);
                    if central.exists() {
                        eprintln!("[NKR-COMPOSE] RootFS '{}' → {}", rootfs_path, central.display());
                        *rootfs_path = central.to_string_lossy().to_string();
                    }
                }
            }
            // Resolver shares relativas al yaml_dir para rootfs mode
            // Formato: host_path:guest_path[:ro|:rw]
            for share in &mut svc.shares {
                let parts: Vec<&str> = share.splitn(3, ':').collect();
                if parts.len() >= 2 && !Path::new(parts[0]).is_absolute() {
                    let abs_host = yaml_dir.join(parts[0]).to_string_lossy().to_string();
                    let suffix = if parts.len() == 3 { format!(":{}", parts[2]) } else { String::new() };
                    *share = format!("{}:{}{}", abs_host, parts[1], suffix);
                }
            }
        }

        // Resolver kernel PRIMERO (necesitamos su timestamp para decidir si regenerar initramfs)
        svc.kernel = Some(resolve_kernel(svc.kernel.as_deref(), &yaml_dir));

        // Resolver initramfs (explícito → local → central → auto-detección)
        svc.initramfs = resolve_initramfs(
            svc.initramfs.as_deref(),
            name,
            &svc.disks,
            &yaml_dir,
        );

        // Auto-regenerar initramfs si no existe, si el kernel es más nuevo,
        // o si el binario NKR es más nuevo (cambios en generate_init_script)
        let kernel_path = svc.kernel.as_deref().unwrap_or("nanolinux");
        let nkr_binary = std::env::current_exe().ok();
        let needs_regen = match &svc.initramfs {
            None => true, // No existe → generar
            Some(initramfs_path) => {
                if !Path::new(initramfs_path).exists() {
                    true // El archivo referenciado no existe → regenerar
                } else {
                    let initramfs_mtime = fs::metadata(initramfs_path)
                        .and_then(|m| m.modified())
                        .ok();
                    let kernel_mtime = fs::metadata(kernel_path)
                        .and_then(|m| m.modified())
                        .ok();
                    let binary_mtime = nkr_binary.as_deref()
                        .and_then(|p| fs::metadata(p).ok())
                        .and_then(|m| m.modified().ok());
                    match initramfs_mtime {
                        None => true,
                        Some(it) => {
                            kernel_mtime.map_or(false, |kt| kt > it)
                                || binary_mtime.map_or(false, |bt| bt > it)
                        }
                    }
                }
            }
        };

        if needs_regen {
            let disk_path_owned: String;
            let disk_path = if let Some(d) = svc.disks.first() {
                d.as_str()
            } else if let Some(ref r) = svc.rootfs {
                disk_path_owned = r.clone();
                disk_path_owned.as_str()
            } else { "" };
            eprintln!("[NKR-COMPOSE] Regenerando initramfs para '{}' (kernel nuevo o initramfs ausente)...", name);
            match initramfs::generate_initramfs(name, disk_path, None) {
                Ok(path) => {
                    eprintln!("[NKR-COMPOSE] ✅ Initramfs regenerado: {}", path);
                    svc.initramfs = Some(path);
                }
                Err(e) => {
                    eprintln!("[NKR-COMPOSE] ⚠️  No se pudo regenerar initramfs para '{}': {}", name, e);
                    // Mantener el initramfs anterior (si existía)
                }
            }
        }

        // Volumes: solo resolver host path relativo contra yaml_dir
        for vol in &mut svc.volumes {
            // Volumes: "host:guest[:rw]" — solo resolver el host path
            let parts: Vec<&str> = vol.splitn(3, ':').collect();
            if parts.len() >= 2 && !std::path::Path::new(parts[0]).is_absolute() {
                let abs_host = yaml_dir.join(parts[0]).to_string_lossy().to_string();
                *vol = if parts.len() == 3 {
                    format!("{}:{}:{}", abs_host, parts[1], parts[2])
                } else {
                    format!("{}:{}", abs_host, parts[1])
                };
            }
        }
        if let Some(ref mut build_cfg) = svc.build {
            if !std::path::Path::new(&build_cfg.nkrfile).is_absolute() {
                build_cfg.nkrfile = yaml_dir.join(&build_cfg.nkrfile).to_string_lossy().to_string();
            }
            if !std::path::Path::new(&build_cfg.context).is_absolute() {
                build_cfg.context = yaml_dir.join(&build_cfg.context).to_string_lossy().to_string();
            }
        }
    }

    // Promover volúmenes :rw de directorios a discos ext4 dedicados (VirtIO-BLK).
    // Razón: el rootfs virtiofs es RO; inject/extract sólo sirve para archivos pequeños.
    // Para filestore/pgdata necesitamos RW real con persistencia sobre crash.
    let data_dir = yaml_dir.join(".nkr-data");
    for (name, svc) in services.iter_mut() {
        let service_tag = sanitize_component(name);
        let mut promoted: Vec<usize> = Vec::new();
        for (i, vol) in svc.volumes.iter().enumerate() {
            let parts: Vec<&str> = vol.splitn(3, ':').collect();
            if parts.len() != 3 || parts[2] != "rw" { continue; }
            let host_path = parts[0];
            let guest_path = parts[1];
            let md = match std::fs::metadata(host_path) {
                Ok(m) => m,
                Err(_) => continue,
            };
            if !md.is_dir() { continue; }

            let disk_stem = sanitize_component(&guest_path.replace('/', "_"));
            let disk_name = format!("{}-{}.ext4", service_tag, disk_stem.trim_start_matches('_'));
            let disk_path = data_dir.join(&disk_name);

            if !disk_path.exists() {
                let _ = fs::create_dir_all(&data_dir);
                eprintln!("[NKR-COMPOSE] Creando disco dedicado para volumen :rw → {}", disk_path.display());
                // 2 GB sparse + pre-allocated extents → sin fragmentación futura
                let fallocate = std::process::Command::new("fallocate")
                    .args(["-l", "2G", &disk_path.to_string_lossy()])
                    .status();
                if !fallocate.map(|s| s.success()).unwrap_or(false) {
                    eprintln!("[NKR-COMPOSE] WARN: fallocate falló, intentando truncate");
                    let _ = std::process::Command::new("truncate")
                        .args(["-s", "2G", &disk_path.to_string_lossy()]).status();
                }
                let mkfs = std::process::Command::new("mkfs.ext4")
                    .args(["-F", "-E", "lazy_itable_init=0,lazy_journal_init=0",
                           "-O", "^has_journal",
                           &disk_path.to_string_lossy()])
                    .status();
                if !mkfs.map(|s| s.success()).unwrap_or(false) {
                    return Err(format!("mkfs.ext4 falló en {}", disk_path.display()).into());
                }
                // Semilla inicial: copiar contenido existente del host dir al ext4
                let entries_exist = std::fs::read_dir(host_path)
                    .map(|mut d| d.next().is_some()).unwrap_or(false);
                if entries_exist {
                    let seed_mnt = format!("/tmp/nkr_seed_{}", std::process::id());
                    let _ = fs::create_dir_all(&seed_mnt);
                    let mounted = std::process::Command::new("mount")
                        .args(["-o", "loop", &disk_path.to_string_lossy(), &seed_mnt])
                        .status().map(|s| s.success()).unwrap_or(false);
                    if mounted {
                        let _ = std::process::Command::new("cp")
                            .args(["-a", &format!("{}/.", host_path), &seed_mnt]).status();
                        let _ = std::process::Command::new("umount").arg(&seed_mnt).status();
                    }
                    let _ = fs::remove_dir(&seed_mnt);
                }
            }

            // Añadir como share (vmm auto-detecta .ext4 → VirtIO-BLK, mount RW con noatime)
            svc.shares.push(format!("{}:{}", disk_path.to_string_lossy(), guest_path));
            promoted.push(i);
        }
        for i in promoted.into_iter().rev() {
            svc.volumes.remove(i);
        }
    }

    // Overrides de archivos individuales (host es un archivo).
    // El rootfs compartido es VirtIO-FS RO → inject_volumes no corre.
    // Estrategia: staging dir por servicio en .nkr-data/<svc>-overrides/, montado RO
    // vía VirtIO-FS en /tmp/nkr-overrides; initramfs hace bind-mount de cada archivo.
    for (name, svc) in services.iter_mut() {
        let service_tag = sanitize_component(name);
        let stage_dir = data_dir.join(format!("{}-overrides", service_tag));
        let mut staged_any = false;
        let mut to_remove: Vec<usize> = Vec::new();
        for (i, vol) in svc.volumes.iter().enumerate() {
            let parts: Vec<&str> = vol.splitn(3, ':').collect();
            if parts.len() < 2 { continue; }
            let host_path = parts[0];
            let guest_path = parts[1];
            let md = match std::fs::metadata(host_path) { Ok(m) => m, Err(_) => continue };
            if !md.is_file() { continue; }
            if !guest_path.starts_with('/') { continue; }

            let rel = guest_path.trim_start_matches('/');
            let dest = stage_dir.join(rel);
            if let Some(parent) = dest.parent() { let _ = fs::create_dir_all(parent); }
            match fs::copy(host_path, &dest) {
                Ok(_) => {
                    eprintln!("[NKR-COMPOSE] Override: {} → guest:{}", host_path, guest_path);
                    staged_any = true;
                    to_remove.push(i);
                }
                Err(e) => eprintln!("[NKR-COMPOSE] WARN: no se pudo copiar {}: {}", host_path, e),
            }
        }
        if staged_any {
            let share = format!("{}:/tmp/nkr-overrides:ro", stage_dir.to_string_lossy());
            if !svc.shares.iter().any(|s| s == &share) {
                svc.shares.push(share);
            }
        }
        for i in to_remove.into_iter().rev() {
            svc.volumes.remove(i);
        }
    }

    // Auto-build: si un servicio tiene `build:` y el disco no existe, construirlo
    for (name, svc) in &services {
        if let Some(build_cfg) = &svc.build {
            let disk = &svc.disks[0];
            if !std::path::Path::new(disk).exists() {
                eprintln!("[NKR-COMPOSE] Disco '{}' no existe, construyendo desde {}...", disk, build_cfg.nkrfile);
                // Crear directorio padre si no existe
                if let Some(parent) = std::path::Path::new(disk).parent() {
                    let _ = fs::create_dir_all(parent);
                }
                build::build_disk(&build_cfg.nkrfile, disk, build_cfg.size_mb, &build_cfg.context)?;
                eprintln!("[NKR-COMPOSE] '{}' → disco '{}' construido", name, disk);
            }
        }
    }

    // Detectar discos actualmente en uso por otras VMs activas.
    // Snapshot automático solo si hay contención real sobre un ext4 externo.
    let mut active_disks: HashSet<String> = HashSet::new();
    for vm in state::list_vms() {
        for disk in vm.disks {
            active_disks.insert(canonical_string(Path::new(&disk)));
        }
    }

    // Aislar automáticamente discos ext4 externos al stack con snapshots gestionados por NKR
    // solo cuando el disco base ya está en uso por otra VM.
    for (_idx, (name, svc)) in services.iter_mut().enumerate() {
        let nvm_name = svc.nvm_name.clone().unwrap_or_else(|| format!("{}-{}", stack_name, name));
        let vm_id = resolve_service_id_scoped(cell_name.as_deref(), &nvm_name, svc.id)?;
        let service_tag = sanitize_component(name);

        for (disk_idx, disk) in svc.disks.iter_mut().enumerate() {
            let disk_path = Path::new(disk);

            if !is_ext4_disk(disk_path) {
                continue;
            }

            if is_within_dir(disk_path, &yaml_dir) {
                continue;
            }

            let canonical_base = canonical_string(disk_path);
            if !active_disks.contains(&canonical_base) {
                continue;
            }

            let snapshot_name = format!("{}-vm{}-disk{}.ext4", service_tag, vm_id, disk_idx);
            let snapshot_dir = resolve_snapshot_dir(&yaml_dir);
            let snapshot_path = snapshot_dir.join(&snapshot_name);

            ensure_snapshot(disk_path, &snapshot_path)?;
            *disk = snapshot_path.to_string_lossy().to_string();
        }
    }

    let mut handles = Vec::new();

    for (_idx, (name, svc)) in services.into_iter().enumerate() {
        let config_name = svc.nvm_name.clone().unwrap_or_else(|| format!("{}-{}", stack_name, name));
        let vm_id = resolve_service_id_scoped(cell_name.as_deref(), &config_name, svc.id)?;
        let guest_ip = registry::id_to_ip(cell_id, vm_id);

        eprintln!("[NKR-COMPOSE] Lanzando '{}' (cell={}, id={}, IP={})",
            name, cell_id, vm_id, guest_ip);

        let config = VmConfig {
            hash: "".to_string(), // Auto-generado por el subprocess 'nkr run'
            name: config_name.clone(),
            ram_mb: svc.ram,
            chrs: svc.chrs,
            vm_id,
            disks: svc.disks.clone(),
            kernel_path: svc.kernel.unwrap_or_else(|| "nanolinux".to_string()),
            initramfs_path: svc.initramfs,
            port_forwards: svc.ports.clone(),
            volumes: svc.volumes.clone(),
            env_vars: svc.environment.iter().map(|(k, v)| format!("{}={}", k, v)).collect(),
            tap_name: svc.tap,
            shares: svc.shares.clone(),
            rootfs: svc.rootfs.clone(),
            use_pmem: svc.pmem,
            balloon_mb: 0,
            burst: svc.burst,
            cell_id,
        };

        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap_or_else(|_| "nkr".into()));
        cmd.arg("run")
           .arg("--name").arg(&config_name)
           .arg("--ram").arg(config.ram_mb.to_string())
           .arg("-c").arg(config.chrs.to_string())
           .arg("--id").arg(config.vm_id.to_string())
           .arg("--cell-id").arg(config.cell_id.to_string());

        for disk in &config.disks {
            cmd.arg("--disk").arg(disk);
        }

        cmd.arg("--kernel").arg(&config.kernel_path);

        if let Some(initramfs) = &config.initramfs_path {
            cmd.arg("--initramfs").arg(initramfs);
        }

        for port in &config.port_forwards {
            cmd.arg("--port").arg(port);
        }

        for vol in &config.volumes {
            cmd.arg("--volume").arg(vol);
        }

        for share in &config.shares {
            cmd.arg("--share").arg(share);
        }

        for (key, val) in &svc.environment {
            cmd.arg("--env").arg(format!("{}={}", key, val));
        }

        if let Some(tap) = &config.tap_name {
            cmd.arg("--tap").arg(tap);
        }

        if let Some(ref rootfs_path) = config.rootfs {
            cmd.arg("--rootfs").arg(rootfs_path);
        }

        // Smart defaults v1.3
        if config.use_pmem {
            cmd.arg("--pmem");
        }

        // Burst CPU: solo pasar explícitamente cuando está desactivado (default=true en CLI)
        if !config.burst {
            cmd.arg("--burst=false");
        }

        // Redirigir stdout y stderr para agregar prefijo por servicio
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("Fallo al ejecutar nkr run");

        eprintln!("[NKR-COMPOSE] VM '{}' iniciada (PID {})", name, child.id());

        // Thread para prefixear stdout (serial del guest) + detección de servicio listo
        let svc_name_out = name.clone();
        if let Some(stdout) = child.stdout.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                let mut ready_announced = false;
                for line in reader.lines() {
                    if let Ok(line) = line {
                        eprintln!("[{}] {}", svc_name_out, line);
                        if !ready_announced {
                            if line.contains("database system is ready to accept connections")
                                || line.contains("listening on")
                                || line.contains("HTTP service")
                                || line.contains("Listening on")
                                || line.contains("process up")
                                || line.contains("Bus READY")
                            {
                                eprintln!("[NKR-COMPOSE] servicio '{}' listo", svc_name_out);
                                ready_announced = true;
                            }
                        }
                    }
                }
            });
        }

        // Thread para prefixear stderr (logs NKR)
        let svc_name_err = name.clone();
        if let Some(stderr) = child.stderr.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stderr);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        eprintln!("[{}] {}", svc_name_err, line);
                    }
                }
            });
        }

        handles.push((name.clone(), vm_id, guest_ip.clone(), svc.healthcheck.clone(), child, config_name.clone()));

        // Pequeña pausa entre launches para evitar conflictos
        thread::sleep(std::time::Duration::from_millis(500));
    }

    // Guardar PIDs (process IDs como referencia)
    let mut pid_file = fs::File::create(PID_FILE)?;
    for (_, _, _, _, child, _) in &handles {
        writeln!(pid_file, "{}", child.id())?;
    }

    eprintln!("[NKR-COMPOSE] Todos los servicios lanzados. PID file: {}", PID_FILE);

    // Health checks
    let mut health_threads = Vec::new();
    for (name, _vm_id, guest_ip, hc, _, config_name) in &handles {
        if let Some(check) = hc {
            let name = name.clone();
            let ip = guest_ip.clone();
            let check = check.clone();
            // Pasar config_name (= cgroup name) para throttle post-warmup (Nitro Dinámico)
            let cgroup_name = config_name.clone();
            health_threads.push((name.clone(), thread::spawn(move || {
                run_health_check(&name, &ip, &check, &cgroup_name)
            })));
        }
    }

    // Esperar a que terminen los health checks y reportar resumen
    let mut all_ok = true;
    let mut ready_names = Vec::new();
    let mut failed_names = Vec::new();
    for (name, t) in health_threads {
        match t.join() {
            Ok(true) => ready_names.push(name),
            _ => { all_ok = false; failed_names.push(name); }
        }
    }
    if all_ok && !ready_names.is_empty() {
        eprintln!("[NKR-READY] Todos los servicios listos: {}", ready_names.join(", "));
    } else if !ready_names.is_empty() || !failed_names.is_empty() {
        if !ready_names.is_empty() {
            eprintln!("[NKR-READY] Servicios listos: {}", ready_names.join(", "));
        }
        if !failed_names.is_empty() {
            eprintln!("[NKR-READY] Servicios fallidos: {}", failed_names.join(", "));
        }
    }

    // Esperar a que todos los procesos terminen
    for (_name, _, _, _, mut child, _) in handles {
        let _ = child.wait();
    }

    // Limpiar PID file
    let _ = fs::remove_file(PID_FILE);
    eprintln!("[NKR-COMPOSE] Stack finalizado.");
    Ok(())
}

// =============================================================================
// compose down — Detener stack
// =============================================================================

pub fn compose_down(yaml_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[NKR-COMPOSE] Deteniendo stack...");

    // Modo estricto: compose down solo opera sobre el stack definido en yaml_path.
    // Si no existe o no parsea, devolver error (nunca fallback global).
    let content = fs::read_to_string(yaml_path)
        .map_err(|e| format!("No se pudo leer '{}': {}", yaml_path, e))?;
    let compose: ComposeFile = serde_yaml::from_str(&content)
        .map_err(|e| format!("Error parseando YAML '{}': {}", yaml_path, e))?;

    // Auto-detectar célula para usar scoped resolve_id
    let yaml_parent = std::path::Path::new(yaml_path)
        .parent()
        .unwrap_or_else(|| std::path::Path::new("."));
    let yaml_dir = if yaml_parent.as_os_str().is_empty() {
        std::path::Path::new(".").canonicalize()
    } else {
        yaml_parent.canonicalize()
    }.unwrap_or_else(|_| std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from(".")));
    let cell_config = cell::load_cell_from_dir(&yaml_dir);
    let (cell_id, cell_name): (u8, Option<String>) = match &cell_config {
        Some(c) => (c.cell_id, Some(c.name.clone())),
        None => (0, None),
    };

    let mut services: Vec<(String, ServiceConfig)> = compose.services.into_iter().collect();
    services.sort_by(|a, b| a.0.cmp(&b.0));
    let target_ids: Vec<u8> = services.iter()
        .map(|(_name, svc)| {
            let nvm = svc.nvm_name.clone().unwrap_or_else(|| _name.clone());
            resolve_service_id_scoped(cell_name.as_deref(), &nvm, svc.id).unwrap_or(0)
        })
        .filter(|id| *id > 0)
        .collect();

    eprintln!("[NKR-COMPOSE] Archivo: {} (cell_id={})", yaml_path, cell_id);
    eprintln!("[NKR-COMPOSE] Deteniendo VMs con IDs: {:?}", target_ids);

    // Usar el módulo de estado para detener VMs limpiamente
    let vms = state::list_vms();
    if vms.is_empty() {
        eprintln!("[NKR-COMPOSE] No hay VMs activas para este stack.");
    } else {
        let mut any_stopped = false;
        for vm in &vms {
            if !target_ids.contains(&vm.vm_id) {
                eprintln!("[NKR-COMPOSE] VM {} no pertenece a este stack, omitida", vm.vm_id);
                continue;
            }

            match state::stop_vm(vm.vm_id) {
                Ok(()) => {
                    any_stopped = true;
                    eprintln!("[NKR-COMPOSE] VM {} detenida", vm.vm_id);
                }
                Err(e) => eprintln!("[NKR-COMPOSE] Error deteniendo VM {}: {}", vm.vm_id, e),
            }
        }
        if !any_stopped {
            eprintln!("[NKR-COMPOSE] No se encontró ninguna VM activa de este compose.");
        }
    }

    // Limpiar TAPs solo de las VMs de este stack (cell-aware)
    for i in &target_ids {
        let tap = if cell_id == 0 {
            format!("nkr-tap{}", i)
        } else {
            format!("nkr-c{}-tap{}", cell_id, i)
        };
        let _ = std::process::Command::new("ip")
            .args(["link", "delete", &tap])
            .status();
    }

    // Limpiar PID file
    let _ = fs::remove_file(PID_FILE);

    eprintln!("[NKR-COMPOSE] Stack detenido y limpiado.");
    Ok(())
}

// =============================================================================
// compose ps — Listar servicios
// =============================================================================

pub fn compose_ps() -> Result<(), Box<dyn std::error::Error>> {
    // Usar el módulo de estado para listar VMs (más fiable que PID file)
    let vms = state::list_vms();

    if !vms.is_empty() {
        state::print_vm_table();
    } else if let Ok(content) = fs::read_to_string(PID_FILE) {
        // Fallback al PID file legacy
        eprintln!("╔══════════════════════════════════════════════════════════════╗");
        eprintln!("║  NKR Compose — Servicios (legacy PID file)                   ║");
        eprintln!("╠══════════════════════════════════════════════════════════════╣");
        for (idx, line) in content.lines().enumerate() {
            let vm_id = idx + 1;
            let ip = format!("10.0.0.{}", vm_id + 1);
            eprintln!("║  PID {} │ id={} │ IP {} │ running", line, vm_id, ip);
        }
        eprintln!("╚══════════════════════════════════════════════════════════════╝");
    } else {
        eprintln!("[NKR-COMPOSE] No hay stack activo");
    }
    Ok(())
}

// =============================================================================
// Health Check — Verificación TCP post-launch
// =============================================================================

fn run_health_check(service_name: &str, guest_ip: &str, check: &HealthCheck, vm_name: &str) -> bool {
    eprintln!("[NKR-HEALTH] '{}' — esperando {}s antes de verificar puerto {}...",
        service_name, check.initial_delay, check.port);

    // ── Nitro Dinámico: relajar cgroup al iniciar (CPU sin límite durante boot) ──
    nitro_relax_cgroup(vm_name);

    thread::sleep(Duration::from_secs(check.initial_delay));

    let addr = format!("{}:{}", guest_ip, check.port);

    for attempt in 1..=check.retries {
        match TcpStream::connect_timeout(
            &addr.parse().unwrap(),
            Duration::from_secs(2),
        ) {
            Ok(_) => {
                eprintln!("[NKR-HEALTH] ✅ '{}' — puerto {} accesible (intento {})",
                    service_name, check.port, attempt);

                // ── Pre-Vuelo: warmup HTTP para forzar compilación de assets ──
                run_warmup(service_name, guest_ip, check.port);

                // ── Nitro Dinámico: restaurar cgroup al quota original ──
                nitro_throttle_cgroup(vm_name);

                return true;
            }
            Err(_) => {
                eprintln!("[NKR-HEALTH] '{}' — intento {}/{} fallido, reintentando en {}s...",
                    service_name, attempt, check.retries, check.interval);
                thread::sleep(Duration::from_secs(check.interval));
            }
        }
    }

    // Timeout: restaurar cgroup de todas formas
    nitro_throttle_cgroup(vm_name);

    eprintln!("[NKR-HEALTH] ❌ '{}' — puerto {} NO accesible tras {} intentos",
        service_name, check.port, check.retries);
    false
}

// =============================================================================
// Pre-Vuelo: Warmup HTTP silencioso post-healthcheck
// =============================================================================

/// Dispara un GET /web/login al guest para forzar la compilación de assets
/// antes de marcar el servicio como listo. El cliente nunca ve el cold-start.
fn run_warmup(service_name: &str, guest_ip: &str, port: u16) {
    let addr = format!("{}:{}", guest_ip, port);
    eprintln!("[NKR-WARMUP] '{}' — pre-vuelo GET /web/login (compilando assets)...", service_name);

    let start = std::time::Instant::now();

    match TcpStream::connect_timeout(&addr.parse().unwrap(), Duration::from_secs(5)) {
        Ok(mut stream) => {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(120)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

            let request = format!(
                "GET /web/login HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n",
                guest_ip
            );
            if stream.write_all(request.as_bytes()).is_ok() {
                // Leer toda la respuesta (fuerza a Odoo a compilar assets)
                let mut buf = [0u8; 4096];
                let mut total = 0usize;
                loop {
                    match std::io::Read::read(&mut stream, &mut buf) {
                        Ok(0) => break,
                        Ok(n) => total += n,
                        Err(_) => break,
                    }
                }
                let elapsed = start.elapsed().as_secs_f64();
                eprintln!("[NKR-WARMUP] ✅ '{}' — assets compilados ({:.1}s, {} bytes)",
                    service_name, elapsed, total);
            }
        }
        Err(e) => {
            eprintln!("[NKR-WARMUP] '{}' — warmup fallido ({}), continuando sin pre-vuelo",
                service_name, e);
        }
    }
}

// =============================================================================
// Nitro Dinámico: CPU boost temporal via cgroupv2
// =============================================================================

/// Relaja el cgroup de la VM: cpu.max = "max 100000" (sin límite).
/// Se llama al inicio del healthcheck para que el boot use 100% CPU.
fn nitro_relax_cgroup(vm_name: &str) {
    let cpu_max = format!("/sys/fs/cgroup/nkr/{}/cpu.max", vm_name);
    if std::path::Path::new(&cpu_max).exists() {
        match std::fs::write(&cpu_max, "max 100000") {
            Ok(_) => eprintln!("[NKR-NITRO] '{}' — CPU sin límite (boost de arranque)", vm_name),
            Err(e) => eprintln!("[NKR-NITRO] '{}' — no se pudo relajar cgroup: {}", vm_name, e),
        }
    }
}

/// Restaura el cgroup de la VM a su quota original leyéndolo del fichero de estado.
/// Si no puede determinar el quota, aplica un default conservador (40000/100000 = 40%).
fn nitro_throttle_cgroup(vm_name: &str) {
    let cpu_max = format!("/sys/fs/cgroup/nkr/{}/cpu.max", vm_name);
    if !std::path::Path::new(&cpu_max).exists() {
        return;
    }

    // Leer chrs del JSON de estado de la VM para calcular el quota correcto
    let quota = read_vm_chrs(vm_name)
        .map(|chrs| chrs * 20_000)
        .unwrap_or(40_000); // fallback: 2 chrs × 20000 = 40%

    match std::fs::write(&cpu_max, format!("{} 100000", quota)) {
        Ok(_) => eprintln!("[NKR-NITRO] '{}' — CPU throttled a {}µs/100ms (velocidad crucero)", vm_name, quota),
        Err(e) => eprintln!("[NKR-NITRO] '{}' — no se pudo restaurar cgroup: {}", vm_name, e),
    }
}

/// Lee los CHRs configurados para una VM desde su JSON de estado en /tmp/nkr-vms/.
fn read_vm_chrs(vm_name: &str) -> Option<u32> {
    // Buscar en /tmp/nkr-vms/ el JSON que tenga este vm_name
    let dir = std::path::Path::new("/tmp/nkr-vms");
    if !dir.exists() { return None; }
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            if content.contains(vm_name) {
                // Parsear chrs del JSON: buscar "chrs":N
                if let Some(pos) = content.find("\"chrs\":") {
                    let rest = &content[pos + 7..];
                    let num_str: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
                    return num_str.parse().ok();
                }
            }
        }
    }
    None
}

// =============================================================================
// Resolución de ID via registry
// =============================================================================

/// Resuelve el ID de un servicio (scoped por célula):
/// - Si tiene id: explícito → lo registra y usa (backward-compat)
/// - Si no tiene id: → lo resuelve automáticamente por nombre via registry
/// - Si cell_name es Some, la key es "cell/vm"; None = legacy "vm"
fn resolve_service_id_scoped(
    cell_name: Option<&str>,
    nvm_name: &str,
    explicit_id: Option<u8>,
) -> Result<u8, Box<dyn std::error::Error>> {
    match explicit_id {
        Some(id) => {
            registry::register_explicit_scoped(cell_name, nvm_name, id)?;
            Ok(id)
        }
        None => registry::resolve_id_scoped(cell_name, nvm_name),
    }
}
