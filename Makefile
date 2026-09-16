# claudeos
#
#   make            build the kernel and userland
#   make run        boot into an interactive shell
#   make test       run both self-test suites and report
#   make demo       run the scripted tour
#   make clean      remove build products
#
# ARCH picks the machine, x86_64 unless told otherwise. The scripts it hands
# off to read it out of the environment; the two below are chosen by name, and
# the products of the two machines sit side by side in build/.

ARCH ?= x86_64
export ARCH
ifeq ($(ARCH),aarch64)
  USERLAND = ./scripts/build-user-aarch64.sh
  IMAGE    = build/initramfs-aarch64.cpio
else
  USERLAND = ./scripts/build-user.sh
  IMAGE    = build/initramfs.cpio
endif

.PHONY: all kernel user run test demo busybox alpine cloudflared clean

all: kernel user

kernel:
	@./scripts/build.sh

user:
	@$(USERLAND)

run: all
	@./scripts/run.sh --timeout 3600 --initrd $(IMAGE) --append shell_exit=poweroff

test: all
	@./scripts/test.sh

demo: all
	@./scripts/run.sh --timeout 120 --initrd $(IMAGE) --append /tests/demo.sh

busybox:
	@./scripts/fetch-busybox.sh
	@$(USERLAND)

alpine: kernel
	@./scripts/fetch-alpine.sh

cloudflared:
	@./scripts/fetch-cloudflared.sh
	@$(USERLAND)

clean:
	@# build/thirdparty holds downloads; keep them so a rebuild stays offline.
	rm -rf build/kernel.elf build/kernel64.elf build/rootfs build/initramfs.cpio \
	       build/alpine-rootfs build/alpine.cpio build/toolchain build/hello_c.o \
	       build/kernel-aarch64.elf build/kernel8.img build/rootfs-aarch64 \
	       build/initramfs-aarch64.cpio build/alpine-rootfs-aarch64 \
	       build/stage build/stage-aarch64 build/rootfs-aarch64-board \
	       build/initramfs-aarch64-board.cpio build/data-aarch64 \
	       build/alpine-aarch64.cpio build/hello_c-aarch64.o \
	       kernel/target user/cbox/target
