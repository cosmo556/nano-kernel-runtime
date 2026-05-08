// =============================================================================
// NKR Cell — Cell management (groups of VMs with isolated network)
// =============================================================================
//
// Each cell is an independent stack (e.g. 20 Odoo + PgBouncer + PG) with its
// own L2/L3 subnet:
//
//   Cell "nazcatex" (cell_id=1) → bridge nkr-br1, subnet 10.0.1.0/24
//   Cell "cafeteria" (cell_id=2) → bridge nkr-br2, subnet 10.0.2.0/24
//
// The cell_id is auto-assigned by the Cell Registry (persisted to disk).
// cell_id=0 is the legacy mode (bridge nkr0, subnet 10.0.0.0/24).
// =============================================================================

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// NKR base data directory
fn nkr_data_dir() -> PathBuf {
    PathBuf::from(
        std::env::var("NKR_DATA_DIR").unwrap_or_else(|_| "/mnt/nkr".to_string()),
    )
}

fn cell_registry_path() -> PathBuf {
    nkr_data_dir().join("cell-registry.json")
}

pub fn cells_dir() -> PathBuf {
    nkr_data_dir().join("cells")
}

const MIN_CELL_ID: u8 = 1;
const MAX_CELL_ID: u8 = 254;

// =============================================================================
// Cell Registry — Persistent map name → cell_id
// =============================================================================

#[derive(Serialize, Deserialize, Default, Clone)]
struct CellRegistry {
    entries: HashMap<String, u8>,
}

impl CellRegistry {
    fn load() -> Self {
        let path = cell_registry_path();
        if path.exists() {
            if let Ok(content) = fs::read_to_string(&path) {
                if let Ok(reg) = serde_json::from_str::<CellRegistry>(&content) {
                    return reg;
                }
            }
        }
        CellRegistry::default()
    }

    fn save(&self) -> Result<(), Box<dyn std::error::Error>> {
        let path = cell_registry_path();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        fs::write(&path, json)?;
        Ok(())
    }

    fn used_ids(&self) -> Vec<u8> {
        self.entries.values().cloned().collect()
    }

    fn next_free_id(&self) -> Option<u8> {
        let used = self.used_ids();
        (MIN_CELL_ID..=MAX_CELL_ID).find(|id| !used.contains(id))
    }
}

// =============================================================================
// CellConfig — Metadata persisted in cell.yml
// =============================================================================

#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CellConfig {
    pub name: String,
    pub cell_id: u8,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odoo_version: Option<String>,
}

impl CellConfig {
    /// Subnet prefix for this cell: "10.0.{cell_id}"
    #[allow(dead_code)]
    pub fn subnet_prefix(&self) -> String {
        cell_id_to_subnet(self.cell_id)
    }

    /// Linux bridge name for this cell
    #[allow(dead_code)]
    pub fn bridge_name(&self) -> String {
        cell_bridge_name(self.cell_id)
    }
}

// =============================================================================
// Public API
// =============================================================================

/// Returns the subnet prefix for a cell_id: "10.0.{cell_id}"
pub fn cell_id_to_subnet(cell_id: u8) -> String {
    format!("10.0.{}", cell_id)
}

/// Bridge name for a cell_id
pub fn cell_bridge_name(cell_id: u8) -> String {
    if cell_id == 0 {
        "nkr0".to_string()
    } else {
        format!("nkr-br{}", cell_id)
    }
}

/// Creates a new cell: registers, creates directories, writes cell.yml
pub fn create_cell(name: &str, odoo_version: Option<&str>) -> Result<CellConfig, Box<dyn std::error::Error>> {
    let key = name.trim().to_lowercase();
    if key.is_empty() {
        return Err("El nombre de la célula no puede estar vacío".into());
    }

    let mut reg = CellRegistry::load();

    // Check if it already exists
    if let Some(&existing_id) = reg.entries.get(&key) {
        return Err(format!(
            "La célula '{}' ya existe (cell_id={}, subnet=10.0.{}.0/24)",
            name, existing_id, existing_id
        ).into());
    }

    // Assign cell_id
    let cell_id = reg.next_free_id()
        .ok_or("No hay cell_ids disponibles (rango 1-254 agotado)")?;

    reg.entries.insert(key.clone(), cell_id);
    reg.save()?;

    // Create directory structure
    let cell_dir = cells_dir().join(&key);
    fs::create_dir_all(cell_dir.join("addons"))?;
    fs::create_dir_all(cell_dir.join("files"))?;
    fs::create_dir_all(cell_dir.join("config"))?;
    fs::create_dir_all(cell_dir.join("logs"))?;
    fs::create_dir_all(cell_dir.join("pg"))?;

    // Write cell.yml
    let config = CellConfig {
        name: key.clone(),
        cell_id,
        odoo_version: odoo_version.map(|s| s.to_string()),
    };

    let cell_yml_path = cell_dir.join("cell.yml");
    let yaml = serde_yaml::to_string(&config)?;
    fs::write(&cell_yml_path, yaml)?;

    eprintln!("[NKR-CELL] Célula '{}' creada (cell_id={}, subnet=10.0.{}.0/24)",
        name, cell_id, cell_id);
    eprintln!("[NKR-CELL] Directorio: {}", cell_dir.display());

    // Per-cell reflink of shared master images (postgres, pgbouncer). Skipped
    // silently if any master is missing — the cell can still be created and
    // the operator can call `provision_cell_root_disks` later (or run
    // `nkr build -f Nkrfile.pg` first). This preserves the legacy flow where
    // a cell.yml is created before any master exists in the host.
    if let Err(e) = provision_cell_root_disks(&key) {
        eprintln!("[NKR-CELL] WARN: provision_cell_root_disks: {} \
                   — la cell se creó sin reflinks; correr el helper a mano \
                   tras `nkr build -f Nkrfile.pg`/`Nkrfile.pgbouncer`.", e);
    }

    Ok(config)
}

/// Master ext4 images that get a private per-cell reflink copy. Each tuple is
/// (master file under /mnt/nkr/images/, name of the per-cell copy under the
/// cell directory). Extend this list when a new shared image becomes part of
/// a cell's bring-up.
///
/// Why per-cell copies: the master files are shared across cells (same path,
/// `/mnt/nkr/images/postgres.ext4` is referenced by every cell). Mapping the
/// master directly via virtio-pmem (MAP_SHARED + PROT_WRITE) means two cells
/// that run simultaneously write to the same backing → eventual ext4
/// corruption. The reflink copy gives each cell its own physical file with
/// btrfs CoW: the kernel handles divergent writes per-inode, master is never
/// touched.
const CELL_ROOTFS_MASTERS: &[(&str, &str)] = &[
    ("postgres.ext4",  "postgres-root.ext4"),
    ("pgbouncer.ext4", "pgbouncer-root.ext4"),
];

/// Provisions per-cell reflink copies of the well-known master ext4 images
/// (postgres, pgbouncer). Idempotent: skips masters that have already been
/// reflinked. On hosts where btrfs reflink is unavailable, `cp --reflink=auto`
/// transparently falls back to a physical copy — slower but functionally
/// equivalent.
///
/// NOTE: deliberately does NOT apply `chattr +C` to the resulting copies.
/// Applying NoCoW to a file whose extents are shared via reflink is a no-op
/// in btrfs (the flag is accepted but does not affect existing extents, and
/// shared extents must always CoW on write to avoid corrupting the source).
/// The rootfs is read-mostly in operation, so the residual fragmentation is
/// operationally negligible. PG checkpoint/WAL traffic goes to
/// `pg/data.ext4`, which is created fresh with `+C` on an empty file by
/// `fsutil::create_ext4_disk` — that path keeps NoCoW effective.
pub fn provision_cell_root_disks(
    cell_name: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let cell_dir = cells_dir().join(cell_name);
    let images_dir = nkr_data_dir().join("images");
    provision_cell_root_disks_with_paths(&cell_dir, &images_dir)
}

/// Path-injectable variant of `provision_cell_root_disks`, used by tests that
/// can't touch the real /mnt/nkr layout. Production code should call the
/// public wrapper above.
pub(crate) fn provision_cell_root_disks_with_paths(
    cell_dir: &Path,
    images_dir: &Path,
) -> Result<(), Box<dyn std::error::Error>> {
    if !cell_dir.exists() {
        return Err(format!("cell dir missing: {}", cell_dir.display()).into());
    }
    if !images_dir.exists() {
        return Err(format!("images dir missing: {}", images_dir.display()).into());
    }

    for (src_name, dst_name) in CELL_ROOTFS_MASTERS {
        let src = images_dir.join(src_name);
        let dst = cell_dir.join(dst_name);
        if !src.exists() {
            return Err(format!(
                "master ext4 missing: {} (run `nkr build -f Nkrfile.{}` first)",
                src.display(),
                src_name.trim_end_matches(".ext4")
            ).into());
        }
        if dst.exists() {
            // Idempotent: master was already reflinked into this cell.
            continue;
        }
        // cp -a --reflink=auto: O(1) on btrfs (CoW), falls back to a physical
        // copy on ext4/xfs hosts.
        let status = std::process::Command::new("cp")
            .args(["-a", "--reflink=auto",
                   &src.to_string_lossy(),
                   &dst.to_string_lossy()])
            .status()?;
        if !status.success() {
            return Err(format!(
                "cp --reflink failed: {} → {}",
                src.display(), dst.display()
            ).into());
        }
        // Best-effort consistency check on the new copy. e2fsck -p only fixes
        // safe issues and exits 0; non-zero is informative, not fatal.
        let _ = std::process::Command::new("e2fsck")
            .args(["-p", &dst.to_string_lossy()])
            .status();
        eprintln!("[NKR-CELL] reflinked {} → {}",
            src.display(), dst.display());
    }
    Ok(())
}

/// Loads a cell's configuration from its cell.yml
pub fn load_cell(name: &str) -> Result<CellConfig, Box<dyn std::error::Error>> {
    let key = name.trim().to_lowercase();
    let cell_dir = cells_dir().join(&key);
    let cell_yml = cell_dir.join("cell.yml");

    if !cell_yml.exists() {
        return Err(format!("Célula '{}' no encontrada (no existe {})", name, cell_yml.display()).into());
    }

    let content = fs::read_to_string(&cell_yml)?;
    let config: CellConfig = serde_yaml::from_str(&content)?;
    Ok(config)
}

/// Attempts to load cell.yml from a directory (for compose auto-detection)
pub fn load_cell_from_dir(dir: &Path) -> Option<CellConfig> {
    let cell_yml = dir.join("cell.yml");
    if !cell_yml.exists() {
        return None;
    }
    let content = fs::read_to_string(&cell_yml).ok()?;
    serde_yaml::from_str(&content).ok()
}

/// Lists all registered cells
pub fn list_cells() -> Vec<CellConfig> {
    let cells = cells_dir();
    if !cells.exists() {
        return Vec::new();
    }

    let mut result = Vec::new();
    if let Ok(entries) = fs::read_dir(&cells) {
        for entry in entries.flatten() {
            let cell_yml = entry.path().join("cell.yml");
            if cell_yml.exists() {
                if let Ok(content) = fs::read_to_string(&cell_yml) {
                    if let Ok(config) = serde_yaml::from_str::<CellConfig>(&content) {
                        result.push(config);
                    }
                }
            }
        }
    }

    result.sort_by_key(|c| c.cell_id);
    result
}

/// Looks up the cell_id for a name, if it exists
pub fn lookup_cell_id(name: &str) -> Option<u8> {
    let reg = CellRegistry::load();
    let key = name.trim().to_lowercase();
    reg.entries.get(&key).cloned()
}

/// Reverse lookup: cell_id → cell name. Returns None if it doesn't exist or id=0.
pub fn lookup_cell_name(cell_id: u8) -> Option<String> {
    if cell_id == 0 { return None; }
    let reg = CellRegistry::load();
    reg.entries.iter()
        .find(|(_, &v)| v == cell_id)
        .map(|(k, _)| k.clone())
}

/// Removes a cell from the registry (does not delete disk data)
pub fn destroy_cell(name: &str) -> Result<bool, Box<dyn std::error::Error>> {
    let key = name.trim().to_lowercase();
    let mut reg = CellRegistry::load();

    let cell_id = match reg.entries.remove(&key) {
        Some(id) => id,
        None => return Ok(false),
    };
    reg.save()?;

    // Remove cell.yml but NOT the data
    let cell_yml = cells_dir().join(&key).join("cell.yml");
    let _ = fs::remove_file(&cell_yml);

    // Destroy bridge if it exists
    let _ = destroy_cell_bridge(cell_id);

    eprintln!("[NKR-CELL] Célula '{}' eliminada del registry (datos preservados en {})",
        name, cells_dir().join(&key).display());
    Ok(true)
}

/// Path to a cell's compose file
pub fn cell_compose_path(name: &str) -> PathBuf {
    let key = name.trim().to_lowercase();
    cells_dir().join(&key).join("nkr-compose.yml")
}

/// Directory of a cell
#[allow(dead_code)]
pub fn cell_dir(name: &str) -> PathBuf {
    let key = name.trim().to_lowercase();
    cells_dir().join(&key)
}

// =============================================================================
// Bridge Management — Creates/destroys per-cell Linux bridges
// =============================================================================

/// Creates the network bridge for a cell with NAT and forwarding
pub fn ensure_cell_bridge(cell_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    let bridge = cell_bridge_name(cell_id);
    let subnet = cell_id_to_subnet(cell_id);
    let gateway = format!("{}.1/24", subnet);
    let subnet_cidr = format!("{}.0/24", subnet);

    // Check if it already exists (outside the lock — the check is cheap and serves as fast-path)
    let check = std::process::Command::new("ip")
        .args(["link", "show", &bridge])
        .output();
    if check.map_or(false, |o| o.status.success()) {
        return Ok(()); // Already exists
    }

    // Serializes bridge creation + iptables rules across concurrent `nkr run`
    // processes (avoids duplicates and "File exists" in rtnetlink).
    let _netlock = crate::netlock::NetLock::acquire("cell-bridge");

    // Re-check inside the lock: another process may have created the bridge
    // while we were waiting.
    let recheck = std::process::Command::new("ip")
        .args(["link", "show", &bridge])
        .output();
    if recheck.map_or(false, |o| o.status.success()) {
        return Ok(());
    }

    eprintln!("[NKR-CELL] Creando bridge {} ({})...", bridge, subnet_cidr);

    // Create bridge
    let status = std::process::Command::new("ip")
        .args(["link", "add", "name", &bridge, "type", "bridge"])
        .status()
        .map_err(|e| format!("Fallo creando bridge {}: {}", bridge, e))?;
    if !status.success() {
        return Err(format!("Fallo 'ip link add {}' (¿ejecutando con sudo?)", bridge).into());
    }

    // Assign gateway IP
    let _ = std::process::Command::new("ip")
        .args(["addr", "add", &gateway, "dev", &bridge])
        .status();

    // Bring bridge up
    let _ = std::process::Command::new("ip")
        .args(["link", "set", &bridge, "up"])
        .status();

    // IP forwarding
    let _ = fs::write("/proc/sys/net/ipv4/ip_forward", "1");
    let _ = fs::write(&format!("/proc/sys/net/ipv4/conf/{}/route_localnet", bridge), "1");
    let _ = fs::write("/proc/sys/net/ipv4/conf/all/route_localnet", "1");

    // Helper: adds iptables rule only if it doesn't exist (silences stderr of -C)
    let iptables_ensure = |args_check: &[&str], args_add: &[&str]| {
        let check = crate::netlock::iptables()
            .args(args_check)
            .stderr(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .status();
        if !check.map(|s| s.success()).unwrap_or(false) {
            let _ = crate::netlock::iptables()
                .args(args_add)
                .status();
        }
    };

    // NAT/Masquerade
    iptables_ensure(
        &["-t", "nat", "-C", "POSTROUTING", "-s", &subnet_cidr, "-j", "MASQUERADE"],
        &["-t", "nat", "-A", "POSTROUTING", "-s", &subnet_cidr, "-j", "MASQUERADE"],
    );

    // FORWARD rules
    iptables_ensure(
        &["-C", "FORWARD", "-i", &bridge, "-j", "ACCEPT"],
        &["-A", "FORWARD", "-i", &bridge, "-j", "ACCEPT"],
    );
    iptables_ensure(
        &["-C", "FORWARD", "-o", &bridge, "-j", "ACCEPT"],
        &["-A", "FORWARD", "-o", &bridge, "-j", "ACCEPT"],
    );

    eprintln!("[NKR-CELL] Bridge {} creado ({}, NAT habilitado)", bridge, subnet_cidr);
    Ok(())
}

/// Destroys a cell's bridge and cleans up iptables rules
pub fn destroy_cell_bridge(cell_id: u8) -> Result<(), Box<dyn std::error::Error>> {
    if cell_id == 0 {
        return Ok(()); // Do not destroy legacy bridge nkr0
    }

    let bridge = cell_bridge_name(cell_id);
    let subnet_cidr = format!("{}.0/24", cell_id_to_subnet(cell_id));

    // Remove iptables rules
    let _ = crate::netlock::iptables()
        .args(["-t", "nat", "-D", "POSTROUTING", "-s", &subnet_cidr, "-j", "MASQUERADE"])
        .status();
    let _ = crate::netlock::iptables()
        .args(["-D", "FORWARD", "-i", &bridge, "-j", "ACCEPT"])
        .status();
    let _ = crate::netlock::iptables()
        .args(["-D", "FORWARD", "-o", &bridge, "-j", "ACCEPT"])
        .status();

    // Bring down and remove bridge
    let _ = std::process::Command::new("ip")
        .args(["link", "set", &bridge, "down"])
        .status();
    let _ = std::process::Command::new("ip")
        .args(["link", "delete", &bridge])
        .status();

    eprintln!("[NKR-CELL] Bridge {} eliminado", bridge);
    Ok(())
}

// =============================================================================
// ASCII table for `nkr cell ls`
// =============================================================================

pub fn print_cell_table() {
    let cells = list_cells();

    if cells.is_empty() {
        eprintln!("[NKR] No hay células registradas. Crear con: nkr cell create <nombre>");
        return;
    }

    let active_vms = crate::state::list_vms();

    let headers = [
        ("CELL ID", 7usize),
        ("NOMBRE", 22),
        ("SUBNET", 18),
        ("ODOO", 9),
        ("NKRs VIVAS", 10),
    ];
    crate::state::print_header_row(&headers);

    for cell in &cells {
        let vm_count = active_vms.iter()
            .filter(|vm| vm.cell_id == cell.cell_id)
            .count();
        let version = cell.odoo_version.as_deref().unwrap_or("—").to_string();
        let subnet = format!("10.0.{}.0/24", cell.cell_id);

        let row = [
            cell.cell_id.to_string(),
            truncate(cell.name.as_str(), 22),
            subnet,
            version,
            vm_count.to_string(),
        ];
        crate::state::print_data_row(&headers, &row);
    }

    crate::state::print_footer_separator(&headers);
    eprintln!("[NKR] {} célula(s) registrada(s)", cells.len());
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max { s.to_string() } else { format!("{}…", &s[..max - 1]) }
}

// =============================================================================
// Clone — Duplicates an Odoo instance within the same cell
// =============================================================================

/// Locates which cell an instance lives in given its nkr_name.
/// Searches `cells/*/instances/<nkr_name>/` and returns (CellConfig, path).
fn find_instance_cell(nkr_name: &str) -> Result<(CellConfig, PathBuf), Box<dyn std::error::Error>> {
    let base = cells_dir();
    if !base.exists() {
        return Err(format!("No existe {} — no hay cells", base.display()).into());
    }
    for entry in fs::read_dir(&base)? {
        let entry = entry?;
        let cell_yml = entry.path().join("cell.yml");
        let inst_dir = entry.path().join("instances").join(nkr_name);
        if cell_yml.exists() && inst_dir.exists() && inst_dir.is_dir() {
            let content = fs::read_to_string(&cell_yml)?;
            let config: CellConfig = serde_yaml::from_str(&content)?;
            return Ok((config, inst_dir));
        }
    }
    Err(format!("Instancia '{}' no encontrada bajo ninguna cell en {}", nkr_name, base.display()).into())
}

/// Rewrites the destination's odoo.conf substituting strings from the source.
/// Handles `db_name`, `dbfilter` and any path containing the nkr_name.
fn rewrite_odoo_conf(dst_dir: &Path, src_nkr: &str, dst_nkr: &str) -> Result<(), Box<dyn std::error::Error>> {
    let conf_path = dst_dir.join("config").join("odoo.conf");
    if !conf_path.exists() {
        eprintln!("[NKR-CLONE] WARN: {} no existe, omitido", conf_path.display());
        return Ok(());
    }
    let content = fs::read_to_string(&conf_path)?;
    let new_content = content.replace(src_nkr, dst_nkr);
    fs::write(&conf_path, new_content)?;
    eprintln!("[NKR-CLONE] odoo.conf: {} → {}", src_nkr, dst_nkr);
    Ok(())
}

/// Clones the DB via `CREATE DATABASE ... TEMPLATE`. Requires `psql` on the host.
/// Strategy: ALTER DATABASE ... ALLOW_CONNECTIONS=false → pg_terminate_backend
/// → CREATE DATABASE WITH TEMPLATE → ALTER ... ALLOW_CONNECTIONS=true.
/// During the window (~seconds) src clients disconnect and reconnect via
/// pgbouncer when it reopens.
fn clone_database(cell_id: u8, src_nkr: &str, dst_nkr: &str) -> Result<(), Box<dyn std::error::Error>> {
    // By cell convention: pg has vm_id=1 → IP .2
    let db_ip = format!("10.0.{}.2", cell_id);
    let src_db = format!("db-{}", src_nkr);
    let dst_db = format!("db-{}", dst_nkr);

    // Connectivity sanity check
    let check = std::process::Command::new("pg_isready")
        .args(["-h", &db_ip, "-p", "5432", "-U", "odoo", "-t", "3"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !check.map(|s| s.success()).unwrap_or(false) {
        return Err(format!(
            "PostgreSQL en {}:5432 no responde. Asegúrate de que la cell esté arriba \
             (nkr cell up <cell>) o usa --no-db para saltar la clonación de DB.",
            db_ip
        ).into());
    }

    // Each statement is sent via stdin so psql executes it in its own
    // transaction block (auto-commit in non-interactive mode). `CREATE DATABASE`
    // CANNOT run inside a BEGIN...COMMIT, so -c with multiple statements would
    // fail ("CREATE DATABASE cannot run inside a transaction block").
    let sql = format!(
        "ALTER DATABASE \"{src}\" WITH ALLOW_CONNECTIONS false;\n\
         SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
           WHERE datname = '{src}' AND pid <> pg_backend_pid();\n\
         CREATE DATABASE \"{dst}\" WITH TEMPLATE \"{src}\" OWNER odoo;\n\
         ALTER DATABASE \"{src}\" WITH ALLOW_CONNECTIONS true;\n",
        src = src_db, dst = dst_db
    );

    eprintln!("[NKR-CLONE] Clonando DB '{}' → '{}' en {}:5432...", src_db, dst_db, db_ip);
    let out = run_psql_stdin(&db_ip, &sql);

    if out.as_ref().map(|o| !o.status.success()).unwrap_or(true) {
        // Reopen connections to src if it was left capped (best-effort, cannot
        // be in a transaction with DROP/CREATE so it goes via separate stdin).
        let _ = run_psql_stdin(&db_ip,
            &format!("ALTER DATABASE \"{}\" WITH ALLOW_CONNECTIONS true;\n", src_db));
        let msg = out.as_ref()
            .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
            .unwrap_or_else(|e| e.to_string());
        return Err(format!("psql falló: {}", msg).into());
    }

    eprintln!("[NKR-CLONE] DB '{}' creada desde TEMPLATE '{}'", dst_db, src_db);
    Ok(())
}

/// Executes `psql` feeding SQL via stdin — each statement runs in its own
/// transaction block (auto-commit). Required for CREATE/DROP DATABASE which
/// cannot run inside an implicit BEGIN...COMMIT of `-c`.
fn run_psql_stdin(db_ip: &str, sql: &str) -> Result<std::process::Output, std::io::Error> {
    use std::io::Write;
    let mut child = std::process::Command::new("psql")
        .env("PGPASSWORD", "odoo")
        .args([
            "-h", db_ip, "-p", "5432", "-U", "odoo", "-d", "postgres",
            "-v", "ON_ERROR_STOP=1", "-X", "-q", "-f", "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    if let Some(stdin) = child.stdin.as_mut() {
        stdin.write_all(sql.as_bytes())?;
    }
    child.wait_with_output()
}

/// Appends a `services.<name>:` block to nkr-compose.yml cloning the src block.
/// Works with text (does not re-emit YAML) to preserve comments and formatting.
/// If `extra_env` has entries, they are injected into the `environment:` section
/// of the new block — or the section is created if the src doesn't have one.
fn append_compose_block(
    cell: &CellConfig,
    src_nkr: &str,
    dst_nkr: &str,
    dst_vm_id: u8,
    extra_env: &[(String, String)],
    ram_mb_override: Option<u32>,
    chrs_override: Option<u32>,
    balloon_mb_override: Option<u32>,
    include_enterprise: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let compose_path = cell_compose_path(&cell.name);
    if !compose_path.exists() {
        return Err(format!("No existe {} — omite --no-compose para editar manual", compose_path.display()).into());
    }

    let content = fs::read_to_string(&compose_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // 1. Locate the src block: we look for the service whose `nkr_name: "<src>"`
    //    matches exactly. The block starts at the closest previous service header
    //    (2 spaces + key + ':').
    let mut src_block_start: Option<usize> = None;
    let mut src_block_end: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("nkr_name:") {
            let rhs = trimmed.trim_start_matches("nkr_name:").trim().trim_matches('"').trim_matches('\'');
            if rhs == src_nkr {
                // Rewind to the header (2 spaces, no extra whitespace)
                for j in (0..=i).rev() {
                    let l = lines[j];
                    if l.len() >= 3
                        && l.starts_with("  ")
                        && !l.starts_with("   ")
                        && l.trim_end().ends_with(':')
                        && !l.trim_start().starts_with('-')
                    {
                        src_block_start = Some(j);
                        break;
                    }
                }
                break;
            }
        }
    }
    let block_start = src_block_start
        .ok_or_else(|| format!("No encontré el bloque 'services.*' con nkr_name: \"{}\"", src_nkr))?;

    // 2. End of block: next service header at same indent, or EOF / new section.
    for i in (block_start + 1)..lines.len() {
        let l = lines[i];
        let is_service_header = l.len() >= 3
            && l.starts_with("  ")
            && !l.starts_with("   ")
            && l.trim_end().ends_with(':')
            && !l.trim_start().starts_with('-')
            && !l.trim_start().starts_with('#');
        let is_top_level = !l.is_empty() && !l.starts_with(' ') && !l.starts_with('#');
        if is_service_header || is_top_level {
            src_block_end = Some(i);
            break;
        }
    }
    let block_end = src_block_end.unwrap_or(lines.len());

    // 3. Clone the block, substituting:
    //    - Header (first line): `  odoo-XX:` → `  <short_dst>:`
    //    - `id: N` → `id: <dst_vm_id>`
    //    - Any occurrence of src_nkr → dst_nkr
    let short_dst = dst_nkr
        .strip_prefix(&format!("{}-", cell.name))
        .unwrap_or(dst_nkr)
        .to_string();

    let mut new_block: Vec<String> = Vec::with_capacity(block_end - block_start + 2);
    new_block.push(String::new()); // blank separator line
    let mut env_section_injected = false;
    let mut has_env_section = false;
    let mut has_skip_warmup = false;
    let mut has_ram = false;
    let mut has_chrs = false;
    // v1.5.x guarantee: every Odoo instance born from a clone must have the
    // balloon active. Balloon is essential for density — without it a cell
    // with 20 tenants will exhaust host RAM. If the src_block doesn't carry
    // it (legacy template, manual edit, etc.), we inject 128 MB by default.
    let mut has_balloon = false;
    // Si el template tiene `addons:/mnt/extra-addons` como volume legacy,
    // capturamos la línea convertida a share aquí para inyectarla bajo `shares:`.
    let mut legacy_addons_share: Option<String> = None;
    for (idx, line) in lines[block_start..block_end].iter().enumerate() {
        let mut s = line.to_string();
        if idx == 0 {
            // Header: `  odoo-01:` → `  <short_dst>:`
            // (translated: first line of cloned block is rewritten as new service header)
            s = format!("  {}:", short_dst);
        } else {
            let t = s.trim_start();
            if let Some(rest) = t.strip_prefix("id:") {
                let indent = &s[..s.len() - t.len()];
                s = format!("{}id: {}{}", indent, dst_vm_id, rest_trailing_comment(rest));
            } else {
                s = s.replace(src_nkr, dst_nkr);
            }
        }
        let trimmed = s.trim_start();
        if trimmed.starts_with("ram:") {
            has_ram = true;
            if let Some(new_ram) = ram_mb_override {
                let orig = &lines[block_start + idx];
                let indent_len = orig.len() - orig.trim_start().len();
                let indent_str = " ".repeat(indent_len);
                let rest_after_key = orig.trim_start()
                    .strip_prefix("ram:").unwrap_or("");
                let comment = rest_trailing_comment(rest_after_key);
                new_block.push(format!("{}ram: {}{}", indent_str, new_ram, comment));
                continue;
            }
        }
        if trimmed.starts_with("balloon_mb:") {
            has_balloon = true;
            if let Some(new_b) = balloon_mb_override {
                let orig = &lines[block_start + idx];
                let indent_len = orig.len() - orig.trim_start().len();
                let indent_str = " ".repeat(indent_len);
                let rest_after_key = orig.trim_start()
                    .strip_prefix("balloon_mb:").unwrap_or("");
                let comment = rest_trailing_comment(rest_after_key);
                new_block.push(format!("{}balloon_mb: {}{}", indent_str, new_b, comment));
                continue;
            }
        }
        if trimmed.starts_with("chrs:") {
            has_chrs = true;
            if let Some(new_chrs) = chrs_override {
                let orig = &lines[block_start + idx];
                let indent_len = orig.len() - orig.trim_start().len();
                let indent_str = " ".repeat(indent_len);
                let rest_after_key = orig.trim_start()
                    .strip_prefix("chrs:").unwrap_or("");
                let comment = rest_trailing_comment(rest_after_key);
                new_block.push(format!("{}chrs: {}{}", indent_str, new_chrs, comment));
                continue;
            }
        }
        if trimmed.starts_with("skip_warmup:") {
            has_skip_warmup = true;
            // Force true in clones (overrides whatever was there)
            let indent_len = lines[block_start + idx].len() - lines[block_start + idx].trim_start().len();
            let indent_str = " ".repeat(indent_len);
            new_block.push(format!("{}skip_warmup: true", indent_str));
            continue;
        }
        if trimmed.starts_with("disabled:") {
            // El template tiene `disabled: true` por design (no debe arrancar
            // nunca solo). Pero los clones SÍ deben arrancar — heredar el flag
            // dejaría al nuevo tenant apagado para siempre. Forzar `false`.
            let indent_len = lines[block_start + idx].len() - lines[block_start + idx].trim_start().len();
            let indent_str = " ".repeat(indent_len);
            new_block.push(format!("{}disabled: false", indent_str));
            continue;
        }
        if trimmed.starts_with("environment:") {
            has_env_section = true;
            new_block.push(s);
            let indent_len = lines[block_start + idx].len() - lines[block_start + idx].trim_start().len();
            let inner_indent = " ".repeat(indent_len + 2);
            for (k, v) in extra_env {
                new_block.push(format!("{}{}: \"{}\"", inner_indent, k, v));
            }
            env_section_injected = true;
            continue;
        }
        // Migración legacy: si el template todavía tiene addons como `volume`
        // (formato pre-v1.6), `inject_volumes` no lo procesa cuando el rootfs
        // se promociona a master compartido (config.disks queda vacío) y el
        // dir nunca llega al guest. Lo movemos a `shares` automáticamente.
        //
        // ATENCIÓN: el `odoo.conf:/etc/odoo/odoo.conf` SÍ debe quedarse en
        // `volumes:` porque compose.rs lo detecta como archivo (no dir) y
        // lo stagea en `<tenant>-overrides/etc/odoo/odoo.conf` para
        // bind-mount via /tmp/nkr-overrides en el initramfs. Sin esa línea,
        // los overrides no se generan y Odoo arranca con la conf base
        // (db_host=localhost) → connection refused.
        if trimmed.starts_with("- ") && trimmed.contains(":/mnt/extra-addons")
            && !trimmed.contains(":ro") && !trimmed.contains(":rw")
        {
            let new_share = format!("{}:rw", s.trim_end());
            legacy_addons_share = Some(new_share);
            continue;
        }
        // Enterprise opt-in: si el clone es community, descartar la share que
        // monta /mnt/nkr/enterprise/<v>:/mnt/extra-enterprise:ro. Sin esa
        // share el dir no existe en el guest y addons_path lo skipea, dejando
        // al tenant 100% community sin warnings.
        if !include_enterprise && trimmed.contains(":/mnt/extra-enterprise") {
            continue;
        }
        new_block.push(s);
    }
    // The src didn't have skip_warmup → add it at the top of the block (after header).
    if !has_skip_warmup && !new_block.is_empty() {
        // new_block[0] = "" (separator), new_block[1] = "  <short_dst>:"
        // Insert at position 2 (first line of the service body).
        let insert_at = 2.min(new_block.len());
        new_block.insert(insert_at, "    skip_warmup: true".to_string());
    }
    // The src didn't have `ram:` but we want to set one → inject after header.
    if !has_ram && ram_mb_override.is_some() && !new_block.is_empty() {
        let insert_at = 2.min(new_block.len());
        new_block.insert(insert_at, format!("    ram: {}", ram_mb_override.unwrap()));
    }
    if !has_chrs && chrs_override.is_some() && !new_block.is_empty() {
        let insert_at = 2.min(new_block.len());
        new_block.insert(insert_at, format!("    chrs: {}", chrs_override.unwrap()));
    }
    // Ensure balloon is set on every cloned instance. If the template didn't
    // carry it, inject the panel override (when provided) or a sane 128 MB
    // default.
    if !has_balloon && !new_block.is_empty() {
        let value = balloon_mb_override.unwrap_or(128);
        let insert_at = 2.min(new_block.len());
        new_block.insert(insert_at, format!("    balloon_mb: {}", value));
        eprintln!("[NKR-CLONE] balloon_mb missing on '{}' — injected={} MB", src_nkr, value);
    }
    // If the src didn't have `environment:` and the caller passed extras, add them at the end.
    if !has_env_section && !extra_env.is_empty() {
        new_block.push("    environment:".to_string());
        for (k, v) in extra_env {
            new_block.push(format!("      {}: \"{}\"", k, v));
        }
        env_section_injected = true;
    }
    let _ = env_section_injected; // defense: silence warning if flow changes above

    // Si capturamos un addons-as-volume legacy → inyectar como share.
    if let Some(line) = legacy_addons_share {
        // Buscar `shares:` y agregar la línea justo después.
        let mut injected = false;
        let mut new_block_with_share = Vec::with_capacity(new_block.len() + 1);
        for nl in &new_block {
            new_block_with_share.push(nl.clone());
            if !injected && nl.trim() == "shares:" {
                new_block_with_share.push(line.clone());
                injected = true;
            }
        }
        if !injected {
            // No había `shares:` → crear uno al final.
            new_block_with_share.push("    shares:".to_string());
            new_block_with_share.push(line);
        }
        new_block = new_block_with_share;
    }

    // 4. Rewrite the file: original content + new block at the end
    let mut out = String::with_capacity(content.len() + 1024);
    out.push_str(&content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&new_block.join("\n"));
    out.push('\n');

    // Backup before writing (rotated: keeps only the last N backups).
    let bak = backup_compose_with_rotation(&compose_path);

    fs::write(&compose_path, out)?;
    eprintln!("[NKR-CLONE] Compose actualizado: bloque '{}' añadido ({}). Backup: {}",
        short_dst, compose_path.display(), bak.display());
    Ok(())
}

/// Preserves any comment/whitespace after the `id:` value.
fn rest_trailing_comment(rest_after_id: &str) -> String {
    // `rest_after_id` includes: " 3  # foo" or " 3"
    // We want to extract the possible `  # foo` (indent + comment) if present.
    let trimmed = rest_after_id.trim_start();
    if let Some(hash_pos) = trimmed.find('#') {
        // Reinsert two spaces + comment
        return format!("  {}", &trimmed[hash_pos..]);
    }
    String::new()
}

/// Legacy entry point (CLI): clones an Odoo instance with default options.
/// For use from the HTTP API, call `clone_instance_with_opts`.
pub fn clone_instance(
    src_nkr: &str,
    dst_nkr: &str,
    no_db: bool,
    no_compose: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let opts = CloneOptions {
        // Mode mapping del CLI legacy: --no-db es advanced; siempre marcamos
        // como Production semánticamente, pero el flag real que decide el clone
        // es skip_db_clone (la API HTTP nunca lo setea).
        mode: InstanceMode::Production,
        no_compose,
        skip_db_clone: no_db,
        ..CloneOptions::default()
    };
    clone_instance_with_opts(src_nkr, dst_nkr, &opts).map(|_| ())
}

// ============================================================================
// CloneScratch — automatic rollback on crash or partial error
// ============================================================================
//
// `clone_instance_with_opts` runs 8+ steps, each with persistent side effects:
//   1. resolve_id_scoped         → entry in registry.json
//   2. cp -a / btrfs snapshot    → full destination dir
//   3. preserve_nocow + addons/  → destination ext4 files
//   4. clone_nkr_data_files      → files under .nkr-data/<short>-*
//   5. rewrite_odoo_conf*        → writes inside dst_dir
//   6. clone_database            → entry in pgbouncer (via psql)
//   7. append_compose_block      → block in nkr-compose.yml
//   8. save_instance_meta        → meta.json
//
// Without RAII: if step 6 fails, dst_dir + nkr-data + registry are left
// littered with garbage. The next POST with the same nkr_name aborts because
// the dir already exists, but the operator has no tool to clean it up (there
// is no `nkr instance gc`).
//
// With CloneScratch: each step records its artifact in the scratch. If the
// function returns Err (via ?), Drop runs the rollback in reverse order.
// `commit()` at the end disables it. Drop ALWAYS runs — including panics
// under `panic = unwind`, but with the current `panic = abort` profile a
// panic skips Drop entirely.
struct CloneScratch {
    /// Clone destination dir (e.g. /mnt/nkr/cells/<cell>/instances/<dst_nkr>).
    dst_dir: Option<PathBuf>,
    /// Files cloned under .nkr-data/ (filestore, per-instance .ext4).
    nkr_data_files: Vec<PathBuf>,
    /// Entry created in registry.json — key "cell/name" lowercased.
    registry_key: Option<String>,
    /// DB cloned through pgbouncer — (cell_id, nkr_name).
    cloned_db: Option<(u8, String)>,
    /// If commit was called, Drop does NOT roll back.
    committed: bool,
}

impl CloneScratch {
    fn new() -> Self {
        CloneScratch {
            dst_dir: None,
            nkr_data_files: Vec::new(),
            registry_key: None,
            cloned_db: None,
            committed: false,
        }
    }

    /// Marks the clone as successful. After this, Drop is a no-op.
    fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for CloneScratch {
    fn drop(&mut self) {
        if self.committed { return; }
        eprintln!("[NKR-CLONE] ROLLBACK: cleaning up partial artifacts from failed clone...");

        // Reverse order vs. setup: DB first (may need network/PG), then
        // local files, registry last.

        // 1. drop DB if it was cloned (best-effort).
        if let Some((cell_id, name)) = self.cloned_db.take() {
            match drop_database(cell_id, &name) {
                Ok(_) => eprintln!("[NKR-CLONE] ROLLBACK: DB '{}' dropped", name),
                Err(e) => eprintln!("[NKR-CLONE] ROLLBACK: drop_database '{}' failed: {} (DB orphaned)", name, e),
            }
        }

        // 2. whole dst_dir.
        if let Some(dir) = self.dst_dir.take() {
            if dir.exists() {
                match fs::remove_dir_all(&dir) {
                    Ok(_) => eprintln!("[NKR-CLONE] ROLLBACK: dir {} removed", dir.display()),
                    Err(e) => eprintln!("[NKR-CLONE] ROLLBACK: remove_dir_all({}) failed: {}", dir.display(), e),
                }
            }
        }

        // 3. files under .nkr-data/.
        for f in self.nkr_data_files.drain(..) {
            if f.exists() {
                let _ = fs::remove_file(&f);
            }
        }

        // 4. registry entry.
        if let Some(key) = self.registry_key.take() {
            match crate::registry::remove(&key) {
                Ok(true) => eprintln!("[NKR-CLONE] ROLLBACK: registry entry '{}' released", key),
                _ => {}
            }
        }
    }
}

/// Clones + configures an instance from the API. Returns destination metadata.
pub fn clone_instance_with_opts(
    src_nkr: &str,
    dst_nkr: &str,
    opts: &CloneOptions,
) -> Result<InstanceInfo, Box<dyn std::error::Error>> {
    if src_nkr == dst_nkr {
        return Err("src y dst son idénticos".into());
    }

    let (cell, src_dir) = find_instance_cell(src_nkr)?;
    let dst_dir = cells_dir().join(&cell.name).join("instances").join(dst_nkr);
    if dst_dir.exists() {
        return Err(format!("La instancia destino '{}' ya existe: {}", dst_nkr, dst_dir.display()).into());
    }

    // Version validation: each cell supports a single odoo_version. If the panel
    // sends a version, it must match the source cell's version.
    if let Some(ref req_v) = opts.odoo_version {
        match cell.odoo_version.as_deref() {
            Some(cell_v) if cell_v == req_v => {}
            Some(cell_v) => return Err(format!(
                "Versión incompatible: source '{}' vive en cell '{}' (odoo_version={}), \
                 panel pidió odoo_version={}. Cada cell soporta una sola versión.",
                src_nkr, cell.name, cell_v, req_v
            ).into()),
            None => return Err(format!(
                "Cell '{}' no tiene odoo_version registrada (cell.yml) pero panel \
                 pidió odoo_version={}. Setear cell.odoo_version antes de clonar.",
                cell.name, req_v
            ).into()),
        }
    }

    // Capacity validation: max 20 Odoos per cell.
    let used = count_odoo_instances(&cell.name);
    if used >= MAX_ODOOS_PER_CELL {
        return Err(format!(
            "Cell '{}' llena: {}/{} Odoos. Crear nueva cell o borrar instancia.",
            cell.name, used, MAX_ODOOS_PER_CELL
        ).into());
    }

    // Check if the src VM is active. If it is, warn — hot TEMPLATE requires
    // disconnecting sessions. It's allowed, but there's a ~seconds window.
    let active_src = crate::state::list_vms().iter().any(|v| v.name == src_nkr);
    if active_src && !opts.skip_db_clone {
        eprintln!("[NKR-CLONE] WARN: '{}' está activo. La clonación cerrará sus sesiones \
                   PG por ~segundos para ejecutar CREATE DATABASE ... TEMPLATE.", src_nkr);
    }

    // python_libs not yet supported: requires master ext4 build pipeline
    // (nkr build with Nkrfile that does pip install before exporting). We report
    // it as an explicit error so the panel knows to call /build first.
    if !opts.python_libs.is_empty() {
        return Err(format!(
            "python_libs={:?} requiere rebuild del master ext4 vía 'nkr build'. \
             Endpoint /build pendiente — por ahora usá el master existente.",
            opts.python_libs
        ).into());
    }

    // Initialize the scratch — from here until `scratch.commit()` at the end,
    // any `?` that fires triggers an automatic rollback.
    let mut scratch = CloneScratch::new();

    // Register new vm_id (scope = cell)
    let dst_vm_id = crate::registry::resolve_id_scoped(Some(&cell.name), dst_nkr)?;
    let dst_ip = crate::registry::id_to_ip(cell.cell_id, dst_vm_id);
    // Only register the entry as rollback-owned because we know it's NEW —
    // resolve_id_scoped is idempotent, but dst_dir.exists() was rejected
    // above, so the registry entry created right now ALWAYS belongs to this
    // flow.
    scratch.registry_key = Some(format!("{}/{}",
        cell.name.to_lowercase(), dst_nkr.to_lowercase()));

    // Copy strategy ordered by preference:
    //  1. If src is a btrfs subvolume → `btrfs subvolume snapshot` (real O(1)).
    //  2. Otherwise, `cp -a --reflink=auto` (reflink on btrfs/xfs, physical copy
    //     on ext4 or as fallback).
    //
    // The .ext4 files inside the dir (filestore, pg/data.ext4) have `chattr +C`
    // applied at creation via fsutil::create_ext4_disk/compose, so the snapshot
    // does not degrade their CoW-free behavior.
    eprintln!("[NKR-CLONE] Copiando {} → {} ...", src_dir.display(), dst_dir.display());

    let snapshot_ok = crate::fsutil::try_btrfs_snapshot(&src_dir, &dst_dir)
        .unwrap_or(false);

    if !snapshot_ok {
        let status = std::process::Command::new("cp")
            .args([
                "-a", "--reflink=auto",
                &src_dir.to_string_lossy(),
                &dst_dir.to_string_lossy(),
            ])
            .status()?;
        if !status.success() {
            return Err(format!("cp -a falló al copiar {} → {}", src_dir.display(), dst_dir.display()).into());
        }
    }
    // The destination dir exists on disk — register for rollback.
    scratch.dst_dir = Some(dst_dir.clone());

    // Clean up clone logs (no sense inheriting src logs)
    let dst_logs = dst_dir.join("logs");
    if dst_logs.exists() {
        for e in fs::read_dir(&dst_logs)?.flatten() {
            let _ = fs::remove_file(e.path());
        }
    }

    // Ensure addons/ and pylibs/lib/ exist — the panel writes into them via the
    // HTTPS API and virtiofs shares will fail to mount if the host path is missing.
    let _ = fs::create_dir_all(dst_dir.join("addons"));
    let _ = fs::create_dir_all(dst_dir.join("pylibs").join("lib"));

    // Re-apply +C to the cloned .ext4 files. btrfs subvolume snapshot and cp --reflink
    // do NOT inherit the NODATACOW flag → without this the new odoo.ext4 fragments in CoW.
    for ext4 in walk_ext4_files(&dst_dir)? {
        if let Err(e) = crate::fsutil::preserve_nocow(&ext4) {
            eprintln!("[NKR-CLONE] WARN: preserve_nocow falló en {}: {}", ext4.display(), e);
        }
    }

    // Clone `.nkr-data/` files (filestore + pg-per-instance volumes).
    // Without this, the clone starts with empty filestore and Odoo throws
    // FileNotFoundError when looking up ir.attachment referenced in the DB
    // cloned via TEMPLATE.
    if let Err(e) = clone_nkr_data_files(&cell, src_nkr, dst_nkr) {
        eprintln!("[NKR-CLONE] WARN: clone_nkr_data_files: {} — filestore puede quedar vacío", e);
    }
    // Register for rollback the files created by clone_nkr_data_files.
    // The naming convention is .nkr-data/<short_dst>-* (regardless of
    // whether it's an .ext4 or any other type).
    {
        let nkr_data_dir = cells_dir().join(&cell.name).join(".nkr-data");
        let cell_prefix = format!("{}-", cell.name);
        let short_dst = dst_nkr.strip_prefix(&cell_prefix).unwrap_or(dst_nkr);
        let match_prefix = format!("{}-", short_dst);
        if let Ok(it) = fs::read_dir(&nkr_data_dir) {
            for entry in it.flatten() {
                let fname = entry.file_name().to_string_lossy().to_string();
                if fname.starts_with(&match_prefix) {
                    if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                        scratch.nkr_data_files.push(entry.path());
                    }
                }
            }
        }
    }

    rewrite_odoo_conf(&dst_dir, src_nkr, dst_nkr)?;
    rewrite_odoo_conf_full(&dst_dir, dst_nkr, opts)?;

    if !opts.skip_db_clone {
        clone_database(cell.cell_id, src_nkr, dst_nkr)?;
        // DB created — register for rollback (best-effort: if pg is down
        // at rollback time the DROP DATABASE fails and the DB is orphaned,
        // but we log it).
        scratch.cloned_db = Some((cell.cell_id, dst_nkr.to_string()));
    } else {
        eprintln!("[NKR-CLONE] skip_db_clone=true: DB NO clonada (uso avanzado, panel debe restaurar desde backup).");
    }

    if !opts.no_compose {
        // Inject env vars so the guest renames filestore/db-<src> → filestore/db-<dst>
        // on first boot (see `src/initramfs.rs` nkr-start.sh). This replaces the hack
        // of mount -o loop + mv we had on the host.
        let extra_env: Vec<(String, String)> = vec![
            ("NKR_RENAME_FILESTORE_FROM".to_string(), format!("db-{}", src_nkr)),
            ("NKR_RENAME_FILESTORE_TO".to_string(),   format!("db-{}", dst_nkr)),
        ];
        // Enterprise opt-in per-instance: el panel decide al crear. Default
        // = community (sin extra-enterprise share ni en addons_path).
        let include_enterprise = matches!(opts.edition, Some(Edition::Enterprise));
        append_compose_block(&cell, src_nkr, dst_nkr, dst_vm_id, &extra_env,
            opts.ram_mb, opts.chrs, opts.balloon_mb, include_enterprise)?;
    } else {
        eprintln!("[NKR-CLONE] no_compose=true: añade el bloque al nkr-compose.yml manualmente.");
    }

    // Persist metadata for the API (/instances/{name} GET)
    let meta = InstanceMeta {
        nkr_name: dst_nkr.to_string(),
        cell: cell.name.clone(),
        source: Some(src_nkr.to_string()),
        mode: opts.mode,
        dns: opts.dns.clone(),
        edition: opts.edition,
        odoo_version: opts.odoo_version.clone().or_else(|| cell.odoo_version.clone()),
        pg_version: opts.pg_version.clone(),
        workers: opts.workers,
        list_db: opts.list_db,
        limit_memory_soft: opts.limit_memory_soft,
        limit_memory_hard: opts.limit_memory_hard,
        addons_path: opts.addons_path.clone(),
        created_at: now_unix_secs(),
    };
    save_instance_meta(&dst_dir, &meta)?;

    eprintln!("[NKR-CLONE] ✅ '{}' clonado → '{}' (vm_id={}, IP={})",
        src_nkr, dst_nkr, dst_vm_id, dst_ip);
    if !opts.no_compose {
        eprintln!("[NKR-CLONE]   Arrancar: cd {} && sudo nkr compose up -d",
            cells_dir().join(&cell.name).display());
    }

    // Clone successful → disable rollback. Without this, Drop on scope exit
    // would erase everything we just created.
    scratch.commit();
    Ok(build_instance_info(&cell, dst_nkr, dst_vm_id, &meta))
}

// =============================================================================
// Types exposed to the HTTP API
// =============================================================================

/// Instance mode. Affects whether the DB is cloned.
#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum InstanceMode {
    /// Full clone: copies files + clones DB (TEMPLATE). Development client.
    Dev,
    /// Copies files but empty DB — the panel hydrates it separately. Production.
    Production,
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Edition {
    Community,
    Enterprise,
}

/// Clone options sent by the panel.
#[derive(Clone, Debug, Default)]
pub struct CloneOptions {
    pub mode: InstanceMode,
    pub no_compose: bool,
    pub dns: Option<String>,
    pub edition: Option<Edition>,
    pub odoo_version: Option<String>,
    pub pg_version: Option<String>,
    pub workers: Option<u32>,
    pub list_db: Option<bool>,
    pub limit_memory_soft: Option<u64>, // bytes → odoo.conf
    pub limit_memory_hard: Option<u64>, // bytes → odoo.conf
    /// If passed, overrides `addons_path` in odoo.conf. If null, the src's is preserved.
    pub addons_path: Option<String>,
    /// Extra Python libraries — require master rebuild (not supported today).
    pub python_libs: Vec<String>,
    /// Master password del Odoo del tenant. Si None, se genera uno aleatorio.
    /// Se persiste ÚNICAMENTE en odoo.conf (`admin_passwd = ...`) dentro del
    /// instance_dir. Se devuelve al panel en la respuesta del clone y nunca más.
    pub admin_passwd: Option<String>,
    /// Opt-out del proxy_mode (que es True por default en cells productivas).
    pub proxy_mode: Option<bool>,
    /// RAM asignada a la VM guest, en MB. Si None, se hereda del compose del
    /// template. Independiente de `limit_memory_hard` (que es el tope interno
    /// de Odoo). Recomendación: `ram_mb >= limit_memory_hard/1MB + 256` para
    /// tener headroom de kernel + cgroup (memory.max = ram_mb × 1.15).
    pub ram_mb: Option<u32>,
    /// CPU quota para la cgroup de la VM, en unidades de 20% de un core
    /// (1 chr = 20%). Si None, se hereda del compose del template.
    pub chrs: Option<u32>,
    /// Override for the template's `balloon_mb`. If None, the value is
    /// inherited from the template. If the template didn't carry balloon,
    /// `append_compose_block` injects the 128 MB default.
    pub balloon_mb: Option<u32>,
    /// Skip del paso de clonar la DB. Sólo para uso avanzado (CLI / restore
    /// desde backup externo). El API HTTP nunca lo setea — siempre clona DB
    /// (mode=production usa el template de la cell, mode=dev usa el source
    /// explícito del panel).
    pub skip_db_clone: bool,
}

impl Default for InstanceMode {
    fn default() -> Self { InstanceMode::Production }
}

/// Per-instance persisted metadata (`meta.json` next to the instance dir).
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct InstanceMeta {
    pub nkr_name: String,
    pub cell: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    pub mode: InstanceMode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub edition: Option<Edition>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odoo_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pg_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workers: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub list_db: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_memory_soft: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_memory_hard: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub addons_path: Option<String>,
    pub created_at: u64,
}

/// Response payload the panel consumes.
#[derive(Serialize, Clone, Debug)]
pub struct InstanceInfo {
    pub nkr_name: String,
    pub cell: String,
    pub vm_id: u8,
    pub guest_ip: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dns: Option<String>,
    pub db_name: String,
    pub addons_path: String,
    pub logs_path: String,
    pub config_path: String,
    pub instance_dir: String,
    pub meta: InstanceMeta,
    pub nkr_status: NkrStatus,
}

/// Runtime state: alive/not-alive, PID, HTTP port reachable, + fase lógica
/// para que el panel muestre progreso ("VM booteando → Odoo cargando → listo").
#[derive(Serialize, Clone, Debug)]
pub struct NkrStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ram_mb: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_s: Option<u64>,
    pub port_8069_up: bool,
    /// Máquina de estados derivada de los flags de arriba + detección de DB.
    /// Valores: "provisioning" (clone hecho, VM no arrancada),
    ///          "booting" (VM running, :8069 aún no),
    ///          "odoo_loading" (:8069 up, DB del tenant aún no existe),
    ///          "ready" (todo OK, DB presente, panel puede usar),
    ///          "error" (running=false pero meta.json dice que debería estar vivo).
    pub phase: String,
    /// True si hay una DB con el db_name del tenant en PG.
    pub db_present: bool,
    /// Versión de Odoo que corre (leída del meta.json → cell.yml).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub odoo_version_running: Option<String>,
    /// Última línea de error en nkr-compose.log (si la última fase es `error`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Estado del job de init-db (si fue invocado). Valores:
    /// "running" | "success" | "failed". None si nunca se llamó `POST /init-db`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub init_db: Option<serde_json::Value>,
}

fn now_unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn instance_meta_path(instance_dir: &Path) -> PathBuf {
    instance_dir.join("meta.json")
}

pub fn save_instance_meta(instance_dir: &Path, meta: &InstanceMeta) -> Result<(), Box<dyn std::error::Error>> {
    let path = instance_meta_path(instance_dir);
    let json = serde_json::to_string_pretty(meta)?;
    fs::write(&path, json)?;
    Ok(())
}

pub fn load_instance_meta(instance_dir: &Path) -> Option<InstanceMeta> {
    let path = instance_meta_path(instance_dir);
    let content = fs::read_to_string(&path).ok()?;
    serde_json::from_str(&content).ok()
}

fn build_instance_info(cell: &CellConfig, nkr_name: &str, vm_id: u8, meta: &InstanceMeta) -> InstanceInfo {
    let instance_dir = cells_dir().join(&cell.name).join("instances").join(nkr_name);
    let guest_ip = crate::registry::id_to_ip(cell.cell_id, vm_id);
    let running_vm = crate::state::list_vms().into_iter().find(|v| v.name == nkr_name);
    let port_up = tcp_probe(&guest_ip, 8069, std::time::Duration::from_millis(300));
    let (pid, ram_mb, uptime_s) = match &running_vm {
        Some(v) => (
            Some(v.pid),
            read_rss_mb(v.pid),
            started_at_secs(v.pid),
        ),
        None => (None, None, None),
    };
    let db_name = format!("db-{}", nkr_name);
    let running = running_vm.is_some();
    let db_present = check_db_present(&cell.name, &db_name);
    let phase = compute_phase(running, port_up, db_present);
    let last_error = if phase == "error" {
        read_last_error_line(&cell.name, nkr_name)
    } else { None };
    let odoo_ver = meta.odoo_version.clone().or_else(|| cell.odoo_version.clone());
    // Leer estado del job init-db si existe. El job se persiste en
    // <instance_dir>/.nkr-init-db.json por handle_init_db (api.rs).
    let init_db = std::fs::read_to_string(instance_dir.join(".nkr-init-db.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok());
    InstanceInfo {
        nkr_name: nkr_name.to_string(),
        cell: cell.name.clone(),
        vm_id,
        guest_ip: guest_ip.clone(),
        dns: meta.dns.clone(),
        db_name,
        addons_path: instance_dir.join("addons").to_string_lossy().to_string(),
        logs_path: instance_dir.join("logs").join("odoo.log").to_string_lossy().to_string(),
        config_path: instance_dir.join("config").join("odoo.conf").to_string_lossy().to_string(),
        instance_dir: instance_dir.to_string_lossy().to_string(),
        meta: meta.clone(),
        nkr_status: NkrStatus {
            running,
            pid,
            ram_mb,
            uptime_s,
            port_8069_up: port_up,
            phase,
            db_present,
            odoo_version_running: odoo_ver,
            last_error,
            init_db,
        },
    }
}

/// Fase lógica. Tres inputs: VM viva, puerto Odoo arriba, DB del tenant existe.
/// Valores alineados con spec del panel: provisioning | booting | loading | ready | error.
fn compute_phase(running: bool, port_up: bool, db_present: bool) -> String {
    match (running, port_up, db_present) {
        (false, _, _)         => "provisioning".to_string(),
        (true, false, _)      => "booting".to_string(),
        (true, true, false)   => "loading".to_string(),
        (true, true, true)    => "ready".to_string(),
    }
}

/// Chequea si existe la DB del tenant en el PG de la cell vía `psql -l`.
/// Rápido (~50ms) y no requiere nueva conexión pool. Best-effort: si el PG
/// no responde (raro), devuelve false.
fn check_db_present(cell_name: &str, db_name: &str) -> bool {
    // Necesitamos IP del PG de la cell. Por convención cell_id=N → pg_ip=10.0.N.2.
    let cell_id = match lookup_cell_id(cell_name) {
        Some(c) => c,
        None => return false,
    };
    let pg_ip = format!("10.0.{}.2", cell_id);
    let out = std::process::Command::new("psql")
        .env("PGPASSWORD", "odoo")
        .env("PGCONNECT_TIMEOUT", "2")
        .args([
            "-h", &pg_ip, "-p", "5432", "-U", "odoo", "-d", "postgres",
            "-tA", "-c",
            &format!("SELECT 1 FROM pg_database WHERE datname='{}' LIMIT 1;",
                db_name.replace('\'', "''")),
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout).trim() == "1"
        }
        _ => false,
    }
}

fn read_last_error_line(cell_name: &str, nkr_name: &str) -> Option<String> {
    let log_path = cells_dir().join(cell_name).join("logs").join("nkr-compose.log");
    let content = fs::read_to_string(&log_path).ok()?;
    // Buscar líneas que matcheen el tenant y contengan ERROR / FATAL / Process exited.
    let mut last: Option<String> = None;
    for line in content.lines() {
        if !line.contains(nkr_name) { continue; }
        let lower = line.to_lowercase();
        if lower.contains("error") || lower.contains("fatal")
            || lower.contains("process exited") || lower.contains("traceback") {
            last = Some(line.to_string());
        }
    }
    last.map(|l| l.chars().take(300).collect())
}

fn tcp_probe(ip: &str, port: u16, timeout: std::time::Duration) -> bool {
    let addr = format!("{}:{}", ip, port);
    addr.parse()
        .ok()
        .and_then(|sa: std::net::SocketAddr| std::net::TcpStream::connect_timeout(&sa, timeout).ok())
        .is_some()
}

fn read_rss_mb(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{}/status", pid)).ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1024);
        }
    }
    None
}

fn started_at_secs(pid: u32) -> Option<u64> {
    let stat = fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    // start_time is field 22 (after the comm in parens which may contain spaces).
    let after_comm = stat.rsplit_once(')').map(|(_, r)| r)?.trim();
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // start_time is fields[19] (field 22 of stat, with offset due to split).
    let start_clock_ticks: u64 = fields.get(19)?.parse().ok()?;
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) as u64 };
    if hz == 0 { return None; }
    let boot = fs::read_to_string("/proc/stat").ok()?;
    let btime: u64 = boot.lines()
        .find_map(|l| l.strip_prefix("btime ").and_then(|s| s.parse().ok()))?;
    let started_unix = btime + start_clock_ticks / hz;
    let now = now_unix_secs();
    Some(now.saturating_sub(started_unix))
}

// =============================================================================
// Full rewrite of odoo.conf with workers/list_db/memory limits/addons_path
// =============================================================================

/// Applies the optional fields of `CloneOptions` to the destination's `odoo.conf`.
/// Runs AFTER `rewrite_odoo_conf` (which already replaced the nkr_name).
fn rewrite_odoo_conf_full(
    dst_dir: &Path,
    dst_nkr: &str,
    opts: &CloneOptions,
) -> Result<(), Box<dyn std::error::Error>> {
    let conf_path = dst_dir.join("config").join("odoo.conf");
    if !conf_path.exists() {
        eprintln!("[NKR-CLONE] WARN: odoo.conf no existe en {}, omitido full-rewrite", conf_path.display());
        return Ok(());
    }

    let content = fs::read_to_string(&conf_path)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();

    // Always fix dbfilter to dst. list_db se fuerza a False en cells productivas
    // (opts.list_db ignora request del panel, es NKR quien decide por seguridad).
    let db_name = format!("db-{}", dst_nkr);
    upsert_key(&mut lines, "dbfilter", &format!("^{}$", db_name));
    upsert_key(&mut lines, "list_db", "False");
    // Nota: db_prepared_statements era válido en v15-17 pero Odoo v18+ lo
    // descontinuó (pgbouncer transaction pooling lo maneja distinto). No
    // forzamos un valor — si el template lo tenía, ya lo limpiamos a mano.
    // proxy_mode = True por default (cells siempre detrás de nginx/Cloudflare).
    // Opt-out con opts.proxy_mode == Some(false) — útil para tests locales.
    let proxy_mode_on = opts.proxy_mode.unwrap_or(true);
    upsert_key(&mut lines, "proxy_mode", if proxy_mode_on { "True" } else { "False" });

    if let Some(workers) = opts.workers {
        upsert_key(&mut lines, "workers", &workers.to_string());
    }
    if let Some(soft) = opts.limit_memory_soft {
        upsert_key(&mut lines, "limit_memory_soft", &soft.to_string());
    }
    if let Some(hard) = opts.limit_memory_hard {
        upsert_key(&mut lines, "limit_memory_hard", &hard.to_string());
    }
    if let Some(ref ap) = opts.addons_path {
        upsert_key(&mut lines, "addons_path", ap);
    } else if !matches!(opts.edition, Some(Edition::Enterprise)) {
        // Sin addons_path explícito Y community: filtrar /mnt/extra-enterprise
        // del path heredado del template para que Odoo no warnee por dir vacío
        // y no cargue manifests enterprise. Si más tarde el panel quiere
        // upgrade a enterprise, lo hace via PATCH /config con addons_path
        // explícito incluyendo /mnt/extra-enterprise.
        if let Some(idx) = lines.iter().position(|l| l.trim_start().starts_with("addons_path"))
        {
            let line = &lines[idx];
            if let Some(eq) = line.find('=') {
                let prefix = &line[..eq + 1];
                let value = &line[eq + 1..];
                let cleaned: Vec<&str> = value.split(',')
                    .map(|p| p.trim())
                    .filter(|p| !p.is_empty() && *p != "/mnt/extra-enterprise")
                    .collect();
                lines[idx] = format!("{} {}", prefix.trim_end(), cleaned.join(","));
            }
        }
    }
    if let Some(ref ap) = opts.admin_passwd {
        upsert_key(&mut lines, "admin_passwd", ap);
    }
    // db_name is forced by dbfilter; we also leave it explicit in case the conf had it.
    upsert_key(&mut lines, "db_name", &db_name);
    // logfile: forzar archivo dentro del share rw montado en /var/log/odoo. Sin esto
    // Odoo escribe a stdout (el pipe del proceso nkr) y el host nunca ve odoo.log,
    // dejando GET /logs vacío. El template legacy puede traer "logfile = None".
    upsert_key(&mut lines, "logfile", "/var/log/odoo/odoo.log");

    fs::write(&conf_path, lines.join("\n") + "\n")?;
    eprintln!("[NKR-CLONE] odoo.conf actualizado: dbfilter, workers, list_db=False, proxy_mode={}, admin_passwd={}, logfile=/var/log/odoo/odoo.log",
        proxy_mode_on, if opts.admin_passwd.is_some() { "set" } else { "preserved" });
    Ok(())
}

/// Maximum number of `nkr-compose.yml.bak.<ts>` backups kept per cell. Older
/// ones are deleted on every new mutation. The default of 20 covers ~weeks
/// of churn in normal panel usage; raise it via env if the operator wants
/// deeper history.
const COMPOSE_BACKUPS_TO_KEEP: usize = 20;

/// Copies `compose_path` to `<compose_path>.bak.<unix_ts>` and then prunes
/// older backups in the same directory, keeping at most COMPOSE_BACKUPS_TO_KEEP.
/// Returns the path of the new backup. Best-effort: if the copy fails, returns
/// the path it WOULD have used so callers can still reference it in logs.
/// The pruning step is independent of the copy and never propagates errors.
fn backup_compose_with_rotation(compose_path: &Path) -> PathBuf {
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let bak = compose_path.with_extension(format!("yml.bak.{}", ts));
    if let Err(e) = fs::copy(compose_path, &bak) {
        eprintln!("[NKR-COMPOSE] WARN: backup copy {} failed: {}",
            bak.display(), e);
    }
    rotate_compose_backups(compose_path, COMPOSE_BACKUPS_TO_KEEP);
    bak
}

/// Deletes the oldest `<compose_path>.bak.<ts>` files in the same directory
/// until at most `keep` remain. The timestamp encoded in the filename is
/// authoritative (mtime can be wrong if files were rsync'd from elsewhere).
fn rotate_compose_backups(compose_path: &Path, keep: usize) {
    let parent = match compose_path.parent() {
        Some(p) => p,
        None => return,
    };
    let stem = match compose_path.file_name().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return,
    };
    let prefix = format!("{}.bak.", stem);

    let mut backups: Vec<(PathBuf, u64)> = match fs::read_dir(parent) {
        Ok(rd) => rd
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let suffix = name.strip_prefix(&prefix)?;
                let ts: u64 = suffix.parse().ok()?;
                Some((e.path(), ts))
            })
            .collect(),
        Err(_) => return,
    };
    if backups.len() <= keep {
        return;
    }
    backups.sort_by_key(|(_, ts)| *ts); // ascending: oldest first
    let drop_n = backups.len() - keep;
    for (path, _) in backups.iter().take(drop_n) {
        if let Err(e) = fs::remove_file(path) {
            eprintln!("[NKR-COMPOSE] WARN: prune {} failed: {}",
                path.display(), e);
        }
    }
}

/// Inserts or replaces `key = value` under the `[options]` section of an
/// INI-style file. Section-aware: replacement only happens within the
/// `[options]` range, so a `[some-other-section]` containing the same key
/// is never overwritten. Standard Odoo conf files use only `[options]`, so
/// the behavior matches the previous implementation in the common case;
/// the change only matters when an operator has added extra sections.
fn upsert_key(lines: &mut Vec<String>, key: &str, value: &str) {
    let target = format!("{} = {}", key, value);

    // Locate `[options]` boundaries. start_idx is the line AFTER the header
    // (first line of the section body). end_idx is the line of the next
    // `[...]` header or `lines.len()` if [options] runs to EOF.
    let opts_header = lines.iter().position(|l| l.trim() == "[options]");
    let (start_idx, end_idx) = match opts_header {
        Some(h) => {
            let next = lines[h + 1..]
                .iter()
                .position(|l| {
                    let t = l.trim();
                    t.starts_with('[') && t.ends_with(']')
                })
                .map(|rel| h + 1 + rel)
                .unwrap_or(lines.len());
            (h + 1, next)
        }
        None => (lines.len(), lines.len()),
    };

    // Attempt 1: in-place replacement WITHIN the [options] range.
    let key_eq = format!("{} =", key);
    let key_eq_tight = format!("{}=", key);
    for i in start_idx..end_idx {
        let trimmed = lines[i].trim_start();
        if trimmed.starts_with(&key_eq) || trimmed.starts_with(&key_eq_tight) {
            lines[i] = target;
            return;
        }
    }

    // Attempt 2: append under [options] (creating the section if missing).
    if let Some(h) = opts_header {
        // Insert right after the header so new keys cluster at the top of the
        // section. Operators reading the conf scan top-down and expect new
        // additions to be visible without scrolling.
        lines.insert(h + 1, target);
    } else {
        if !lines.is_empty() && !lines.last().unwrap().is_empty() {
            lines.push(String::new());
        }
        lines.push("[options]".to_string());
        lines.push(target);
    }
}

// =============================================================================
// Patch odoo.conf — selective key upsert usado por PATCH /config
// =============================================================================

/// Upsert de un conjunto de keys en `odoo.conf` del tenant. Preserva el resto
/// del archivo. Usa el mismo `upsert_key` interno (sección `[options]`).
///
/// Las keys SE APLICAN literal — el caller es responsable de renderizar valores
/// (p.ej. `"True"`/`"False"` para booleans, número como string para enteros).
///
/// Nota: `dbfilter`, `db_name`, `proxy_mode`, `list_db` no se aceptan por este
/// método (inmutables tras el clone — cambiarlos rompería routing/seguridad).
/// El caller DEBE filtrar antes de llamar.
pub fn patch_odoo_conf(
    config_path: &str,
    upserts: &[(String, String)],
) -> Result<(), Box<dyn std::error::Error>> {
    let p = Path::new(config_path);
    if !p.exists() {
        return Err(format!("odoo.conf no existe: {}", config_path).into());
    }
    let content = fs::read_to_string(p)?;
    let mut lines: Vec<String> = content.lines().map(|l| l.to_string()).collect();
    for (k, v) in upserts {
        upsert_key(&mut lines, k, v);
    }
    fs::write(p, lines.join("\n") + "\n")?;
    Ok(())
}

/// Reescribe `ram: N` y/o `chrs: N` en el bloque del `nkr-compose.yml` que
/// corresponde al tenant. Si el bloque no tiene la key, la inserta después del
/// header. Cambios sólo aplican tras restart (stop → start) del proceso `nkr run`.
pub fn patch_compose_block_resources(
    nkr_name: &str,
    ram_mb: Option<u32>,
    chrs: Option<u32>,
    balloon_mb: Option<u32>,
) -> Result<(), Box<dyn std::error::Error>> {
    if ram_mb.is_none() && chrs.is_none() && balloon_mb.is_none() {
        return Ok(());
    }
    let (cell, _instance_dir) = find_instance_cell(nkr_name)?;
    let compose_path = cell_compose_path(&cell.name);
    if !compose_path.exists() {
        return Err(format!("No existe {}", compose_path.display()).into());
    }

    let content = fs::read_to_string(&compose_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // Locate block via nkr_name.
    let mut blk_start: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("nkr_name:") {
            let rhs = trimmed.trim_start_matches("nkr_name:").trim()
                .trim_matches('"').trim_matches('\'');
            if rhs == nkr_name {
                for j in (0..=i).rev() {
                    let l = lines[j];
                    if l.len() >= 3
                        && l.starts_with("  ")
                        && !l.starts_with("   ")
                        && l.trim_end().ends_with(':')
                        && !l.trim_start().starts_with('-')
                    {
                        blk_start = Some(j);
                        break;
                    }
                }
                break;
            }
        }
    }
    let start = blk_start.ok_or_else(|| format!("bloque con nkr_name={} no encontrado", nkr_name))?;

    let mut blk_end: usize = lines.len();
    for i in (start + 1)..lines.len() {
        let l = lines[i];
        let is_service_header = l.len() >= 3 && l.starts_with("  ")
            && !l.starts_with("   ") && l.trim_end().ends_with(':')
            && !l.trim_start().starts_with('-') && !l.trim_start().starts_with('#');
        let is_top_level = !l.is_empty() && !l.starts_with(' ') && !l.starts_with('#');
        if is_service_header || is_top_level {
            blk_end = i;
            break;
        }
    }

    let mut out: Vec<String> = lines.iter().map(|s| s.to_string()).collect();
    let mut ram_done = ram_mb.is_none();
    let mut chrs_done = chrs.is_none();
    let mut balloon_done = balloon_mb.is_none();
    for i in (start + 1)..blk_end {
        let l = &out[i];
        let t = l.trim_start();
        if !ram_done && t.starts_with("ram:") {
            let indent = &l[..l.len() - t.len()];
            let comment = rest_trailing_comment(t.strip_prefix("ram:").unwrap_or(""));
            out[i] = format!("{}ram: {}{}", indent, ram_mb.unwrap(), comment);
            ram_done = true;
        } else if !chrs_done && t.starts_with("chrs:") {
            let indent = &l[..l.len() - t.len()];
            let comment = rest_trailing_comment(t.strip_prefix("chrs:").unwrap_or(""));
            out[i] = format!("{}chrs: {}{}", indent, chrs.unwrap(), comment);
            chrs_done = true;
        } else if !balloon_done && t.starts_with("balloon_mb:") {
            let indent = &l[..l.len() - t.len()];
            let comment = rest_trailing_comment(t.strip_prefix("balloon_mb:").unwrap_or(""));
            out[i] = format!("{}balloon_mb: {}{}", indent, balloon_mb.unwrap(), comment);
            balloon_done = true;
        }
        if ram_done && chrs_done && balloon_done { break; }
    }
    // Insert missing keys after header.
    let mut insert_at = start + 1;
    if !chrs_done {
        out.insert(insert_at, format!("    chrs: {}", chrs.unwrap()));
        insert_at += 1;
    }
    if !ram_done {
        out.insert(insert_at, format!("    ram: {}", ram_mb.unwrap()));
        insert_at += 1;
    }
    if !balloon_done {
        out.insert(insert_at, format!("    balloon_mb: {}", balloon_mb.unwrap()));
    }

    let bak = backup_compose_with_rotation(&compose_path);
    fs::write(&compose_path, out.join("\n") + "\n")?;
    eprintln!("[NKR-PATCH] compose '{}' updated: ram={:?} chrs={:?} balloon_mb={:?} (backup {})",
        nkr_name, ram_mb, chrs, balloon_mb, bak.display());
    Ok(())
}


// =============================================================================
// Delete instance — stop VM, drop DB, remove dir, remove compose block
// =============================================================================

/// Fully removes an instance: stops VM, drops DB, removes dir and compose block.
/// Idempotent: if something doesn't exist, keeps going. Returns cell name for the panel.
pub fn delete_instance(nkr_name: &str, drop_db: bool) -> Result<String, Box<dyn std::error::Error>> {
    let (cell, instance_dir) = find_instance_cell(nkr_name)?;

    // 1. Stop VM if running (con fallback por process name si state file falta).
    let running = crate::state::list_vms().into_iter().find(|v| v.name == nkr_name);
    if let Some(vm) = running {
        eprintln!("[NKR-DELETE] Deteniendo VM '{}' (PID {})...", nkr_name, vm.pid);
        if let Err(e) = crate::state::stop_vm(vm.vm_id) {
            eprintln!("[NKR-DELETE] WARN: stop_vm falló: {} — intentando por nombre", e);
            if let Err(e2) = crate::state::stop_vm_by_name(nkr_name) {
                eprintln!("[NKR-DELETE] WARN: stop_vm_by_name también falló: {} (continuando)", e2);
            }
        }
    } else {
        // No hay state file — intentar fallback por process name por si el
        // proceso quedó zombie.
        if let Err(_) = crate::state::stop_vm_by_name(nkr_name) {
            // Silencioso: si tampoco hay proceso, está OK (nada que parar).
        }
    }

    // 2. Drop DB
    if drop_db {
        if let Err(e) = drop_database(cell.cell_id, nkr_name) {
            eprintln!("[NKR-DELETE] WARN: drop DB falló: {} (continuando)", e);
        }
    }

    // 3. Remove block from compose YAML
    if let Err(e) = remove_compose_block(&cell, nkr_name) {
        eprintln!("[NKR-DELETE] WARN: no se pudo editar compose: {} (continuando)", e);
    }

    // 4. Release vm_id from the instances registry (key scoped "cell/vm")
    let registry_key = format!("{}/{}", cell.name.to_lowercase(), nkr_name.to_lowercase());
    let _ = crate::registry::remove(&registry_key);

    // 5. Delete instance directory (client persistent data)
    if instance_dir.exists() {
        fs::remove_dir_all(&instance_dir)?;
        eprintln!("[NKR-DELETE] dir removido: {}", instance_dir.display());
    }

    // 6. Clean up associated `.nkr-data/` files (filestore + per-instance disks)
    //    Naming convention: `.nkr-data/<short_name>-<suffix>` or `.nkr-data/<short_name>-<suffix>.ext4`
    let cell_prefix = format!("{}-", cell.name);
    let short = nkr_name.strip_prefix(&cell_prefix).unwrap_or(nkr_name);
    let match_prefix = format!("{}-", short);
    let nkr_data = cells_dir().join(&cell.name).join(".nkr-data");
    if let Ok(it) = fs::read_dir(&nkr_data) {
        for e in it.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            if fname.starts_with(&match_prefix) {
                let p = e.path();
                if p.is_dir() {
                    let _ = fs::remove_dir_all(&p);
                } else {
                    let _ = fs::remove_file(&p);
                }
                eprintln!("[NKR-DELETE] nkr-data removido: {}", p.display());
            }
        }
    }

    eprintln!("[NKR-DELETE] ✅ instancia '{}' eliminada", nkr_name);
    Ok(cell.name)
}

fn drop_database(cell_id: u8, nkr_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let db_ip = format!("10.0.{}.2", cell_id);
    let db_name = format!("db-{}", nkr_name);

    let check = std::process::Command::new("pg_isready")
        .args(["-h", &db_ip, "-p", "5432", "-U", "odoo", "-t", "3"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    if !check.map(|s| s.success()).unwrap_or(false) {
        return Err(format!("PostgreSQL {}:5432 no responde — salteo drop DB", db_ip).into());
    }

    // Disconnect clients first, then DROP. DROP DATABASE also cannot run inside
    // a transaction → we send via stdin (see run_psql_stdin).
    let sql = format!(
        "ALTER DATABASE \"{db}\" WITH ALLOW_CONNECTIONS false;\n\
         SELECT pg_terminate_backend(pid) FROM pg_stat_activity \
           WHERE datname = '{db}' AND pid <> pg_backend_pid();\n\
         DROP DATABASE IF EXISTS \"{db}\";\n",
        db = db_name
    );
    let out = run_psql_stdin(&db_ip, &sql)?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!("psql drop falló: {}", stderr.trim()).into());
    }
    eprintln!("[NKR-DELETE] DB '{}' eliminada en {}:5432", db_name, db_ip);
    Ok(())
}

fn remove_compose_block(cell: &CellConfig, nkr_name: &str) -> Result<(), Box<dyn std::error::Error>> {
    let compose_path = cell_compose_path(&cell.name);
    if !compose_path.exists() {
        return Ok(());
    }
    let content = fs::read_to_string(&compose_path)?;
    let lines: Vec<&str> = content.lines().collect();

    // Locate the block by `nkr_name: "<name>"` — same criterion as append_compose_block.
    let mut blk_start: Option<usize> = None;
    let mut blk_end: Option<usize> = None;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("nkr_name:") {
            let rhs = trimmed.trim_start_matches("nkr_name:").trim().trim_matches('"').trim_matches('\'');
            if rhs == nkr_name {
                for j in (0..=i).rev() {
                    let l = lines[j];
                    if l.len() >= 3
                        && l.starts_with("  ")
                        && !l.starts_with("   ")
                        && l.trim_end().ends_with(':')
                        && !l.trim_start().starts_with('-')
                    {
                        blk_start = Some(j);
                        break;
                    }
                }
                break;
            }
        }
    }
    let start = match blk_start { Some(s) => s, None => return Ok(()) };
    for i in (start + 1)..lines.len() {
        let l = lines[i];
        let is_service_header = l.len() >= 3
            && l.starts_with("  ")
            && !l.starts_with("   ")
            && l.trim_end().ends_with(':')
            && !l.trim_start().starts_with('-')
            && !l.trim_start().starts_with('#');
        let is_top_level = !l.is_empty() && !l.starts_with(' ') && !l.starts_with('#');
        if is_service_header || is_top_level {
            blk_end = Some(i);
            break;
        }
    }
    let end = blk_end.unwrap_or(lines.len());

    // Backup (rotated: keeps only the last N backups).
    let bak = backup_compose_with_rotation(&compose_path);

    let kept: Vec<String> = lines.iter().enumerate()
        .filter(|(i, _)| *i < start || *i >= end)
        .map(|(_, l)| l.to_string())
        .collect();
    fs::write(&compose_path, kept.join("\n") + "\n")?;
    eprintln!("[NKR-DELETE] bloque '{}' removido de {} (backup {})",
        nkr_name, compose_path.display(), bak.display());
    Ok(())
}

// =============================================================================
// Capacity / version planning — for cell auto-selection in the API
// =============================================================================

/// Fixed limit of Odoos per cell (NKR convention). 20 Odoos + 1 PG + 1 PgB = 22 VMs
/// per cell, max 5 cells in 32 GB RAM = 110 VMs.
pub const MAX_ODOOS_PER_CELL: usize = 20;

/// Counts Odoo instances under `cells/<cell>/instances/*` (PG and pgbouncer
/// don't live there, so any dir counts as a deployed Odoo).
pub fn count_odoo_instances(cell_name: &str) -> usize {
    let dir = cells_dir().join(cell_name).join("instances");
    match fs::read_dir(&dir) {
        Ok(it) => it.flatten().filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false)).count(),
        Err(_) => 0,
    }
}

/// Selects the first cell with matching `odoo_version` and at least 1 free slot.
/// Sorts by ascending cell_id (deterministic). Returns error if none matches
/// or all are full.
pub fn select_cell_for_version(
    odoo_version: &str,
) -> Result<CellConfig, Box<dyn std::error::Error>> {
    let candidates: Vec<(CellConfig, usize)> = list_cells().into_iter()
        .filter(|c| c.odoo_version.as_deref() == Some(odoo_version))
        .map(|c| {
            let used = count_odoo_instances(&c.name);
            (c, used)
        })
        .collect();

    if candidates.is_empty() {
        return Err(format!(
            "No hay cells con odoo_version={}. Cells disponibles: {:?}",
            odoo_version,
            list_cells().iter().map(|c| format!("{}={}",
                c.name,
                c.odoo_version.as_deref().unwrap_or("?"))).collect::<Vec<_>>()
        ).into());
    }

    // Prefer the LEAST full cell: balances load without panel intervention.
    let mut with_slots: Vec<_> = candidates.into_iter()
        .filter(|(_, used)| *used < MAX_ODOOS_PER_CELL)
        .collect();
    with_slots.sort_by_key(|(c, used)| (*used, c.cell_id));

    with_slots.into_iter().next()
        .map(|(c, _)| c)
        .ok_or_else(|| format!(
            "Todas las cells con odoo_version={} están llenas ({}/{} Odoos)",
            odoo_version, MAX_ODOOS_PER_CELL, MAX_ODOOS_PER_CELL
        ).into())
}

/// Resolves the "source" for cloning when the panel doesn't specify it.
/// Convention: first instance dir sorted alphabetically in the cell.
/// In a newly-created cell with no instances, returns error.
#[allow(dead_code)]  // mantenido para CLI legacy / futuras extensiones
pub fn default_source_in_cell(cell_name: &str) -> Result<String, Box<dyn std::error::Error>> {
    let dir = cells_dir().join(cell_name).join("instances");
    let mut names: Vec<String> = fs::read_dir(&dir)
        .map_err(|e| format!("Cell '{}' sin dir instances/: {}", cell_name, e))?
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    names.sort();
    names.into_iter().next()
        .ok_or_else(|| format!("Cell '{}' no tiene instancias para usar como template", cell_name).into())
}

/// Normalizes nkr_name accepting short form (no cell prefix) or full.
/// E.g. ensure_cell_prefix("nazcatex", "tst-1") → "nazcatex-tst-1"
///      ensure_cell_prefix("nazcatex", "nazcatex-tst-1") → "nazcatex-tst-1"
pub fn ensure_cell_prefix(cell_name: &str, nkr_name: &str) -> String {
    let prefix = format!("{}-", cell_name);
    if nkr_name.starts_with(&prefix) {
        nkr_name.to_string()
    } else {
        format!("{}{}", prefix, nkr_name)
    }
}

/// Recursive walk — returns all `.ext4` under `dir`. Used to re-apply +C
/// to each cloned disk.
fn walk_ext4_files(dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut out = Vec::new();
    fn recurse(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for e in fs::read_dir(dir)?.flatten() {
            let p = e.path();
            let ft = e.file_type()?;
            if ft.is_dir() { recurse(&p, out)?; }
            else if ft.is_file() && p.extension().map(|s| s == "ext4").unwrap_or(false) {
                out.push(p);
            }
        }
        Ok(())
    }
    recurse(dir, &mut out)?;
    Ok(out)
}

/// Clones `.nkr-data/` files associated with `src_nkr` as equivalents for
/// `dst_nkr`. Naming convention (see `src/compose.rs:745`):
///   `.nkr-data/<short_name>-<guest_path_sanitized>.ext4`
/// where `short_name = strip_prefix("<cell>-", nkr_name)`.
///
/// Example: for cell "nazcatex" when cloning "nazcatex-odoo-01" → "nazcatex-odoo-04"
///   - `odoo-01-var_lib_odoo.ext4` → `odoo-04-var_lib_odoo.ext4`
///
/// Uses `cp -a --reflink=auto` (O(1) on btrfs) and re-applies `+C` via preserve_nocow.
/// If the dst already existed (compose created it empty on-demand), it gets overwritten.
fn clone_nkr_data_files(
    cell: &CellConfig,
    src_nkr: &str,
    dst_nkr: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let nkr_data = cells_dir().join(&cell.name).join(".nkr-data");
    if !nkr_data.exists() { return Ok(()); }

    let cell_prefix = format!("{}-", cell.name);
    let short_src = src_nkr.strip_prefix(&cell_prefix).unwrap_or(src_nkr);
    let short_dst = dst_nkr.strip_prefix(&cell_prefix).unwrap_or(dst_nkr);
    let match_prefix = format!("{}-", short_src);

    let mut cloned = 0usize;
    for entry in fs::read_dir(&nkr_data)?.flatten() {
        let fname_os = entry.file_name();
        let fname = fname_os.to_string_lossy().to_string();
        let suffix = match fname.strip_prefix(&match_prefix) {
            Some(s) => s,
            None => continue,
        };
        // We only clone files (ignore subdirs that started with the prefix by coincidence)
        if !entry.file_type().map(|t| t.is_file()).unwrap_or(false) { continue; }

        let src_path = entry.path();
        let dst_path = nkr_data.join(format!("{}-{}", short_dst, suffix));

        if dst_path.exists() {
            let _ = fs::remove_file(&dst_path);
        }

        let status = std::process::Command::new("cp")
            .args([
                "-a", "--reflink=auto",
                &*src_path.to_string_lossy(),
                &*dst_path.to_string_lossy(),
            ])
            .status()?;
        if !status.success() {
            return Err(format!("cp nkr-data falló: {} → {}", src_path.display(), dst_path.display()).into());
        }

        // Re-apply +C if the cloned file is an .ext4 on btrfs.
        if dst_path.extension().map(|s| s == "ext4").unwrap_or(false) {
            if let Err(e) = crate::fsutil::preserve_nocow(&dst_path) {
                eprintln!("[NKR-CLONE] WARN: preserve_nocow falló en {}: {}", dst_path.display(), e);
            }
            // NOTE: the rename `filestore/db-<src>/` → `filestore/db-<dst>/` is
            // done in-guest on first boot (`nkr-start.sh` reads env vars
            // NKR_RENAME_FILESTORE_FROM/TO). This avoids mount -o loop on the host,
            // which serialized concurrent clones and risked corruption.
        }
        eprintln!("[NKR-CLONE] nkr-data: {} → {}",
            src_path.file_name().unwrap().to_string_lossy(),
            dst_path.file_name().unwrap().to_string_lossy());
        cloned += 1;
    }
    if cloned > 0 {
        eprintln!("[NKR-CLONE] {} archivos .nkr-data clonados de '{}' → '{}'", cloned, src_nkr, dst_nkr);
    }
    Ok(())
}

/// Returns InstanceInfo for an existing instance (for GET on the API).
pub fn get_instance_info(nkr_name: &str) -> Result<InstanceInfo, Box<dyn std::error::Error>> {
    let (cell, dir) = find_instance_cell(nkr_name)?;
    let meta = load_instance_meta(&dir).unwrap_or_else(|| {
        // Pre-existing instance to the API (hand-created): minimal inferred meta.
        InstanceMeta {
            nkr_name: nkr_name.to_string(),
            cell: cell.name.clone(),
            source: None,
            mode: InstanceMode::Production,
            dns: None,
            edition: None,
            odoo_version: cell.odoo_version.clone(),
            pg_version: None,
            workers: None,
            list_db: None,
            limit_memory_soft: None,
            limit_memory_hard: None,
            addons_path: None,
            created_at: 0,
        }
    });
    let vm_id = crate::registry::resolve_id_scoped(Some(&cell.name), nkr_name)?;
    Ok(build_instance_info(&cell, nkr_name, vm_id, &meta))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::env;

    #[test]
    fn clonescratch_rollback_removes_dst_dir_and_files() {
        // Pure Drop-logic test, doesn't touch registry/PG.
        let tmp = env::temp_dir().join(format!("nkr-clonescratch-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let dst = tmp.join("dst");
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("marker"), b"x").unwrap();

        let nkr_data_file = tmp.join("foo-disk.ext4");
        fs::write(&nkr_data_file, b"y").unwrap();

        {
            let mut s = CloneScratch::new();
            s.dst_dir = Some(dst.clone());
            s.nkr_data_files.push(nkr_data_file.clone());
            // No commit → Drop must clean up.
        }

        assert!(!dst.exists(), "dst_dir should have been removed");
        assert!(!nkr_data_file.exists(), "nkr-data file should have been removed");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn clonescratch_commit_preserves_artifacts() {
        let tmp = env::temp_dir().join(format!("nkr-clonecommit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let dst = tmp.join("dst");
        fs::create_dir_all(&dst).unwrap();
        fs::write(dst.join("marker"), b"x").unwrap();

        {
            let mut s = CloneScratch::new();
            s.dst_dir = Some(dst.clone());
            s.commit();
        }

        assert!(dst.exists(), "commit() should have preserved the dir");
        assert!(dst.join("marker").exists(), "commit() should have preserved the file");
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn upsert_key_replaces_within_options_only() {
        // [admin] has the same key — must NOT be touched.
        let mut lines: Vec<String> = vec![
            String::from("[admin]"),
            String::from("workers = 99"),
            String::new(),
            String::from("[options]"),
            String::from("workers = 2"),
            String::from("list_db = True"),
        ];

        upsert_key(&mut lines, "workers", "8");

        assert_eq!(lines[1], "workers = 99", "[admin] section must be untouched");
        assert!(lines.iter().any(|l| l == "workers = 8"), "[options] workers must be replaced");
        // The [options] block still contains list_db.
        assert!(lines.iter().any(|l| l == "list_db = True"));
    }

    #[test]
    fn upsert_key_inserts_at_top_of_options_when_missing() {
        let mut lines: Vec<String> = vec![
            String::from("[options]"),
            String::from("workers = 2"),
        ];

        upsert_key(&mut lines, "logfile", "/var/log/odoo/odoo.log");

        // logfile must be the first line under [options], not appended elsewhere.
        assert_eq!(lines[0], "[options]");
        assert_eq!(lines[1], "logfile = /var/log/odoo/odoo.log");
        assert_eq!(lines[2], "workers = 2");
    }

    #[test]
    fn upsert_key_creates_options_section_when_absent() {
        let mut lines: Vec<String> = vec![String::from("; bare comment")];
        upsert_key(&mut lines, "workers", "4");
        // Must have created [options] and added the key under it.
        assert!(lines.iter().any(|l| l == "[options]"));
        assert!(lines.iter().any(|l| l == "workers = 4"));
    }

    #[test]
    fn provision_creates_reflink_copies_idempotent() {
        let tmp = env::temp_dir().join(format!("nkr-prov-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let cell_dir = tmp.join("cells/test-cell");
        let images_dir = tmp.join("images");
        fs::create_dir_all(&cell_dir).unwrap();
        fs::create_dir_all(&images_dir).unwrap();
        // Stub masters with distinct content so we can verify the copies are
        // independent inodes (cp does not deduplicate content beyond the
        // reflink mechanism).
        fs::write(images_dir.join("postgres.ext4"), b"PG_MASTER_BYTES").unwrap();
        fs::write(images_dir.join("pgbouncer.ext4"), b"PGB_MASTER_BYTES").unwrap();

        // First call: copies must be created.
        provision_cell_root_disks_with_paths(&cell_dir, &images_dir).unwrap();
        assert!(cell_dir.join("postgres-root.ext4").exists());
        assert!(cell_dir.join("pgbouncer-root.ext4").exists());
        assert_eq!(
            fs::read(cell_dir.join("postgres-root.ext4")).unwrap(),
            b"PG_MASTER_BYTES"
        );

        // Second call: idempotent, no error and no duplicate work.
        provision_cell_root_disks_with_paths(&cell_dir, &images_dir).unwrap();

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn provision_fails_if_master_missing() {
        let tmp = env::temp_dir().join(format!("nkr-prov-miss-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let cell_dir = tmp.join("cells/c");
        let images_dir = tmp.join("images");
        fs::create_dir_all(&cell_dir).unwrap();
        fs::create_dir_all(&images_dir).unwrap();
        // No master files → the helper must report the missing one.

        let res = provision_cell_root_disks_with_paths(&cell_dir, &images_dir);
        assert!(res.is_err());
        let msg = res.unwrap_err().to_string();
        assert!(msg.contains("master ext4 missing"), "unexpected msg: {}", msg);

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn provision_skips_existing_dst() {
        let tmp = env::temp_dir().join(format!("nkr-prov-skip-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        let cell_dir = tmp.join("cells/c");
        let images_dir = tmp.join("images");
        fs::create_dir_all(&cell_dir).unwrap();
        fs::create_dir_all(&images_dir).unwrap();
        fs::write(images_dir.join("postgres.ext4"), b"NEW").unwrap();
        fs::write(images_dir.join("pgbouncer.ext4"), b"NEW").unwrap();

        // Pre-existing copy with stale content. The helper must NOT overwrite
        // it — once a cell has been provisioned, the per-cell copy diverges
        // from the master and must be preserved.
        fs::write(cell_dir.join("postgres-root.ext4"), b"STALE_BUT_OURS").unwrap();

        provision_cell_root_disks_with_paths(&cell_dir, &images_dir).unwrap();

        assert_eq!(
            fs::read(cell_dir.join("postgres-root.ext4")).unwrap(),
            b"STALE_BUT_OURS",
            "existing copy must not be overwritten"
        );
        // The other master (pgbouncer) had no pre-existing copy, so it should
        // now be reflinked.
        assert_eq!(
            fs::read(cell_dir.join("pgbouncer-root.ext4")).unwrap(),
            b"NEW"
        );

        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn rotate_compose_backups_keeps_last_n() {
        let tmp = env::temp_dir().join(format!("nkr-rotate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        let compose = tmp.join("nkr-compose.yml");
        fs::write(&compose, b"x").unwrap();

        // Create 25 fake backups with increasing timestamps.
        for ts in 1000u64..1025 {
            fs::write(tmp.join(format!("nkr-compose.yml.bak.{}", ts)), b"y").unwrap();
        }

        rotate_compose_backups(&compose, 10);

        // Should keep only the 10 newest (ts >= 1015).
        let kept: Vec<u64> = fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter_map(|e| {
                let n = e.file_name().to_string_lossy().into_owned();
                let s = n.strip_prefix("nkr-compose.yml.bak.")?;
                s.parse::<u64>().ok()
            })
            .collect();
        assert_eq!(kept.len(), 10);
        assert!(kept.iter().all(|ts| *ts >= 1015), "kept old backup: {:?}", kept);

        let _ = fs::remove_dir_all(&tmp);
    }
}

