// =============================================================================
// NKR CLI — Interfaz de línea de comandos
// =============================================================================

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nkr",
    version = "1.0.0",
    about = "NKR — Nano-Kernel Runtime: Contenedores ultra-rápidos con micro-VMs",
    long_about = "NKR reemplaza Docker usando micro-VMs con KVM.\nCada contenedor corre en su propia VM con aislamiento total y acceso directo al hardware."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand)]
pub enum Command {
    /// Ejecutar una micro-VM con un disco ext4
    Run {
        /// Hash único del contenedor (auto-generado si no se provee)
        #[arg(long)]
        hash: Option<String>,

        /// Nombre amigable del contenedor
        #[arg(long, default_value = "")]
        name: String,

        /// RAM en MB (exclusiva, pinned)
        #[arg(long, default_value = "512")]
        ram: u32,

        /// Chrs de CPU (1 chr = 20% de un core físico, exclusivo)
        #[arg(short, long, default_value_t = 1)]
        chrs: u32,

        /// ID de la micro-VM (determina TAP, MAC e IP)
        #[arg(long, default_value = "1")]
        id: u8,

        /// Discos .ext4 a montar (el primero es root, los siguientes son persistentes)
        #[arg(long, required = true)]
        disk: Vec<String>,

        /// Ruta al kernel bzImage
        #[arg(long, default_value = "bzImage")]
        kernel: String,

        /// Ruta al initramfs (auto-detectado si no se especifica)
        #[arg(long)]
        initramfs: Option<String>,

        /// Port forwarding host:guest (e.g. 8069:8069)
        #[arg(long)]
        port: Vec<String>,

        /// Volúmenes host_path:guest_path (e.g. ./odoo.conf:/etc/odoo/odoo.conf)
        #[arg(short, long)]
        volume: Vec<String>,

        /// Variables de entorno KEY=VALUE inyectadas al guest en /etc/nkr-env
        #[arg(short, long)]
        env: Vec<String>,

        /// Nombre del TAP device para networking (e.g. tap0)
        #[arg(long)]
        tap: Option<String>,
    },

    /// Listar micro-VMs activas
    Ps,

    /// Detener una micro-VM
    Stop {
        /// ID o nombre de la micro-VM
        id: String,
    },

    /// Orquestar un stack multi-servicio con YAML
    Compose {
        /// Acción: up, down, ps
        action: String,

        /// Archivo YAML (default: nkr-compose.yml)
        #[arg(short, long, default_value = "nkr-compose.yml")]
        file: String,

        /// Ejecutar en background (daemon mode)
        #[arg(short, long, default_value_t = false)]
        detach: bool,
    },

    /// Descarga una imagen OCI (Docker Hub) y la convierte en un disco ext4
    Pull {
        /// Nombre de la imagen (ej. postgres:15)
        image: String,

        /// Archivo de destino (ej. postgres.ext4). Si es "auto", deposita en /mnt/nkr/images/
        #[arg(default_value = "auto")]
        dest: String,

        /// Tamaño del disco en MB (default: 2048)
        #[arg(long, default_value_t = 2048)]
        size_mb: u32,

        /// No generar initramfs automáticamente
        #[arg(long, default_value_t = false)]
        no_initramfs: bool,
    },

    /// Construir un disco ext4 desde un Nkrfile (Dockerfile compatible)
    Build {
        /// Ruta al Nkrfile (default: Nkrfile)
        #[arg(short, long, default_value = "Nkrfile")]
        file: String,

        /// Archivo de salida (ej. odoo.ext4). Si es "auto", deposita en /mnt/nkr/images/
        #[arg(short, long, default_value = "auto")]
        output: String,

        /// Nombre del NVM (se deduce del Nkrfile si no se especifica)
        #[arg(short, long, default_value = "")]
        name: String,

        /// Tamaño del disco en MB (default: 4096)
        #[arg(long, default_value_t = 4096)]
        size_mb: u32,

        /// Directorio de contexto (default: .)
        #[arg(long, default_value = ".")]
        context: String,

        /// No generar initramfs automáticamente
        #[arg(long, default_value_t = false)]
        no_initramfs: bool,
    },
}

/// Configuración de una micro-VM derivada de los argumentos CLI
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
}

pub fn parse() -> Cli {
    Cli::parse()
}
