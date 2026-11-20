SHELL := /bin/bash
BIN := $(or $(CARGO_TARGET_DIR),target)/release

.PHONY: all build gateway musl-setup images test e2e e2e-vm dev-up clippy fmt clean

all: build

build: gateway ## Build warrend, warren, warren-mcp (release) and the static gateway
	cargo build --release --locked -p warrend -p warren-cli -p warren-mcp

MUSL := x86_64-unknown-linux-musl
TARGET_DIR := $(or $(CARGO_TARGET_DIR),target)

gateway: ## Fully static gateway binary (musl) used by the template images and injected into registered images
	CC_x86_64_unknown_linux_musl=musl-gcc cargo build --release --locked -p warren-gateway --target $(MUSL)
	mkdir -p images/.build && cp $(TARGET_DIR)/$(MUSL)/release/warren-gateway images/.build/warren-gateway

musl-setup: ## One-time: the musl C compiler and Rust target the gateway build needs
	sudo apt-get install -y musl-tools
	rustup target add $(MUSL)

images: gateway ## Build the built-in template images (warren/base, warren/agent, warren/opencode, warren/claude)
	./scripts/build-images.sh

test: ## Unit tests for every crate
	cargo test --workspace --exclude warren-e2e

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

fmt:
	cargo fmt --all

e2e: build images ## End-to-end tests against a real daemon and Docker
	cargo test -p warren-e2e -- --ignored --test-threads=1 --nocapture

e2e-vm: build images ## Firecracker vm-type tests (root and /dev/kvm required)
	sudo -E env PATH=$$PATH CARGO_HOME=$$CARGO_HOME RUSTUP_HOME=$$RUSTUP_HOME cargo test -p warren-e2e -- --ignored --test-threads=1 --nocapture vm_

dev-up: build ## Run the daemon in dev mode on :7337 with data under ./data
	./scripts/dev-up.sh

clean:
	cargo clean
	rm -rf images/.build
