# claudeos
#
#   make            build the kernel and the userland images
#   make run        boot into an interactive shell
#   make test       run both self-test suites and report
#   make demo       run the scripted tour
#   make alpine     build Alpine's root filesystem with its suite, to boot it
#   make fetch      fill the download cache, build/cache, and build nothing
#   make clean      remove build products
#
# ARCH picks the machine, x86_64 unless told otherwise. The scripts it hands
# off to read it out of the environment, and tools/distro builds the variants
# of the images for it into build/distro: test-ARCH, and on aarch64 also
# board-aarch64, the image and card a Raspberry Pi 4 boots.

ARCH ?= x86_64
export ARCH
DISTRO = cargo run -q --release -p distro --
ifeq ($(ARCH),aarch64)
  VARIANTS = test-aarch64 board-aarch64
else
  VARIANTS = test-x86_64
endif
IMAGE = build/distro/test-$(ARCH)/initramfs.cpio

.PHONY: all kernel user run test demo alpine fetch clean

all: kernel user

kernel:
	@./scripts/build.sh

# After the kernel, since each image's manifest holds the kernel's digest.
user: kernel
	@for variant in $(VARIANTS); do $(DISTRO) build $$variant || exit 1; done

run: all
	@./scripts/run.sh --timeout 3600 --initrd $(IMAGE) --append shell_exit=poweroff

test: all
	@./scripts/test.sh

demo: all
	@./scripts/run.sh --timeout 120 --initrd $(IMAGE) --append /tests/demo.sh

alpine:
	@$(DISTRO) build alpine-$(ARCH)
	@echo "boot it with: ARCH=$(ARCH) ./scripts/run.sh --initrd build/distro/alpine-$(ARCH)/initramfs.cpio --append 'init=/bin/sh'"

fetch:
	@$(DISTRO) fetch

clean:
	@# build/cache holds downloads; keep them so a rebuild stays offline.
	rm -rf build/kernel.elf build/kernel64.elf build/kernel-aarch64.elf \
	       build/kernel8.img build/distro
	cargo clean
