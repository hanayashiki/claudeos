# claudeos
#
#   make            build the kernel and userland
#   make run        boot into an interactive shell
#   make test       run both self-test suites and report
#   make demo       run the scripted tour
#   make clean      remove build products

.PHONY: all kernel user run test demo busybox clean

all: kernel user

kernel:
	@./scripts/build.sh

user:
	@./scripts/build-user.sh

run: all
	@./scripts/run.sh --timeout 3600 --initrd build/initramfs.cpio

test: all
	@./scripts/test.sh

demo: all
	@./scripts/run.sh --timeout 120 --initrd build/initramfs.cpio --append /root/demo.sh

busybox:
	@./scripts/fetch-busybox.sh
	@./scripts/build-user.sh

clean:
	@# build/thirdparty holds downloads; keep them so a rebuild stays offline.
	rm -rf build/kernel.elf build/kernel64.elf build/rootfs build/initramfs.cpio \
	       build/toolchain build/hello_c.o kernel/target user/cbox/target
