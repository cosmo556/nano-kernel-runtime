#!/bin/bash
# =============================================================================
# build-nanolinux.sh — Constructor integral de NanoLinux para NKR v1.3
# =============================================================================
set -e

S="./scripts/config"

echo "▶ [NKR] Paso 1: Aplicando base x86_64_defconfig..."
make x86_64_defconfig

echo "▶ [NKR] Paso 2: Inyectando ADN NanoLinux (PMEM + VirtIO-FS)..."

# ── LA BALA DE PLATA: CERO MÓDULOS ──
$S --disable MODULES
$S --disable MODULE_UNLOAD

# ── EXPERT: necesario para que ZONE_DEVICE sea visible ──
$S --enable  EXPERT

# ── KVM Guest paravirt ──
$S --enable  HYPERVISOR_GUEST
$S --enable  PARAVIRT
$S --enable  KVM_GUEST

# ── Sin KASLR: carga directa en dirección física fija ──
$S --disable RANDOMIZE_BASE
$S --disable RANDOMIZE_MEMORY

# ── Cadena PMEM ──
$S --enable  SPARSEMEM_VMEMMAP
$S --enable  SPARSEMEM_VMEMMAP_ENABLE
$S --enable  MEMORY_HOTPLUG
$S --enable  MEMORY_HOTREMOVE
$S --enable  ZONE_DEVICE
$S --enable  LIBNVDIMM
$S --enable  BLK_DEV_PMEM
$S --enable  NVDIMM_PFN
$S --enable  NVDIMM_DAX
$S --enable  DAX
$S --enable  FS_DAX

# ── VirtIO MMIO (NKR no usa PCI) ──
$S --enable  VIRTIO_MMIO
$S --enable  VIRTIO_MMIO_CMDLINE_DEVICES

# ── Built-in crítico ──
$S --enable  VIRTIO
$S --disable VIRTIO_PCI
$S --enable  VIRTIO_BLK
$S --enable  VIRTIO_NET
$S --enable  VIRTIO_CONSOLE
$S --enable  EXT4_FS
$S --enable  EXT4_USE_FOR_EXT2
$S --enable  CRYPTO_CRC32C
$S --enable  CRC32C_INTEL
$S --enable  LIBCRC32C
$S --enable  CRC16
$S --enable  JBD2
$S --enable  MBCACHE
$S --enable  NET_FAILOVER
$S --enable  FAILOVER
$S --enable  VIRTIO_PMEM
$S --enable  X86_PMEM_LEGACY
$S --enable  FUSE_FS
$S --enable  VIRTIO_FS
$S --enable  VIRTIO_BALLOON
$S --enable  BALLOON_COMPACTION
$S --enable  TMPFS_POSIX_ACL
$S --enable  INOTIFY_USER
$S --enable  EPOLL
$S --enable  SYSVIPC

# ── Consola Serial y Debug Temprano ──
$S --enable  SERIAL_8250
$S --enable  SERIAL_8250_CONSOLE
$S --enable  EARLY_PRINTK

# ── Huge Pages ──
$S --enable  TRANSPARENT_HUGEPAGE
$S --enable  TRANSPARENT_HUGEPAGE_MADVISE

# ══════════════════════════════════════════════════════════════════════════════
# ELIMINAR BLOAT: SECCIÓN DE CIRUGÍA RADICAL (Manteniendo tus líneas originales)
# ══════════════════════════════════════════════════════════════════════════════

# 1. Matar el Power Management global (Esto es lo que faltaba para liberar al RTC)
$S --disable PM
$S --disable PM_SLEEP
$S --disable SUSPEND
$S --disable HIBERNATION
$S --disable X86_PM_TIMER

# 2. Matar el RTC y Timers físicos definitivamente
$S --disable RTC_CLASS
$S --disable RTC_DRV_CMOS
$S --disable RTC_HCTOSYS
$S --disable RTC_SYSTOHC
$S --disable HPET_TIMER
$S --disable HPET_EMULATE_RTC

# 3. Hardware y subsistemas innecesarios en microVM (Tus líneas originales)
$S --disable PCI
$S --disable PCI_MSI
$S --disable PCIEPORTBUS
$S --disable ACPI
$S --disable ACPI_WMI
$S --disable ACPI_AC
$S --disable ACPI_BATTERY
$S --disable ACPI_FAN
$S --disable ACPI_THERMAL
$S --disable USB_SUPPORT
$S --disable USB
$S --disable SOUND
$S --disable SND
$S --disable SND_PCI
$S --disable WIRELESS
$S --disable CFG80211
$S --disable MAC80211
$S --disable RFKILL
$S --disable BLUETOOTH
$S --disable NFC
$S --disable WIMAX
$S --disable SCSI
$S --disable ATA
$S --disable MD
$S --disable HID
$S --disable HID_GENERIC
$S --disable INPUT_MOUSE
$S --disable INPUT_KEYBOARD
$S --disable INPUT_TOUCHSCREEN
$S --disable JOYSTICK
$S --disable SERIO
$S --disable SERIO_I8042
$S --disable SERIO_SERPORT
$S --disable KEYBOARD_ATKBD
$S --disable MOUSE_PS2
$S --disable VT
$S --disable VT_CONSOLE
$S --disable VGA_CONSOLE
$S --disable LEGACY_PTYS
$S --disable HWMON
$S --disable WATCHDOG
$S --disable I2C
$S --disable SPI
$S --disable W1
$S --disable LEDS_CLASS
$S --disable EDAC
$S --disable RAS

# 4. Depuración (Tus líneas originales)
$S --disable DEBUG_KERNEL
$S --disable KGDB
$S --disable FTRACE
$S --disable KPROBES
$S --disable DYNAMIC_DEBUG
$S --disable SLUB_DEBUG
$S --disable DEBUG_PAGEALLOC
$S --disable LOCK_STAT
$S --disable PROVE_LOCKING
$S --disable LOCKDEP
$S --disable PERF_EVENTS
$S --disable PROFILING
$S --disable KALLSYMS_ALL

# 5. Sistemas de archivos redundantes (Tus líneas originales)
$S --disable XFS_FS
$S --disable BTRFS_FS
$S --disable REISERFS_FS
$S --disable JFS_FS
$S --disable F2FS_FS
$S --disable NTFS_FS
$S --disable VFAT_FS
$S --disable ISO9660_FS
$S --disable UDF_FS
$S --disable CRAMFS
$S --disable SQUASHFS
$S --disable NFS_FS
$S --disable NFSD
$S --disable CIFS

# 6. Mantener soporte 9P y Paravirt
$S --enable  PARAVIRT_CLOCK
$S --enable  X86_TSC
$S --enable  NET_9P
$S --enable  NET_9P_VIRTIO
$S --enable  9P_FS

echo "▶ [NKR] Paso 3: Resolviendo dependencias..."
make olddefconfig

echo "▶ [NKR] Paso 4: Compilando núcleo ELF (Cero Módulos)..."
make -j$(nproc) vmlinux

echo "▶ [NKR] Paso 5: Optimizando y generando 'nanolinux'..."
strip -g vmlinux -o nanolinux

echo "============================================================"
echo "✅ ÉXITO: El archivo 'nanolinux' ha sido generado."
ls -lh nanolinux
echo "============================================================"