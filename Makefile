# claudeos
#
#   make            build the kernel and userland
#   make run        boot into an interactive shell
#   make test       run both self-test suites and report
#   make demo       run the scripted tour
#   make clean      remove build products

.PHONY: all kernel user run test demo clean

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

clean:
	rm -rf build kernel/target user/cbox/target
