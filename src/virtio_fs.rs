// =============================================================================
// NKR VirtIO-FS — vhost-user frontend + auto-spawn virtiofsd
// =============================================================================
//
// VirtIO-FS (Device ID 26) shares host directories with the guest using
// virtiofsd as the FUSE backend. Advantages over VirtIO-9P:
//
//  • Full POSIX semantics (fcntl, flock, O_DIRECT)
//  • DAX: guest maps files directly from the host's page cache
//  • Throughput 3-5× higher for Python/Odoo module loading
//
// Flow:
//   1. NKR launches virtiofsd as a subprocess (--socket-path /run/nkrfs/<tag>.sock)
//   2. NKR connects to the socket and performs the vhost-user handshake
//   3. When the guest writes DRIVER_OK (status=15), NKR sends:
//      SET_MEM_TABLE → virtqueues become visible to virtiofsd
//      SET_VRING_* for queue 0 (hiprio) and queue 1 (request)
//      After that virtiofsd handles FUSE directly without going through NKR
//
// Guest kernel: CONFIG_VIRTIO_FS=y + CONFIG_FUSE_DAX=y
// Cmdline: virtio_mmio.device=4K@0xd0010000:8 nkr.fs0=nkrfs0 nkr.fsm0=/mnt
// =============================================================================

use std::io::{self, Read, Write as IoWrite};
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::time::Duration;

use libc;
use vmm_sys_util::eventfd::EventFd;
use vm_memory::GuestMemoryMmap;

// ── Device constants ──────────────────────────────────────────────────────────

pub const VIRTIO_FS_DEVICE_ID: u32 = 26;
pub const VIRTIO_FS_BASE_IRQ: u32  = 8;

pub const VIRTIO_FS_DAX_GUEST_PHYS: u64 = 0x2_0000_0000;
#[allow(dead_code)]
pub const VIRTIO_FS_DAX_SIZE: u64       = 4 * 1024 * 1024 * 1024;

const VIRTIOFSD_SOCK_DIR: &str = "/run/nkrfs";

// ── vhost-user constants ──────────────────────────────────────────────────────

const VHOST_USER_SET_OWNER: u32             = 3;
const VHOST_USER_GET_FEATURES: u32          = 1;
const VHOST_USER_SET_FEATURES: u32          = 2;
const VHOST_USER_GET_PROTOCOL_FEATURES: u32 = 15;
const VHOST_USER_SET_PROTOCOL_FEATURES: u32 = 16;
const VHOST_USER_GET_QUEUE_NUM: u32         = 17;
const VHOST_USER_SET_MEM_TABLE: u32         = 5;
const VHOST_USER_SET_VRING_NUM: u32         = 8;
const VHOST_USER_SET_VRING_ADDR: u32        = 9;
const VHOST_USER_SET_VRING_BASE: u32        = 10;
const VHOST_USER_SET_VRING_KICK: u32        = 12;
const VHOST_USER_SET_VRING_CALL: u32        = 13;
const VHOST_USER_SET_VRING_ENABLE: u32      = 18;

const VHOST_USER_VERSION: u32               = 0x0001;
#[allow(dead_code)]
const VHOST_USER_REPLY_MASK: u32            = 0x0004;
const VHOST_USER_NEED_REPLY_MASK: u32       = 0x0008;

// VHOST_USER_F_PROTOCOL_FEATURES bit 30
const VHOST_USER_F_PROTOCOL_FEATURES: u64  = 1 << 30;

// ── vhost-user wire structs ───────────────────────────────────────────────────

#[repr(C)]
struct VhostUserMsgHdr {
    request: u32,
    flags: u32,
    size: u32,
}

#[repr(C)]
struct VhostUserVringState {
    index: u32,
    num: u32,
}

#[repr(C)]
struct VhostUserVringAddr {
    index: u32,
    flags: u32,
    desc_user_addr: u64,
    used_user_addr: u64,
    avail_user_addr: u64,
    log_guest_addr: u64,
}

#[repr(C)]
struct VhostUserMemoryRegion {
    guest_phys_addr: u64,
    memory_size: u64,
    userspace_addr: u64,
    mmap_offset: u64,
}

// ── Per-queue state ───────────────────────────────────────────────────────────

#[derive(Default, Clone)]
pub struct QueueState {
    pub num: u32,
    pub desc_low: u32,
    pub desc_high: u32,
    pub avail_low: u32,
    pub avail_high: u32,
    pub used_low: u32,
    pub used_high: u32,
    pub ready: bool,
}

impl QueueState {
    pub fn desc_addr(&self) -> u64 {
        (self.desc_high as u64) << 32 | self.desc_low as u64
    }
    pub fn avail_addr(&self) -> u64 {
        (self.avail_high as u64) << 32 | self.avail_low as u64
    }
    pub fn used_addr(&self) -> u64 {
        (self.used_high as u64) << 32 | self.used_low as u64
    }
}

// ── Guest memory region (for SET_MEM_TABLE) ───────────────────────────────────

pub struct GuestMemRegion {
    pub gpa: u64,
    pub size: usize,
    pub hva: u64,
    pub memfd_offset: u64,
}

// ── VirtIO-FS device ──────────────────────────────────────────────────────────

pub struct VirtioFsDevice {
    pub tag: String,
    pub mmio_addr: u64,
    /// Path absoluto al socket UDS que abrió virtiofsd. Lo guardamos para
    /// poder borrarlo (junto con `<sock>.pid`) en `Drop`. Sin esto, los
    /// archivos quedan huérfanos en /run/nkrfs/ y el siguiente virtiofsd
    /// que arranca con el mismo tag falla con "Resource temporarily
    /// unavailable" al intentar crear el .pid.
    sock_path: String,
    /// Unix socket connected to virtiofsd (None if not available)
    vhost_sock: Option<UnixStream>,
    /// Child virtiofsd process
    virtiofsd: Option<std::process::Child>,
    /// memfd for SET_MEM_TABLE (raw fd, duplicated from vmm.rs)
    memfd: RawFd,
    /// Guest memory regions for SET_MEM_TABLE
    mem_regions: Vec<GuestMemRegion>,

    // ── DAX window ──
    pub dax_enabled: bool,
    pub dax_ptr: *mut libc::c_void,
    pub dax_size: usize,
    pub dax_guest_phys: u64,

    // ── VirtIO MMIO registers ──
    pub status: u32,
    pub interrupt_status: u32,
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,
    pub queue_sel: u32,
    pub shm_sel: u32,

    // ── Per-queue state (0=hiprio, 1=request) ──
    pub queues: [QueueState; 2],

    // ── Eventfds ──
    /// kick[0]=hiprio queue (datamatch=0), kick[1]=request queue (datamatch=1)
    pub kicks: [EventFd; 2],
    /// call: shared irq for both queues
    pub call: EventFd,

    #[allow(dead_code)]
    pub mem: Arc<GuestMemoryMmap>,
    pub queues_setup_done: bool,
}

unsafe impl Send for VirtioFsDevice {}

impl VirtioFsDevice {
    pub fn new(
        tag: &str,
        host_path: &str,
        mem: Arc<GuestMemoryMmap>,
        memfd: RawFd,
        mem_regions: Vec<GuestMemRegion>,
        cache_policy: &str,
        dax_size_bytes: u64,
        writeback: bool,
    ) -> Self {
        let kick0 = EventFd::new(libc::EFD_NONBLOCK).expect("[NKR-FS] kick0 eventfd falló");
        let kick1 = EventFd::new(libc::EFD_NONBLOCK).expect("[NKR-FS] kick1 eventfd falló");
        let call  = EventFd::new(libc::EFD_NONBLOCK).expect("[NKR-FS] call eventfd falló");

        // Allocate DAX window
        let (dax_ptr, dax_size, dax_enabled) = Self::alloc_dax_window(dax_size_bytes as usize);

        // Launch virtiofsd and connect
        let sock_path = format!("{}/{}.sock", VIRTIOFSD_SOCK_DIR, tag);
        let (virtiofsd, vhost_sock) = Self::spawn_and_connect(host_path, &sock_path, cache_policy, writeback);

        VirtioFsDevice {
            tag: tag.to_string(),
            mmio_addr: 0,
            sock_path: sock_path.clone(),
            vhost_sock,
            virtiofsd,
            memfd,
            mem_regions,
            dax_enabled,
            dax_ptr,
            dax_size,
            dax_guest_phys: 0,
            status: 0,
            interrupt_status: 0,
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            shm_sel: 0,
            queues: [QueueState::default(), QueueState::default()],
            kicks: [kick0, kick1],
            call,
            mem,
            queues_setup_done: false,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.vhost_sock.is_some()
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Config Space
    // ─────────────────────────────────────────────────────────────────────────

    pub fn config_read(&self, offset: u64, data: &mut [u8]) {
        match offset {
            off @ 0x100..=0x123 => {
                let idx = (off - 0x100) as usize;
                let tag_bytes = self.tag.as_bytes();
                for (i, byte) in data.iter_mut().enumerate() {
                    *byte = tag_bytes.get(idx + i).copied().unwrap_or(0);
                }
            }
            0x124 => {
                let v = 1u32.to_le_bytes();
                let n = data.len().min(4);
                data[..n].copy_from_slice(&v[..n]);
            }
            _ => data.fill(0),
        }
    }

    pub fn features_for_sel(&self, sel: u32) -> u32 {
        match sel {
            0 => 0u32,
            1 => 1u32, // VIRTIO_F_VERSION_1 (bit 32)
            _ => 0,
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Lifecycle MMIO
    // ─────────────────────────────────────────────────────────────────────────

    pub fn reset(&mut self) {
        self.status = 0;
        self.queues = [QueueState::default(), QueueState::default()];
        self.queues_setup_done = false;
    }

    /// Called when the guest writes DRIVER_OK (status=15)
    pub fn on_driver_ok(&mut self) {
        if self.queues_setup_done { return; }
        if self.vhost_sock.is_none() { return; }
        self.setup_queues();
    }

    // Fallback: if virtiofsd is not on ioeventfd, the kick must be signaled manually
    pub fn process_queue(&mut self, qi: usize) {
        if qi < 2 {
            let _ = self.kicks[qi].write(1);
        }
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Full vhost-user protocol
    // ─────────────────────────────────────────────────────────────────────────

    fn setup_queues(&mut self) {
        let sock = match self.vhost_sock.as_mut() {
            Some(s) => s,
            None => return,
        };

        // 1. GET_PROTOCOL_FEATURES
        let pf = match Self::rpc_u64(sock, VHOST_USER_GET_PROTOCOL_FEATURES) {
            Ok(v) => v,
            Err(e) => { eprintln!("[NKR-FS] GET_PROTOCOL_FEATURES falló: {e}"); return; }
        };

        // 2. SET_PROTOCOL_FEATURES (enable what was negotiated)
        if let Err(e) = Self::send_plain(sock, VHOST_USER_SET_PROTOCOL_FEATURES, &pf.to_le_bytes()) {
            eprintln!("[NKR-FS] SET_PROTOCOL_FEATURES falló: {e}"); return;
        }

        // 3. GET_QUEUE_NUM (verify there are ≥ 2 queues)
        if let Ok(qn) = Self::rpc_u64(sock, VHOST_USER_GET_QUEUE_NUM) {
            eprintln!("[NKR-FS] virtiofsd soporta {} colas", qn);
        }

        // 4. SET_MEM_TABLE with ancillary=memfd
        if let Err(e) = self.send_mem_table() {
            eprintln!("[NKR-FS] SET_MEM_TABLE falló: {e}"); return;
        }
        eprintln!("[NKR-FS] SET_MEM_TABLE enviado ({} regiones)", self.mem_regions.len());

        // 5. Configure each queue (hiprio=0, request=1)
        for qi in 0u32..2 {
            let q = &self.queues[qi as usize];
            let num = if q.num == 0 { 128 } else { q.num };

            // SET_VRING_NUM
            let state = VhostUserVringState { index: qi, num };
            let payload = unsafe {
                std::slice::from_raw_parts(&state as *const _ as *const u8,
                    std::mem::size_of::<VhostUserVringState>())
            };
            if let Err(e) = Self::send_plain(self.vhost_sock.as_mut().unwrap(),
                    VHOST_USER_SET_VRING_NUM, payload) {
                eprintln!("[NKR-FS] SET_VRING_NUM[{qi}] falló: {e}"); return;
            }

            // Helper to convert GPA to user address (HVA)
            let gpa_to_hva = |gpa: u64| -> u64 {
                for r in &self.mem_regions {
                    if gpa >= r.gpa && gpa < r.gpa + r.size as u64 {
                        return r.hva + (gpa - r.gpa);
                    }
                }
                gpa
            };

            // SET_VRING_ADDR
            let vra = VhostUserVringAddr {
                index: qi,
                flags: 0,
                desc_user_addr:  gpa_to_hva(q.desc_addr()),
                avail_user_addr: gpa_to_hva(q.avail_addr()),
                used_user_addr:  gpa_to_hva(q.used_addr()),
                log_guest_addr:  0,
            };
            let payload = unsafe {
                std::slice::from_raw_parts(&vra as *const _ as *const u8,
                    std::mem::size_of::<VhostUserVringAddr>())
            };
            if let Err(e) = Self::send_plain(self.vhost_sock.as_mut().unwrap(),
                    VHOST_USER_SET_VRING_ADDR, payload) {
                eprintln!("[NKR-FS] SET_VRING_ADDR[{qi}] falló: {e}"); return;
            }

            // SET_VRING_BASE
            let base = VhostUserVringState { index: qi, num: 0 };
            let payload = unsafe {
                std::slice::from_raw_parts(&base as *const _ as *const u8,
                    std::mem::size_of::<VhostUserVringState>())
            };
            if let Err(e) = Self::send_plain(self.vhost_sock.as_mut().unwrap(),
                    VHOST_USER_SET_VRING_BASE, payload) {
                eprintln!("[NKR-FS] SET_VRING_BASE[{qi}] falló: {e}"); return;
            }

            // SET_VRING_CALL — shared irqfd (ancillary)
            let call_state = VhostUserVringState { index: qi, num: 0 };
            let payload = unsafe {
                std::slice::from_raw_parts(&call_state as *const _ as *const u8,
                    std::mem::size_of::<VhostUserVringState>())
            };
            let call_fd = self.call.as_raw_fd();
            if let Err(e) = Self::send_with_fd(self.vhost_sock.as_mut().unwrap(),
                    VHOST_USER_SET_VRING_CALL, payload, call_fd) {
                eprintln!("[NKR-FS] SET_VRING_CALL[{qi}] falló: {e}"); return;
            }

            // SET_VRING_KICK — kick[qi] (ancillary)
            let kick_state = VhostUserVringState { index: qi, num: 0 };
            let payload = unsafe {
                std::slice::from_raw_parts(&kick_state as *const _ as *const u8,
                    std::mem::size_of::<VhostUserVringState>())
            };
            let kick_fd = self.kicks[qi as usize].as_raw_fd();
            if let Err(e) = Self::send_with_fd(self.vhost_sock.as_mut().unwrap(),
                    VHOST_USER_SET_VRING_KICK, payload, kick_fd) {
                eprintln!("[NKR-FS] SET_VRING_KICK[{qi}] falló: {e}"); return;
            }

            // SET_VRING_ENABLE
            let en = VhostUserVringState { index: qi, num: 1 };
            let payload = unsafe {
                std::slice::from_raw_parts(&en as *const _ as *const u8,
                    std::mem::size_of::<VhostUserVringState>())
            };
            if let Err(e) = Self::send_plain(self.vhost_sock.as_mut().unwrap(),
                    VHOST_USER_SET_VRING_ENABLE, payload) {
                eprintln!("[NKR-FS] SET_VRING_ENABLE[{qi}] falló: {e}"); return;
            }

            eprintln!("[NKR-FS] Cola {} configurada en virtiofsd", qi);
        }

        self.queues_setup_done = true;
        eprintln!("[NKR-FS] vhost-user: todas las colas configuradas (tag='{}')", self.tag);
    }

    fn send_mem_table(&mut self) -> io::Result<()> {
        let n = self.mem_regions.len() as u32;
        // payload: u32 nregions + u32 padding + n×VhostUserMemoryRegion
        let region_size = std::mem::size_of::<VhostUserMemoryRegion>();
        let mut payload = Vec::with_capacity(8 + n as usize * region_size);
        payload.extend_from_slice(&n.to_le_bytes());
        payload.extend_from_slice(&0u32.to_le_bytes()); // padding

        for r in &self.mem_regions {
            let region = VhostUserMemoryRegion {
                guest_phys_addr: r.gpa,
                memory_size: r.size as u64,
                userspace_addr: r.hva,
                mmap_offset: r.memfd_offset,
            };
            let bytes = unsafe {
                std::slice::from_raw_parts(&region as *const _ as *const u8, region_size)
            };
            payload.extend_from_slice(bytes);
        }

        let sock = self.vhost_sock.as_mut().unwrap();
        // SET_MEM_TABLE sends the memfd n times (one per region) as ancillary data
        // In practice virtiofsd accepts a single shared fd for all regions
        let fds: Vec<RawFd> = (0..n).map(|_| self.memfd).collect();
        Self::send_with_fds(sock, VHOST_USER_SET_MEM_TABLE, &payload, &fds)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Low-level vhost-user helpers
    // ─────────────────────────────────────────────────────────────────────────

    fn rpc_u64(sock: &mut UnixStream, request: u32) -> io::Result<u64> {
        Self::send_plain(sock, request, &[])?;
        let reply = Self::recv_reply(sock)?;
        if reply.len() < 8 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "reply corto"));
        }
        Ok(u64::from_le_bytes(reply[..8].try_into().unwrap()))
    }

    fn send_plain(sock: &mut UnixStream, request: u32, payload: &[u8]) -> io::Result<()> {
        let hdr = VhostUserMsgHdr {
            request,
            flags: VHOST_USER_VERSION | VHOST_USER_NEED_REPLY_MASK,
            size: payload.len() as u32,
        };
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(&hdr as *const _ as *const u8, 12)
        };
        sock.write_all(hdr_bytes)?;
        if !payload.is_empty() { sock.write_all(payload)?; }
        Ok(())
    }

    fn send_with_fd(sock: &mut UnixStream, request: u32, payload: &[u8], fd: RawFd) -> io::Result<()> {
        Self::send_with_fds(sock, request, payload, &[fd])
    }

    /// Sends vhost-user message with file descriptors via SCM_RIGHTS
    fn send_with_fds(sock: &mut UnixStream, request: u32, payload: &[u8], fds: &[RawFd]) -> io::Result<()> {
        let hdr = VhostUserMsgHdr {
            request,
            flags: VHOST_USER_VERSION | VHOST_USER_NEED_REPLY_MASK,
            size: payload.len() as u32,
        };
        let hdr_bytes = unsafe {
            std::slice::from_raw_parts(&hdr as *const _ as *const u8, 12)
        };

        // Build iovec: header + payload
        let mut iov = vec![
            libc::iovec { iov_base: hdr_bytes.as_ptr() as *mut _, iov_len: 12 },
        ];
        if !payload.is_empty() {
            iov.push(libc::iovec { iov_base: payload.as_ptr() as *mut _, iov_len: payload.len() });
        }

        // Build ancillary data (SCM_RIGHTS) with the fds
        let cmsg_space = unsafe {
            libc::CMSG_SPACE((fds.len() * std::mem::size_of::<RawFd>()) as u32) as usize
        };
        let mut cmsg_buf = vec![0u8; cmsg_space];

        let mut msghdr: libc::msghdr = unsafe { std::mem::zeroed() };
        msghdr.msg_iov = iov.as_mut_ptr();
        msghdr.msg_iovlen = iov.len();
        msghdr.msg_control = cmsg_buf.as_mut_ptr() as *mut _;
        msghdr.msg_controllen = cmsg_space;

        let cmsg = unsafe { libc::CMSG_FIRSTHDR(&msghdr) };
        if !cmsg.is_null() {
            unsafe {
                (*cmsg).cmsg_level = libc::SOL_SOCKET;
                (*cmsg).cmsg_type  = libc::SCM_RIGHTS;
                (*cmsg).cmsg_len   = libc::CMSG_LEN(
                    (fds.len() * std::mem::size_of::<RawFd>()) as u32) as _;
                std::ptr::copy_nonoverlapping(
                    fds.as_ptr(),
                    libc::CMSG_DATA(cmsg) as *mut RawFd,
                    fds.len(),
                );
            }
        }

        let ret = unsafe { libc::sendmsg(sock.as_raw_fd(), &msghdr, 0) };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn recv_reply(sock: &mut UnixStream) -> io::Result<Vec<u8>> {
        let mut hdr_buf = [0u8; 12];
        sock.read_exact(&mut hdr_buf)?;
        let size = u32::from_le_bytes(hdr_buf[8..12].try_into().unwrap()) as usize;
        let mut payload = vec![0u8; size];
        if size > 0 { sock.read_exact(&mut payload)?; }
        Ok(payload)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // Spawn + connect
    // ─────────────────────────────────────────────────────────────────────────

    fn spawn_and_connect(
        host_path: &str,
        sock_path: &str,
        cache_policy: &str,
        writeback: bool,
    ) -> (Option<std::process::Child>, Option<UnixStream>) {
        // Remove previous socket if it exists
        let _ = std::fs::remove_file(sock_path);

        let cache_flag = format!("--cache={}", cache_policy);
        let mut args = vec![
            "--socket-path".to_string(), sock_path.to_string(),
            "--shared-dir".to_string(),  host_path.to_string(),
            cache_flag,
            "--sandbox=none".to_string(),
            "--log-level=error".to_string(),
        ];
        if writeback {
            args.push("--writeback".to_string());
        }
        let child = match std::process::Command::new("virtiofsd")
            .args(&args)
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[NKR-FS] No se pudo lanzar virtiofsd: {e}");
                eprintln!("[NKR-FS] Instalar con: cargo install virtiofsd && sudo cp ~/.cargo/bin/virtiofsd /usr/local/bin/");
                return (None, None);
            }
        };
        eprintln!("[NKR-FS] virtiofsd lanzado (pid={}, shared='{}')", child.id(), host_path);

        // Wait for socket (max 3s)
        let mut waited_ms = 0u64;
        loop {
            if std::path::Path::new(sock_path).exists() { break; }
            if waited_ms >= 3000 {
                eprintln!("[NKR-FS] Timeout esperando socket de virtiofsd: {}", sock_path);
                return (Some(child), None);
            }
            std::thread::sleep(Duration::from_millis(50));
            waited_ms += 50;
        }

        // Connect
        let mut stream = match UnixStream::connect(sock_path) {
            Ok(s) => {
                let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
                s
            }
            Err(e) => {
                eprintln!("[NKR-FS] No se pudo conectar a virtiofsd: {e}");
                return (Some(child), None);
            }
        };

        // Initial handshake: SET_OWNER → GET_FEATURES → SET_FEATURES
        if let Err(e) = Self::send_plain(&mut stream, VHOST_USER_SET_OWNER, &[]) {
            eprintln!("[NKR-FS] SET_OWNER falló: {e}");
            return (Some(child), None);
        }

        let features = match Self::rpc_u64(&mut stream, VHOST_USER_GET_FEATURES) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[NKR-FS] GET_FEATURES falló: {e}");
                return (Some(child), None);
            }
        };

        let selected = features & ((1u64 << 32) | VHOST_USER_F_PROTOCOL_FEATURES);
        if let Err(e) = Self::send_plain(&mut stream, VHOST_USER_SET_FEATURES, &selected.to_le_bytes()) {
            eprintln!("[NKR-FS] SET_FEATURES falló: {e}");
            return (Some(child), None);
        }

        eprintln!("[NKR-FS] Handshake vhost-user OK (features={:#018X}, tag en socket {})",
            features, sock_path);
        (Some(child), Some(stream))
    }

    // ─────────────────────────────────────────────────────────────────────────
    // DAX
    // ─────────────────────────────────────────────────────────────────────────

    fn alloc_dax_window(size: usize) -> (*mut libc::c_void, usize, bool) {
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            eprintln!("[NKR-FS] WARN: No se pudo asignar ventana DAX — DAX desactivado");
            return (std::ptr::null_mut(), 0, false);
        }
        unsafe { libc::madvise(ptr, size, libc::MADV_NOHUGEPAGE); }
        eprintln!("[NKR-FS] Ventana DAX: {} MB mapeados en host → guest phys {:#X}",
            size >> 20, VIRTIO_FS_DAX_GUEST_PHYS);
        (ptr, size, true)
    }
}

impl Drop for VirtioFsDevice {
    fn drop(&mut self) {
        if self.dax_enabled && !self.dax_ptr.is_null() {
            unsafe { libc::munmap(self.dax_ptr, self.dax_size); }
            eprintln!("[NKR-FS] Ventana DAX liberada (tag='{}')", self.tag);
        }
        if let Some(mut child) = self.virtiofsd.take() {
            let _ = child.kill();
            // Use try_wait instead of wait() to avoid blocking if the process
            // is in state D (uninterruptible I/O). 2s timeout.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    _ => break, // timeout or error
                }
            }
            eprintln!("[NKR-FS] virtiofsd terminado (tag='{}')", self.tag);
        }
        // Limpiar archivos del filesystem que virtiofsd dejó atrás. Sin esto
        // se acumulan huérfanos en /run/nkrfs/ y el siguiente virtiofsd con
        // el mismo tag falla al intentar crear su .pid file (EAGAIN).
        let _ = std::fs::remove_file(&self.sock_path);
        let _ = std::fs::remove_file(format!("{}.pid", self.sock_path));
    }
}
