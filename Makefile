# HVF needs the hypervisor entitlement, and every `cargo build` invalidates the
# code signature — so building and codesigning must happen together. Use `make`
# instead of a bare `cargo build` and you can't forget the re-sign.

BIN := target/release/amber
ENTITLEMENTS := amber.entitlements

.PHONY: build sign release test test-int lint kernel clean

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
