// =============================================================================
// NKR VirtIO-Console — Canal de control host→guest
// =============================================================================
//
// Dispositivo minimal VirtIO Console (Device ID 3) con un único puerto.
// Aparece en el guest como /dev/hvc0.
// El host escribe "SHUTDOWN\n" en la receiveq → el guest init lo lee y
// para PostgreSQL limpiamente antes de llamar poweroff.
//
// MMIO: 0xD005_0000, IRQ: 11
// Queues: 0=receiveq (host→guest), 1=transmitq (guest→host, ignorado)
// =============================================================================

use std::sync::Arc;
use libc;
use vmm_sys_util::eventfd::EventFd;
use vm_memory::{Bytes, GuestAddress, GuestMemoryMmap};

pub const CONSOLE_MMIO_ADDR: u64 = 0xD005_0000;
pub const CONSOLE_IRQ: u32       = 11;
pub const CONSOLE_DEVICE_ID: u32 = 3;
const CONSOLE_QUEUE_SIZE: u32    = 64;

pub struct VirtioConsoleDevice {
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

    pub irqfd:     EventFd, // señal IRQ al guest
    pub ioeventfd: EventFd, // kick del transmitq (ignorado, solo para registro)

    pub mem: Arc<GuestMemoryMmap>,

    // Último avail.idx procesado de la receiveq
    last_avail: u16,

    // Datos pendientes de inyectar (cuando la cola aún no está lista)
    pub pending_inject: Option<Vec<u8>>,
}

unsafe impl Send for VirtioConsoleDevice {}

impl VirtioConsoleDevice {
    pub fn new(mem: Arc<GuestMemoryMmap>) -> Self {
        let irqfd     = EventFd::new(libc::EFD_NONBLOCK).expect("[NKR-CTL] irqfd falló");
        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK).expect("[NKR-CTL] ioeventfd falló");
        VirtioConsoleDevice {
            status: 0, interrupt_status: 0,
            device_features_sel: 0, driver_features_sel: 0, driver_features: 0,
            queue_sel: 0,
            queue_num:   [CONSOLE_QUEUE_SIZE; 2],
            queue_ready: [false; 2],
            desc_low:  [0; 2], desc_high:  [0; 2],
            avail_low: [0; 2], avail_high: [0; 2],
            used_low:  [0; 2], used_high:  [0; 2],
            irqfd, ioeventfd,
            mem,
            last_avail: 0,
            pending_inject: None,
        }
    }

    /// Llamado en cada iteración del vCPU loop — inyecta datos pendientes
    /// si la receiveq ya está configurada por el guest.
    pub fn poll_pending(&mut self) {
        if self.queue_ready[0] {
            if let Some(data) = self.pending_inject.take() {
                if !self.inject_to_receiveq(&data) {
                    // Sigue sin tener buffers disponibles, reencolar
                    self.pending_inject = Some(data);
                }
            }
        }
    }

    /// Inyecta `data` en la receiveq. Si la cola no está lista o no hay
    /// buffers disponibles, guarda los datos para reintento en poll_pending.
    pub fn try_inject(&mut self, data: &[u8]) {
        if !self.queue_ready[0] {
            self.pending_inject = Some(data.to_vec());
            return;
        }
        if !self.inject_to_receiveq(data) {
            self.pending_inject = Some(data.to_vec());
        }
    }

    /// Escribe `data` en el primer descriptor disponible de la receiveq.
    /// Retorna true si la inyección fue exitosa.
    fn inject_to_receiveq(&mut self, data: &[u8]) -> bool {
        let qi: usize = 0; // receiveq

        let desc_addr  = ((self.desc_high[qi]  as u64) << 32) | (self.desc_low[qi]  as u64);
        let avail_addr = ((self.avail_high[qi] as u64) << 32) | (self.avail_low[qi] as u64);
        let used_addr  = ((self.used_high[qi]  as u64) << 32) | (self.used_low[qi]  as u64);

        if desc_addr == 0 || avail_addr == 0 || used_addr == 0 {
            return false;
        }

        // avail ring: flags(u16), idx(u16), ring[queue_size](u16), ...
        let avail_idx: u16 = match self.mem.read_obj::<u16>(GuestAddress(avail_addr + 2)) {
            Ok(v) => u16::from_le(v),
            Err(_) => return false,
        };

        if avail_idx == self.last_avail {
            return false; // Guest no tiene buffers libres todavía
        }

        // Leer descriptor index: avail.ring[last_avail % queue_size]
        let ring_slot = (self.last_avail as u64 % self.queue_num[qi] as u64) * 2;
        let desc_idx: u16 = match self.mem.read_obj::<u16>(GuestAddress(avail_addr + 4 + ring_slot)) {
            Ok(v) => u16::from_le(v),
            Err(_) => return false,
        };

        // Descriptor: GPA(u64), len(u32), flags(u16), next(u16) = 16 bytes
        let desc_base = desc_addr + (desc_idx as u64) * 16;
        let buf_gpa: u64 = match self.mem.read_obj::<u64>(GuestAddress(desc_base)) {
            Ok(v) => u64::from_le(v),
            Err(_) => return false,
        };
        let buf_len: u32 = match self.mem.read_obj::<u32>(GuestAddress(desc_base + 8)) {
            Ok(v) => u32::from_le(v),
            Err(_) => return false,
        };

        let write_len = data.len().min(buf_len as usize);
        if self.mem.write_slice(&data[..write_len], GuestAddress(buf_gpa)).is_err() {
            return false;
        }

        self.last_avail = self.last_avail.wrapping_add(1);

        // used ring: flags(u16), idx(u16), ring[queue_size]{ id(u32), len(u32) }
        let used_idx: u16 = match self.mem.read_obj::<u16>(GuestAddress(used_addr + 2)) {
            Ok(v) => u16::from_le(v),
            Err(_) => return false,
        };
        let used_slot = (used_idx as u64 % self.queue_num[qi] as u64) * 8;
        let _ = self.mem.write_obj::<u32>(u32::to_le(desc_idx as u32),    GuestAddress(used_addr + 4 + used_slot));
        let _ = self.mem.write_obj::<u32>(u32::to_le(write_len as u32),   GuestAddress(used_addr + 4 + used_slot + 4));
        let _ = self.mem.write_obj::<u16>(u16::to_le(used_idx.wrapping_add(1)), GuestAddress(used_addr + 2));

        // Bit 0 = VIRTIO_MMIO_INT_VRING (used ring actualizado)
        self.interrupt_status |= 0x01;
        let _ = self.irqfd.write(1);

        eprintln!("[NKR-CTL] '{}' inyectado en /dev/hvc0 del guest",
            std::str::from_utf8(&data[..write_len]).unwrap_or("?").trim());
        true
    }
}
