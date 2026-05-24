.PHONY: check fmt fmt-check lint test build release clean

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

build:
	cargo build

release:
	cargo build --release

clean:
	cargo clean
