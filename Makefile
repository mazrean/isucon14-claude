.PHONY: all build deploy bench build-rust deploy-rust

BIN ?= webapp/go/isuride
MATCHER ?= 0

all: deploy bench

build:
	cd webapp/go && CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -o isuride .

deploy: build
	BIN=$(BIN) MATCHER=$(MATCHER) ./scripts/deploy.sh

# Rust 版 (webapp/rust)。toolchain は webapp/rust/rust-toolchain.toml (nightly)、rustflags は .cargo/config.toml
RUST_BIN = webapp/rust/target/x86_64-unknown-linux-gnu/release/isuride

build-rust:
	cd webapp/rust && cargo build --release --locked

deploy-rust: build-rust
	BIN=$(RUST_BIN) MATCHER=$(MATCHER) ./scripts/deploy.sh

bench:
	./scripts/bench.sh
