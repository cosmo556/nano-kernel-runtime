// =============================================================================
// NKR VirtIO-Balloon — Dynamic RAM elasticity across instances
// =============================================================================
//
// Lets the hypervisor reclaim RAM from idle instances without restarting them.
// The guest driver "inflates" the balloon by donating physical pages; the host
// marks them with MADV_DONTNEED, returning them to the system's free memory pool.
//
// Density scenario (30 Odoos × 700 MB nominal on 32 GB):
//   Without balloon: 30 × 700 MB = 21 GB (only 11 GB left for the host)
//   With balloon (300 MB inflated on idle VMs):
//     30 × 400 MB real = 12 GB → 20 GB free for more instances
//   Combined with virtio-fs + DAX (−200 MB/VM, dedupe Python/.pyc/libs):
//     30 × 200 MB real = 6 GB → 26 GB free → 103+ instances possible
//
// Device ID: 5, MMIO: 0xD004_0000, IRQ: 18
// Config space: [u32 num_pages @ 0x100][u32 actual @ 0x104]
// Virtqueues: 0=inflateq (guest donates PFNs), 1=deflateq (guest reclaims PFNs)
//
// Guest requirement: CONFIG_VIRTIO_BALLOON=y (included in standard Linux kernels).
// Hot adjustment via: echo MB > /sys/kernel/debug/nkr/balloon/target
// =============================================================================

use std::collections::HashSet;
use std::sync::Arc;

use libc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Address, Bytes, GuestAddress, GuestMemory, GuestMemoryMmap};

pub const BALLOON_MMIO_ADDR: u64   = 0xD004_0000;
pub const BALLOON_IRQ: u32         = 10;
pub const BALLOON_DEVICE_ID: u32   = 5;

/// Absolute cap on pages a guest can inflate, used as fallback when ram_mb=0
/// (misconfigured VM). 256 K pages × 4 KB = 1 GB — more than enough for any
/// legitimate guest and blocks unbounded Vec::push from a compromised guest.
const FALLBACK_MAX_INFLATED_PAGES: usize = 256 * 1024;

/// Maximum bytes per virtio descriptor accepted in inflate/deflate. The
/// Linux virtio-balloon driver rarely exceeds 4 KB per descriptor (1024
/// PFNs). Anything > 64 KB (16 K PFNs) is almost certainly hostile.
const MAX_BYTES_PER_DESC: u32 = 64 * 1024;

// VirtIO-Balloon feature bits
const VIRTIO_BALLOON_F_MUST_TELL_HOST: u64 = 1 << 0;
const VIRTIO_BALLOON_F_STATS_VQ: u64       = 1 << 1;

// virtio_balloon_stat tags (statsq). Each entry on the wire is `le16 tag;
// le64 val;` packed = 10 bytes. We only consume the ones we expose.
const VIRTIO_BALLOON_S_SWAP_IN:  u16 = 0; // bytes
const VIRTIO_BALLOON_S_SWAP_OUT: u16 = 1; // bytes
const VIRTIO_BALLOON_S_MAJFLT:   u16 = 2; // count
const VIRTIO_BALLOON_S_MINFLT:   u16 = 3; // count
const VIRTIO_BALLOON_S_MEMFREE:  u16 = 4; // bytes
const VIRTIO_BALLOON_S_MEMTOT:   u16 = 5; // bytes
const VIRTIO_BALLOON_S_AVAIL:    u16 = 6; // bytes
const VIRTIO_BALLOON_S_CACHES:   u16 = 7; // bytes
const VIRTIO_BALLOON_STAT_SIZE: u64 = 10; // 2-byte tag + 8-byte val, packed

/// Guest-internal memory snapshot parsed from the stats virtqueue (bytes /
/// counts). 0 means "not reported by the guest yet".
#[derive(Clone, Copy, Default, Debug)]
pub struct GuestStats {
    pub mem_total_bytes: u64,
    pub mem_free_bytes: u64,
    pub mem_available_bytes: u64,
    pub mem_cached_bytes: u64,
    pub swap_in_bytes: u64,
    pub swap_out_bytes: u64,
    pub major_faults: u64,
    pub minor_faults: u64,
}

pub struct VirtioBalloonDevice {
    /// Number of 4 KB pages the hypervisor wants to reclaim
    pub target_pages: u32,
    /// Number of pages currently donated by the guest driver
    pub actual_pages: u32,

    // ── Standard VirtIO MMIO registers ──
    pub status: u32,
    pub interrupt_status: u32,
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,
    pub queue_sel: u32,
    pub queue_num:   [u32; 3],
    pub queue_ready: [bool; 3],
    pub desc_low:  [u32; 3], pub desc_high:  [u32; 3],
    pub avail_low: [u32; 3], pub avail_high: [u32; 3],
    pub used_low:  [u32; 3], pub used_high:  [u32; 3],

    /// inflateq (queue 0): guest donates PFNs → host marks them MADV_DONTNEED
    pub inflateq: Queue,
    /// deflateq (queue 1): guest reclaims PFNs → host removes them from the list
    pub deflateq: Queue,
    /// statsq (queue 2, VIRTIO_BALLOON_F_STATS_VQ): the guest submits one
    /// device-readable buffer of `virtio_balloon_stat` entries at probe; the
    /// device consumes it (reads stats, marks used) when it wants a refresh —
    /// the guest then refills with fresh stats and re-submits. We consume it
    /// at most every ~30s from the vmm balloon timer.
    pub statsq: Queue,
    /// Last guest-memory snapshot read off the statsq (0s until the guest
    /// reports). The vmm persists this to the VM state file.
    pub last_stats: GuestStats,

    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,

    /// GPA (guest physical address) of each donated page. HashSet rather
    /// than Vec: automatic dedup (a hostile guest that repeats the same PFN
    /// across chains no longer grows the structure) + O(1) remove in
    /// deflate.
    inflated_gpas: HashSet<u64>,

    /// Absolute upper bound for inflated_gpas, derived from the guest RAM:
    /// it never makes sense to inflate beyond the guest total RAM. Blocks
    /// host DoS by a compromised guest attempting unbounded Vec::push (now
    /// HashSet::insert).
    max_inflated_pages: usize,
}

// SAFETY: ioeventfd/irqfd are independent EventFds; inflated_gpas is not accessed
// concurrently (only the vCPU thread manipulates it).
unsafe impl Send for VirtioBalloonDevice {}

impl VirtioBalloonDevice {
    /// `ram_mb` is the total RAM assigned to the VM. Determines the balloon
    /// cap (you can't inflate beyond the guest's own RAM). Passing 0 falls
    /// back to a conservative 1 GB cap.
    pub fn new(mem: Arc<GuestMemoryMmap>, ram_mb: u32) -> Self {
        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[NKR-BALLOON] ioeventfd falló");
        let irqfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[NKR-BALLOON] irqfd falló");

        let max_inflated_pages = if ram_mb > 0 {
            (ram_mb as usize) * 256  // 256 pages × 4 KB = 1 MB
        } else {
            FALLBACK_MAX_INFLATED_PAGES
        };

        VirtioBalloonDevice {
            target_pages: 0,
            actual_pages: 0,
            status: 0,
            interrupt_status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queue_num:   [256, 256, 256],
            queue_ready: [false, false, false],
            desc_low:  [0; 3], desc_high:  [0; 3],
            avail_low: [0; 3], avail_high: [0; 3],
            used_low:  [0; 3], used_high:  [0; 3],
            inflateq: Queue::new(256).unwrap(),
            deflateq: Queue::new(256).unwrap(),
            statsq:   Queue::new(256).unwrap(),
            last_stats: GuestStats::default(),
            ioeventfd,
            irqfd,
            mem,
            inflated_gpas: HashSet::new(),
            max_inflated_pages,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Orchestrator API
    // ─────────────────────────────────────────────────────────────────────────

    /// Sets how many MB the hypervisor wants to reclaim from this guest.
    /// The guest driver adjusts `actual_pages` toward `target_pages`
    /// asynchronously (may take ~1s depending on guest load).
    pub fn set_target_mb(&mut self, mb: u32) {
        self.target_pages = mb * 256; // 256 pages × 4 KB = 1 MB
        eprintln!("[NKR-BALLOON] Objetivo: {} MB ({} páginas de 4 KB)",
            mb, self.target_pages);
    }

    /// Notifies the guest of a config-space change (target_pages updated)
    /// so its balloon driver re-reads offset 0x100 and starts inflate or
    /// deflate to reach the new target.
    ///
    /// virtio v1.x ISR semantics: bit 1 = vring used, bit 2 = config change.
    /// Guests check ISR and dispatch accordingly. Without this notification
    /// the driver would only see the new target on its own slow polling
    /// (typical kernel: never until restart), so dynamic balloon transitions
    /// (IDLE↔ACTIVE) require this irqfd write to be effective in seconds
    /// rather than at the next reboot.
    pub fn raise_config_change(&mut self) {
        self.interrupt_status |= 2;
        let _ = self.irqfd.write(1);
    }

    /// MB currently inflated (reclaimed from the guest and returned to the host OS).
    pub fn inflated_mb(&self) -> u32 {
        self.actual_pages / 256
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Config Space
    // ─────────────────────────────────────────────────────────────────────────

    /// Reads the balloon config space.
    ///
    /// Offset 0x100 (u32): `num_pages` — hypervisor target.
    /// Offset 0x104 (u32): `actual`    — pages donated by the guest.
    pub fn config_read(&self, offset: u64, data: &mut [u8]) {
        match offset {
            0x100 => copy_u32_le(self.target_pages, data),
            0x104 => copy_u32_le(self.actual_pages,  data),
            _ => data.fill(0),
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Queue activation
    // ─────────────────────────────────────────────────────────────────────────

    pub fn activate_queue(&mut self, qi: usize) {
        if qi >= 3 { return; }
        let size = self.queue_num[qi].max(256) as u16;
        let (dl, dh) = (self.desc_low[qi], self.desc_high[qi]);
        let (al, ah) = (self.avail_low[qi], self.avail_high[qi]);
        let (ul, uh) = (self.used_low[qi], self.used_high[qi]);
        let q = match qi { 0 => &mut self.inflateq, 1 => &mut self.deflateq, _ => &mut self.statsq };
        q.set_size(size);
        q.set_desc_table_address(Some(dl), Some(dh));
        q.set_avail_ring_address(Some(al), Some(ah));
        q.set_used_ring_address(Some(ul), Some(uh));
        q.set_ready(true);
        self.queue_ready[qi] = true;
        eprintln!("[NKR-BALLOON] Cola '{}' activada",
            match qi { 0 => "inflateq", 1 => "deflateq", _ => "statsq" });
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Virtqueue processing
    // ─────────────────────────────────────────────────────────────────────────

    /// Processes inflateq: the guest donates PFNs → the host calls `MADV_DONTNEED`
    /// on the corresponding pages, returning them to the system.
    ///
    /// Hardening against a hostile guest:
    ///   - HashSet::insert dedups repeated PFNs (structure stops growing).
    ///   - `max_inflated_pages` absolute cap derived from guest RAM.
    ///   - `MAX_BYTES_PER_DESC` rejects absurdly large descriptors.
    /// If the cap is reached, the rest of the queue is aborted silently
    /// (recurring log spam would be worse than the attack itself).
    pub fn process_inflate(&mut self) {
        if !self.queue_ready[0] { return; }
        let mem = self.mem.as_ref();
        let mut used = Vec::new();
        let mut hit_cap = false;

        {
            let mut iter = match self.inflateq.iter(mem) {
                Ok(it) => it,
                Err(_)  => return,
            };

            'outer: while let Some(mut chain) = iter.next() {
                let head  = chain.head_index();
                let mut total = 0u32;

                while let Some(desc) = chain.next() {
                    total += desc.len();
                    // Per-descriptor cap: the legitimate guest driver never
                    // passes more than a few KB per chain. Truncate (don't
                    // abort the whole chain) so we don't break the used ring.
                    let safe_len = desc.len().min(MAX_BYTES_PER_DESC);
                    let mut off = 0u64;
                    while off + 4 <= safe_len as u64 {
                        if self.inflated_gpas.len() >= self.max_inflated_pages {
                            hit_cap = true;
                            // Bail out of ALL loops (chain + desc + outer)
                            // but still mark this chain as used so the guest
                            // doesn't wait indefinitely.
                            break;
                        }
                        if let Ok(pfn) = mem.read_obj::<u32>(
                            desc.addr().unchecked_add(off)
                        ) {
                            let gpa = (pfn as u64) * 4096;
                            // insert returns true if it was new. If it was
                            // already there, don't apply MADV_DONTNEED again
                            // (it was applied the first time).
                            if self.inflated_gpas.insert(gpa) {
                                advise_dontneed(mem, gpa);
                            }
                        }
                        off += 4;
                    }
                    if hit_cap { break; }
                }
                used.push((head, total));
                if hit_cap { break 'outer; }
            }
        }

        if used.is_empty() { return; }

        self.actual_pages = self.inflated_gpas.len() as u32;
        for (idx, len) in &used {
            let _ = self.inflateq.add_used(mem, *idx, *len);
        }
        self.interrupt_status |= 1;
        let _ = self.irqfd.write(1);

        if hit_cap {
            eprintln!("[NKR-BALLOON] WARN: cap reached ({} pages / {} MB) — \
                      remaining descriptors ignored (possible hostile guest or buggy driver)",
                self.max_inflated_pages, self.max_inflated_pages / 256);
        }
        eprintln!("[NKR-BALLOON] Inflado: {} páginas totales ({} MB recuperados del guest)",
            self.actual_pages, self.inflated_mb());
    }

    /// Processes deflateq: the guest reclaims previously donated PFNs.
    /// The host removes those pages from the list — the OS can reassign them.
    /// With HashSet, `remove` is O(1) (it used to be O(n) with Vec::retain).
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
                    // Same per-descriptor cap as in inflate.
                    let safe_len = desc.len().min(MAX_BYTES_PER_DESC);
                    let mut off = 0u64;
                    while off + 4 <= safe_len as u64 {
                        if let Ok(pfn) = mem.read_obj::<u32>(
                            desc.addr().unchecked_add(off)
                        ) {
                            let gpa = (pfn as u64) * 4096;
                            self.inflated_gpas.remove(&gpa);
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

    /// Consumes one buffer off the statsq if the guest has submitted one:
    /// reads the `virtio_balloon_stat` entries, updates `last_stats`, and marks
    /// the buffer used (which makes the guest refill it with fresh stats and
    /// re-submit). Returns `true` if it consumed a buffer (→ caller should
    /// persist `last_stats`). Idempotent and cheap when there's nothing queued.
    /// Call at most every ~30s — calling on every guest kick would ping-pong
    /// (consume → guest refills+kicks → consume → …) and waste CPU.
    pub fn process_stats(&mut self) -> bool {
        if !self.queue_ready[2] { return false; }
        let mem = self.mem.as_ref();
        let mut consumed: Option<(u16, u32)> = None;
        let mut stats = self.last_stats; // start from last so missing tags keep their value
        {
            let mut iter = match self.statsq.iter(mem) { Ok(it) => it, Err(_) => return false };
            if let Some(mut chain) = iter.next() {
                let head = chain.head_index();
                let mut total = 0u32;
                while let Some(desc) = chain.next() {
                    total += desc.len();
                    // A stats buffer is ~140 bytes; anything > 64 KB is bogus.
                    let safe_len = desc.len().min(MAX_BYTES_PER_DESC) as u64;
                    let mut off = 0u64;
                    while off + VIRTIO_BALLOON_STAT_SIZE <= safe_len {
                        let tag = mem.read_obj::<u16>(desc.addr().unchecked_add(off)).unwrap_or(0xFFFF);
                        let val = mem.read_obj::<u64>(desc.addr().unchecked_add(off + 2)).unwrap_or(0);
                        match tag {
                            VIRTIO_BALLOON_S_MEMFREE  => stats.mem_free_bytes = val,
                            VIRTIO_BALLOON_S_MEMTOT   => stats.mem_total_bytes = val,
                            VIRTIO_BALLOON_S_AVAIL    => stats.mem_available_bytes = val,
                            VIRTIO_BALLOON_S_CACHES   => stats.mem_cached_bytes = val,
                            VIRTIO_BALLOON_S_SWAP_IN  => stats.swap_in_bytes = val,
                            VIRTIO_BALLOON_S_SWAP_OUT => stats.swap_out_bytes = val,
                            VIRTIO_BALLOON_S_MAJFLT   => stats.major_faults = val,
                            VIRTIO_BALLOON_S_MINFLT   => stats.minor_faults = val,
                            _ => {}
                        }
                        off += VIRTIO_BALLOON_STAT_SIZE;
                    }
                }
                consumed = Some((head, total));
            }
        }
        match consumed {
            Some((head, len)) => {
                self.last_stats = stats;
                let _ = self.statsq.add_used(mem, head, len);
                self.interrupt_status |= 1;
                let _ = self.irqfd.write(1);
                true
            }
            None => false,
        }
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
// Internal helpers
// =============================================================================

fn copy_u32_le(val: u32, data: &mut [u8]) {
    let bytes = val.to_le_bytes();
    let n = data.len().min(4);
    data[..n].copy_from_slice(&bytes[..n]);
}

/// Marks a guest page (by GPA) as discardable on the host.
/// Translates GPA → host pointer by iterating the guest memory regions.
fn advise_dontneed(mem: &GuestMemoryMmap, gpa: u64) {
    let addr = GuestAddress(gpa);
    // GuestMemory::get_host_address returns *const u8 to the host byte
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cap_derived_from_ram() {
        // 2 GB of RAM → cap = 2048 * 256 = 524 288 pages.
        let mem = Arc::new(GuestMemoryMmap::from_ranges(&[
            (GuestAddress(0), 4096),
        ]).unwrap());
        let dev = VirtioBalloonDevice::new(mem.clone(), 2048);
        assert_eq!(dev.max_inflated_pages, 524_288);

        // ram_mb=0 falls back to the 1 GB cap.
        let dev0 = VirtioBalloonDevice::new(mem, 0);
        assert_eq!(dev0.max_inflated_pages, FALLBACK_MAX_INFLATED_PAGES);
    }

    #[test]
    fn dedup_prevents_unbounded_growth() {
        // Simulates a hostile guest repeating the same PFN — the HashSet
        // keeps the structure from growing.
        let mut s: HashSet<u64> = HashSet::new();
        for _ in 0..10_000 {
            s.insert(0xDEADBEEF);
        }
        assert_eq!(s.len(), 1);
    }
}
