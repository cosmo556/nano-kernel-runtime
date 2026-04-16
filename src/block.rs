// =============================================================================
// NKR VirtIO-Block — E/S asíncrona con io_uring + fallback síncrono
// =============================================================================
//
// Feature B: si el kernel del host soporta io_uring (>= 5.1), las operaciones
// de lectura/escritura se envían al submission queue (SQ) sin bloquear el hilo
// del vCPU. Las completions se drenan en poll_completions(), llamada al inicio
// de cada iteración del bucle del vCPU.
//
// Fallback: si io_uring no está disponible (self.ring.is_none()), se ejecuta
// la implementación síncrona original con seek+read/write.
// =============================================================================

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use io_uring::{IoUring, opcode, types};
use libc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Descriptor, Queue, QueueOwnedT, QueueT};
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap, GuestMemory};

pub struct VirtioBlockDevice {
    pub file: File,
    pub queue: Queue,
    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,

    // Estado del dispositivo VirtIO
    pub status: u32,
    pub interrupt_status: u32,

    // Selección de features
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,

    // Configuración de colas
    pub queue_sel: u32,
    pub queue_num: u32,
    pub queue_ready: bool,

    // Direcciones de los Vrings
    pub desc_low: u32,
    pub desc_high: u32,
    pub avail_low: u32,
    pub avail_high: u32,
    pub used_low: u32,
    pub used_high: u32,

    // Tamaño del disco en sectores de 512 bytes
    pub capacity_sectors: u64,

    // Feature B — io_uring: None si el kernel no lo soporta (degradación silenciosa)
    ring: Option<IoUring>,
    // Mapa de operaciones en vuelo: user_data → (head_index, data_len, status_guest_addr)
    pending: HashMap<u64, (u16, u32, GuestAddress)>,
    next_ud: u64,
}

impl VirtioBlockDevice {
    pub fn new(disk_path: &str, mem: Arc<GuestMemoryMmap>) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)
            .expect("[BLOCK] Error: No se pudo abrir el archivo del disco virtual");

        let capacity_sectors = file.metadata()
            .expect("[BLOCK] No se pudo leer metadata del disco")
            .len() / 512;

        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[BLOCK] Fallo al crear ioeventfd");
        let irqfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[BLOCK] Fallo al crear irqfd");
        let queue = Queue::new(256).unwrap();

        // Intentar crear el io_uring ring; si falla (kernel < 5.1) usar None
        let ring = match IoUring::new(128) {
            Ok(r) => {
                eprintln!("[NKR-BLOCK] io_uring activo (ring size=128)");
                Some(r)
            }
            Err(e) => {
                eprintln!("[NKR-BLOCK] io_uring no disponible ({e}), usando E/S síncrona");
                None
            }
        };

        VirtioBlockDevice {
            file,
            queue,
            ioeventfd,
            irqfd,
            mem,
            status: 0,
            interrupt_status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queue_num: 0,
            queue_ready: false,
            desc_low: 0,
            desc_high: 0,
            avail_low: 0,
            avail_high: 0,
            used_low: 0,
            used_high: 0,
            capacity_sectors,
            ring,
            pending: HashMap::new(),
            next_ud: 0,
        }
    }

    /// Activa la cola: configura direcciones de los vrings y marca como ready
    pub fn activate_queue(&mut self) {
        self.queue.set_size(self.queue_num as u16);
        self.queue.set_desc_table_address(Some(self.desc_low), Some(self.desc_high));
        self.queue.set_avail_ring_address(Some(self.avail_low), Some(self.avail_high));
        self.queue.set_used_ring_address(Some(self.used_low), Some(self.used_high));
        self.queue.set_ready(true);
        self.queue_ready = true;

        eprintln!("[NKR-BLOCK] Vrings configurados! Desc: {:#X}_{:08X}, Avail: {:#X}_{:08X}, Used: {:#X}_{:08X}, Size: {}",
            self.desc_high, self.desc_low,
            self.avail_high, self.avail_low,
            self.used_high, self.used_low,
            self.queue_num);
    }

    /// Reset del dispositivo
    pub fn reset(&mut self) {
        self.status = 0;
        self.interrupt_status = 0;
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.queue_num = 0;
        self.queue_ready = false;
        self.desc_low = 0; self.desc_high = 0;
        self.avail_low = 0; self.avail_high = 0;
        self.used_low = 0; self.used_high = 0;
        self.queue = Queue::new(256).unwrap();
        self.pending.clear();
        eprintln!("[NKR-BLOCK] Dispositivo reseteado");
    }

    /// Encola operaciones de I/O en el submission queue de io_uring.
    /// Las completions se procesan en poll_completions().
    /// Si io_uring no está disponible, ejecuta la implementación síncrona.
    pub fn process_queue(&mut self) {
        if !self.queue_ready {
            return;
        }

        // Si no hay ring disponible, usar implementación síncrona original
        if self.ring.is_none() {
            self.process_queue_sync();
            return;
        }

        let mem = self.mem.as_ref();
        let file_fd = self.file.as_raw_fd();

        let mut submitted = 0u32;

        {
            let ring = self.ring.as_mut().unwrap();
            let mut sq = unsafe { ring.submission_shared() };

            let mut iter = match self.queue.iter(mem) {
                Ok(it) => it,
                Err(e) => {
                    eprintln!("[NKR-BLOCK] Error al crear iterador de cola: {e}");
                    return;
                }
            };

            while let Some(mut chain) = iter.next() {
                let head_index = chain.head_index();

                // 1. Header (16 bytes: type + reserved + sector)
                let head: Descriptor = match chain.next() {
                    Some(d) => d,
                    None => continue,
                };
                let mut header_data = [0u8; 16];
                if mem.read_slice(&mut header_data, head.addr()).is_err() {
                    continue;
                }
                let request_type = u32::from_le_bytes(header_data[0..4].try_into().unwrap());
                let sector = u64::from_le_bytes(header_data[8..16].try_into().unwrap());
                let offset = sector * 512;

                // 2. Data descriptor
                let data_desc: Descriptor = match chain.next() {
                    Some(d) => d,
                    None => continue,
                };
                // 3. Status descriptor
                let status_desc: Descriptor = match chain.next() {
                    Some(d) => d,
                    None => continue,
                };

                // Obtener puntero del host para la DMA sin copia
                let host_ptr = match mem.get_host_address(data_desc.addr()) {
                    Ok(p) => p,
                    Err(_) => {
                        // Fallback: marcar como completado con error si no podemos obtener host addr
                        eprintln!("[NKR-BLOCK] WARN: get_host_address falló, saltando descriptor");
                        continue;
                    }
                };

                let ud = self.next_ud;
                self.next_ud += 1;
                self.pending.insert(ud, (head_index, data_desc.len(), status_desc.addr()));

                let entry = if request_type == 0 {
                    // LECTURA: host → guest (Read desde fichero al buffer del guest)
                    opcode::Read::new(
                        types::Fd(file_fd),
                        host_ptr as *mut u8,
                        data_desc.len(),
                    )
                    .offset(offset)
                    .build()
                    .user_data(ud)
                } else {
                    // ESCRITURA: guest → host
                    opcode::Write::new(
                        types::Fd(file_fd),
                        host_ptr as *const u8,
                        data_desc.len(),
                    )
                    .offset(offset)
                    .build()
                    .user_data(ud)
                };

                // Si el SQ está lleno, submitear el lote actual antes de seguir
                if sq.is_full() {
                    drop(sq);
                    if let Some(r) = self.ring.as_mut() {
                        let _ = r.submit();
                    }
                    sq = unsafe { self.ring.as_mut().unwrap().submission_shared() };
                }

                unsafe {
                    if sq.push(&entry).is_err() {
                        // SQ lleno de nuevo: revertir pending y hacer sync fallback
                        self.pending.remove(&ud);
                    } else {
                        submitted += 1;
                    }
                }
            }
        } // drop sq borrow

        // Submitear todas las operaciones encoladas en un solo syscall
        if submitted > 0 {
            if let Some(ring) = self.ring.as_mut() {
                let _ = ring.submit();
            }
        }
    }

    /// Drena el completion queue y notifica al guest por cada operación completada.
    /// Llamar desde run_vcpu_loop() al inicio de cada iteración.
    pub fn poll_completions(&mut self) {
        let ring = match self.ring.as_mut() {
            Some(r) => r,
            None => return,
        };

        let mem = self.mem.as_ref();
        let mut fired = false;

        // Sincronizar CQ antes de iterar
        ring.completion().sync();

        for cqe in ring.completion() {
            if let Some((head_idx, data_len, status_addr)) = self.pending.remove(&cqe.user_data()) {
                // Escribir VIRTIO_BLK_S_OK (0) en el byte de status del guest
                let _ = mem.write_obj(0u8, status_addr);
                // Notificar al guest que el descriptor está usado
                let _ = self.queue.add_used(mem, head_idx, data_len + 1);
                fired = true;
            }
        }

        if fired {
            self.interrupt_status |= 1; // VIRTIO_MMIO_INT_VRING
            let _ = self.irqfd.write(1);
        }
    }

    /// Implementación síncrona original (fallback cuando io_uring no está disponible).
    fn process_queue_sync(&mut self) {
        let mem = self.mem.as_ref();
        let mut used_results = Vec::new();

        {
            let mut iter = match self.queue.iter(mem) {
                Ok(it) => it,
                Err(e) => {
                    eprintln!("[NKR-BLOCK] Error al crear iterador de cola: {e}");
                    return;
                }
            };

            while let Some(mut chain) = iter.next() {
                let head_index = chain.head_index();

                let head: Descriptor = chain.next().expect("Fallo al leer header");
                let mut header_data = [0u8; 16];
                mem.read_slice(&mut header_data, head.addr()).unwrap();

                let request_type = u32::from_le_bytes(header_data[0..4].try_into().unwrap());
                let sector = u64::from_le_bytes(header_data[8..16].try_into().unwrap());

                let data_desc: Descriptor = chain.next().expect("Fallo al leer data");
                let status_desc: Descriptor = chain.next().expect("Fallo al leer status");

                let len_written = match request_type {
                    0 => { // LECTURA
                        let offset = sector * 512;
                        let mut buffer = vec![0u8; data_desc.len() as usize];
                        self.file.seek(SeekFrom::Start(offset)).unwrap();
                        let _ = self.file.read(&mut buffer).unwrap_or(0);
                        mem.write_slice(&buffer, data_desc.addr()).unwrap();
                        mem.write_obj(0u8, status_desc.addr()).unwrap();
                        data_desc.len() + 1
                    }
                    1 => { // ESCRITURA
                        let offset = sector * 512;
                        let mut buffer = vec![0u8; data_desc.len() as usize];
                        mem.read_slice(&mut buffer, data_desc.addr()).unwrap();
                        self.file.seek(SeekFrom::Start(offset)).unwrap();
                        self.file.write_all(&buffer).unwrap();
                        mem.write_obj(0u8, status_desc.addr()).unwrap();
                        1
                    }
                    _ => {
                        mem.write_obj(0u8, status_desc.addr()).unwrap();
                        1
                    }
                };

                used_results.push((head_index, len_written));
            }
        }

        if used_results.is_empty() {
            return;
        }

        for (idx, len) in &used_results {
            self.queue.add_used(mem, *idx, *len).unwrap();
        }

        self.interrupt_status |= 1;
        self.irqfd.write(1).expect("Fallo al inyectar IRQ");
    }
}
