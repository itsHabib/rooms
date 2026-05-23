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
	cargo test --all-features

build:
	cargo build --all-features

release:
	cargo build --release --all-features

clean:
	cargo clean
