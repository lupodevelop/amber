# HVF needs the hypervisor entitlement, and every `cargo build` invalidates the
# code signature — so building and codesigning must happen together. Use `make`
# instead of a bare `cargo build` and you can't forget the re-sign.

BIN := target/release/amber
ENTITLEMENTS := amber.entitlements

.PHONY: build sign release test clean

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

# Unit tests (no codesign needed).
test:
	cargo test

clean:
	cargo clean
