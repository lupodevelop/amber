# HVF needs the hypervisor entitlement, and every `cargo build` invalidates the
# code signature, so building and codesigning must happen together. Use `make`
# instead of a bare `cargo build` and you can't forget the re-sign.

BIN := target/release/amber
ENTITLEMENTS := amber.entitlements

.PHONY: build sign release test test-int lint fuzz check-linux install uninstall kernel clean

PREFIX ?= $(HOME)/.local

# Install a source build: binary + assets into $(PREFIX)/lib/amber, with `amber`
# symlinked onto $(PREFIX)/bin. The binary finds its assets next to the real file,
# so it runs from anywhere. PREFIX=/usr/local for a system install (needs sudo).
install: build
	mkdir -p "$(PREFIX)/lib/amber" "$(PREFIX)/bin"
	cp "$(BIN)" "$(PREFIX)/lib/amber/amber"
	rm -rf "$(PREFIX)/lib/amber/assets"
	cp -R assets "$(PREFIX)/lib/amber/assets"
	codesign -f --entitlements $(ENTITLEMENTS) -s - "$(PREFIX)/lib/amber/amber"
	ln -sf "$(PREFIX)/lib/amber/amber" "$(PREFIX)/bin/amber"
	@echo "installed: $(PREFIX)/bin/amber (ensure $(PREFIX)/bin is on PATH)"

uninstall:
	rm -rf "$(PREFIX)/lib/amber" "$(PREFIX)/bin/amber"
	@echo "removed amber from $(PREFIX)"

# Fuzz the guest→host parsers (untrusted-input attack surface). Needs nightly +
# cargo-fuzz; the rustup shims must win over any homebrew rust in PATH.
# Usage: make fuzz TARGET=vsock_packet SECS=60   (targets: vsock_packet, virtio_chain)
TARGET ?= vsock_packet
SECS ?= 60
fuzz:
	cd fuzz && PATH="$$HOME/.cargo/bin:$$PATH" cargo +nightly fuzz run $(TARGET) -- -max_total_time=$(SECS)

# Validate the Linux-only code (KVM backend, /proc paths, seccomp+Landlock) without
# a Linux host: build + clippy + test + run the lockdown probe natively in an arm64
# Linux container (Apple Silicon runs it natively). Covers everything but a live KVM
# boot, which needs real /dev/kvm. Needs Docker.
check-linux:
	docker run --rm -v "$(PWD):/work" -w /work rust:bookworm bash -euc '\
	  rustup component add clippy >/dev/null 2>&1; \
	  export CARGO_TARGET_DIR=/work/target-linux; \
	  cargo clippy -p amber -p amber-kvm --all-targets -- -D warnings; \
	  cargo test -p amber-core; \
	  cargo build -q -p amber; \
	  /work/target-linux/debug/amber __lockdown-probe | grep -q LOCKDOWN_OK; \
	  /work/target-linux/debug/amber __lockdown-probe "$$(mktemp -d)" | grep -q LOCKDOWN_OK; \
	  echo "check-linux: OK (build + clippy + test + lockdown probe)"'

# Default: build the release binary and codesign it (software GIC + net on).
build:
	cargo build --release
	@$(MAKE) --no-print-directory sign

# Re-codesign the existing binary (needed after any rebuild).
sign:
	codesign --entitlements $(ENTITLEMENTS) -s - $(BIN)
	@echo "signed $(BIN)"

# Alias.
release: build

# Unit tests (cross-platform, no codesign needed).
test:
	cargo test

# Clippy with warnings as errors (what CI gates on).
lint:
	cargo clippy --workspace --all-targets -- -D warnings

# End-to-end smoke test of the real pipeline (needs HVF + assets). Builds first.
test-int: build
	./scripts/integration-test.sh

# Build the resin guest kernel (trimmed, built-in-everything arm64) into
# assets/Image via Docker. Fetches the Alpine virt config base on first run.
kernel:
	./scripts/build-kernel.sh

clean:
	cargo clean
