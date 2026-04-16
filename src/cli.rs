// =============================================================================
// NKR CLI — Interfaz de línea de comandos
// =============================================================================

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "nkr",
    version = "1.3.0",
    about = "NKR — Nano-Kernel Runtime v1.3 Full Elastic Connection: Contenedores ultra-rápidos con micro-VMs",
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
        /// No requerido si se usa --rootfs
        #[arg(long)]
        disk: Vec<String>,

        /// Ruta al kernel nanolinux ELF (recomendado)
        #[arg(long, default_value = "nanolinux")]
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

        /// Compartir directorio del host con el guest via VirtIO-FS
        /// Formato: host_path:guest_mountpoint (e.g. /opt/filestore:/mnt/shared)
        #[arg(long)]
        share: Vec<String>,

        /// Directorio del host a montar como rootfs (/) via VirtIO-FS compartido.
        /// Reemplaza --disk: la VM arranca sin disco de bloque propio.
        /// Permite que 100+ VMs compartan el mismo código (ej. Odoo) con una
        /// sola copia en page cache del host.
        #[arg(long)]
        rootfs: Option<String>,

        /// Activar VirtIO-PMEM + DAX para el disco principal (Feature A).
        /// Requiere kernel guest con CONFIG_VIRTIO_PMEM=y y CONFIG_FS_DAX=y.
        /// Ahorra ~150-200 MB de RAM por instancia eliminando la duplicación
        /// de page cache entre host y guest.
        #[arg(long, default_value_t = false)]
        pmem: bool,

        /// Inflar el VirtIO-Balloon en MB: el guest devuelve esa RAM al host.
        /// Útil para recuperar memoria de instancias idle sin reiniciarlas.
        /// Ejemplo: --balloon-mb 300 recupera 300 MB de una VM de 700 MB.
        #[arg(long, default_value_t = 0)]
        balloon_mb: u32,

        /// Activar burst de CPU (Smart default v1.3: true).
        /// Permite usar ciclos de CPU ociosos en ráfagas cortas (kernel >= 5.15, cpu.max.burst).
        /// Poner en false para VMs que requieran latencia de CPU estrictamente predecible.
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        burst: bool,

        /// Cell ID (0 = legacy bridge nkr0, 1-254 = bridge nkr-br{N} aislado).
        /// Usado internamente por `nkr cell up` para propagar la célula al subproceso.
        #[arg(long, default_value_t = 0)]
        cell_id: u8,
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

    /// Mostrar métricas de recursos en tiempo real (CPU%, RAM real, red, disco)
    Stats {
        /// Filtrar por ID, hash o nombre de VM (muestra todas si se omite)
        #[arg(default_value = "")]
        filter: String,
    },

    /// Gestionar KSM (Kernel Same-page Merging) para ahorrar RAM entre VMs
    ///
    /// KSM fusiona páginas de memoria idénticas entre procesos (ej. múltiples
    /// instancias de Odoo comparten las mismas páginas de Python/librerías).
    /// Típicamente ahorra 20-40% de RAM en stacks multi-tenant Odoo.
    Ksm {
        /// Acción: on, off, status
        #[arg(default_value = "status")]
        action: String,
    },

    /// Iniciar servidor de métricas compatible con Prometheus (Feature 5)
    ///
    /// Expone /metrics en formato Prometheus text exposition 0.0.4.
    /// Ejemplo: curl http://localhost:9090/metrics
    /// Configurar Grafana → Prometheus → Add datasource → http://host:9090
    Serve {
        /// Puerto HTTP donde escuchar (default: 9090)
        #[arg(long, default_value_t = 9090)]
        port: u16,
    },

    /// Gestionar Células (stacks multi-VM con red aislada por cell)
    ///
    /// Una célula es un grupo de VMs (ej. 20 Odoo + PgBouncer + PG) con su
    /// propio bridge Linux, subnet (10.0.{cell_id}.0/24) y NAT. Permite correr
    /// múltiples versiones de Odoo (15, 17, 19) en paralelo sin conflictos.
    Cell {
        #[command(subcommand)]
        action: CellAction,
    },
}

#[derive(Subcommand)]
pub enum CellAction {
    /// Crear una nueva célula (registra nombre, asigna cell_id, crea cell.yml)
    Create {
        /// Nombre de la célula (ej. nazcatex, cafeteria)
        name: String,

        /// Versión de Odoo asociada (ej. 17.0, 15.0, 19.0)
        #[arg(long)]
        odoo_version: Option<String>,
    },

    /// Listar todas las células registradas
    Ls,

    /// Arrancar una célula (ejecuta compose up en su nkr-compose.yml)
    Up {
        /// Nombre de la célula
        name: String,

        /// Ejecutar en background (daemon mode)
        #[arg(short, long, default_value_t = false)]
        detach: bool,
    },

    /// Detener todas las VMs de una célula
    Down {
        /// Nombre de la célula
        name: String,
    },

    /// Ver VMs activas de una célula (o todas si se omite)
    Ps {
        /// Nombre de la célula (opcional)
        name: Option<String>,
    },

    /// Eliminar célula del registry (no borra datos persistidos)
    Destroy {
        /// Nombre de la célula
        name: String,
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
    /// Directorios compartidos vía VirtIO-FS: "host_path:guest_path"
    pub shares: Vec<String>,
    /// Directorio del host montado como rootfs (/) vía VirtIO-FS compartido.
    /// Si Some, la VM arranca sin disco de bloque (100+ VMs pueden compartir el mismo código).
    pub rootfs: Option<String>,
    /// Activar VirtIO-PMEM + DAX para el disco principal
    pub use_pmem: bool,
    /// MB a inflar en el VirtIO-Balloon (0 = balloon desactivado)
    pub balloon_mb: u32,
    /// Activar burst de CPU: permite usar ciclos ociosos del resto de VMs en ráfagas cortas.
    /// Smart default v1.3: true. Solo desactivar en VMs que requieran latencia predecible.
    pub burst: bool,
    /// Cell ID (0 = legacy nkr0, 1-254 = bridge nkr-br{N} aislado)
    pub cell_id: u8,
}

pub fn parse() -> Cli {
    Cli::parse()
}
