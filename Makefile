.PHONY: all build deploy bench build-rust deploy-rust build-go deploy-go build-nginx

MATCHER ?= 0

all: deploy bench

# 既定は Rust 版 (app ノードの CPU が Go 版より 4 割ほど空き、その分 TLS を持たせられる)
build: build-rust
deploy: deploy-rust

build-go:
	cd webapp/go && CGO_ENABLED=0 GOOS=linux GOARCH=amd64 go build -o isuride .

deploy-go: build-go
	BIN=webapp/go/isuride MATCHER=$(MATCHER) ./scripts/deploy.sh

# AWS-LC 版 nginx (build/nginx-awslc)。deploy.sh はこれがあれば配る
build-nginx:
	./scripts/build-nginx-awslc.sh

# Rust 版 (webapp/rust)。toolchain は webapp/rust/rust-toolchain.toml (nightly)、rustflags は .cargo/config.toml
RUST_BIN = webapp/rust/target/x86_64-unknown-linux-gnu/release/isuride

build-rust:
	cd webapp/rust && cargo build --release --locked

deploy-rust: build-rust
	BIN=$(RUST_BIN) MATCHER=$(MATCHER) ./scripts/deploy.sh

bench:
	./scripts/bench.sh
