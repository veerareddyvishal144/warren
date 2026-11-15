SHELL := /bin/bash
BIN := target/release

.PHONY: all build gateway images test e2e e2e-vm dev-up clippy fmt clean

all: build

build: ## Build warrend, warren, warren-gateway, warren-mcp (release)
	cargo build --release -p warrend -p warren-cli -p warren-gateway -p warren-mcp

gateway: ## Static-enough gateway binary for the template images (glibc, Debian bookworm compatible)
	cargo build --release -p warren-gateway
	mkdir -p images/.build && cp $(BIN)/warren-gateway images/.build/warren-gateway

images: gateway ## Build the built-in template images (warren/base, warren/agent, warren/opencode)
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
