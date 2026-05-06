// =============================================================================
// NKR VirtIO-Net — Network device with TAP backend
// =============================================================================
//
// Feature B: TX uses io_uring for batched writes to the TAP fd.
// RX keeps the existing blocking thread (minimal-risk change).

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd};
use std::sync::{Arc, Mutex};
use std::sync::atomic::Ordering;
use std::thread;

use libc;

use io_uring::{IoUring, opcode, types};
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Queue, QueueOwnedT, QueueT};
use vm_memory::{Bytes, GuestMemoryMmap};

/// 6-byte MAC address
type MacAddr = [u8; 6];

pub struct NetState {
    pub queue_rx: Queue,
    pub queue_ready: [bool; 2],
    pub interrupt_status: u32,
    pub status: u32,
}

pub struct VirtioNetDevice {
    tap_file: Option<Arc<Mutex<File>>>,
    pub state: Arc<Mutex<NetState>>,
    pub queue_tx: Queue,
    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,

    // Queues
    pub queue_sel: u32,
    pub queue_num: [u32; 2],     // [RX, TX]

    // Vring addresses (per queue)
    pub desc_low: [u32; 2],
    pub desc_high: [u32; 2],
    pub avail_low: [u32; 2],
    pub avail_high: [u32; 2],
    pub used_low: [u32; 2],
    pub used_high: [u32; 2],

    // Config
    pub mac: MacAddr,

    // Feature B — io_uring TX: None if the kernel doesn't support it
    tx_ring: Option<IoUring>,
    // In-flight TX packet buffer (user_data → packet bytes)
    // We need to retain the Vec while io_uring processes it
    tx_pending: std::collections::HashMap<u64, Vec<u8>>,
    tx_next_ud: u64,
}

impl VirtioNetDevice {
    /// Creates the device. If `tap_name` is None, creates without TAP (stub).
    pub fn new(mem: Arc<GuestMemoryMmap>, mac: MacAddr, tap_name: Option<&str>) -> Self {
        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK).expect("Fallo ioeventfd net");
        let irqfd = EventFd::new(libc::EFD_NONBLOCK).expect("Fallo irqfd net");

        let (tap_file, tap_file_for_thread) = if let Some(name) = tap_name {
            match Self::open_tap(name) {
                Ok(file) => {
                    let fd = file.as_raw_fd();
                    eprintln!("[NKR-NET] TAP '{}' abierto (fd={})", name, fd);
                    let shared_file = Arc::new(Mutex::new(file));
                    
                    // We need to duplicate the fd for the blocking read thread
                    let file_dup = unsafe { libc::dup(fd) };
                    let file_for_thread = if file_dup >= 0 {
                        Some(unsafe { File::from_raw_fd(file_dup) })
                    } else { None };

                    (Some(shared_file), file_for_thread)
                }
                Err(e) => {
                    eprintln!("[NKR-NET] WARN: No se pudo abrir TAP '{}': {}", name, e);
                    (None, None)
                }
            }
        } else {
            (None, None)
        };

        let state = Arc::new(Mutex::new(NetState {
            queue_rx: Queue::new(256).unwrap(),
            queue_ready: [false, false],
            interrupt_status: 0,
            status: 0,
        }));

        // Feature B — io_uring TX ring
        let tx_ring = match IoUring::new(64) {
            Ok(r)  => { eprintln!("[NKR-NET] io_uring TX activo"); Some(r) }
            Err(e) => { eprintln!("[NKR-NET] io_uring TX no disponible ({e}), usando write síncrono"); None }
        };

        let net_dev = VirtioNetDevice {
            tap_file,
            state: state.clone(),
            queue_tx: Queue::new(256).unwrap(),
            ioeventfd,
            irqfd,
            mem: mem.clone(),
            device_features_sel: 0,
            driver_features_sel: 0,
            driver_features: 0,
            queue_sel: 0,
            queue_num: [0, 0],
            desc_low: [0, 0],
            desc_high: [0, 0],
            avail_low: [0, 0],
            avail_high: [0, 0],
            used_low: [0, 0],
            used_high: [0, 0],
            mac,
            tx_ring,
            tx_pending: std::collections::HashMap::new(),
            tx_next_ud: 0,
        };

        if let Some(mut file) = tap_file_for_thread {
            let irqfd_clone = net_dev.irqfd.try_clone().unwrap();
            let state_clone = state.clone();
            
            thread::spawn(move || {
                let mut buf = [0u8; 65536];
                let raw_fd = file.as_raw_fd();
                loop {
                    // poll with 200ms timeout to detect SHUTDOWN_REQUESTED
                    let mut pfd = libc::pollfd {
                        fd: raw_fd,
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    let ret = unsafe { libc::poll(&mut pfd, 1, 200) };
                    if ret < 0 { break; } // poll error
                    if ret == 0 {
                        // timeout: check if shutdown was requested
                        if crate::vmm::SHUTDOWN_REQUESTED.load(Ordering::SeqCst) { break; }
                        continue;
                    }
                    if pfd.revents & (libc::POLLHUP | libc::POLLERR | libc::POLLNVAL) != 0 { break; }
                    match file.read(&mut buf) {
                        Ok(n) if n > 0 => {
                            let mut st = state_clone.lock().unwrap_or_else(|p| p.into_inner());
                            if !st.queue_ready[0] { continue; }
                            
                            // Build packet with null header
                            let mut packet = vec![0u8; 12];
                            packet.extend_from_slice(&buf[..n]);
                            
                            if let Ok(mut iter) = st.queue_rx.iter(mem.as_ref()) {
                                if let Some(mut chain) = iter.next() {
                                    let head_index = chain.head_index();
                                    let mut offset = 0usize;
                                    while let Some(desc) = chain.next() {
                                        let to_write = (packet.len() - offset).min(desc.len() as usize);
                                        if to_write > 0 {
                                            let _ = mem.write_slice(&packet[offset..offset+to_write], desc.addr());
                                            offset += to_write;
                                        }
                                        if offset >= packet.len() { break; }
                                    }
                                    let _ = st.queue_rx.add_used(mem.as_ref(), head_index, offset as u32);
                                    st.interrupt_status |= 1;
                                    let _ = irqfd_clone.write(1);
                                } else {
                                    eprintln!("[NKR-NET] WARN: RX packet dropped, queue empty");
                                }
                            }
                        }
                        _ => break,
                    }
                }
            });
        }

        net_dev
    }

    fn open_tap(name: &str) -> Result<File, Box<dyn std::error::Error>> {
        let tun = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")?;

        let mut ifr = [0u8; 40]; // struct ifreq
        let name_bytes = name.as_bytes();
        let copy_len = name_bytes.len().min(15);
        ifr[..copy_len].copy_from_slice(&name_bytes[..copy_len]);
        
        ifr[16] = 0x02; // IFF_TAP (0x0002)
        ifr[17] = 0x10; // IFF_NO_PI (0x1000)

        let ret = unsafe { libc::ioctl(tun.as_raw_fd(), 0x400454CA, ifr.as_ptr()) };
        
        if ret < 0 {
            return Err(format!("TUNSETIFF failed: {}", std::io::Error::last_os_error()).into());
        }
        if ret < 0 {
            return Err(format!("TUNSETIFF failed: {}", std::io::Error::last_os_error()).into());
        }

        Ok(tun)
    }

    pub fn activate_queue(&mut self) {
        let sel = self.queue_sel as usize;
        if sel > 1 { return; }

        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        
        if sel == 0 {
            st.queue_rx.set_size(self.queue_num[sel] as u16);
            st.queue_rx.set_desc_table_address(Some(self.desc_low[sel]), Some(self.desc_high[sel]));
            st.queue_rx.set_avail_ring_address(Some(self.avail_low[sel]), Some(self.avail_high[sel]));
            st.queue_rx.set_used_ring_address(Some(self.used_low[sel]), Some(self.used_high[sel]));
            st.queue_rx.set_ready(true);
            st.queue_ready[sel] = true;
        } else {
            self.queue_tx.set_size(self.queue_num[sel] as u16);
            self.queue_tx.set_desc_table_address(Some(self.desc_low[sel]), Some(self.desc_high[sel]));
            self.queue_tx.set_avail_ring_address(Some(self.avail_low[sel]), Some(self.avail_high[sel]));
            self.queue_tx.set_used_ring_address(Some(self.used_low[sel]), Some(self.used_high[sel]));
            self.queue_tx.set_ready(true);
            st.queue_ready[sel] = true;
        }

        let qname = if sel == 0 { "RX" } else { "TX" };
        eprintln!("[NKR-NET] Cola {} activada (size={})", qname, self.queue_num[sel]);
    }

    pub fn reset(&mut self) {
        {
            let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
            st.status = 0;
            st.interrupt_status = 0;
            st.queue_ready = [false, false];
            st.queue_rx = Queue::new(256).unwrap();
        }
        self.device_features_sel = 0;
        self.driver_features_sel = 0;
        self.driver_features = 0;
        self.queue_sel = 0;
        self.queue_num = [0, 0];
        self.desc_low = [0, 0];
        self.desc_high = [0, 0];
        self.avail_low = [0, 0];
        self.avail_high = [0, 0];
        self.used_low = [0, 0];
        self.used_high = [0, 0];
        self.queue_tx = Queue::new(256).unwrap();
        eprintln!("[NKR-NET] Dispositivo reseteado");
    }

    /// Processes TX packets: reads from the guest queue → writes to the TAP.
    /// Feature B: uses io_uring Write if available; falls back to synchronous write_all.
    pub fn process_tx(&mut self) {
        let is_ready = { self.state.lock().unwrap_or_else(|p| p.into_inner()).queue_ready[1] };
        if !is_ready { return; }

        let tap_fd = self.tap_file.as_ref()
            .and_then(|arc| arc.lock().ok().map(|f| f.as_raw_fd()));

        let mem = self.mem.as_ref();
        let mut used_results: Vec<(u16, u32)> = Vec::new();

        {
            let mut iter = match self.queue_tx.iter(mem) {
                Ok(it) => it,
                Err(_) => return,
            };

            while let Some(mut chain) = iter.next() {
                let head_index = chain.head_index();
                let mut total_len = 0u32;

                // Read descriptors and concatenate the full packet
                let mut packet = Vec::new();
                while let Some(desc) = chain.next() {
                    let mut buf = vec![0u8; desc.len() as usize];
                    let _ = mem.read_slice(&mut buf, desc.addr());
                    packet.extend_from_slice(&buf);
                    total_len += desc.len();
                }

                // The first 12 bytes are the virtio-net header
                if packet.len() > 12 {
                    let payload = packet[12..].to_vec();

                    if let (Some(fd), Some(ref mut ring)) = (tap_fd, self.tx_ring.as_mut()) {
                        // io_uring path: async submission
                        let ud = self.tx_next_ud;
                        self.tx_next_ud += 1;

                        let entry = opcode::Write::new(
                            types::Fd(fd),
                            payload.as_ptr(),
                            payload.len() as u32,
                        )
                        .build()
                        .user_data(ud);

                        // Save buffer to keep it alive until io_uring processes it
                        self.tx_pending.insert(ud, payload);

                        unsafe {
                            let mut sq = ring.submission_shared();
                            if sq.push(&entry).is_err() {
                                // SQ full: send synchronously instead
                                self.tx_pending.remove(&ud);
                                if let Some(ref tap_arc) = self.tap_file {
                                    if let Ok(mut tap) = tap_arc.lock() {
                                        let _ = tap.write_all(&packet[12..]);
                                    }
                                }
                            }
                        }
                    } else if let Some(ref tap_arc) = self.tap_file {
                        // Synchronous fallback
                        if let Ok(mut tap) = tap_arc.lock() {
                            let _ = tap.write_all(&payload);
                        }
                    }
                } else if !packet.is_empty() {
                    eprintln!("[NKR-NET] WARN: TX packet too small: {} bytes", packet.len());
                }

                used_results.push((head_index, total_len));
            }
        } // drop iter borrow

        // Submit io_uring TX batch
        if let Some(ref mut ring) = self.tx_ring {
            let _ = ring.submit();
            // Drain completions to release pending buffers
            ring.completion().sync();
            for cqe in ring.completion() {
                self.tx_pending.remove(&cqe.user_data());
            }
        }

        if used_results.is_empty() { return; }

        for (idx, len) in &used_results {
            let _ = self.queue_tx.add_used(mem, *idx, *len);
        }

        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.interrupt_status |= 1;
        let _ = self.irqfd.write(1);
    }

}
