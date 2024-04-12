# .PHONY: $(MAKECMDGOALS) all
.DEFAULT_GOAL := help

mkfile_path := $(abspath $(lastword $(MAKEFILE_LIST)))
mkfile_dir := $(dir $(mkfile_path))

# Override autoinstalling of tools. (Eg `cargo install`)
export AUTOINSTALL ?= true
# Override the container tool. Tries docker first and then tries podman.
export CONTAINER_TOOL ?= auto
ifeq ($(CONTAINER_TOOL),auto)
	ifeq ($(shell docker version >/dev/null 2>&1 && echo docker), docker)
		override CONTAINER_TOOL = docker
	else ifeq ($(shell podman version >/dev/null 2>&1 && echo podman), podman)
		override CONTAINER_TOOL = podman
	else
		override CONTAINER_TOOL = unknown
	endif
endif
# Basic settings for base build images. These are varied between local development and CI.
export RUST_VERSION ?= $(shell grep channel rust-toolchain.toml | cut -d '"' -f 2)
export BUILD_IMAGE ?= rust:$(RUST_VERSION)-buster
export APP_IMAGE ?= debian:buster-slim
export CARGO_BIN_DIR ?= $(shell echo "${HOME}/.cargo/bin")

FMT_YELLOW = \033[0;33m
FMT_BLUE = \033[0;36m
FMT_SALUKI_LOGO = \033[1m\033[38;5;55m
FMT_END = \033[0m

# "One weird trick!" https://www.gnu.org/software/make/manual/make.html#Syntax-of-Functions
EMPTY:=
SPACE:= ${EMPTY} ${EMPTY}
COMMA:= ,

help:
	@printf -- "${FMT_SALUKI_LOGO} .----------------. .----------------. .----------------. .----------------. .----------------. .----------------.${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| .--------------. | .--------------. | .--------------. | .--------------. | .--------------. | .--------------. |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| |    _______   | | |      __      | | |   _____      | | | _____  _____ | | |  ___  ____   | | |     _____    | |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| |   /  ___  |  | | |     /  \     | | |  |_   _|     | | ||_   _||_   _|| | | |_  ||_  _|  | | |    |_   _|   | |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| |  |  (__ \_|  | | |    / /\ \    | | |    | |       | | |  | |    | |  | | |   | |_/ /    | | |      | |     | |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| |   '.___\`-.   | | |   / ____ \   | | |    | |   _   | | |  | '    ' |  | | |   |  __'.    | | |      | |     | |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| |  |\`\____) |  | | | _/ /    \ \_ | | |   _| |__/ |  | | |   \ \`--' /   | | |  _| |  \ \_  | | |     _| |_    | |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| |  |_______.'  | | ||____|  |____|| | |  |________|  | | |    \`.__.'    | | | |____||____| | | |    |_____|   | |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| |              | | |              | | |              | | |              | | |              | | |              | |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO}| '--------------' | '--------------' | '--------------' | '--------------' | '--------------' | '--------------' |${FMT_END}\n"
	@printf -- "${FMT_SALUKI_LOGO} '----------------' '----------------' '----------------' '----------------' '----------------' '----------------'${FMT_END}\n"
	@printf -- "\n"
	@printf -- "                        An experimental toolkit for building telemetry data planes in Rust.\n"
	@printf -- "\n"
	@printf -- "===================================================================================================================\n\n"
	@printf -- "Want to use ${FMT_YELLOW}\`docker\`${FMT_END} or ${FMT_YELLOW}\`podman\`${FMT_END}? Set ${FMT_YELLOW}\`CONTAINER_TOOL\`${FMT_END} environment variable. (Defaults to ${FMT_YELLOW}\`docker\`${FMT_END})\n"
	@printf -- "\n"
	@awk 'BEGIN {FS = ":.*##"; printf "Usage: make ${FMT_BLUE}<target>${FMT_END}\n"} /^[a-zA-Z0-9_-]+:.*?##/ { printf "  ${FMT_BLUE}%-46s${FMT_END} %s\n", $$1, $$2 } /^##@/ { printf "\n\033[1m%s\033[0m\n", substr($$0, 5) } ' $(MAKEFILE_LIST)

##@ Building

.PHONY: build-adp
build-adp: check-rust-build-tools
build-adp: ## Builds the ADP binary in release mode
	@echo "[*] Building ADP locally..."
	@cargo build --release --package agent-data-plane

.PHONY: build-adp-image
build-adp-image: ## Builds the ADP container image ('latest' tag)
	@echo "[*] Building ADP image..."
	@$(CONTAINER_TOOL) build \
		--tag agent-data-plane:latest \
		--build-arg BUILD_IMAGE=$(BUILD_IMAGE) \
		--build-arg APP_IMAGE=$(APP_IMAGE) \
		--file ./docker/Dockerfile.agent-data-plane \
		.

.PHONY: build-dsd-client
build-dsd-client: ## Builds the Dogstatsd client (used for sending DSD payloads)
	@echo "[*] Building Dogstatsd client..."
	@go build -C tooling/dogstatsd_client -o ../bin/dogstatsd_client .

.PHONY: check-rust-build-tools
check-rust-build-tools:
ifeq ($(shell command -v cargo >/dev/null || echo not-found), not-found)
	$(error "Please install Rust: https://www.rust-lang.org/tools/install")
endif

##@ Running

.PHONY: run-dsd-basic-udp
run-dsd-basic-udp: build-dsd-client ## Runs a basic set of metrics via the Dogstatsd client (UDP)
	@echo "[*] Sending basic metrics via Dogstatsd (UDP, 127.0.0.1:9191)..."
	@./tooling/bin/dogstatsd_client 127.0.0.1:9191 count:1,gauge:2,histogram:3,distribution:4,set:five

.PHONY: run-dsd-basic-uds
run-dsd-basic-uds: build-dsd-client ## Runs a basic set of metrics via the Dogstatsd client (UDS)
	@echo "[*] Sending basic metrics via Dogstatsd (unixgram:///tmp/adp-dsd.sock)..."
	@./tooling/bin/dogstatsd_client unixgram:///tmp/adp-dsd.sock count:1,gauge:2,histogram:3,distribution:4,set:five

.PHONY: run-dsd-basic-uds-stream
run-dsd-basic-uds-stream: build-dsd-client ## Runs a basic set of metrics via the Dogstatsd client (UDS Stream)
	@echo "[*] Sending basic metrics via Dogstatsd (unix:///tmp/adp-dsd.sock)..."
	@./tooling/bin/dogstatsd_client unix:///tmp/adp-dsd.sock count:1,gauge:2,histogram:3,distribution:4,set:five

##@ Checking

.PHONY: check-all
check-all: ## Check everything
check-all: check-fmt check-clippy check-features check-deny check-licenses

.PHONY: check-clippy
check-clippy: check-rust-build-tools
check-clippy: ## Check Rust source code with Clippy
	@echo "[*] Checking Clippy lints..."
	@cargo clippy --all-targets --workspace -- -D warnings

.PHONY: check-deny
check-deny: check-rust-build-tools cargo-install-cargo-deny
check-deny: ## Check all crate dependencies for outstanding advisories or usage restrictions
	@echo "[*] Checking for dependency advisories, license conflicts, and untrusted dependency sources..."
	@cargo deny check --hide-inclusion-graph --show-stats

.PHONY: check-fmt
check-fmt: check-rust-build-tools
check-fmt: ## Check that all Rust source files are formatted properly
	@echo "[*] Checking Rust source code formatting..."
	@cargo fmt -- --check

.PHONY: check-licenses
check-licenses: check-rust-build-tools cargo-install-dd-rust-license-tool
check-licenses: ## Check that the third-party license file is up to date
	@echo "[*] Checking if third-party license file is up-to-date..."
	@$(HOME)/.cargo/bin/dd-rust-license-tool check

.PHONY: check-features
check-features: check-rust-build-tools cargo-install-cargo-hack
check-features: ## Check that ADP builds with all possible combinations of feature flags
	@echo "[*] Checking feature flag compatibility matrix..."
	@cargo hack check --feature-powerset --tests --quiet

##@ Testing

.PHONY: test
test: check-rust-build-tools cargo-install-cargo-nextest
test: ## Runs all unit tests
	@echo "[*] Running unit tests..."
	cargo nextest run

##@ Utility

.PHONY: clean
clean: check-rust-build-tools
clean: ## Clean all build artifacts (debug/release)
	@echo "[*] Cleaning Rust build artifacts..."
	@cargo clean

.PHONY: fmt
fmt: check-rust-build-tools
fmt: ## Format Rust source code
	@echo "[*] Formatting Rust source code..."
	@cargo fmt

.PHONY: sync-licenses
sync-licenses: check-rust-build-tools cargo-install-dd-rust-license-tool
sync-licenses: ## Synchronizes the third-party license file with the current crate dependencies
	@echo "[*] Synchronizing third-party license file to current dependencies...""
	@$(HOME)/.cargo/bin/dd-rust-license-tool write

.PHONY: cargo-install-%
cargo-install-%: override TOOL = $(@:cargo-install-%=%)
cargo-install-%: check-rust-build-tools
	@$(if $(findstring true,$(AUTOINSTALL)),test -f ${CARGO_BIN_DIR}/${TOOL} || cargo install ${TOOL} --quiet,)