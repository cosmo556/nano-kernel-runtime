// =============================================================================
// NKR VirtIO-PMEM — Persistent memory with direct DAX access
// =============================================================================
//
// Feature A: The VM's ext4 disk is mapped into host RAM with mmap(MAP_SHARED).
// It's registered as an additional KVM slot at guest physical address
// 0x1_0000_0000 (4 GB). The guest kernel (with CONFIG_VIRTIO_PMEM=y and
// CONFIG_FS_DAX=y) exposes it as /dev/pmem0 and mounts rootfs with "dax".
//
// Benefit: guest reads access the host's page cache directly (zero-copy),
// eliminating the duplication of ~150-200 MB of page cache per instance.
//
// MMIO: 0xD0020000, IRQ: 16, Device ID: 27
// Config space (offset 0x100): [u64 start][u64 size]
//
// Guest kernel requirements: CONFIG_VIRTIO_PMEM=y, CONFIG_FS_DAX=y
// Degradation: if the disk cannot be mmap'ed, NKR uses VirtIO-Block as fallback.
// =============================================================================

use std::fs::OpenOptions;
use std::os::unix::io::AsRawFd;
use std::sync::Arc;

use libc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

/// Guest physical address where the PMEM region appears (above the max 4 GB of RAM)
pub const PMEM_GUEST_PHYS_ADDR: u64 = 0x1_0000_0000; // 4 GB
pub const PMEM_MMIO_ADDR: u64 = 0xD002_0000;
pub const PMEM_IRQ: u32 = 7;
#[allow(dead_code)]
pub const PMEM_DEVICE_ID: u32 = 27;

pub struct VirtioPmemDevice {
    // Disk mmap in host address space
    pub host_mmap_ptr: *mut libc::c_void,
    pub host_mmap_len: usize,
    pub guest_phys_addr: u64,

    // Standard VirtIO MMIO registers (same pattern as block.rs)
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

// SAFETY: host_mmap_ptr is a static mmap(MAP_SHARED) for the device lifetime.
// Only the vCPU thread uses it, with no concurrent access.
unsafe impl Send for VirtioPmemDevice {}

impl VirtioPmemDevice {
    /// Maps the host disk into shared memory and configures the VirtIO-PMEM device.
    /// Returns Err if the mmap fails (e.g. disk on a filesystem without mmap support).
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

        // Map the disk as a shared memory region with the guest.
        //
        // MAP_SHARED: guest writes propagate to the disk file (correct).
        // MAP_NORESERVE: don't reserve swap for this mmap (the file is its backing store).
        //   Without MAP_NORESERVE, the kernel might reject the mmap if swap < disk size.
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

        // Memory management hints for lazy/reclaimable PMEM:
        //
        // MADV_NOHUGEPAGE: keep pages at 4KB granularity.
        //   Previously we used MADV_HUGEPAGE (THP) "to reduce TLB pressure",
        //   but with 50 VMs × 4GB mapped, THP COLLAPSES pages into 2MB blocks
        //   that the kernel CANNOT evict individually → RAM stays "hostage".
        //   With 4KB, each page can be reclaimed independently under pressure.
        //
        // MADV_COLD: marks pages as cold in the LRU from the start.
        //   The kernel prioritizes them for eviction over active anonymous pages.
        //   Result: under RAM pressure, the PMEM disk page cache is evicted
        //   first, freeing RAM for guest RAM (which is actually active).
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

    /// Reads PMEM config space registers.
    /// Offset 0x100: start (u64) — guest physical address
    /// Offset 0x108: size  (u64) — size in bytes
    pub fn config_read(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0x100 => {
                let v = self.guest_phys_addr.to_le_bytes();
                let n = data.len().min(8);
                data[..n].copy_from_slice(&v[..n]);
            }
            0x104 => {
                // Bytes 4-7 of start (u64)
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

    /// Feature bits exposed to the guest. VIRTIO_F_VERSION_1 (bit 32) is mandatory
    /// for virtio-mmio v2; ACCESS_PLATFORM (bit 33) is requested by the driver under KVM.
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

    /// Processes FLUSH requests from the guest (sync mmap to disk).
    /// The guest kernel sends VIRTIO_PMEM_REQ_TYPE_FLUSH to ensure persistence.
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

                // Read all descriptors (request + response)
                while let Some(desc) = chain.next() {
                    total_len += desc.len();
                    // Write VIRTIO_PMEM_RESP_TYPE_OK (0) to the response descriptor
                    // Format is: [u32 ret] — 0 = success
                    if desc.is_write_only() {
                        let _ = mem.write_obj(0u32, desc.addr());
                    }
                }

                // Execute msync to ensure the mmap is on disk
                unsafe {
                    libc::msync(
                        self.host_mmap_ptr,
                        self.host_mmap_len,
                        libc::MS_ASYNC, // async: don't block the vCPU
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
            // Sync pending changes before unmapping
            unsafe {
                libc::msync(self.host_mmap_ptr, self.host_mmap_len, libc::MS_SYNC);
                libc::munmap(self.host_mmap_ptr, self.host_mmap_len);
            }
            eprintln!("[NKR-PMEM] mmap liberado");
        }
    }
}
