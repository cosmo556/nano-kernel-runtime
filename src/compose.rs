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
use std::path::{Path, PathBuf};
use std::collections::HashSet;

use serde::Deserialize;

use crate::cli::VmConfig;
use crate::state;
use crate::build;
use crate::registry;

// =============================================================================
// Modelo YAML
// =============================================================================

#[derive(Deserialize)]
pub struct ComposeFile {
    pub services: HashMap<String, ServiceConfig>,
}

#[derive(Deserialize)]
pub struct ServiceConfig {
    pub disks: Vec<String>,
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
    pub tap: Option<String>,
    pub kernel: Option<String>,
    pub initramfs: Option<String>,
    pub healthcheck: Option<HealthCheck>,
    pub build: Option<BuildConfig>,
    /// Variables de entorno inyectadas al guest
    #[serde(default)]
    pub environment: HashMap<String, String>,
}

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
fn default_retries() -> u32 { 12 }

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

    // Buscar en almacén central
    let central = nkr_data_dir().join("kernel").join("bzImage");
    if central.exists() {
        return central.to_string_lossy().to_string();
    }

    // Buscar junto al ejecutable nkr
    if let Ok(exe) = std::env::current_exe() {
        if let Some(exe_dir) = exe.parent() {
            let sibling = exe_dir.join("bzImage");
            if sibling.exists() {
                return sibling.to_string_lossy().to_string();
            }
        }
    }

    // Fallback
    if let Some(path_str) = explicit {
        if Path::new(path_str).is_absolute() {
            path_str.to_string()
        } else {
            yaml_dir.join(path_str).to_string_lossy().to_string()
        }
    } else {
        "bzImage".to_string()
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

        let child = std::process::Command::new(exe)
            .args(["compose", "up", "-f", yaml_path])
            .stdout(log)
            .stderr(log_err)
            .stdin(Stdio::null())
            .spawn()
            .map_err(|e| format!("No se pudo demonizar: {}", e))?;

        let log_display = log_path.display();
        eprintln!("╔══════════════════════════════════════════════════════════════╗");
        eprintln!("║  NKR Compose — lanzado en background (PID {:<6})             ║", child.id());
        eprintln!("╠══════════════════════════════════════════════════════════════╣");
        eprintln!("║  Logs   : tail -f {:<42}                                     ║", log_display);
        eprintln!("║  Estado : nkr compose ps                                     ║");
        eprintln!("║  Parar  : nkr compose down                                   ║");
        eprintln!("╚══════════════════════════════════════════════════════════════╝");
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

        // Resolver initramfs (explícito → local → central → auto-detección)
        svc.initramfs = resolve_initramfs(
            svc.initramfs.as_deref(),
            name,
            &svc.disks,
            &yaml_dir,
        );

        // Resolver kernel (explícito → local → central kernel/ → junto a ejecutable)
        svc.kernel = Some(resolve_kernel(svc.kernel.as_deref(), &yaml_dir));

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
        let vm_id = resolve_service_id(&nvm_name, svc.id)?;
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
        let vm_id = resolve_service_id(&config_name, svc.id)?;
        let guest_ip = registry::id_to_ip(vm_id);

        eprintln!("[NKR-COMPOSE] Lanzando '{}' (id={}, IP={})", name, vm_id, guest_ip);

        let config = VmConfig {
            hash: "".to_string(), // Auto-generado por el subprocess 'nkr run'
            name: config_name.clone(),
            ram_mb: svc.ram,
            chrs: svc.chrs,
            vm_id,
            disks: svc.disks.clone(),
            kernel_path: svc.kernel.unwrap_or_else(|| "bzImage".to_string()),
            initramfs_path: svc.initramfs,
            port_forwards: svc.ports.clone(),
            volumes: svc.volumes.clone(),
            env_vars: svc.environment.iter().map(|(k, v)| format!("{}={}", k, v)).collect(),
            tap_name: svc.tap,
        };

        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap_or_else(|_| "nkr".into()));
        cmd.arg("run")
           .arg("--name").arg(&config_name)
           .arg("--ram").arg(config.ram_mb.to_string())
           .arg("-c").arg(config.chrs.to_string())
           .arg("--id").arg(config.vm_id.to_string());

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

        for (key, val) in &svc.environment {
            cmd.arg("--env").arg(format!("{}={}", key, val));
        }

        if let Some(tap) = &config.tap_name {
            cmd.arg("--tap").arg(tap);
        }

        // Redirigir stdout y stderr para agregar prefijo por servicio
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().expect("Fallo al ejecutar nkr run");

        // Thread para prefixear stdout (serial del guest)
        let svc_name_out = name.clone();
        if let Some(stdout) = child.stdout.take() {
            thread::spawn(move || {
                let reader = BufReader::new(stdout);
                for line in reader.lines() {
                    if let Ok(line) = line {
                        eprintln!("[{}] {}", svc_name_out, line);
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

        handles.push((name.clone(), vm_id, guest_ip.clone(), svc.healthcheck.clone(), child));

        // Pequeña pausa entre launches para evitar conflictos
        thread::sleep(std::time::Duration::from_millis(500));
    }

    // Guardar PIDs (process IDs como referencia)
    let mut pid_file = fs::File::create(PID_FILE)?;
    for (_, _, _, _, child) in &handles {
        writeln!(pid_file, "{}", child.id())?;
    }

    eprintln!("[NKR-COMPOSE] Todos los servicios lanzados. PID file: {}", PID_FILE);

    // Health checks
    let mut health_threads = Vec::new();
    for (name, _vm_id, guest_ip, hc, _) in &handles {
        if let Some(check) = hc {
            let name = name.clone();
            let ip = guest_ip.clone();
            let check = check.clone();
            health_threads.push(thread::spawn(move || {
                run_health_check(&name, &ip, &check);
            }));
        }
    }

    // Esperar a que terminen los health checks (best-effort)
    for t in health_threads {
        let _ = t.join();
    }

    // Esperar a que todos los procesos terminen
    for (_name, _, _, _, mut child) in handles {
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

    let mut services: Vec<(String, ServiceConfig)> = compose.services.into_iter().collect();
    services.sort_by(|a, b| a.0.cmp(&b.0));
    let target_ids: Vec<u8> = services.iter()
        .map(|(_name, svc)| {
            let nvm = svc.nvm_name.clone().unwrap_or_else(|| _name.clone());
            resolve_service_id(&nvm, svc.id).unwrap_or(0)
        })
        .filter(|id| *id > 0)
        .collect();

    eprintln!("[NKR-COMPOSE] Archivo: {}", yaml_path);
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

    // Limpiar TAPs solo de las VMs de este stack
    for i in &target_ids {
        let tap = format!("nkr-tap{}", i);
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

fn run_health_check(service_name: &str, guest_ip: &str, check: &HealthCheck) {
    eprintln!("[NKR-HEALTH] '{}' — esperando {}s antes de verificar puerto {}...",
        service_name, check.initial_delay, check.port);

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
                return;
            }
            Err(_) => {
                eprintln!("[NKR-HEALTH] '{}' — intento {}/{} fallido, reintentando en {}s...",
                    service_name, attempt, check.retries, check.interval);
                thread::sleep(Duration::from_secs(check.interval));
            }
        }
    }

    eprintln!("[NKR-HEALTH] ❌ '{}' — puerto {} NO accesible tras {} intentos",
        service_name, check.port, check.retries);
}

// =============================================================================
// Resolución de ID via registry
// =============================================================================

/// Resuelve el ID de un servicio:
/// - Si tiene id: explícito → lo registra y usa (backward-compat)
/// - Si no tiene id: → lo resuelve automáticamente por nombre via registry
fn resolve_service_id(nvm_name: &str, explicit_id: Option<u8>) -> Result<u8, Box<dyn std::error::Error>> {
    match explicit_id {
        Some(id) => {
            // ID explícito: registrarlo para evitar colisiones futuras
            registry::register_explicit(nvm_name, id)?;
            Ok(id)
        }
        None => {
            // Auto-asignar via registry (determinístico por nombre)
            registry::resolve_id(nvm_name)
        }
    }
}
