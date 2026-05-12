// =============================================================================
// NKR CLI — Command-line interface
// =============================================================================

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nkr",
    version = "1.6.3",
    about = "NKR — Nano-Kernel Runtime v1.6.3: auto_start flag + auto-seal web.base.url + snapshot_at en nkr_status",
    long_about = "NKR reemplaza Docker usando micro-VMs con KVM.\nCada contenedor corre en su propia VM con aislamiento total y acceso directo al hardware."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Run a micro-VM with an ext4 disk
    Run {
        /// Unique container hash (auto-generated if not provided)
        #[arg(long)]
        hash: Option<String>,

        /// Friendly container name
        #[arg(long, default_value = "")]
        name: String,

        /// RAM in MB (exclusive, pinned)
        #[arg(long, default_value = "512")]
        ram: u32,

        /// CPU chrs (1 chr = 20% of a physical core, exclusive)
        #[arg(short, long, default_value_t = 1)]
        chrs: u32,

        /// Micro-VM ID (determines TAP, MAC, and IP)
        #[arg(long, default_value = "1")]
        id: u8,

        /// .ext4 disks to mount (first is root, the rest are persistent)
        /// Not required if --rootfs is used
        #[arg(long)]
        disk: Vec<String>,

        /// Path to the nanolinux ELF kernel (recommended)
        #[arg(long, default_value = "nanolinux")]
        kernel: String,

        /// Path to the initramfs (auto-detected if not specified)
        #[arg(long)]
        initramfs: Option<String>,

        /// Port forwarding host:guest (e.g. 8069:8069)
        #[arg(long)]
        port: Vec<String>,

        /// Volumes host_path:guest_path (e.g. ./odoo.conf:/etc/odoo/odoo.conf)
        #[arg(short, long)]
        volume: Vec<String>,

        /// KEY=VALUE environment variables injected into the guest at /etc/nkr-env
        #[arg(short, long)]
        env: Vec<String>,

        /// TAP device name for networking (e.g. tap0)
        #[arg(long)]
        tap: Option<String>,

        /// Share host directory with the guest via VirtIO-FS
        /// Format: host_path:guest_mountpoint (e.g. /opt/filestore:/mnt/shared)
        #[arg(long)]
        share: Vec<String>,

        /// Host directory to mount as rootfs (/) via shared VirtIO-FS.
        /// Replaces --disk: the VM boots without its own block device.
        /// Allows 100+ VMs to share the same code (e.g. Odoo) with a
        /// single copy in the host's page cache.
        #[arg(long)]
        rootfs: Option<String>,

        /// Enable VirtIO-PMEM + DAX for the main disk (Feature A).
        /// Requires guest kernel with CONFIG_VIRTIO_PMEM=y and CONFIG_FS_DAX=y.
        /// Saves ~150-200 MB of RAM per instance by eliminating page cache
        /// duplication between host and guest.
        #[arg(long, default_value_t = false)]
        pmem: bool,

        /// Inflate the VirtIO-Balloon by MB: the guest returns that RAM to the host.
        /// Useful to reclaim memory from idle instances without restarting them.
        /// Example: --balloon-mb 300 reclaims 300 MB from a 700 MB VM.
        ///
        /// Bajo ballooning IDLE/ACTIVE (CLAUDE.md v2.2) este es el target ACTIVE
        /// (valor de BOOT — la VM nace con este balloon). Para DEV=0, STAG=256,
        /// PROD=0. Debe ser bajo para evitar OOM en bootstrap de Odoo.
        #[arg(long, default_value_t = 0)]
        balloon_mb: u32,

        /// Target IDLE del balloon (MB inflados cuando la VM lleva
        /// `balloon-decay-secs` sin actividad). 0 (default) = balloon estático:
        /// la VM se queda en `balloon_mb` siempre, sin transición dinámica.
        /// Con valor != balloon_mb, vmm transiciona a este target tras el
        /// decay y vuelve a ACTIVE en cada SIGUSR2 (POST /balloon).
        #[arg(long, default_value_t = 0)]
        balloon_idle_mb: u32,

        /// Segundos sin renovación SIGUSR2 antes de transicionar a IDLE.
        /// Sólo aplica si balloon-idle-mb != balloon-mb. Default 600s (10 min).
        /// El reloj arranca al boot — pero la VM nace ACTIVE (balloon=balloon_mb)
        /// y la primera transición a IDLE recién ocurre tras decay_secs sin SIGUSR2.
        #[arg(long, default_value_t = 600)]
        balloon_decay_secs: u32,

        /// Enable CPU burst (Smart default v1.3: true).
        /// Allows using idle CPU cycles in short bursts (kernel >= 5.15, cpu.max.burst).
        /// Set to false for VMs requiring strictly predictable CPU latency.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        burst: bool,

        /// Cell ID (0 = legacy bridge nkr0, 1-254 = isolated bridge nkr-br{N}).
        /// Used internally by `nkr cell up` to propagate the cell to the subprocess.
        #[arg(long, default_value_t = 0)]
        cell_id: u8,
    },

    /// List active micro-VMs
    Ps,

    /// Stop a micro-VM
    Stop {
        /// Micro-VM ID or name
        id: String,
    },

    /// Restart a micro-VM preserving its original args
    /// (useful within a cell to restart a single Odoo without touching DB/PgB)
    Restart {
        /// Micro-VM ID or name
        id: String,
    },

    /// Release a VM's cgroup (CPU without throttle) for a limited time.
    /// Useful for installing heavy Odoo modules (-i account, mrp, website)
    /// without being throttled to the cruise quota.
    /// Example: nkr nitro nazcatex-odoo-01 --duration 10m
    Nitro {
        /// Micro-VM ID or name
        id: String,

        /// Boost duration (e.g. 30s, 5m, 10m, 1h). Default: 10m.
        #[arg(long, default_value = "10m")]
        duration: String,
    },

    /// Orchestrate a multi-service stack with YAML
    Compose {
        /// Action: up, down, ps
        action: String,

        /// YAML file (default: nkr-compose.yml)
        #[arg(short, long, default_value = "nkr-compose.yml")]
        file: String,

        /// Run in background (daemon mode)
        #[arg(short, long, default_value_t = false)]
        detach: bool,
    },

    /// Download an OCI image (Docker Hub) and convert it into an ext4 disk
    Pull {
        /// Image name (e.g. postgres:15)
        image: String,

        /// Destination file (e.g. postgres.ext4). If "auto", drops into /mnt/nkr/images/
        #[arg(default_value = "auto")]
        dest: String,

        /// Disk size in MB (default: 2048)
        #[arg(long, default_value_t = 2048)]
        size_mb: u32,

        /// Do not generate initramfs automatically
        #[arg(long, default_value_t = false)]
        no_initramfs: bool,
    },

    /// Build an ext4 disk from an Nkrfile (Dockerfile compatible)
    Build {
        /// Path to the Nkrfile (default: Nkrfile)
        #[arg(short, long, default_value = "Nkrfile")]
        file: String,

        /// Output file (e.g. odoo.ext4). If "auto", drops into /mnt/nkr/images/
        #[arg(short, long, default_value = "auto")]
        output: String,

        /// NVM name (inferred from the Nkrfile if not specified)
        #[arg(short, long, default_value = "")]
        name: String,

        /// Disk size in MB (default: 4096)
        #[arg(long, default_value_t = 4096)]
        size_mb: u32,

        /// Context directory (default: .)
        #[arg(long, default_value = ".")]
        context: String,

        /// Do not generate initramfs automatically
        #[arg(long, default_value_t = false)]
        no_initramfs: bool,
    },

    /// Show real-time resource metrics (CPU%, real RAM, network, disk)
    Stats {
        /// Filter by VM ID, hash, or name (shows all if omitted)
        #[arg(default_value = "")]
        filter: String,

        /// Refresh every N seconds (htop-like). Ctrl-C to exit.
        #[arg(short = 'w', long)]
        watch: Option<u32>,
    },

    /// Manage KSM (Kernel Same-page Merging) to save RAM between VMs
    ///
    /// KSM merges identical memory pages between processes (e.g. multiple
    /// Odoo instances share the same Python/library pages).
    /// Typically saves 20-40% RAM in multi-tenant Odoo stacks.
    Ksm {
        /// Action: on, off, status
        #[arg(default_value = "status")]
        action: String,
    },

    /// Start a Prometheus-compatible metrics server (Feature 5)
    ///
    /// Exposes /metrics in Prometheus text exposition 0.0.4 format.
    /// Example: curl http://localhost:9090/metrics
    /// Configure Grafana → Prometheus → Add datasource → http://host:9090
    Serve {
        /// HTTP port to listen on (default: 9090)
        #[arg(long, default_value_t = 9090)]
        port: u16,
    },

    /// Manage Cells (multi-VM stacks with per-cell isolated network)
    ///
    /// A cell is a group of VMs (e.g. 20 Odoo + PgBouncer + PG) with its
    /// own Linux bridge, subnet (10.0.{cell_id}.0/24) and NAT. Allows running
    /// multiple Odoo versions (15, 17, 19) in parallel without conflicts.
    Cell {
        #[command(subcommand)]
        action: CellAction,
    },
}

#[derive(Subcommand)]
pub enum CellAction {
    /// Create a new cell (registers name, assigns cell_id, creates cell.yml)
    Create {
        /// Cell name (e.g. nazcatex, cafeteria)
        name: String,

        /// Associated Odoo version (e.g. 17.0, 15.0, 19.0)
        #[arg(long)]
        odoo_version: Option<String>,
    },

    /// List all registered cells
    Ls,

    /// Start a cell (runs compose up on its nkr-compose.yml)
    Up {
        /// Cell name
        name: String,

        /// Run in background (daemon mode)
        #[arg(short, long, default_value_t = false)]
        detach: bool,
    },

    /// Stop all VMs of a cell
    Down {
        /// Cell name
        name: String,
    },

    /// Show active VMs of a cell (or all if omitted)
    Ps {
        /// Cell name (optional)
        name: Option<String>,
    },

    /// Remove cell from the registry (does not delete persisted data)
    Destroy {
        /// Cell name
        name: String,
    },

    /// Clone an Odoo instance within the same cell.
    /// Duplicates filestore + addons + config + ext4 disk, assigns new vm_id,
    /// clones the DB via `CREATE DATABASE ... TEMPLATE` and adds a block to the compose.
    /// Typical use: create a test environment from prod without touching the original.
    Clone {
        /// Source instance nkr_name (e.g. nazcatex-odoo-01)
        src: String,
        /// Destination instance nkr_name (e.g. nazcatex-odoo-04)
        dst: String,
        /// Skip database cloning (only filestore/config/addons)
        #[arg(long, default_value_t = false)]
        no_db: bool,
        /// Do not modify nkr-compose.yml (manual cloning of the block)
        #[arg(long, default_value_t = false)]
        no_compose: bool,
    },
}

/// Micro-VM configuration derived from CLI arguments
pub struct VmConfig {
    pub hash: String,
    pub name: String,
    pub ram_mb: u32,
    pub chrs: u32,
    pub vm_id: u8,
    pub disks: Vec<String>,
    pub kernel_path: String,
    pub initramfs_path: Option<String>,
    pub port_forwards: Vec<String>,
    pub volumes: Vec<String>,
    pub env_vars: Vec<String>,
    pub tap_name: Option<String>,
    /// Shared directories via VirtIO-FS: "host_path:guest_path"
    pub shares: Vec<String>,
    /// Host directory mounted as rootfs (/) via shared VirtIO-FS.
    /// If Some, the VM boots without a block disk (100+ VMs can share the same code).
    pub rootfs: Option<String>,
    /// Enable VirtIO-PMEM + DAX for the main disk
    pub use_pmem: bool,
    /// MB to inflate in the VirtIO-Balloon (0 = balloon disabled).
    /// Bajo ballooning dinámico, este es el target ACTIVE (valor de BOOT).
    pub balloon_mb: u32,
    /// Target IDLE del balloon (MB inflados sin tráfico, post-decay). Si
    /// == `balloon_mb`, la VM se queda estática (sin transición dinámica).
    /// Default 0.
    pub balloon_idle_mb: u32,
    /// Segundos sin renovación antes de transicionar de ACTIVE a IDLE. Default 600s.
    pub balloon_decay_secs: u32,
    /// Enable CPU burst: allows using other VMs' idle cycles in short bursts.
    /// Smart default v1.3: true. Only disable on VMs requiring predictable latency.
    pub burst: bool,
    /// Cell ID (0 = legacy nkr0, 1-254 = isolated bridge nkr-br{N})
    pub cell_id: u8,
}

pub fn parse() -> Cli {
    Cli::parse()
}
