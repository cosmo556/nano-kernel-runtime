// =============================================================================
// NKR VirtIO-Block — Async I/O with io_uring + sync fallback
// =============================================================================
//
// Feature B: if the host kernel supports io_uring (>= 5.1), read/write
// operations are submitted to the submission queue (SQ) without blocking the
// vCPU thread. Completions are drained in poll_completions(), called at the
// start of each vCPU loop iteration.
//
// Fallback: if io_uring is not available (self.ring.is_none()), the original
// synchronous implementation runs with seek+read/write.
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

    // VirtIO device state
    pub status: u32,
    pub interrupt_status: u32,

    // Feature selection
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,

    // Queue configuration
    pub queue_sel: u32,
    pub queue_num: u32,
    pub queue_ready: bool,

    // Vring addresses
    pub desc_low: u32,
    pub desc_high: u32,
    pub avail_low: u32,
    pub avail_high: u32,
    pub used_low: u32,
    pub used_high: u32,

    // Disk size in 512-byte sectors
    pub capacity_sectors: u64,

    // Feature B — io_uring: None if the kernel doesn't support it (silent degradation)
    ring: Option<IoUring>,
    // In-flight operations map: user_data → (head_index, data_len, status_guest_addr)
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

        // Try to create the io_uring ring; if it fails (kernel < 5.1) use None
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

    /// Activates the queue: configures vring addresses and marks as ready
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

    /// Device reset
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

    /// Enqueues I/O operations in the io_uring submission queue.
    /// Completions are processed in poll_completions().
    /// If io_uring is not available, runs the synchronous implementation.
    pub fn process_queue(&mut self) {
        if !self.queue_ready {
            return;
        }

        // If no ring is available, use the original synchronous implementation
        if self.ring.is_none() {
            self.process_queue_sync();
            return;
        }

        let mem = self.mem.as_ref();
        let file_fd = self.file.as_raw_fd();

        let mut submitted = 0u32;

        // Cap on inflight io_uring requests. The ring itself is sized at 128
        // (see `IoUring::new(128)` in `new`), so the legitimate steady-state
        // size of `pending` stays well below this. The cap blocks a guest
        // that floods the avail ring faster than completions drain — without
        // it, `pending` could grow unboundedly and exhaust host memory.
        const PENDING_CAP: usize = 1024;

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
                // Stop accepting new descriptors when pending is full. The
                // remaining chains stay in the avail ring; the next
                // process_queue() call (after poll_completions drains) picks
                // them up.
                if self.pending.len() >= PENDING_CAP {
                    break;
                }
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

                // Get host pointer for zero-copy DMA
                let host_ptr = match mem.get_host_address(data_desc.addr()) {
                    Ok(p) => p,
                    Err(_) => {
                        // Fallback: mark as completed with error if we can't get host addr
                        eprintln!("[NKR-BLOCK] WARN: get_host_address falló, saltando descriptor");
                        continue;
                    }
                };

                let ud = self.next_ud;
                self.next_ud += 1;
                self.pending.insert(ud, (head_index, data_desc.len(), status_desc.addr()));

                let entry = if request_type == 0 {
                    // READ: host → guest (Read from file into guest buffer)
                    opcode::Read::new(
                        types::Fd(file_fd),
                        host_ptr as *mut u8,
                        data_desc.len(),
                    )
                    .offset(offset)
                    .build()
                    .user_data(ud)
                } else {
                    // WRITE: guest → host
                    opcode::Write::new(
                        types::Fd(file_fd),
                        host_ptr as *const u8,
                        data_desc.len(),
                    )
                    .offset(offset)
                    .build()
                    .user_data(ud)
                };

                // If the SQ is full, submit the current batch before continuing
                if sq.is_full() {
                    drop(sq);
                    if let Some(r) = self.ring.as_mut() {
                        let _ = r.submit();
                    }
                    sq = unsafe { self.ring.as_mut().unwrap().submission_shared() };
                }

                unsafe {
                    if sq.push(&entry).is_err() {
                        // SQ full again: revert pending and do sync fallback
                        self.pending.remove(&ud);
                    } else {
                        submitted += 1;
                    }
                }
            }
        } // drop sq borrow

        // Submit all enqueued operations in a single syscall
        if submitted > 0 {
            if let Some(ring) = self.ring.as_mut() {
                let _ = ring.submit();
            }
        }
    }

    /// Drains the completion queue and notifies the guest for each completed operation.
    /// Call from run_vcpu_loop() at the start of each iteration.
    pub fn poll_completions(&mut self) {
        let ring = match self.ring.as_mut() {
            Some(r) => r,
            None => return,
        };

        let mem = self.mem.as_ref();
        let mut fired = false;

        // Sync CQ before iterating
        ring.completion().sync();

        for cqe in ring.completion() {
            if let Some((head_idx, data_len, status_addr)) = self.pending.remove(&cqe.user_data()) {
                // Write VIRTIO_BLK_S_OK (0) to the guest's status byte
                let _ = mem.write_obj(0u8, status_addr);
                // Notify the guest that the descriptor is used
                let _ = self.queue.add_used(mem, head_idx, data_len + 1);
                fired = true;
            }
        }

        if fired {
            self.interrupt_status |= 1; // VIRTIO_MMIO_INT_VRING
            let _ = self.irqfd.write(1);
        }
    }

    /// Original synchronous implementation (fallback when io_uring is not available).
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
                    0 => { // READ
                        let offset = sector * 512;
                        let mut buffer = vec![0u8; data_desc.len() as usize];
                        self.file.seek(SeekFrom::Start(offset)).unwrap();
                        let _ = self.file.read(&mut buffer).unwrap_or(0);
                        mem.write_slice(&buffer, data_desc.addr()).unwrap();
                        mem.write_obj(0u8, status_desc.addr()).unwrap();
                        data_desc.len() + 1
                    }
                    1 => { // WRITE
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
