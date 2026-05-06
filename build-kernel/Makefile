# =============================================================================
# NKR NanoLinux Builder — Makefile
# =============================================================================
# Compila un kernel Linux 6.6 LTS mínimo con soporte PMEM/DAX para NKR.
# Produce un único artefacto ELF puro llamado 'nanolinux'.
#
# Comandos principales:
#   make build        — compila en Docker (recomendado)
#   make install      — instala nanolinux en /mnt/nkr/kernel/
#   make verify       — verifica los flags críticos en .config
#   make clean        — limpia artefactos locales
#   make clean-docker — elimina imagen Docker
#
# En este host (2 CPUs): compilación ~35-50 minutos.
# Con 8 CPUs: ~10-15 minutos.
# =============================================================================

KERNEL_VERSION ?= 6.6.87
IMAGE_NAME     := nkr-nanolinux-builder:$(KERNEL_VERSION)
OUTPUT_DIR     := /home/cosmo99pruebas/nano-kernel-runtime/build-kernel/output
INSTALL_DIR    := /mnt/nkr/kernel

.PHONY: build install verify clean clean-docker help

## build: Construir imagen Docker y compilar NanoLinux
build: $(OUTPUT_DIR)/nanolinux

$(OUTPUT_DIR)/nanolinux: Dockerfile build-nanolinux.sh
	@echo "══════════════════════════════════════════════════════"
	@echo " NKR NanoLinux Builder — Linux $(KERNEL_VERSION)"
	@echo " Host CPUs: $$(nproc) — Tiempo estimado: $$(( 300 / $$(nproc) )) min"
	@echo "══════════════════════════════════════════════════════"
	mkdir -p $(OUTPUT_DIR)
	sudo docker build \
		--build-arg KERNEL_VERSION=$(KERNEL_VERSION) \
		-t $(IMAGE_NAME) \
		.
	@echo "▶ Extrayendo NanoLinux..."
	sudo docker run --rm \
		-v $(OUTPUT_DIR):/output \
		$(IMAGE_NAME)
	@echo ""
	@echo "✓ NanoLinux listo:"
	@ls -lh $(OUTPUT_DIR)/

## install: Instalar nanolinux (ELF puro) en NKR
install: $(OUTPUT_DIR)/nanolinux
	@echo "▶ Instalando nanolinux en $(INSTALL_DIR)/"
	sudo cp $(OUTPUT_DIR)/nanolinux $(INSTALL_DIR)/nanolinux
	@echo "✓ Instalado como núcleo base:"
	@ls -lh $(INSTALL_DIR)/nanolinux
	@echo ""
	@echo "⚠️ ATENCIÓN: Asegúrate de actualizar tu orquestador (vmm.rs o compose.yml)"
	@echo "para que apunte a $(INSTALL_DIR)/nanolinux en lugar de vmlinux/bzImage."

## verify: Verificar flags críticos de PMEM en el kernel compilado
verify:
	@echo "══════════════════════════════════════════════════════"
	@echo " Verificando flags críticos PMEM/VirtIO en NanoLinux"
	@echo "══════════════════════════════════════════════════════"
	@sudo docker run --rm $(IMAGE_NAME) sh -c \
		"grep -E 'VIRTIO_PMEM|FS_DAX|LIBNVDIMM|BLK_DEV_PMEM|NVDIMM|ZONE_DEVICE|SPARSEMEM_VMEMMAP|VIRTIO_FS|VIRTIO_BALLOON|VIRTIO_MMIO|KVM_GUEST|RANDOMIZE_BASE|RTC_DRV_CMOS' /build/linux/.config | sort" \
		2>/dev/null || \
		echo "(imagen no disponible — ejecuta 'make build' primero)"

## clean: Eliminar artefactos locales
clean:
	rm -rf $(OUTPUT_DIR)/

## clean-docker: Eliminar imagen Docker (libera ~3GB de disco)
clean-docker:
	sudo docker rmi $(IMAGE_NAME) || true

## help: Mostrar este mensaje
help:
	@echo "NKR NanoLinux Builder"
	@echo ""
	@grep -E '^## [a-z]' Makefile | sed 's/## /  make /'