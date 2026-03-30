use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::sync::Arc;
use vmm_sys_util::eventfd::EventFd;
use virtio_queue::{Descriptor, Queue, QueueOwnedT, QueueT};
use vm_memory::{GuestMemoryMmap, Bytes};

pub struct VirtioBlockDevice {
    pub file: File,
    pub queue: Queue,
    pub ioeventfd: EventFd,
    pub irqfd: EventFd,
    pub mem: Arc<GuestMemoryMmap>,
    
    // Estado del dispositivo VirtIO
    pub status: u32,
    pub interrupt_status: u32,  // Flag de interrupción pendiente (bit 0 = VRING, bit 1 = CONFIG)
    
    // Selección de features
    pub device_features_sel: u32,
    pub driver_features_sel: u32,
    pub driver_features: u64,   // Features aceptadas por el driver
    
    // Configuración de colas
    pub queue_sel: u32,         // Cola actualmente seleccionada
    pub queue_num: u32,         // Tamaño de cola configurado por el driver
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
}

impl VirtioBlockDevice {
    pub fn new(disk_path: &str, mem: Arc<GuestMemoryMmap>) -> Self {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open(disk_path)
            .expect("[BLOCK] Error: No se pudo abrir el archivo del disco virtual");

        // Calcular capacidad del disco en sectores de 512 bytes
        let capacity_sectors = file.metadata()
            .expect("[BLOCK] No se pudo leer metadata del disco")
            .len() / 512;

        let ioeventfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[BLOCK] Fallo al crear ioeventfd");
            
        let irqfd = EventFd::new(libc::EFD_NONBLOCK)
            .expect("[BLOCK] Fallo al crear irqfd");

        let queue = Queue::new(256).unwrap();

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
        self.desc_low = 0;
        self.desc_high = 0;
        self.avail_low = 0;
        self.avail_high = 0;
        self.used_low = 0;
        self.used_high = 0;
        // Recrear la cola
        self.queue = Queue::new(256).unwrap();
        eprintln!("[NKR-BLOCK] Dispositivo reseteado");
    }

    pub fn process_queue(&mut self) {
        if !self.queue_ready {
            eprintln!("[NKR-BLOCK] process_queue llamado pero la cola NO está ready, ignorando");
            return;
        }

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

                // 1. HEADER
                let head: Descriptor = chain.next().expect("Fallo al leer header");
                let mut header_data = [0u8; 16];
                mem.read_slice(&mut header_data, head.addr()).unwrap();
                
                let request_type = u32::from_le_bytes(header_data[0..4].try_into().unwrap());
                let sector = u64::from_le_bytes(header_data[8..16].try_into().unwrap());

                // 2. DATA
                let data_desc: Descriptor = chain.next().expect("Fallo al leer data");
                
                // 3. STATUS
                let status_desc: Descriptor = chain.next().expect("Fallo al leer status");

                let len_written = match request_type {
                    0 => { // LECTURA (IN)
                        let offset = sector * 512;
                        let mut buffer = vec![0u8; data_desc.len() as usize];
                        
                        self.file.seek(SeekFrom::Start(offset)).unwrap();
                        let _ = self.file.read(&mut buffer).unwrap_or(0);
                        
                        mem.write_slice(&buffer, data_desc.addr()).unwrap();
                        mem.write_obj(0u8, status_desc.addr()).unwrap(); // VIRTIO_BLK_S_OK
                        
                        data_desc.len() + 1
                    }
                    1 => { // ESCRITURA (OUT)
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

        // Marcar interrupción VRING pendiente y notificar al guest
        self.interrupt_status |= 1; // VIRTIO_MMIO_INT_VRING
        self.irqfd.write(1).expect("Fallo al inyectar IRQ");
    }
}