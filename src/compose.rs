// =============================================================================
// NKR Compose — Multi-service orchestrator with YAML
// =============================================================================

use std::collections::HashMap;
use std::fs;
use std::net::TcpStream;
use std::thread;
use std::io::{Read, Write, BufRead, BufReader};
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
// YAML model
// =============================================================================

#[derive(Deserialize)]
pub struct ComposeFile {
    pub services: HashMap<String, ServiceConfig>,
}

#[derive(Deserialize)]
pub struct ServiceConfig {
    #[serde(default)]
    pub disks: Vec<String>,
    /// Host directory to mount as rootfs (/) via shared VirtIO-FS.
    /// Replaces 'disks': the VM boots without its own block disk.
    /// Allows 100+ VMs to share the same code (e.g. Odoo) with 1 copy in page cache.
    pub rootfs: Option<String>,
    #[serde(default = "default_ram")]
    pub ram: u32,
    #[serde(default = "default_chrs")]
    pub chrs: u32,
    /// Explicit VM ID (optional, default: index+1)
    pub id: Option<u8>,
    /// Friendly NKR name (equivalent to --name). Service logical key:
    /// derives instance paths, DB name, nginx backend.
    pub nkr_name: Option<String>,
    #[serde(default)]
    pub ports: Vec<String>,
    #[serde(default)]
    pub volumes: Vec<String>,
    /// Host directories shared via VirtIO-FS (no loop-mount, no copy).
    /// Format: "host_path:guest_mountpoint"
    /// Use for large :rw data (PG data, filestore) — avoids the ~5s of mount/cp/umount.
    #[serde(default)]
    pub shares: Vec<String>,
    pub tap: Option<String>,
    pub kernel: Option<String>,
    pub initramfs: Option<String>,
    pub healthcheck: Option<HealthCheck>,
    pub build: Option<BuildConfig>,
    /// Environment variables injected into the guest
    #[serde(default)]
    pub environment: HashMap<String, String>,
    /// Enable VirtIO-PMEM+DAX for the main disk (default: true).
    /// Bypasses guest page-cache: saves ~150-200 MB/VM on Odoos.
    /// To disable (very large disks with random access saturating host
    /// page-cache), set `pmem: false` explicitly in compose.
    #[serde(default = "default_pmem")]
    pub pmem: bool,
    /// Enable CPU burst (default: true): allows using leftover CPU from other CHRs.
    #[serde(default = "default_burst")]
    pub burst: bool,
    /// Inflate VirtIO-Balloon at boot by N MB: the guest returns that RAM to host.
    /// 0 = disabled (default). Typical value: 256 for a 1024 MB Odoo VM.
    #[serde(default)]
    pub balloon_mb: u32,
    /// Skip HTTP warmup post-TCP-UP. Default `false` (performs warmup).
    /// It's set to `true` automatically when cloning via API because the clone
    /// already carries `ir_attachment` populated via `CREATE DATABASE TEMPLATE`
    /// and compiled files via clone of the filestore ext4 — running warmup with
    /// `/web/assets/debug/*` forces unnecessary recompile (55s) and impacts
    /// neighboring cells via CPU spike.
    #[serde(default)]
    pub skip_warmup: bool,
    /// Si `true`, el servicio se omite al hacer `compose up` (no se levanta
    /// ni se le pasa health-check). Default `false`.
    /// Uso típico: tenant `*-odoo-template` que sólo existe como source de
    /// clones (DB en PG, archivos en disco). No necesita Odoo corriendo.
    /// Liberar ~500 MB RAM + 1 chr CPU. Encender manualmente sólo para
    /// instalar/actualizar módulos en el template.
    #[serde(default)]
    pub disabled: bool,
}

// PMEM+DAX active by default since v1.4 (post btrfs+C migration).
// Reduces page-cache duplication (host + guest) which is the biggest RAM leak
// when scaling to 110 VMs in 32 GB. With +C the backing is stable and does
// not fragment. Disable with `pmem: false` only for backing > 4 GB with random
// access that would saturate host page-cache per VM.
fn default_pmem() -> bool { true }
fn default_burst() -> bool { true }

/// Build configuration for a service (Nkrfile)
#[derive(Deserialize, Clone)]
pub struct BuildConfig {
    /// Path to the Nkrfile
    pub nkrfile: String,
    /// Context directory (default: .)
    #[serde(default = "default_context")]
    pub context: String,
    /// Disk size in GB (default: 4)
    #[serde(default = "default_build_size")]
    pub size_mb: u32,
}

fn default_context() -> String { ".".to_string() }
fn default_build_size() -> u32 { 4 }

/// Health check configuration for a service
#[derive(Deserialize, Clone)]
pub struct HealthCheck {
    /// TCP port to verify (in the guest)
    pub port: u16,
    /// Seconds to wait before the first check (default: 10)
    #[serde(default = "default_initial_delay")]
    pub initial_delay: u64,
    /// Seconds between retries (default: 5)
    #[serde(default = "default_interval")]
    pub interval: u64,
    /// Max retry count (default: 12)
    #[serde(default = "default_retries")]
    pub retries: u32,
}

fn default_initial_delay() -> u64 { 3 }
fn default_interval() -> u64 { 2 }
fn default_retries() -> u32 { 60 }  // 60 × 2s = 120s — covers postgres crash-recovery with fine-grained polling

fn default_ram() -> u32 { 512 }
fn default_chrs() -> u32 { 1 }

const PID_FILE: &str = "/tmp/nkr-compose.pid";
const LOG_DIR: &str = "logs";
const LOG_NAME: &str = "nkr-compose.log";
const LOG_MAX_BYTES: u64 = 10 * 1024 * 1024; // 10 MB
const LOG_KEEP: u32 = 3;                      // keep 3 rotated (.1 .2 .3)
const SNAPSHOT_DIR: &str = ".nkr/snapshots";

// =============================================================================
// Log rotation — Rotates logs when they exceed LOG_MAX_BYTES
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
    // Remove the oldest
    let oldest = format!("{}.{}", base, LOG_KEEP);
    let _ = fs::remove_file(&oldest);
    // Rotate .2→.3, .1→.2, etc.
    for i in (1..LOG_KEEP).rev() {
        let from = format!("{}.{}", base, i);
        let to = format!("{}.{}", base, i + 1);
        let _ = fs::rename(&from, &to);
    }
    // Current → .1
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
// NKR Data Directory — Central store for shared resources
// =============================================================================
// Docker-style automatic resolution: if a resource doesn't exist locally,
// it is looked up in NKR_DATA_DIR (default: /mnt/nkr).
//
//   /mnt/nkr/
//   ├── images/       ← base ext4 disks
//   ├── initramfs/    ← .cpio.gz files
//   ├── kernel/       ← bzImage
//   └── snapshots/    ← CoW snapshots per stack
// =============================================================================

const NKR_DATA_DIR_DEFAULT: &str = "/mnt/nkr";

fn nkr_data_dir() -> PathBuf {
    std::env::var("NKR_DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(NKR_DATA_DIR_DEFAULT))
}

/// Resolves disk path: local → central images/
fn resolve_disk(disk: &str, yaml_dir: &Path) -> String {
    let disk_path = Path::new(disk);

    // 1. Absolute path that exists → use as-is
    if disk_path.is_absolute() {
        if disk_path.exists() || disk_path.read_link().is_ok() {
            return disk.to_string();
        }
    }

    // 2. Relative path: resolve against yaml_dir
    if !disk_path.is_absolute() {
        let local = yaml_dir.join(disk);
        if local.exists() || local.read_link().is_ok() {
            return local.to_string_lossy().to_string();
        }
    }

    // 3. Look up in central store by filename
    let filename = disk_path.file_name().unwrap_or(disk_path.as_os_str());
    let central = nkr_data_dir().join("images").join(filename);
    if central.exists() {
        eprintln!("[NKR-COMPOSE] Disco '{}' → {}", disk, central.display());
        return central.to_string_lossy().to_string();
    }

    // 4. Fallback: resolve against yaml_dir (build scenario, disk doesn't exist yet)
    if disk_path.is_absolute() {
        disk.to_string()
    } else {
        yaml_dir.join(disk).to_string_lossy().to_string()
    }
}

/// Resolves initramfs: explicit → local → central → auto-detection by service name
fn resolve_initramfs(explicit: Option<&str>, service_name: &str, disks: &[String], yaml_dir: &Path) -> Option<String> {
    let data_dir = nkr_data_dir();
    let initramfs_dir = data_dir.join("initramfs");

    // If specified explicitly, resolve its path
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
        // Look up by name in central
        if let Some(filename) = path.file_name() {
            let central = initramfs_dir.join(filename);
            if central.exists() {
                return Some(central.to_string_lossy().to_string());
            }
        }
        // Return resolved path (may not exist, but it's explicit)
        return if path.is_absolute() {
            Some(path_str.to_string())
        } else {
            Some(yaml_dir.join(path_str).to_string_lossy().to_string())
        };
    }

    // Generic auto-detection from central store
    // Priority: exact name → disk name → keyword heuristic
    if !initramfs_dir.exists() {
        return None;
    }

    let name_lower = service_name.to_lowercase();
    let disk_stems: Vec<String> = disks.iter()
        .filter_map(|d| Path::new(d).file_stem().map(|s| s.to_string_lossy().to_lowercase()))
        .collect();

    // 1) Exact service name: <service_name>.cpio.gz
    let by_name = initramfs_dir.join(format!("{}.cpio.gz", name_lower));
    if by_name.exists() {
        eprintln!("[NKR-COMPOSE] Initramfs auto-detectado (por servicio): {}", by_name.display());
        return Some(by_name.to_string_lossy().to_string());
    }

    // 2) Disk name: <disk_stem>.cpio.gz
    for stem in &disk_stems {
        let by_disk = initramfs_dir.join(format!("{}.cpio.gz", stem));
        if by_disk.exists() {
            eprintln!("[NKR-COMPOSE] Initramfs auto-detectado (por disco): {}", by_disk.display());
            return Some(by_disk.to_string_lossy().to_string());
        }
    }

    // 3) Keyword heuristic (fallback for generic names like "db")
    //    Keyword → initramfs map. Evaluated in order; the first that matches
    //    service or disk name wins.
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

/// Resolves kernel: explicit → local → central kernel/ → next to the nkr executable
///
/// Prefers nanolinux (ultrafast ELF) as absolute default.
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

    // Prefer nanolinux ELF (direct load, no gzip decompression in guest)
    let nanolinux = kernel_dir.join("nanolinux");
    if nanolinux.exists() {
        eprintln!("[NKR-COMPOSE] Kernel: nanolinux ELF (−20ms arranque) — {}", nanolinux.display());
        return nanolinux.to_string_lossy().to_string();
    }

    // Look up next to the nkr executable
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let nanolinux_sibling = exe_dir.join("nanolinux");
            if nanolinux_sibling.exists() {
                return nanolinux_sibling.to_string_lossy().to_string();
            }
        }
    }

    // Absolute fallback
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

/// Resolves snapshots directory: central btrfs if it exists, else local .nkr/snapshots
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
// compose up — Launch stack
// =============================================================================

pub fn compose_up(yaml_path: &str, detach: bool) -> Result<(), Box<dyn std::error::Error>> {
    let log_dir = resolve_log_dir(yaml_path);
    let log_path = log_dir.join(LOG_NAME);

    // ── Daemon mode: re-launch as a background process ──
    if detach {
        fs::create_dir_all(&log_dir)
            .map_err(|e| format!("No se pudo crear '{}': {}", log_dir.display(), e))?;
        rotate_logs(&log_path);

        let exe = std::env::current_exe().unwrap_or_else(|_| "nkr".into());
        let log = fs::File::create(&log_path)
            .map_err(|e| format!("No se pudo crear log '{}': {}", log_path.display(), e))?;
        let log_err = log.try_clone()?;

        // setsid() in the child: creates a new session, disconnects from the
        // controlling terminal so SIGHUP at terminal close doesn't kill the VMs.
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

        // Parse YAML to know which services have healthcheck
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
            // No healthchecks: we can't wait for readiness, inform and exit
            let log_display = log_path.display();
            eprintln!("╔══════════════════════════════════════════════════════════════╗");
            eprintln!("║  NKR Compose — {} servicio(s) en background                  ║", total_services);
            eprintln!("╠══════════════════════════════════════════════════════════════╣");
            eprintln!("║  Logs  : tail -f {:<43}║", log_display);
            eprintln!("║  Parar : nkr compose down                                    ║");
            eprintln!("╚══════════════════════════════════════════════════════════════╝");
            return Ok(());
        }

        // Wait for readiness: tail the log looking for [NKR-READY]
        // Generous timeout: initial_delay + retries*interval + margin
        let max_wait: u64 = compose.services.values()
            .filter_map(|svc| svc.healthcheck.as_ref())
            .map(|hc| hc.initial_delay + (hc.retries as u64) * hc.interval + 10)
            .max()
            .unwrap_or(120);

        eprintln!("[NKR-COMPOSE] Esperando a que los servicios estén listos (timeout {}s)...", max_wait);

        let start = std::time::Instant::now();
        let mut ready = false;

        // Wait for the log to exist and have content
        while start.elapsed().as_secs() < 5 {
            if log_path.exists() && fs::metadata(&log_path).map(|m| m.len() > 0).unwrap_or(false) {
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        // Tail the log looking for readiness markers
        let mut last_pos: u64 = 0;
        while start.elapsed().as_secs() < max_wait {
            if let Ok(f) = fs::File::open(&log_path) {
                use std::io::{Seek, SeekFrom};
                let mut f = BufReader::new(f);
                let _ = f.seek(SeekFrom::Start(last_pos));
                let mut line = String::new();
                while f.read_line(&mut line).unwrap_or(0) > 0 {
                    let trimmed = line.trim();
                    // Show health checks in real time
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

    // Base YAML directory for resolving relative paths
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

    // Auto-detect cell.yml in yaml_dir: if it exists, use its cell_id.
    // Backward-compat: no cell.yml → cell_id=0 (legacy bridge nkr0).
    let cell_config = cell::load_cell_from_dir(&yaml_dir);
    let (cell_id, cell_name): (u8, Option<String>) = match &cell_config {
        Some(c) => {
            eprintln!("[NKR-COMPOSE] Célula detectada: '{}' (cell_id={}, subnet=10.0.{}.0/24)",
                c.name, c.cell_id, c.cell_id);
            // Ensure cell bridge before launching VMs
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

    // Sort services by name for deterministic IDs
    let mut services: Vec<(String, ServiceConfig)> = compose.services.into_iter().collect();
    services.sort_by(|a, b| a.0.cmp(&b.0));

    // Filter `disabled: true` services (no se levantan, no se les hace
    // health-check). Caso típico: tenants `*-odoo-template` que sólo existen
    // como source de clones — su DB vive en PG de la cell, archivos en disco;
    // no necesitan Odoo corriendo. Liberar ~500 MB RAM + 1 chr CPU por cell.
    let disabled_count = services.iter().filter(|(_, s)| s.disabled).count();
    if disabled_count > 0 {
        let names: Vec<String> = services.iter()
            .filter(|(_, s)| s.disabled).map(|(n, _)| n.clone()).collect();
        eprintln!("[NKR-COMPOSE] {} servicio(s) marcados disabled: {:?} — omitidos.",
            disabled_count, names);
        services.retain(|(_, s)| !s.disabled);
    }

    // Resolve paths: look up local first, then central store (NKR_DATA_DIR)
    for (name, svc) in &mut services {
        // Resolve disks (local → central images/)
        for disk in &mut svc.disks {
            *disk = resolve_disk(disk, &yaml_dir);
        }

        // Resolve rootfs: local → central rootfs/
        if let Some(ref mut rootfs_path) = svc.rootfs {
            let p = Path::new(rootfs_path.as_str());
            if !p.is_absolute() {
                let local = yaml_dir.join(&*rootfs_path);
                if local.exists() {
                    *rootfs_path = local.to_string_lossy().to_string();
                } else {
                    // Look up in central store: /mnt/nkr/rootfs/<name>
                    let central = nkr_data_dir().join("rootfs").join(&*rootfs_path);
                    if central.exists() {
                        eprintln!("[NKR-COMPOSE] RootFS '{}' → {}", rootfs_path, central.display());
                        *rootfs_path = central.to_string_lossy().to_string();
                    }
                }
            }
            // Resolve shares relative to yaml_dir for rootfs mode
            // Format: host_path:guest_path[:ro|:rw]
            for share in &mut svc.shares {
                let parts: Vec<&str> = share.splitn(3, ':').collect();
                if parts.len() >= 2 && !Path::new(parts[0]).is_absolute() {
                    let abs_host = yaml_dir.join(parts[0]).to_string_lossy().to_string();
                    let suffix = if parts.len() == 3 { format!(":{}", parts[2]) } else { String::new() };
                    *share = format!("{}:{}{}", abs_host, parts[1], suffix);
                }
            }
        }

        // Resolve kernel FIRST (we need its timestamp to decide whether to regen initramfs)
        svc.kernel = Some(resolve_kernel(svc.kernel.as_deref(), &yaml_dir));

        // Resolve initramfs (explicit → local → central → auto-detection)
        svc.initramfs = resolve_initramfs(
            svc.initramfs.as_deref(),
            name,
            &svc.disks,
            &yaml_dir,
        );

        // Auto-regenerate initramfs if it doesn't exist, if the kernel is newer,
        // or if the NKR binary is newer (changes in generate_init_script)
        let kernel_path = svc.kernel.as_deref().unwrap_or("nanolinux");
        let nkr_binary = std::env::current_exe().ok();
        let needs_regen = match &svc.initramfs {
            None => true, // Doesn't exist → generate
            Some(initramfs_path) => {
                if !Path::new(initramfs_path).exists() {
                    true // Referenced file doesn't exist → regenerate
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
            match initramfs::generate_initramfs(name, disk_path, None, Some(&svc.environment)) {
                Ok(path) => {
                    eprintln!("[NKR-COMPOSE] ✅ Initramfs regenerado: {}", path);
                    svc.initramfs = Some(path);
                }
                Err(e) => {
                    eprintln!("[NKR-COMPOSE] ⚠️  No se pudo regenerar initramfs para '{}': {}", name, e);
                    // Keep the previous initramfs (if any)
                }
            }
        }

        // Volumes: only resolve relative host path against yaml_dir
        for vol in &mut svc.volumes {
            // Volumes: "host:guest[:rw]" — only resolve the host path
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

    // Promote :rw volumes from directories to dedicated ext4 disks (VirtIO-BLK).
    // Reason: the virtiofs rootfs is RO; inject/extract only works for small
    // files. For filestore/pgdata we need real RW with crash persistence.
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

                // On btrfs: create empty file + chattr +C BEFORE allocating to
                // avoid catastrophic CoW fragmentation on guest random writes
                // (PG 8K pages, filestore, etc.). Without +C, 2 GB fragments
                // into ~500k extents in weeks.
                if crate::fsutil::detect_fs(&disk_path) == crate::fsutil::FsKind::Btrfs {
                    let _ = std::fs::File::create(&disk_path);
                    let chattr = std::process::Command::new("chattr")
                        .args(["+C", &disk_path.to_string_lossy()]).status();
                    if chattr.map(|s| s.success()).unwrap_or(false) {
                        eprintln!("[NKR-COMPOSE] btrfs: chattr +C aplicado a {}", disk_path.display());
                    } else {
                        eprintln!("[NKR-COMPOSE] WARN: chattr +C falló en {} — sufrirá fragmentación CoW", disk_path.display());
                    }
                }

                // 2 GB sparse + pre-allocated extents → no future fragmentation
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
                // Initial seed: copy existing host dir content to the ext4
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

            // Add as share (vmm auto-detects .ext4 → VirtIO-BLK, mount RW with noatime)
            svc.shares.push(format!("{}:{}", disk_path.to_string_lossy(), guest_path));
            promoted.push(i);
        }
        for i in promoted.into_iter().rev() {
            svc.volumes.remove(i);
        }
    }

    // Individual file overrides (host is a file).
    // Also applies if the first disk is .ext4 (vmm will convert it to a shared
    // VirtIO-FS RO RootFS and inject_volumes won't run). This way the file
    // stays external on the host and editable without touching the image.
    for (name, svc) in services.iter_mut() {
        let will_become_shared_rootfs = svc.rootfs.is_some()
            || svc.disks.first().map_or(false, |d| d.ends_with(".ext4"));
        if !will_become_shared_rootfs { continue; }
        let service_tag = sanitize_component(name);
        let stage_dir = data_dir.join(format!("{}-overrides", service_tag));
        let mut staged_any = false;
        let mut is_odoo_svc = false;
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
                    // Rootfs is VirtIO-FS RO; bind-mount over it is unreliable.
                    // Expose share path as env var so the app uses it directly.
                    let share_path = format!("/tmp/nkr-overrides/{}", rel);
                    match guest_path {
                        "/etc/odoo/odoo.conf" => {
                            is_odoo_svc = true;
                            svc.environment.entry("ODOO_RC".to_string()).or_insert(share_path);
                        }
                        "/etc/pgbouncer/pgbouncer.ini" => {
                            svc.environment.entry("PGBOUNCER_INI".to_string()).or_insert(share_path);
                        }
                        _ => {}
                    }
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
        if is_odoo_svc {
            // sitecustomize.py: monkey-patch a werkzeug.serving para que el
            // request log muestre la IP real del cliente (X-Real-IP /
            // X-Forwarded-For inyectados por nginx) en vez del REMOTE_ADDR
            // del proxy (10.0.x.1 = gateway de la cell). Python lo carga
            // automáticamente al arrancar si el dir está en sys.path
            // (PYTHONPATH se setea en el init script — initramfs.rs).
            let sc_content = "# Auto-generated by NKR — do not edit. See compose.rs::SITECUSTOMIZE.\n\
# Reescribe el log de werkzeug (HTTP :8069) y gevent.pywsgi (longpolling/ws :8072)\n\
# para mostrar la IP real del cliente (X-Real-IP / X-Forwarded-For) en vez del\n\
# REMOTE_ADDR del proxy nginx (10.0.x.1 = gateway de la cell).\n\
try:\n    \
    import werkzeug.serving as _ws\n    \
    _orig_addr = _ws.WSGIRequestHandler.address_string\n    \
    def _nkr_addr(self):\n        \
        h = getattr(self, 'headers', None)\n        \
        if h is not None:\n            \
            ip = h.get('X-Real-IP') or (h.get('X-Forwarded-For') or '').split(',')[0].strip()\n            \
            if ip:\n                \
                return ip\n        \
        return _orig_addr(self)\n    \
    _ws.WSGIRequestHandler.address_string = _nkr_addr\n\
except Exception:\n    \
    pass\n\
try:\n    \
    import gevent.pywsgi as _pw\n    \
    _orig_fmt = _pw.WSGIHandler.format_request\n    \
    def _nkr_fmt(self):\n        \
        env = getattr(self, 'environ', None) or {}\n        \
        ip = env.get('HTTP_X_REAL_IP') or (env.get('HTTP_X_FORWARDED_FOR') or '').split(',')[0].strip()\n        \
        if ip:\n            \
            orig = getattr(self, 'client_address', None)\n            \
            try:\n                \
                if isinstance(orig, tuple):\n                    \
                    self.client_address = (ip, orig[1])\n                \
                else:\n                    \
                    self.client_address = (ip, 0)\n                \
                return _orig_fmt(self)\n            \
            finally:\n                \
                if orig is not None:\n                    \
                    self.client_address = orig\n        \
        return _orig_fmt(self)\n    \
    _pw.WSGIHandler.format_request = _nkr_fmt\n\
except Exception:\n    \
    pass\n";
            let sc_path = stage_dir.join("sitecustomize.py");
            if let Err(e) = fs::write(&sc_path, sc_content) {
                eprintln!("[NKR-COMPOSE] WARN: no pude escribir sitecustomize.py: {}", e);
            } else {
                eprintln!("[NKR-COMPOSE] Override: sitecustomize.py → guest:/tmp/nkr-overrides/ (real IP en logs)");
            }
        }
        for i in to_remove.into_iter().rev() {
            svc.volumes.remove(i);
        }
    }

    // Auto-generate pgbouncer.ini when the service has POOL_MODE (pgbouncer indicator)
    // and the rootfs will be shared RO (the image entrypoint regenerates with wrong defaults).
    for (name, svc) in services.iter_mut() {
        let will_become_shared_rootfs = svc.rootfs.is_some()
            || svc.disks.first().map_or(false, |d| d.ends_with(".ext4"));
        if !will_become_shared_rootfs { continue; }
        if !svc.environment.contains_key("POOL_MODE") { continue; }

        let db_host = svc.environment.get("DB_HOST").cloned().unwrap_or_else(|| "127.0.0.1".to_string());
        let db_port = svc.environment.get("DB_PORT").cloned().unwrap_or_else(|| "5432".to_string());
        let db_user = svc.environment.get("DB_USER").cloned().unwrap_or_else(|| "postgres".to_string());
        let db_password = svc.environment.get("DB_PASSWORD").cloned().unwrap_or_else(|| "".to_string());
        let db_name = svc.environment.get("DB_NAME").cloned().unwrap_or_else(|| "*".to_string());
        let listen_addr = svc.environment.get("LISTEN_ADDR").cloned().unwrap_or_else(|| "0.0.0.0".to_string());
        let listen_port = svc.environment.get("LISTEN_PORT").cloned().unwrap_or_else(|| "6432".to_string());
        let pool_mode = svc.environment.get("POOL_MODE").cloned().unwrap_or_else(|| "transaction".to_string());
        let auth_type = svc.environment.get("AUTH_TYPE").cloned().unwrap_or_else(|| "plain".to_string());
        let max_client_conn = svc.environment.get("MAX_CLIENT_CONN").cloned().unwrap_or_else(|| "200".to_string());
        let default_pool_size = svc.environment.get("DEFAULT_POOL_SIZE").cloned().unwrap_or_else(|| "20".to_string());

        let ini_content = format!(
            "[databases]\n{db_name} = host={db_host} port={db_port} auth_user={db_user}\n\n\
             [pgbouncer]\n\
             listen_addr = {listen_addr}\n\
             listen_port = {listen_port}\n\
             unix_socket_dir =\n\
             user = postgres\n\
             auth_file = /etc/pgbouncer/userlist.txt\n\
             auth_type = {auth_type}\n\
             pool_mode = {pool_mode}\n\
             max_client_conn = {max_client_conn}\n\
             default_pool_size = {default_pool_size}\n\
             ignore_startup_parameters = extra_float_digits\n\
             admin_users = postgres\n\
             server_reset_query = DISCARD ALL\n"
        );
        let userlist = format!("\"{db_user}\" \"{db_password}\"\n");

        let service_tag = sanitize_component(name);
        let stage_dir = data_dir.join(format!("{}-overrides", service_tag));
        let pgb_dir = stage_dir.join("etc/pgbouncer");
        let _ = fs::create_dir_all(&pgb_dir);
        let _ = fs::write(pgb_dir.join("pgbouncer.ini"), &ini_content);
        let _ = fs::write(pgb_dir.join("userlist.txt"), &userlist);
        eprintln!("[NKR-COMPOSE] PgBouncer: auto-generado pgbouncer.ini (listen={}:{})", listen_addr, listen_port);

        let share = format!("{}:/tmp/nkr-overrides:ro", stage_dir.to_string_lossy());
        if !svc.shares.iter().any(|s| s == &share) {
            svc.shares.push(share);
        }
        svc.environment.entry("PGBOUNCER_INI".to_string())
            .or_insert("/tmp/nkr-overrides/etc/pgbouncer/pgbouncer.ini".to_string());
    }

    // Ensure every service with shared rootfs has at least one writable share
    // dir so vmm.rs can deposit nkr-env with the compose env vars.
    for (name, svc) in services.iter_mut() {
        let will_become_shared_rootfs = svc.rootfs.is_some()
            || svc.disks.first().map_or(false, |d| d.ends_with(".ext4"));
        if !will_become_shared_rootfs { continue; }
        if svc.environment.is_empty() { continue; }
        // Does it already have a writable share dir?
        let has_writable_dir = svc.shares.iter().any(|s| {
            let host = s.splitn(2, ':').next().unwrap_or("");
            let is_ro = s.ends_with(":ro");
            !is_ro && std::fs::metadata(host).map(|m| m.is_dir()).unwrap_or(false)
        });
        if has_writable_dir { continue; }
        let service_tag = sanitize_component(name);
        let env_dir = data_dir.join(format!("{}-env", service_tag));
        let _ = fs::create_dir_all(&env_dir);
        svc.shares.push(format!("{}:/tmp/nkr:rw", env_dir.to_string_lossy()));
    }

    // Auto-build: if a service has `build:` and the disk doesn't exist, build it
    for (name, svc) in &services {
        if let Some(build_cfg) = &svc.build {
            let disk = &svc.disks[0];
            if !std::path::Path::new(disk).exists() {
                eprintln!("[NKR-COMPOSE] Disco '{}' no existe, construyendo desde {}...", disk, build_cfg.nkrfile);
                // Create parent directory if it doesn't exist
                if let Some(parent) = std::path::Path::new(disk).parent() {
                    let _ = fs::create_dir_all(parent);
                }
                build::build_disk(&build_cfg.nkrfile, disk, build_cfg.size_mb, &build_cfg.context)?;
                eprintln!("[NKR-COMPOSE] '{}' → disco '{}' construido", name, disk);
            }
        }
    }

    // Detect disks currently in use by other active VMs.
    // Automatic snapshot only if there's real contention over an external ext4.
    let mut active_disks: HashSet<String> = HashSet::new();
    for vm in state::list_vms() {
        for disk in vm.disks {
            active_disks.insert(canonical_string(Path::new(&disk)));
        }
    }

    // Automatically isolate ext4 disks external to the stack with NKR-managed
    // snapshots only when the base disk is already in use by another VM.
    for (_idx, (name, svc)) in services.iter_mut().enumerate() {
        let nkr_name = svc.nkr_name.clone().unwrap_or_else(|| format!("{}-{}", stack_name, name));
        let vm_id = resolve_service_id_scoped(cell_name.as_deref(), &nkr_name, svc.id)?;
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
        let config_name = svc.nkr_name.clone().unwrap_or_else(|| format!("{}-{}", stack_name, name));
        let vm_id = resolve_service_id_scoped(cell_name.as_deref(), &config_name, svc.id)?;
        let guest_ip = registry::id_to_ip(cell_id, vm_id);

        eprintln!("[NKR-COMPOSE] Lanzando '{}' (cell={}, id={}, IP={})",
            name, cell_id, vm_id, guest_ip);

        let config = VmConfig {
            hash: "".to_string(), // Auto-generated by 'nkr run' subprocess
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
            balloon_mb: svc.balloon_mb,
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

        // Burst CPU: only pass explicitly when disabled (default=true in CLI)
        if !config.burst {
            cmd.arg("--burst=false");
        }

        if config.balloon_mb > 0 {
            cmd.arg("--balloon-mb").arg(config.balloon_mb.to_string());
        }

        // Redirect stdout and stderr to add per-service prefix
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        // Pre-write nkr-env in share dirs before launching the child process.
        // vmm.rs rewrites it, but the early write guarantees virtiofsd starts
        // with the file already present in the directory.
        if config.rootfs.is_some() && !svc.environment.is_empty() {
            let mut env_content = String::from("# NKR environment variables (auto-generated)\n");
            for (key, val) in &svc.environment {
                let escaped = val.replace('\'', "'\\''");
                env_content.push_str(&format!("export {}='{}'\n", key, escaped));
            }
            for share in &config.shares {
                let host = share.splitn(2, ':').next().unwrap_or("");
                if host.is_empty() { continue; }
                if !std::fs::metadata(host).map(|m| m.is_dir()).unwrap_or(false) { continue; }
                let env_path = format!("{}/nkr-env", host);
                let _ = std::fs::write(&env_path, &env_content);
            }
        }

        let mut child = cmd.spawn().expect("Fallo al ejecutar nkr run");

        eprintln!("[NKR-COMPOSE] VM '{}' iniciada (PID {})", name, child.id());

        // Thread to prefix stdout (guest serial) + service-ready detection
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

        // Thread to prefix stderr (NKR logs)
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

        handles.push((name.clone(), vm_id, guest_ip.clone(), svc.healthcheck.clone(), child, config_name.clone(), svc.skip_warmup));

        // No sleep between launches: the real netlink/iptables serialization
        // happens in vmm.rs via netlock::NetLock (inter-process flock). Gain:
        // 500ms × (N-1) VMs; with 20 Odoos ~10s less boot time.
    }

    // Save PIDs (process IDs as reference)
    let mut pid_file = fs::File::create(PID_FILE)?;
    for (_, _, _, _, child, _, _) in &handles {
        writeln!(pid_file, "{}", child.id())?;
    }

    eprintln!("[NKR-COMPOSE] Todos los servicios lanzados. PID file: {}", PID_FILE);

    // Health checks
    let mut health_threads = Vec::new();
    for (name, _vm_id, guest_ip, hc, _, config_name, skip_warmup) in &handles {
        if let Some(check) = hc {
            let name = name.clone();
            let ip = guest_ip.clone();
            let check = check.clone();
            let cgroup_name = config_name.clone();
            let skip_warmup = *skip_warmup;
            health_threads.push((name.clone(), thread::spawn(move || {
                run_health_check(&name, &ip, &check, &cgroup_name, skip_warmup)
            })));
        }
    }

    // Wait for health checks to finish and report summary
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

    // Wait for all processes to finish
    for (_name, _, _, _, mut child, _, _) in handles {
        let _ = child.wait();
    }

    // Clean up PID file
    let _ = fs::remove_file(PID_FILE);
    eprintln!("[NKR-COMPOSE] Stack finalizado.");
    Ok(())
}

// =============================================================================
// compose down — Stop stack
// =============================================================================

pub fn compose_down(yaml_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("[NKR-COMPOSE] Deteniendo stack...");

    // Strict mode: compose down only operates on the stack defined in yaml_path.
    // If it doesn't exist or doesn't parse, return error (never global fallback).
    let content = fs::read_to_string(yaml_path)
        .map_err(|e| format!("No se pudo leer '{}': {}", yaml_path, e))?;
    let compose: ComposeFile = serde_yaml::from_str(&content)
        .map_err(|e| format!("Error parseando YAML '{}': {}", yaml_path, e))?;

    // Auto-detect cell to use scoped resolve_id
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
            let nkr = svc.nkr_name.clone().unwrap_or_else(|| _name.clone());
            resolve_service_id_scoped(cell_name.as_deref(), &nkr, svc.id).unwrap_or(0)
        })
        .filter(|id| *id > 0)
        .collect();

    eprintln!("[NKR-COMPOSE] Archivo: {} (cell_id={})", yaml_path, cell_id);
    eprintln!("[NKR-COMPOSE] Deteniendo VMs con IDs: {:?}", target_ids);

    // Use the state module to stop VMs cleanly
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

    // Clean up TAPs only for this stack's VMs (cell-aware)
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

    // Clean up PID file
    let _ = fs::remove_file(PID_FILE);

    eprintln!("[NKR-COMPOSE] Stack detenido y limpiado.");
    Ok(())
}

// =============================================================================
// compose ps — List services
// =============================================================================

pub fn compose_ps() -> Result<(), Box<dyn std::error::Error>> {
    // Use the state module to list VMs (more reliable than PID file)
    let vms = state::list_vms();

    if !vms.is_empty() {
        state::print_vm_table();
    } else if let Ok(content) = fs::read_to_string(PID_FILE) {
        // Fallback to legacy PID file — cell_id derived from cell.yml in CWD if present
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        let cell_id = cell::load_cell_from_dir(&cwd).map(|c| c.cell_id).unwrap_or(0);
        eprintln!("╔══════════════════════════════════════════════════════════════╗");
        eprintln!("║  NKR Compose — Servicios (legacy PID file)                   ║");
        eprintln!("╠══════════════════════════════════════════════════════════════╣");
        for (idx, line) in content.lines().enumerate() {
            let vm_id = (idx + 1) as u8;
            let ip = registry::id_to_ip(cell_id, vm_id);
            eprintln!("║  PID {} │ id={} │ IP {} │ running", line, vm_id, ip);
        }
        eprintln!("╚══════════════════════════════════════════════════════════════╝");
    } else {
        eprintln!("[NKR-COMPOSE] No hay stack activo");
    }
    Ok(())
}

// =============================================================================
// Health Check — Post-launch TCP verification
// =============================================================================

fn run_health_check(service_name: &str, guest_ip: &str, check: &HealthCheck, vm_name: &str, skip_warmup: bool) -> bool {
    eprintln!("[NKR-HEALTH] '{}' — esperando {}s antes de verificar puerto {}...",
        service_name, check.initial_delay, check.port);

    // ── Dynamic Nitro: relax cgroup at startup (uncapped CPU during boot) ──
    nitro_relax_cgroup(vm_name);

    thread::sleep(Duration::from_secs(check.initial_delay));

    let addr = format!("{}:{}", guest_ip, check.port);

    for attempt in 1..=check.retries {
        match TcpStream::connect_timeout(
            &addr.parse().unwrap(),
            Duration::from_secs(2),
        ) {
            Ok(_) => {
                eprintln!("[NKR-HEALTH] ✅ '{}' — puerto {} accesible (intento {}) [NKR-TCP-UP]",
                    service_name, check.port, attempt);

                // ── Pre-flight: HTTP warmup only for NON-cloned web services ──
                // - Skip PG/pgbouncer (non-HTTP port).
                // - Skip clones (skip_warmup=true): they already carry ir_attachment
                //   + filestore from src via TEMPLATE/cp; warmup with
                //   /web/assets/debug/* would force recompile (55s + CPU spike)
                //   ignoring the inherited cache.
                if is_http_port(check.port) && !skip_warmup {
                    run_warmup(service_name, guest_ip, check.port);
                } else if skip_warmup {
                    eprintln!("[NKR-HEALTH] '{}' — skip_warmup=true (clone con cache heredado)", service_name);
                }

                // ── Dynamic Nitro: restore cgroup to original quota ──
                // Warmup already compiled assets; the first real request is <500ms.
                nitro_throttle_cgroup(vm_name);

                eprintln!("[NKR-HEALTH] ✅ '{}' — listo [NKR-READY]", service_name);
                return true;
            }
            Err(_) => {
                eprintln!("[NKR-HEALTH] '{}' — intento {}/{} fallido, reintentando en {}s...",
                    service_name, attempt, check.retries, check.interval);
                thread::sleep(Duration::from_secs(check.interval));
            }
        }
    }

    // Timeout: restore cgroup anyway
    nitro_throttle_cgroup(vm_name);

    eprintln!("[NKR-HEALTH] ❌ '{}' — puerto {} NO accesible tras {} intentos",
        service_name, check.port, check.retries);
    false
}

// =============================================================================
// Pre-flight: silent HTTP warmup post-healthcheck
// =============================================================================

/// Fires a GET /web/login to the guest to force asset compilation before
/// marking the service ready. The client never sees the cold-start.
/// Ports that get warmed up with Odoo assets. For the rest (PG, pgbouncer,
/// redis, etc.) the TCP healthcheck is enough — an HTTP GET doesn't compile
/// anything.
fn is_http_port(port: u16) -> bool {
    matches!(port, 80 | 443 | 8069 | 8072 | 8000 | 8080 | 8081 | 8443)
}

fn run_warmup(service_name: &str, guest_ip: &str, port: u16) {
    let addr = format!("{}:{}", guest_ip, port);
    eprintln!("[NKR-WARMUP] '{}' — pre-vuelo: compilando assets (paralelo)...", service_name);

    let total_start = std::time::Instant::now();

    // Public Odoo assets — no authentication required.
    let assets: &[(&str, &str)] = &[
        ("/web/assets/debug/web.assets_frontend.css", "frontend CSS"),
        ("/web/assets/debug/web.assets_frontend.js",  "frontend JS"),
        ("/web/assets/debug/web.assets_backend.js",   "backend JS"),
        ("/web/login",                                 "QWeb templates"),
    ];

    // Fires the 4 GETs concurrently — total time is max(asset), not sum.
    // The Nitro cgroup is already relaxed during warmup, absorbing the CPU spike.
    let mut handles = Vec::with_capacity(assets.len());
    for (path, label) in assets {
        let addr = addr.clone();
        let guest_ip = guest_ip.to_string();
        let path = path.to_string();
        let label = label.to_string();
        let service_name = service_name.to_string();
        handles.push(thread::spawn(move || {
            let start = std::time::Instant::now();
            match warmup_get(&addr, &guest_ip, &path) {
                Ok(bytes) => {
                    let elapsed = start.elapsed().as_secs_f64();
                    eprintln!("[NKR-WARMUP] '{}' — ✅ {} compilado ({:.1}s, {} bytes)",
                        service_name, label, elapsed, bytes);
                }
                Err(e) => {
                    eprintln!("[NKR-WARMUP] '{}' — ⚠ {} falló ({}), continuando",
                        service_name, label, e);
                }
            }
        }));
    }
    for h in handles { let _ = h.join(); }

    let total = total_start.elapsed().as_secs_f64();
    eprintln!("[NKR-WARMUP] '{}' — pre-vuelo completado ({:.1}s total)", service_name, total);
}

/// Blocking GET to an endpoint — returns bytes read or error.
fn warmup_get(addr: &str, host: &str, path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let mut stream = TcpStream::connect_timeout(
        &addr.parse()?, Duration::from_secs(5)
    )?;
    stream.set_read_timeout(Some(Duration::from_secs(120)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;

    let req = format!("GET {} HTTP/1.0\r\nHost: {}\r\nConnection: close\r\n\r\n", path, host);
    stream.write_all(req.as_bytes())?;

    let mut buf = [0u8; 16384];
    let mut total = 0usize;
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(_) => break,
        }
    }
    Ok(total)
}

// =============================================================================
// Dynamic Nitro: temporary CPU boost via cgroupv2
// =============================================================================

/// Relaxes the VM's cgroup: cpu.max = "max 100000" (uncapped).
/// Called at healthcheck start so boot uses 100% CPU.
fn nitro_relax_cgroup(vm_name: &str) {
    let cpu_max = format!("/sys/fs/cgroup/nkr/{}/cpu.max", vm_name);
    if std::path::Path::new(&cpu_max).exists() {
        match std::fs::write(&cpu_max, "max 100000") {
            Ok(_) => eprintln!("[NKR-NITRO] '{}' — CPU sin límite (boost de arranque)", vm_name),
            Err(e) => eprintln!("[NKR-NITRO] '{}' — no se pudo relajar cgroup: {}", vm_name, e),
        }
    }
}

/// Restores the VM's cgroup to its original quota by reading from the state file.
/// If the quota can't be determined, applies a conservative default (40000/100000 = 40%).
fn nitro_throttle_cgroup(vm_name: &str) {
    let cpu_max = format!("/sys/fs/cgroup/nkr/{}/cpu.max", vm_name);
    if !std::path::Path::new(&cpu_max).exists() {
        return;
    }

    // Read chrs from the VM state JSON to calculate the correct quota
    let quota = read_vm_chrs(vm_name)
        .map(|chrs| chrs * 20_000)
        .unwrap_or(40_000); // fallback: 2 chrs × 20000 = 40%

    match std::fs::write(&cpu_max, format!("{} 100000", quota)) {
        Ok(_) => eprintln!("[NKR-NITRO] '{}' — CPU throttled a {}µs/100ms (velocidad crucero)", vm_name, quota),
        Err(e) => eprintln!("[NKR-NITRO] '{}' — no se pudo restaurar cgroup: {}", vm_name, e),
    }
}

/// Reads the configured CHRs for a VM from its state JSON in /tmp/nkr-vms/.
fn read_vm_chrs(vm_name: &str) -> Option<u32> {
    // Search /tmp/nkr-vms/ for the JSON that has this vm_name
    let dir = std::path::Path::new("/tmp/nkr-vms");
    if !dir.exists() { return None; }
    for entry in std::fs::read_dir(dir).ok()? {
        let entry = entry.ok()?;
        if let Ok(content) = std::fs::read_to_string(entry.path()) {
            if content.contains(vm_name) {
                // Parse chrs from the JSON: look for "chrs":N
                if let Some(pos) = content.find("\"chrs\":") {
                    let rest = &content[pos + 7..];
                    let num_str: String = rest.chars().skip_while(|c| c.is_whitespace()).take_while(|c| c.is_ascii_digit()).collect();
                    return num_str.parse().ok();
                }
            }
        }
    }
    None
}

// =============================================================================
// ID resolution via registry
// =============================================================================

/// Resolves a service ID (scoped by cell):
/// - If it has explicit id: → registers and uses it (backward-compat)
/// - If no id: → resolves it automatically by name via registry
/// - If cell_name is Some, the key is "cell/vm"; None = legacy "vm"
fn resolve_service_id_scoped(
    cell_name: Option<&str>,
    nkr_name: &str,
    explicit_id: Option<u8>,
) -> Result<u8, Box<dyn std::error::Error>> {
    match explicit_id {
        Some(id) => {
            registry::register_explicit_scoped(cell_name, nkr_name, id)?;
            Ok(id)
        }
        None => registry::resolve_id_scoped(cell_name, nkr_name),
    }
}
