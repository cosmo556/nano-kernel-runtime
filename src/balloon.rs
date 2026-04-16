// =============================================================================
// NKR VirtIO-Balloon — Elasticidad de RAM dinámica entre instancias
// =============================================================================
//
// Permite al hipervisor recuperar RAM de instancias inactivas sin reiniciarlas.
// El guest driver "infla" el balloon donando páginas físicas; el host las marca
// con MADV_DONTNEED, devolviéndolas al pool de memoria libre del sistema.
//
// Escenario de densidad (30 Odoos × 700 MB nominal en 32 GB):
//   Sin balloon: 30 × 700 MB = 21 GB (solo quedan 11 GB para el host)
//   Con balloon (300 MB inflado en VMs idle):
//     30 × 400 MB real = 12 GB → 20 GB libres para más instancias
//   Combinado con PMEM+DAX (−200 MB/VM) y KSM (−100 MB/VM):
//     30 × 200 MB real = 6 GB → 26 GB libres → 103+ instancias posibles
//
// Device ID: 5, MMIO: 0xD004_0000, IRQ: 18
// Config space: [u32 num_pages @ 0x100][u32 actual @ 0x104]
// Virtqueues: 0=inflateq (guest dona PFNs), 1=deflateq (guest reclama PFNs)
//
// Requisito guest: CONFIG_VIRTIO_BALLOON=y (incluido en kernels estándar Linux).
// Ajuste en caliente vía: echo MB > /sys/kernel/debug/nkr/balloon/target
// =============================================================================

use std::sync::Arc;

use libc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

pub const BALLOON_MMIO_ADDR: u64   = 0xD004_0000;
pub const BALLOON_IRQ: u32         = 10;
pub const BALLOON_DEVICE_ID: u32   = 5;

// Feature bits de VirtIO-Balloon
const VIRTIO_BALLOON_F_MUST_TELL_HOST: u64 = 1 << 0;
const VIRTIO_BALLOON_F_STATS_VQ: u64       = 1 << 1;

pub struct VirtioBalloonDevice {
    /// Número de páginas de 4 KB que el hipervisor quiere recuperar
    pub target_pages: u32,
    /// Número de páginas actualmente donadas por el guest driver
    pub actual_pages: u32,

    // ── Registros VirtIO MMIO estándar ──
    pub status: u32,
    pub interrupt_status: u32,
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,
    pub queue_sel: u32,
    pub queue_num:   [u32; 2],
    pub queue_ready: [bool; 2],
    pub desc_low:  [u32; 2], pub desc_high:  [u32; 2],
    pub avail_low: [u32; 2], pub avail_high: [u32; 2],
    pub used_low:  [u32; 2], pub used_high:  [u32; 2],

    /// inflateq (cola 0): el guest dona PFNs → host los marca MADV_DONTNEED
    pub inflateq: Queue,
    /// deflateq (cola 1): el guest reclama PFNs → host los elimina de la lista
    pub deflateq: Queue,

    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,

    /// GPA (dirección física del guest) de cada página donada
    inflated_gpas: Vec<u64>,
}

// SAFETY: ioeventfd/irqfd son EventFd independientes; inflated_gpas no se accede
// concurrentemente (sólo el hilo del vCPU la manipula).
unsafe impl Send for VirtioBalloonDevice {}

impl VirtioBalloonDevice {
    pub fn new(mem: Arc<GuestMemoryMmap>) -> Self {
        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[NKR-BALLOON] ioeventfd falló");
        let irqfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[NKR-BALLOON] irqfd falló");

        VirtioBalloonDevice {
            target_pages: 0,
            actual_pages: 0,
            status: 0,
            interrupt_status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queue_num:   [256, 256],
            queue_ready: [false, false],
            desc_low:  [0; 2], desc_high:  [0; 2],
            avail_low: [0; 2], avail_high: [0; 2],
            used_low:  [0; 2], used_high:  [0; 2],
            inflateq: Queue::new(256).unwrap(),
            deflateq: Queue::new(256).unwrap(),
            ioeventfd,
            irqfd,
            mem,
            inflated_gpas: Vec::new(),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // API del Orquestador
    // ─────────────────────────────────────────────────────────────────────────

    /// Establece cuántos MB quiere recuperar el hipervisor de este guest.
    /// El driver del guest ajusta `actual_pages` hacia `target_pages` de forma
    /// asíncrona (puede tardar ~1s según la carga del guest).
    pub fn set_target_mb(&mut self, mb: u32) {
        self.target_pages = mb * 256; // 256 páginas × 4 KB = 1 MB
        eprintln!("[NKR-BALLOON] Objetivo: {} MB ({} páginas de 4 KB)",
            mb, self.target_pages);
    }

    /// MB actualmente inflados (recuperados del guest y devueltos al SO del host).
    pub fn inflated_mb(&self) -> u32 {
        self.actual_pages / 256
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Config Space
    // ─────────────────────────────────────────────────────────────────────────

    /// Lee el config space del balloon.
    ///
    /// Offset 0x100 (u32): `num_pages` — objetivo del hipervisor.
    /// Offset 0x104 (u32): `actual`    — páginas donadas por el guest.
    pub fn config_read(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0x100 => copy_u32_le(self.target_pages, data),
            0x104 => copy_u32_le(self.actual_pages,  data),
            _ => data.fill(0),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Activación de colas
    // ─────────────────────────────────────────────────────────────────────────

    pub fn activate_queue(&mut self, qi: usize) {
        if qi >= 2 { return; }
        let q = if qi == 0 { &mut self.inflateq } else { &mut self.deflateq };
        q.set_size(self.queue_num[qi].max(256) as u16);
        q.set_desc_table_address(Some(self.desc_low[qi]), Some(self.desc_high[qi]));
        q.set_avail_ring_address(Some(self.avail_low[qi]), Some(self.avail_high[qi]));
        q.set_used_ring_address(Some(self.used_low[qi]), Some(self.used_high[qi]));
        q.set_ready(true);
        self.queue_ready[qi] = true;
        eprintln!("[NKR-BALLOON] Cola '{}' activada",
            if qi == 0 { "inflateq" } else { "deflateq" });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Procesamiento de virtqueues
    // ─────────────────────────────────────────────────────────────────────────

    /// Procesa la inflateq: el guest dona PFNs → el host llama `MADV_DONTNEED`
    /// sobre las páginas correspondientes, devolviéndolas al sistema.
    pub fn process_inflate(&mut self) {
        if !self.queue_ready[0] { return; }
        let mem = self.mem.as_ref();
        let mut used = Vec::new();

        {
            let mut iter = match self.inflateq.iter(mem) {
                Ok(it) => it,
                Err(_)  => return,
            };

            while let Some(mut chain) = iter.next() {
                let head  = chain.head_index();
                let mut total = 0u32;

                while let Some(desc) = chain.next() {
                    total += desc.len();
                    // Cada descriptor contiene un array de u32 PFNs.
                    // Página guest = pfn × 4096 bytes.
                    let mut off = 0u64;
                    while off + 4 <= desc.len() as u64 {
                        if let Ok(pfn) = mem.read_obj::<u32>(
                            desc.addr().unchecked_add(off)
                        ) {
                            let gpa = (pfn as u64) * 4096;
                            self.inflated_gpas.push(gpa);
                            advise_dontneed(mem, gpa);
                        }
                        off += 4;
                    }
                }
                used.push((head, total));
            }
        }

        if used.is_empty() { return; }

        self.actual_pages = self.inflated_gpas.len() as u32;
        for (idx, len) in &used {
            let _ = self.inflateq.add_used(mem, *idx, *len);
        }
        self.interrupt_status |= 1;
        let _ = self.irqfd.write(1);

        eprintln!("[NKR-BALLOON] Inflado: {} páginas totales ({} MB recuperados del guest)",
            self.actual_pages, self.inflated_mb());
    }

    /// Procesa la deflateq: el guest reclama PFNs previamente donados.
    /// El host elimina esas páginas de la lista — el OS puede reasignarlas.
    pub fn process_deflate(&mut self) {
        if !self.queue_ready[1] { return; }
        let mem = self.mem.as_ref();
        let mut used = Vec::new();

        {
            let mut iter = match self.deflateq.iter(mem) {
                Ok(it) => it,
                Err(_)  => return,
            };

            while let Some(mut chain) = iter.next() {
                let head  = chain.head_index();
                let mut total = 0u32;

                while let Some(desc) = chain.next() {
                    total += desc.len();
                    let mut off = 0u64;
                    while off + 4 <= desc.len() as u64 {
                        if let Ok(pfn) = mem.read_obj::<u32>(
                            desc.addr().unchecked_add(off)
                        ) {
                            let gpa = (pfn as u64) * 4096;
                            self.inflated_gpas.retain(|&p| p != gpa);
                        }
                        off += 4;
                    }
                }
                used.push((head, total));
            }
        }

        if used.is_empty() { return; }

        self.actual_pages = self.inflated_gpas.len() as u32;
        for (idx, len) in &used {
            let _ = self.deflateq.add_used(mem, *idx, *len);
        }
        self.interrupt_status |= 1;
        let _ = self.irqfd.write(1);

        eprintln!("[NKR-BALLOON] Desinflado: {} páginas restantes ({} MB en balloon)",
            self.actual_pages, self.inflated_mb());
    }

    pub fn features_for_sel(&self, sel: u32) -> u32 {
        match sel {
            0 => (VIRTIO_BALLOON_F_MUST_TELL_HOST | VIRTIO_BALLOON_F_STATS_VQ) as u32,
            1 => 1u32, // VIRTIO_F_VERSION_1
            _ => 0,
        }
    }
}

// =============================================================================
// Helpers internos
// =============================================================================

fn copy_u32_le(val: u32, data: &mut [u8]) {
    let bytes = val.to_le_bytes();
    let n = data.len().min(4);
    data[..n].copy_from_slice(&bytes[..n]);
}

/// Marca una página guest (por GPA) como descartable en el host.
/// Traduce GPA → puntero del host iterando las regiones de memoria del guest.
fn advise_dontneed(mem: &GuestMemoryMmap, gpa: u64) {
    let addr = GuestAddress(gpa);
    // GuestMemory::get_host_address devuelve *const u8 al byte del host
    if let Ok(host_ptr) = mem.get_host_address(addr) {
        unsafe {
            libc::madvise(
                host_ptr as *mut libc::c_void,
                4096,
                libc::MADV_DONTNEED,
            );
        }
    }
}
