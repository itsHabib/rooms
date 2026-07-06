.PHONY: check fmt fmt-check lint test e2e build release clean

# `make check` is the single command CI runs and you run before commit.
# Same matrix locally and in CI so failures are reproducible.
check: fmt-check lint test

fmt:
	cargo fmt --all

fmt-check:
	cargo fmt --all -- --check

lint:
	cargo clippy --all-targets --all-features -- -D warnings

test:
	# NOT --all-features: the `e2e` feature is opt-in and requires
	# Firecracker + kernel + rootfs on the host. Run e2e tests explicitly
	# via `cargo test --features e2e` on the rooms-host VM.
	cargo test

e2e:
	# Host-e2e harness (rooms-host only): preflight → build → run e2e →
	# assert zero leaks → PASS/FAIL. Needs root for tap creation, so run as
	# `sudo -E make e2e`. See docs/preflight.md and scripts/e2e.sh.
	bash scripts/e2e.sh

build:
	cargo build

release:
	cargo build --release

clean:
	cargo clean
