// =============================================================================
// NKR VirtIO-PMEM — Memoria persistente con acceso DAX directo
// =============================================================================
//
// Feature A: El disco ext4 de la VM se mapea en la RAM del host con mmap(MAP_SHARED).
// Se registra como un slot KVM adicional en la dirección física 0x1_0000_0000 (4 GB)
// del guest. El kernel guest (con CONFIG_VIRTIO_PMEM=y y CONFIG_FS_DAX=y) lo
// expone como /dev/pmem0 y monta el rootfs con la opción "dax".
//
// Beneficio: las lecturas del guest acceden directamente a la page cache del host
// (zero-copy), eliminando la duplicación de ~150-200 MB de page cache por instancia.
//
// MMIO: 0xD0020000, IRQ: 16, Device ID: 27
// Config space (offset 0x100): [u64 start][u64 size]
//
// Requisitos del kernel guest: CONFIG_VIRTIO_PMEM=y, CONFIG_FS_DAX=y
// Degradación: si el disco no se puede mmap, NKR usa VirtIO-Block como fallback.
// =============================================================================

use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use libc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

/// Dirección física guest donde aparece la región PMEM (encima de los 4 GB de RAM máx)
pub const PMEM_GUEST_PHYS_ADDR: u64 = 0x1_0000_0000; // 4 GB
pub const PMEM_MMIO_ADDR: u64 = 0xD002_0000;
pub const PMEM_IRQ: u32 = 7;
#[allow(dead_code)]
pub const PMEM_DEVICE_ID: u32 = 27;

pub struct VirtioPmemDevice {
    // Mmap del disco en espacio del host
    pub host_mmap_ptr: *mut libc::c_void,
    pub host_mmap_len: usize,
    pub guest_phys_addr: u64,

    // Registros VirtIO MMIO estándar (mismo patrón que block.rs)
    pub status: u32,
    pub interrupt_status: u32,
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,
    pub queue_sel: u32,
    pub queue_num: u32,
    pub queue_ready: bool,
    pub desc_low: u32,
    pub desc_high: u32,
    pub avail_low: u32,
    pub avail_high: u32,
    pub used_low: u32,
    pub used_high: u32,

    pub queue: Queue,
    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,
}

// SAFETY: host_mmap_ptr es un mmap(MAP_SHARED) estático durante la vida del dispositivo.
// Solo lo usa el hilo del vCPU, sin acceso concurrente.
unsafe impl Send for VirtioPmemDevice {}

impl VirtioPmemDevice {
    /// Mapea el disco del host en memoria compartida y configura el dispositivo VirtIO-PMEM.
    /// Devuelve Err si el mmap falla (p.ej. disco en filesystem sin soporte de mmap).
    pub fn new(
        disk_path: &str,
        guest_phys_addr: u64,
        mem: Arc<GuestMemoryMmap>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)
            .map_err(|e| format!("PMEM: no se pudo abrir '{}': {}", disk_path, e))?;

        let len = file.metadata()
            .map_err(|e| format!("PMEM: metadata falló: {}", e))?
            .len() as usize;

        if len == 0 {
            return Err("PMEM: disco vacío".into());
        }

        // Mapear el disco como región de memoria compartida con el guest.
        //
        // MAP_SHARED: escrituras del guest se propagan al archivo del disco (correcto).
        // MAP_NORESERVE: no reservar swap para este mmap (el archivo ya es su backing store).
        //   Sin MAP_NORESERVE, el kernel podría rechazar el mmap si swap < tamaño disco.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_NORESERVE,
                file.as_raw_fd(),
                0,
            )
        };

        if ptr == libc::MAP_FAILED {
            return Err(format!(
                "PMEM: mmap falló para '{}': {}",
                disk_path,
                std::io::Error::last_os_error()
            ).into());
        }

        // Hints de gestión de memoria para lazy/reclaimable PMEM:
        //
        // MADV_NOHUGEPAGE: mantiene páginas en granularidad 4KB.
        //   Antes se usaba MADV_HUGEPAGE (THP) "para reducir presión en TLB",
        //   pero con 50 VMs × 4GB mapeados, el THP COLAPSA páginas en bloques de 2MB
        //   que el kernel NO puede desalojar individualmente → la RAM queda "secuestrada".
        //   Con 4KB, cada página puede reclamarse de forma independiente bajo presión.
        //
        // MADV_COLD: marca las páginas como frías en la LRU desde el inicio.
        //   El kernel las prioriza para desalojar antes que páginas anónimas activas.
        //   Resultado: bajo presión de RAM, el page cache del disco PMEM se descarga
        //   primero, liberando RAM para la RAM de los guests (que sí es activa).
        unsafe {
            libc::madvise(ptr, len, libc::MADV_NOHUGEPAGE);
            libc::madvise(ptr, len, libc::MADV_COLD);
        }

        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| format!("PMEM: ioeventfd falló: {}", e))?;
        let irqfd = EventFd::new(libc::EFD_NONBLOCK)
            .map_err(|e| format!("PMEM: irqfd falló: {}", e))?;
        let queue = Queue::new(16).unwrap();

        eprintln!("[NKR-PMEM] '{}' mapeado ({} MB) → guest phys {:#X}",
            disk_path, len >> 20, guest_phys_addr);

        Ok(VirtioPmemDevice {
            host_mmap_ptr: ptr,
            host_mmap_len: len,
            guest_phys_addr,
            status: 0,
            interrupt_status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queue_num: 0,
            queue_ready: false,
            desc_low: 0, desc_high: 0,
            avail_low: 0, avail_high: 0,
            used_low: 0, used_high: 0,
            queue,
            ioeventfd,
            irqfd,
            mem,
        })
    }

    /// Lee registros del config space PMEM.
    /// Offset 0x100: start (u64) — dirección física guest
    /// Offset 0x108: size  (u64) — tamaño en bytes
    pub fn config_read(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0x100 => {
                let v = self.guest_phys_addr.to_le_bytes();
                let n = data.len().min(8);
                data[..n].copy_from_slice(&v[..n]);
            }
            0x104 => {
                // Bytes 4-7 de start (u64)
                let v = self.guest_phys_addr.to_le_bytes();
                let n = data.len().min(4);
                data[..n].copy_from_slice(&v[4..4 + n]);
            }
            0x108 => {
                let v = (self.host_mmap_len as u64).to_le_bytes();
                let n = data.len().min(8);
                data[..n].copy_from_slice(&v[..n]);
            }
            0x10C => {
                let v = (self.host_mmap_len as u64).to_le_bytes();
                let n = data.len().min(4);
                data[..n].copy_from_slice(&v[4..4 + n]);
            }
            _ => data.fill(0),
        }
    }

    /// Feature bits expuestos al guest. VIRTIO_F_VERSION_1 (bit 32) es obligatorio
    /// para virtio-mmio v2; ACCESS_PLATFORM (bit 33) lo pide el driver bajo KVM.
    pub fn features_for_sel(&self, sel: u32) -> u32 {
        const VIRTIO_F_VERSION_1: u64 = 1 << 32;
        let feats = VIRTIO_F_VERSION_1;
        match sel {
            0 => (feats & 0xFFFF_FFFF) as u32,
            1 => (feats >> 32) as u32,
            _ => 0,
        }
    }

    pub fn activate_queue(&mut self) {
        self.queue.set_size(self.queue_num.max(16) as u16);
        self.queue.set_desc_table_address(Some(self.desc_low), Some(self.desc_high));
        self.queue.set_avail_ring_address(Some(self.avail_low), Some(self.avail_high));
        self.queue.set_used_ring_address(Some(self.used_low), Some(self.used_high));
        self.queue.set_ready(true);
        self.queue_ready = true;
        eprintln!("[NKR-PMEM] Cola activada");
    }

    /// Procesa requests de FLUSH del guest (sincronizar mmap al disco).
    /// El kernel guest envía VIRTIO_PMEM_REQ_TYPE_FLUSH para asegurar persistencia.
    pub fn process_queue(&mut self) {
        if !self.queue_ready { return; }

        let mem = self.mem.as_ref();
        let mut used_results = Vec::new();

        {
            let mut iter = match self.queue.iter(mem) {
                Ok(it) => it,
                Err(_) => return,
            };

            while let Some(mut chain) = iter.next() {
                let head_index = chain.head_index();
                let mut total_len = 0u32;

                // Leer todos los descriptores (request + response)
                while let Some(desc) = chain.next() {
                    total_len += desc.len();
                    // Escribir VIRTIO_PMEM_RESP_TYPE_OK (0) en el descriptor de respuesta
                    // El formato es: [u32 ret] — 0 = éxito
                    if desc.is_write_only() {
                        let _ = mem.write_obj(0u32, desc.addr());
                    }
                }

                // Ejecutar msync para garantizar que el mmap está en disco
                unsafe {
                    libc::msync(
                        self.host_mmap_ptr,
                        self.host_mmap_len,
                        libc::MS_ASYNC, // async: no bloquear el vCPU
                    );
                }

                used_results.push((head_index, total_len));
            }
        }

        if used_results.is_empty() { return; }

        for (idx, len) in &used_results {
            let _ = self.queue.add_used(mem, *idx, *len);
        }

        self.interrupt_status |= 1;
        let _ = self.irqfd.write(1);
    }
}

impl Drop for VirtioPmemDevice {
    fn drop(&mut self) {
        if !self.host_mmap_ptr.is_null() && self.host_mmap_ptr != libc::MAP_FAILED {
            // Sincronizar cambios pendientes antes de desmontar
            unsafe {
                libc::msync(self.host_mmap_ptr, self.host_mmap_len, libc::MS_SYNC);
                libc::munmap(self.host_mmap_ptr, self.host_mmap_len);
            }
            eprintln!("[NKR-PMEM] mmap liberado");
        }
    }
}
