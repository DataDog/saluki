# .PHONY: $(MAKECMDGOALS) all
.DEFAULT_GOAL := help

mkfile_path := $(abspath $(lastword $(MAKEFILE_LIST)))
mkfile_dir := $(dir $(mkfile_path))

# High-level settings that ultimately get passed down to build-specific targets and other spots.
export TARGET_ARCH := $(shell uname -m | sed s/x86_64/amd64/ | sed s/aarch64/arm64/)
export BUILD_TARGET := $(or $(BUILD_TARGET),default)
export APP_GIT_HASH := $(or $(CI_COMMIT_SHA),$(shell git rev-parse --short HEAD 2>/dev/null || echo not-in-git))
export APP_BUILD_TIME := $(or $(CI_PIPELINE_CREATED_AT),0000-00-00T00:00:00-00:00)

# ADP-specific settings used during builds.
export ADP_APP_FULL_NAME := Agent Data Plane
export ADP_APP_SHORT_NAME := data-plane
export ADP_APP_IDENTIFIER := adp
export ADP_APP_GIT_HASH := $(APP_GIT_HASH)
export ADP_APP_VERSION_AUTO := $(shell cat bin/agent-data-plane/Cargo.toml | grep -E "^version = \"" | head -n 1 | cut -d '"' -f 2)
export ADP_APP_VERSION := $(or $(ADP_APP_VERSION),$(ADP_APP_VERSION_AUTO))
export ADP_APP_BUILD_TIME := $(APP_BUILD_TIME)

# SPDX license-list-data tag used by package-adp-host to harvest THIRD-PARTY-* license texts.
# Pinned to match docker/Dockerfile.agent-data-plane so the host-built tarball ships the same
# set of license texts as the linux Docker artifact; bump in lockstep with the Dockerfile.
export ADP_SPDX_LICENSES_VERSION := 3.28.0

# ADP-specific settings used when running.
export ADP_STANDALONE_IPC_CERT_FILE := /tmp/adp-ipc-cert.pem

# macOS integration-test settings.
MACOS_TEST_AGENT_VERSION ?= 7.78.0
MACOS_TEST_AGENT_DMG_DIR ?= /tmp/saluki-dda-dmg-cache
MACOS_TEST_AGENT_DMG_URL ?= https://s3.amazonaws.com/dd-agent/datadog-agent-$(MACOS_TEST_AGENT_VERSION)-1.$(shell uname -m).dmg
MACOS_TEST_AGENT_INSTALL_DIR ?= /tmp/saluki-dda/datadog-agent

# General build settings used for tooling, etc.
export GO_BUILD_IMAGE ?= golang:1.23-bullseye
export GO_APP_IMAGE ?= ubuntu:24.04

# Tool configuration.
export AUTOINSTALL ?= true
export CARGO_BIN_DIR := $(shell echo "${HOME}/.cargo/bin")
export CARGO_BINSTALL_STRATEGIES ?= crate-meta-data,compile
ifeq ($(CI),true)
	override CARGO_BINSTALL_STRATEGIES = compile
endif
export CARGO_TOOL_VERSION_cargo-binstall ?= 1.18.1
export CARGO_TOOL_VERSION_dd-rust-license-tool ?= 1.0.6
export CARGO_TOOL_VERSION_cargo-deny ?= 0.18.9
export CARGO_TOOL_VERSION_cargo-hack ?= 0.6.30
export CARGO_TOOL_VERSION_cargo-nextest ?= 0.9.99
export CARGO_TOOL_VERSION_cargo-xwin ?= 0.22.0
export CARGO_TOOL_VERSION_cargo-autoinherit ?= 0.1.5
export CARGO_TOOL_VERSION_cargo-sort ?= 1.0.9
export CARGO_TOOL_VERSION_dummyhttp ?= 1.1.0
export CARGO_TOOL_VERSION_cargo-machete ?= 0.9.1
export CARGO_TOOL_VERSION_rustfilt ?= 0.2.1
export DDPROF_VERSION ?= 0.20.0
export LADING_VERSION ?= sha-d608ffbce8f8c77b147d6750b3bb6d6948af239a

# Windows cross-compilation settings. These targets currently assume a local macOS host.
export WINDOWS_CROSS_TARGET ?= x86_64-pc-windows-msvc
export WINDOWS_CROSS_LLVM_BIN ?= /opt/homebrew/opt/llvm/bin
export WINDOWS_CROSS_CARGO_ARGS ?= --package agent-data-plane

# Version of source repositories (Git tag) for vendored Protocol Buffers definitions.
export PROTOBUF_SRC_REPO_DD_AGENT ?= 7.73.x
export PROTOBUF_SRC_REPO_AGENT_PAYLOAD ?= v5.0.164
export PROTOBUF_SRC_REPO_CONTAINERD ?= v2.2.0
export PROTOBUF_SRC_REPO_SKETCHES_GO ?= v1.4.7

FMT_YELLOW = \033[0;33m
FMT_BLUE = \033[0;36m
FMT_SALUKI_LOGO = \033[1m\033[38;5;55m
FMT_END = \033[0m

# "One weird trick!" https://www.gnu.org/software/make/manual/make.html#Syntax-of-Functions
EMPTY:=
SPACE:= ${EMPTY} ${EMPTY}
COMMA:= ,

.PHONY: help
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

.PHONY: build-adp-base
build-adp-base: check-rust-build-tools
build-adp-base:
	@echo "[*] Building ADP locally (profile: $(BUILD_PROFILE))"
	@APP_FULL_NAME="$(ADP_APP_FULL_NAME)" \
	APP_SHORT_NAME="$(ADP_APP_SHORT_NAME)" \
	APP_IDENTIFIER="$(ADP_APP_IDENTIFIER)" \
	APP_GIT_HASH="$(ADP_APP_GIT_HASH)" \
	APP_VERSION="$(ADP_APP_VERSION)" \
	APP_BUILD_DATE="$(ADP_APP_BUILD_DATE)" \
	cargo build --profile $(BUILD_PROFILE) --package agent-data-plane

.PHONY: build-adp
build-adp: override BUILD_PROFILE=devel
build-adp: build-adp-base
build-adp: ## Builds the ADP binary in debug mode

.PHONY: build-adp-release
build-adp-release: override BUILD_PROFILE=release
build-adp-release: build-adp-base
build-adp-release: ## Builds the ADP binary in release mode

.PHONY: build-adp-system-alloc
build-adp-system-alloc: ## Builds the ADP binary in debug mode with the system allocator (useful for memory profiling)
	@$(MAKE) --no-print-directory build-adp-base BUILD_PROFILE=devel RUSTFLAGS="--cfg tokio_unstable --cfg system_allocator"

.PHONY: build-adp-release-system-alloc
build-adp-release-system-alloc: ## Builds the ADP binary in release mode with the system allocator (useful for memory profiling)
	@$(MAKE) --no-print-directory build-adp-base BUILD_PROFILE=release RUSTFLAGS="--cfg tokio_unstable --cfg system_allocator"

.PHONY: build-schema-overlay
build-schema-overlay: check-rust-build-tools
build-schema-overlay: ## Builds the config schema overlay packages
	@echo "[*] Building config schema overlay packages..."
	@cargo build --profile devel --package datadog-agent-config --package datadog-agent-config-testing

.PHONY: build-adp-image-base
build-adp-image-base:
	@echo "[*] Building ADP image... (target: ${BUILD_TARGET}, profile: ${BUILD_PROFILE}, features: ${BUILD_FEATURES})"
	@docker build \
		--tag saluki-images/agent-data-plane:$(IMAGE_TAG)-$(BUILD_PROFILE) \
		--tag local.dev/saluki-images/agent-data-plane:$(IMAGE_TAG)-$(BUILD_PROFILE) \
		--build-arg "BUILD_TARGET=$(BUILD_TARGET)" \
		--build-arg "BUILD_PROFILE=$(BUILD_PROFILE)" \
		--build-arg "BUILD_FEATURES=$(BUILD_FEATURES)" \
		--build-arg "APP_FULL_NAME=$(ADP_APP_FULL_NAME)" \
		--build-arg "APP_SHORT_NAME=$(ADP_APP_SHORT_NAME)" \
		--build-arg "APP_IDENTIFIER=$(ADP_APP_IDENTIFIER)" \
		--build-arg "APP_VERSION=$(ADP_APP_VERSION)" \
		--build-arg "APP_GIT_HASH=$(ADP_APP_GIT_HASH)" \
		--build-arg "INTERNAL_BUILD=true" \
		--file ./docker/Dockerfile.agent-data-plane \
		.

.PHONY: build-adp-image
build-adp-image: override BUILD_PROFILE = devel
build-adp-image: override BUILD_FEATURES = default
build-adp-image: override IMAGE_TAG = testing
build-adp-image: build-adp-image-base
build-adp-image: ## Builds the ADP container image in debug mode

.PHONY: build-adp-image-release
build-adp-image-release: override BUILD_PROFILE = release
build-adp-image-release: override BUILD_FEATURES = default
build-adp-image-release: override IMAGE_TAG = testing
build-adp-image-release: build-adp-image-base
build-adp-image-release: ## Builds the ADP container image in release mode

.PHONY: build-adp-image-fips
build-adp-image-fips: override BUILD_PROFILE = devel
build-adp-image-fips: override BUILD_FEATURES = fips
build-adp-image-fips: override IMAGE_TAG = testing-fips
build-adp-image-fips: build-adp-image-base
build-adp-image-fips: ## Builds the ADP container image in debug mode (FIPS enabled)

.PHONY: build-adp-image-fips-release
build-adp-image-fips-release: override BUILD_PROFILE = release
build-adp-image-fips-release: override BUILD_FEATURES = fips
build-adp-image-fips-release: override IMAGE_TAG = testing-fips
build-adp-image-fips-release: build-adp-image-base
build-adp-image-fips-release: ## Builds the ADP container image in release mode (FIPS enabled)

.PHONY: build-datadog-agent-image
build-datadog-agent-image: build-adp-image ## Builds the converged Datadog Agent/ADP container image (debug mode)
	@echo "[*] Building converged Datadog Agent image... (debug mode)"
	@docker build \
		--tag saluki-images/datadog-agent:testing-devel \
		--tag local.dev/saluki-images/datadog-agent:testing-devel \
		--build-arg ADP_IMAGE=saluki-images/agent-data-plane:testing-devel \
		--file ./docker/Dockerfile.datadog-agent \
		.

.PHONY: build-datadog-agent-image-release
build-datadog-agent-image-release: build-adp-image-release ## Builds the converged Datadog Agent/ADP container image (release mode)
	@echo "[*] Building converged Datadog Agent image... (release mode)"
	@docker build \
		--tag saluki-images/datadog-agent:testing-release \
		--tag local.dev/saluki-images/datadog-agent:testing-release \
		--build-arg ADP_IMAGE=saluki-images/agent-data-plane:testing-release \
		--file ./docker/Dockerfile.datadog-agent \
		.

.PHONY: build-gen-statsd-image
build-gen-statsd-image: ## Builds the gen-statsd container image ('latest' tag)
	@echo "[*] Building gen-statsd image..."
	@docker build \
		--tag saluki-images/gen-statsd:latest-release \
		--tag local.dev/saluki-images/gen-statsd:testing \
		--build-arg BUILD_IMAGE=$(GO_BUILD_IMAGE) \
		--build-arg APP_IMAGE=$(GO_APP_IMAGE) \
		--file ./docker/Dockerfile.gen-statsd \
		.


.PHONY: build-correctness-tools-image
build-correctness-tools-image: ## Builds the correctness tools suite (datadog-intake + millstone) container image ('latest' tag)
	@echo "[*] Building correctness tools image (datadog-intake + millstone)..."
	@docker build \
		--tag saluki-images/correctness-tools:latest \
		--tag local.dev/saluki-images/correctness-tools:testing \
		--file ./docker/Dockerfile.correctness-tools \
		.

.PHONY: install-windows-cross-tools
install-windows-cross-tools: check-rust-build-tools ## Installs local macOS tools for Windows Rust cross-compilation
ifneq ($(shell uname -s),Darwin)
	$(error "install-windows-cross-tools currently supports local macOS hosts only")
endif
ifeq ($(shell command -v brew >/dev/null || echo not-found), not-found)
	$(error "Please install Homebrew: https://brew.sh/")
endif
	@echo "[*] Installing Rust Windows target ($(WINDOWS_CROSS_TARGET))..."
	@rustup target add $(WINDOWS_CROSS_TARGET)
	@echo "[*] Ensuring cargo-xwin@$(CARGO_TOOL_VERSION_cargo-xwin) is installed..."
	@test -f "$(CARGO_BIN_DIR)/cargo-xwin" || cargo install cargo-xwin --version "$(CARGO_TOOL_VERSION_cargo-xwin)" --locked
	@echo "[*] Ensuring Homebrew LLVM is installed..."
	@brew list llvm >/dev/null 2>&1 || brew install llvm
	@test -x "$(WINDOWS_CROSS_LLVM_BIN)/llvm-lib" || \
		(echo "Missing llvm-lib at $(WINDOWS_CROSS_LLVM_BIN)/llvm-lib. Set WINDOWS_CROSS_LLVM_BIN or install Homebrew LLVM." && exit 1)

.PHONY: build-windows-cross
build-windows-cross: install-windows-cross-tools ## Builds ADP for Windows from a local macOS host (override WINDOWS_CROSS_CARGO_ARGS as needed)
	@echo "[*] Building Windows cross target ($(WINDOWS_CROSS_TARGET)): cargo build $(WINDOWS_CROSS_CARGO_ARGS)"
	@PATH="$(WINDOWS_CROSS_LLVM_BIN):$$PATH" cargo xwin build --target $(WINDOWS_CROSS_TARGET) $(WINDOWS_CROSS_CARGO_ARGS)

.PHONY: build-proxy-dumper-image
build-proxy-dumper-image: check-proxy-dumper-tools ## Builds the proxy-dumper container image ('latest' tag)
ifeq ($(shell test -d test/build/dd-agent-benchmarks || echo not-found), not-found)
	@echo "[*] Cloning Datadog Agent Benchmarks repository..."
	@mkdir -p test/build
	@git -C test/build clone -q --single-branch --branch=tobz/adp-proxy-dumper-improvements \
		git@github.com:DataDog/datadog-agent-benchmarks.git dd-agent-benchmarks
endif
	@echo "[*] Pulling latest changes from Git and updating vendored dependencies..."
	@git -C test/build/dd-agent-benchmarks pull origin
	@cd test/build/dd-agent-benchmarks/docker/proxy-dumper && go mod vendor
	@echo "[*] Building proxy-dumper image..."
	@docker build \
		--tag saluki-images/proxy-dumper:latest \
		--tag local.dev/saluki-images/proxy-dumper:testing \
		--build-arg BUILD_IMAGE=$(GO_BUILD_IMAGE) \
		--build-arg APP_IMAGE=$(GO_APP_IMAGE) \
		--build-context repo=. \
		--file ./docker/Dockerfile.proxy-dumper \
		test/build/dd-agent-benchmarks/docker/proxy-dumper

.PHONY: check-rust-build-tools
check-rust-build-tools:
ifeq ($(shell command -v cargo >/dev/null || echo not-found), not-found)
	$(error "Please install Rust: https://www.rust-lang.org/tools/install")
endif
ifeq ($(shell command -v jq >/dev/null || echo not-found), not-found)
	$(error "Please install jq: https://jqlang.org/download/")
endif
ifeq ($(shell command -v protoc >/dev/null || echo not-found), not-found)
	$(error "Please install protoc: https://protobuf.dev/installation/")
endif
ifeq ($(shell command -v cargo-binstall >/dev/null || echo not-found), not-found)
	@echo "[*] Installing cargo-binstall@$(CARGO_TOOL_VERSION_cargo-binstall)..."
	@set -eu; \
		host_triple=$$(rustc -vV | awk '/^host:/ { print $$2 }'); \
		case "$$host_triple" in \
			*apple-darwin|*windows*) ext=zip ;; \
			*) ext=tgz ;; \
		esac; \
		asset="cargo-binstall-$$host_triple.full.$$ext"; \
		url="https://github.com/cargo-bins/cargo-binstall/releases/download/v$(CARGO_TOOL_VERSION_cargo-binstall)/$$asset"; \
		tmpdir=$$(mktemp -d); \
		trap 'rm -rf "$$tmpdir"' EXIT; \
		echo "[*] Downloading $$url..."; \
		curl -fsSL "$$url" -o "$$tmpdir/$$asset"; \
		case "$$ext" in \
			zip) unzip -q "$$tmpdir/$$asset" -d "$$tmpdir" ;; \
			tgz) tar -xzf "$$tmpdir/$$asset" -C "$$tmpdir" ;; \
		esac; \
		mkdir -p "$(CARGO_BIN_DIR)"; \
		install -m 0755 "$$tmpdir/cargo-binstall" "$(CARGO_BIN_DIR)/cargo-binstall"
endif

##@ Running

.PHONY: create-dummy-agent-config
create-dummy-agent-config:
	@echo "{}" > /tmp/adp-empty-config.yaml

create-dummy-ipc-cert: $(ADP_STANDALONE_IPC_CERT_FILE)
$(ADP_STANDALONE_IPC_CERT_FILE):
ifeq ($(shell command -v openssl >/dev/null || echo not-found), not-found)
	$(error "Please install OpenSSL.")
endif
	@echo "[*] Generating self-signed TLS certificate for privileged API endpoint..."
	@openssl req -x509 -newkey rsa:2048 -keyout $(ADP_STANDALONE_IPC_CERT_FILE) -out $(ADP_STANDALONE_IPC_CERT_FILE) \
		-days 365 -nodes -subj "/CN=localhost" -batch 2>/dev/null

.PHONY: run-adp
run-adp: build-adp
run-adp: ## Runs ADP locally (debug, requires Datadog Agent for tagging)
ifeq ($(shell test -f /etc/datadog-agent/auth_token || echo not-found), not-found)
	$(error "Authentication token not found at /etc/datadog-agent/auth_token. Is the Datadog Agent running? Is the current user in the right group to access it?")
endif
ifeq ($(shell test -n "$(DD_API_KEY)" || echo not-found), not-found)
	$(error "API key not set. Please set the DD_API_KEY environment variable.")
endif
	@echo "[*] Running ADP..."
	@DD_DATA_PLANE_ENABLED=true DD_DATA_PLANE_DOGSTATSD_ENABLED=true \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_AUTH_TOKEN_FILE_PATH=/etc/datadog-agent/auth_token \
	target/devel/agent-data-plane run

.PHONY: run-adp-release
run-adp-release: build-adp-release
run-adp-release: ## Runs ADP locally (release, requires Datadog Agent for tagging)
ifeq ($(shell test -f /etc/datadog-agent/auth_token || echo not-found), not-found)
	$(error "Authentication token not found at /etc/datadog-agent/auth_token. Is the Datadog Agent running? Is the current user in the right group to access it?")
endif
ifeq ($(shell test -n "$(DD_API_KEY)" || echo not-found), not-found)
	$(error "API key not set. Please set the DD_API_KEY environment variable.")
endif
	@echo "[*] Running ADP..."
	@DD_DATA_PLANE_ENABLED=true DD_DATA_PLANE_DOGSTATSD_ENABLED=true \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_AUTH_TOKEN_FILE_PATH=/etc/datadog-agent/auth_token \
	target/release/agent-data-plane run

.PHONY: run-adp-standalone
run-adp-standalone: build-adp create-dummy-agent-config create-dummy-ipc-cert
run-adp-standalone: ## Runs ADP locally in standalone mode (debug)
	@echo "[*] Running ADP..."
	@DD_DATA_PLANE_STANDALONE_MODE=true DD_DATA_PLANE_DOGSTATSD_ENABLED=true \
	DD_API_KEY=api-key-adp-standalone DD_HOSTNAME=adp-standalone \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_IPC_CERT_FILE_PATH=$(ADP_STANDALONE_IPC_CERT_FILE) \
	target/devel/agent-data-plane --config /tmp/adp-empty-config.yaml run

.PHONY: run-adp-standalone-release
run-adp-standalone-release: build-adp-release create-dummy-agent-config create-dummy-ipc-cert
run-adp-standalone-release: ## Runs ADP locally in standalone mode (release)
	@echo "[*] Running ADP..."
	@DD_DATA_PLANE_STANDALONE_MODE=true DD_DATA_PLANE_DOGSTATSD_ENABLED=true \
	DD_API_KEY=api-key-adp-standalone DD_HOSTNAME=adp-standalone \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_IPC_CERT_FILE_PATH=$(ADP_STANDALONE_IPC_CERT_FILE) \
	target/release/agent-data-plane --config /tmp/adp-empty-config.yaml run

##@ Kubernetes

.PHONY: k8s-create-cluster
k8s-create-cluster: check-k8s-tools ## Creates a dedicated Kubernetes cluster (minikube)
	@echo "[*] Creating Kubernetes cluster via minikube..."
	@minikube start --profile=adp-local --container-runtime=containerd --keep-context=true

.PHONY: k8s-install-datadog-agent
k8s-install-datadog-agent: check-k8s-tools k8s-ensure-ns-datadog ## Installs the Datadog Agent (minikube)
ifeq ($(shell test -d test/k8s/charts || echo not-found), not-found)
	@echo "[*] Downloading Datadog Agent Helm chart locally..."
	@git -C test/k8s clone https://github.com/DataDog/helm-charts.git charts
	@helm repo add prometheus https://prometheus-community.github.io/helm-charts
endif
	@git -C test/k8s/charts pull origin
	@helm dependency build ./test/k8s/charts/charts/datadog
ifeq ($(shell helm --kube-context adp-local list 2>&1 | grep datadog || echo not-found), not-found)
	@echo "[*] Installing Datadog Agent..."
	@helm upgrade --install --kube-context adp-local datadog ./test/k8s/charts/charts/datadog \
		--namespace datadog \
		--values ./test/k8s/datadog-agent-values.yaml
endif

.PHONY: k8s-set-dd-api-key
k8s-set-dd-api-key: check-k8s-tools k8s-ensure-ns-datadog ## Creates API key secret for ADP and Datadog Agent (minikube)
	@echo "[*] Creating/updating Kubernetes secret(s) for Datadog Agent..."
	@minikube --profile=adp-local kubectl -- --context adp-local -n datadog delete secret datadog-secret --ignore-not-found
	@echo ${DD_API_KEY} | minikube --profile=adp-local kubectl -- --context adp-local -n datadog create secret generic datadog-secret --from-file=api-key=/dev/stdin --from-literal=app-key=fake-app-key

.PHONY: k8s-push-adp-image
k8s-push-adp-image: check-k8s-tools build-adp-image ## Loads the ADP container image (minikube)
	@echo "[*] Pushing ADP image to cluster..."
	@minikube --profile=adp-local image load local.dev/saluki-images/agent-data-plane:testing --overwrite=true

.PHONY: k8s-push-datadog-agent-image
k8s-push-datadog-agent-image: check-k8s-tools build-datadog-agent-image ## Loads the converged Datadog Agent container image (minikube)
	@echo "[*] Pushing converged Datadog Agent image to cluster..."
	@minikube --profile=adp-local image load local.dev/saluki-images/datadog-agent:testing --overwrite=true

.PHONY: k8s-deploy-statsd-generator
k8s-deploy-statsd-generator: check-k8s-tools k8s-ensure-ns-adp-testing build-gen-statsd-image ## Creates the statsd-generator deployment (minikube)
	@echo "[*] Deleting existing statsd-generator deployment..."
	@minikube --profile=adp-local kubectl -- --context adp-local -n adp-testing delete deployment statsd-generator --ignore-not-found=true
	@echo "[*] Pushing gen-statsd image to cluster..."
	@minikube --profile=adp-local image load local.dev/saluki-images/gen-statsd:testing --overwrite=true
	@echo "[*] Creating statsd-generator deployment..."
	@minikube --profile=adp-local kubectl -- --context adp-local -n adp-testing apply -f ./test/k8s/statsd-generator.yaml

.PHONY: k8s-deploy-proxy-dumper
k8s-deploy-proxy-dumper: check-k8s-tools k8s-ensure-ns-adp-testing build-proxy-dumper-image ## Creates the proxy-dumper deployment (minikube)
	@echo "[*] Deleting existing proxy-dumper deployment..."
	@minikube --profile=adp-local kubectl -- --context adp-local -n adp-testing delete deployment proxy-dumper --ignore-not-found=true
	@echo "[*] Pushing proxy-dumper image to cluster..."
	@minikube --profile=adp-local image load local.dev/saluki-images/proxy-dumper:testing --overwrite=true
	@echo "[*] Creating proxy-dumper deployment..."
	@minikube --profile=adp-local kubectl -- --context adp-local -n adp-testing apply -f ./test/k8s/proxy-dumper.yaml

.PHONY: k8s-tail-adp-logs
k8s-tail-adp-logs: check-k8s-tools ## Tails the container logs for ADP
	@minikube --profile=adp-local kubectl -- --context adp-local -n datadog logs -f daemonset/datadog -c agent-data-plane

.PHONY: k8s-tail-proxy-dumper-logs
k8s-tail-proxy-dumper-logs: check-k8s-tools ## Tails the container logs for proxy-dumper
	@minikube --profile=adp-local kubectl -- --context adp-local -n adp-testing logs -f deployment/proxy-dumper

.PHONY: k8s-ensure-ns-%
k8s-ensure-ns-%: override NS = $(@:k8s-ensure-ns-%=%)
k8s-ensure-ns-%: check-k8s-tools
	@minikube --profile=adp-local kubectl -- --context adp-local get ns ${NS} >/dev/null 2>&1 >/dev/null \
		|| (echo "[*] Creating Kubernetes namespace '${NS}'..." && minikube --profile=adp-local kubectl -- --context adp-local create ns ${NS})

.PHONY: check-k8s-tools
check-k8s-tools:
ifeq ($(shell command -v git >/dev/null || echo not-found), not-found)
	$(error "Please install git.")
endif
ifeq ($(shell command -v helm >/dev/null || echo not-found), not-found)
	$(error "Please install Helm: https://helm.sh/docs/intro/install/")
endif
ifeq ($(shell command -v minikube >/dev/null || echo not-found), not-found)
	$(error "Please install minikube: https://minikube.sigs.k8s.io/docs/start/")
endif

##@ Kind (Correctness Testing)

# kind cluster lifecycle (create, load, delete) is managed by panoramic automatically
# when running kind-runtime tests. Use --no-delete-cluster to keep the cluster alive
# between local runs.

.PHONY: check-kind-tools
check-kind-tools:
ifeq ($(shell command -v kind >/dev/null 2>&1 || echo not-found), not-found)
	$(error "Please install kind: https://kind.sigs.k8s.io/docs/user/quick-start/#installation")
endif
ifeq ($(shell command -v kubectl >/dev/null 2>&1 || echo not-found), not-found)
	$(error "Please install kubectl: https://kubernetes.io/docs/tasks/tools/")
endif

.PHONY: check-proxy-dumper-tools
check-proxy-dumper-tools:
ifeq ($(shell command -v go >/dev/null || echo not-found), not-found)
	$(error "Please install Go.")
endif
ifeq ($(shell timeout 5s git remote show https://github.com/DataDog/dd-source 2>&1 >/dev/null || echo not-available), not-available)
	$(error "Please ensure Git is configured correctly to accessing private/internal Datadog repositories.")
endif

##@ Checking

.PHONY: check-lint-tools
check-lint-tools:
ifeq ($(shell command -v vale >/dev/null || echo not-found), not-found)
	$(error "Please install Vale: https://vale.sh/docs/install")
endif

.PHONY: check-all
check-all: ## Check everything
check-all: check-fmt check-clippy check-docs check-deny check-licenses check-unused-deps generate-api-docs check-features

.PHONY: generate-api-docs
generate-api-docs: check-rust-build-tools
generate-api-docs: ## Check that API documentation builds without errors
	@echo "[*] Checking API documentation build..."
	@RUSTDOCFLAGS="--enable-index-page -Zunstable-options" cargo +nightly doc --no-deps -Zrustdoc-map --lib

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
check-fmt: check-rust-build-tools cargo-install-cargo-sort
check-fmt: ## Check that all Rust source files are formatted properly
	@echo "[*] Checking Rust source code formatting..."
	@cargo +nightly fmt -- --check
	@echo "[*] Checking Cargo.toml formatting..."
	@cargo sort --workspace --check >/dev/null

.PHONY: check-licenses
check-licenses: check-rust-build-tools cargo-install-dd-rust-license-tool
check-licenses: ## Check that the third-party license file is up to date
	@echo "[*] Checking if third-party license file is up-to-date..."
	@$(HOME)/.cargo/bin/dd-rust-license-tool check

.PHONY: check-features
check-features: check-rust-build-tools cargo-install-cargo-hack
check-features: ## Checks that all packages with feature flags can be built with different flag combinations
	@echo "[*] Checking feature flag compatibility matrix..."
	@find . -name Cargo.toml -not -path './target/*' | grep -v '^./Cargo.toml' | \
	xargs -I {} -- cargo read-manifest --manifest-path {} | \
	jq -r "select(.features | del(.default) | length > 0) | .name" | \
	xargs -I {} -- cargo hack --feature-powerset --package {} check --tests --quiet

.PHONY: check-unused-deps
check-unused-deps: check-rust-build-tools cargo-install-cargo-machete
check-unused-deps: ## Checks for any imported dependencies that are not used in code
	@echo "[*] Checking for unused dependencies..."
	@cargo machete

.PHONY: check-docs
check-docs: check-lint-tools
check-docs: ## Checks prose/code documentation against our style guide
	@echo "[*] Checking prose/code documentation against our style guide..."
	@vale --minAlertLevel=error --glob='!{lib/*/target/*,docs/.vitepress/*,*datadog_configuration*}' docs lib bin

.PHONY: sync-docs-config
sync-docs-config: check-lint-tools
sync-docs-config: ## Synchronizes the Vale configuration, updating configured style packages
	@vale sync

##@ Testing

.PHONY: test
test: check-rust-build-tools cargo-install-cargo-nextest
test: ## Runs all unit tests
	@echo "[*] Running unit tests..."
	cargo nextest run --lib --bins --no-fail-fast -E 'not test(/property_test_*/)'

.PHONY: test-property
test-property: check-rust-build-tools cargo-install-cargo-nextest
test-property: ## Runs all property tests
	@echo "[*] Running property tests..."
	cargo nextest run --lib --bins --no-fail-fast --release -E 'test(/property_test_*/)'

.PHONY: test-docs
test-docs: check-rust-build-tools
test-docs: ## Runs all doctests
	@echo "[*] Running doctests..."
	cargo test --workspace --exclude containerd-protos --exclude datadog-protos --exclude otlp-protos --doc

.PHONY: test-miri
test-miri: check-rust-build-tools ensure-rust-miri
test-miri: ## Runs all Miri-specific unit tests
	@echo "[*] Running Miri-specific unit tests..."
	cargo +nightly-2025-06-16 miri test -p stringtheory

.PHONY: test-loom
test-loom: check-rust-build-tools
test-loom: ## Runs all Loom-specific unit tests
	@echo "[*] Running Loom-specific unit tests..."
	cargo nextest run --release --features loom -p stringtheory loom_tests

.PHONY: test-all
test-all: ## Test everything
test-all: test test-property test-docs test-miri test-loom

.PHONY: test-correctness
test-correctness: build-panoramic build-correctness-tools-image build-datadog-agent-image-release
test-correctness: ## Runs the complete correctness suite (all test cases in parallel)
	@echo "[*] Running correctness test suite..."
	@target/release/panoramic run -d $(shell pwd)/test/correctness/cases $(if $(PANORAMIC_PARALLELISM),-p $(PANORAMIC_PARALLELISM))

.PHONY: test-correctness-case
test-correctness-case: build-panoramic build-correctness-tools-image build-datadog-agent-image-release
test-correctness-case: ## Runs a single correctness test case by name (usage: make test-correctness-case CASE=dsd-plain)
	@echo "[*] Running '$(CASE)' correctness test case..."
	@target/release/panoramic run -d $(shell pwd)/test/correctness/cases -t $(CASE) --no-tui

.PHONY: build-panoramic
build-panoramic: check-rust-build-tools
build-panoramic: ## Builds the panoramic binary (ADP integration test runner)
	@echo "[*] Building panoramic..."
	@cargo build --profile release --package panoramic

.PHONY: test-integration
test-integration: build-panoramic build-datadog-agent-image
test-integration: ## Runs all ADP integration tests
	@echo "[*] Running ADP integration tests..."
	@target/release/panoramic run -d $(shell pwd)/test/integration/cases $(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR))

.PHONY: test-integration-quick
test-integration-quick: build-panoramic
test-integration-quick: ## Runs ADP integration tests (assumes images already built)
	@echo "[*] Running ADP integration tests (quick mode)..."
	@target/release/panoramic run -d $(shell pwd)/test/integration/cases $(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR))

.PHONY: list-integration-tests
list-integration-tests: build-panoramic
list-integration-tests: ## Lists available ADP integration tests
	@target/release/panoramic list -d $(shell pwd)/test/integration/cases

.PHONY: build-adp-host
build-adp-host: BUILD_PROFILE ?= release
build-adp-host: check-rust-build-tools
build-adp-host: ## Builds the agent-data-plane binary for the current host (Cargo profile from $$BUILD_PROFILE, default: release)
	@echo "[*] Building agent-data-plane ($(BUILD_PROFILE), host target)..."
	@APP_FULL_NAME="$(ADP_APP_FULL_NAME)" \
		APP_SHORT_NAME="$(ADP_APP_SHORT_NAME)" \
		APP_IDENTIFIER="$(ADP_APP_IDENTIFIER)" \
		APP_GIT_HASH="$(ADP_APP_GIT_HASH)" \
		APP_VERSION="$(ADP_APP_VERSION)" \
		APP_BUILD_DATE="$(ADP_APP_BUILD_DATE)" \
		cargo build --profile $(BUILD_PROFILE) --bin agent-data-plane

.PHONY: package-adp-host
package-adp-host: BUILD_PROFILE ?= release
# Tarball-filename version. Defaults to Cargo.toml; CI overrides with $$ADP_IMAGE_VERSION.
package-adp-host: ADP_PACKAGE_VERSION ?= $(ADP_APP_VERSION)
package-adp-host: build-adp-host
package-adp-host: ## Packages agent-data-plane into a release tarball under target/release-tarball/
	@OUTPUT_DIR="$(CURDIR)/target/release-tarball" \
		BUILD_PROFILE="$(BUILD_PROFILE)" \
		TARGET_OS="$(shell uname -s | tr '[:upper:]' '[:lower:]')" \
		TARGET_ARCH="$(TARGET_ARCH)" \
		ADP_VERSION="$(ADP_PACKAGE_VERSION)" \
		SPDX_LICENSES_VERSION="$(ADP_SPDX_LICENSES_VERSION)" \
		$(CURDIR)/ci/tooling/package-adp-tarball.sh

.PHONY: test-integration-macos-run
# ADP path tracks $$BUILD_PROFILE so a tagged release pipeline (BUILD_PROFILE=optimized-release
# in .gitlab-ci.yml workflow rules) tests the same binary it's about to ship — mirroring how the
# linux flow builds and tests build-adp-image with whatever BUILD_PROFILE the pipeline sets.
# panoramic stays at target/release/ unconditionally because it's the test harness, not the SUT;
# build-panoramic always builds with --profile release, same as linux's build-panoramic-binary.
test-integration-macos-run: BUILD_PROFILE ?= release
test-integration-macos-run: ## Runs the macOS host-process integration tests using already-built binaries (assumes target/$$BUILD_PROFILE/agent-data-plane and target/release/panoramic exist). Defaults to all `mac`-runtime-eligible tests; narrow with CASE=<name>.
	@echo "[*] Running macOS host-process integration tests..."
	@ADP_BINARY_PATH="$(CURDIR)/target/$(BUILD_PROFILE)/agent-data-plane" \
		CORE_AGENT_BINARY_PATH="$(MACOS_TEST_AGENT_INSTALL_DIR)/bin/agent/agent" \
		target/release/panoramic run -d "$(CURDIR)/test/integration/cases" \
		$(if $(CASE),-t $(CASE)) --no-tui -p 1 \
		$(if $(PANORAMIC_LOG_DIR),-l $(PANORAMIC_LOG_DIR))

.PHONY: provision-macos-test-env
provision-macos-test-env: ## Installs the pinned Datadog Agent ($(MACOS_TEST_AGENT_VERSION)) into $(MACOS_TEST_AGENT_INSTALL_DIR) (a sandbox under /tmp) and bootstraps the IPC cert. Idempotent: re-uses the install if it already matches the pinned version.
	@echo "[*] Provisioning macOS test environment..."
	@if [ "$(shell uname -s)" != "Darwin" ]; then \
		echo "provision-macos-test-env only runs on macOS hosts" >&2; exit 1; \
	fi
	@if [ -x $(MACOS_TEST_AGENT_INSTALL_DIR)/bin/agent/agent ] && \
	   [ "$$($(MACOS_TEST_AGENT_INSTALL_DIR)/bin/agent/agent version 2>/dev/null | awk '{print $$2}')" = "$(MACOS_TEST_AGENT_VERSION)" ]; then \
		echo "[*] Datadog Agent $(MACOS_TEST_AGENT_VERSION) already extracted to $(MACOS_TEST_AGENT_INSTALL_DIR)"; \
	else \
		echo "[*] Installing Datadog Agent $(MACOS_TEST_AGENT_VERSION) into $(MACOS_TEST_AGENT_INSTALL_DIR)..."; \
		mkdir -p $(MACOS_TEST_AGENT_DMG_DIR); \
		DMG_PATH=$(MACOS_TEST_AGENT_DMG_DIR)/datadog-agent-$(MACOS_TEST_AGENT_VERSION).dmg; \
		if [ ! -f "$$DMG_PATH" ]; then \
			curl -fL "$(MACOS_TEST_AGENT_DMG_URL)" -o "$$DMG_PATH"; \
		fi; \
		MOUNT_DIR=$$(mktemp -d /tmp/saluki-dda-mount-XXXXXX); \
		hdiutil attach "$$DMG_PATH" -mountpoint "$$MOUNT_DIR" -nobrowse >/dev/null; \
		PKG=$$(find "$$MOUNT_DIR" -name '*.pkg' | head -1); \
		EXPAND_DIR=$$(mktemp -d /tmp/saluki-dda-expand-XXXXXX) && rm -rf "$$EXPAND_DIR"; \
		pkgutil --expand-full "$$PKG" "$$EXPAND_DIR" >/dev/null; \
		hdiutil detach "$$MOUNT_DIR" >/dev/null; \
		rmdir "$$MOUNT_DIR" 2>/dev/null || true; \
		PAYLOAD_DIR=$$(find "$$EXPAND_DIR" -type d -name Payload | head -1); \
		if [ -z "$$PAYLOAD_DIR" ] || [ ! -x "$$PAYLOAD_DIR/bin/agent/agent" ]; then \
			echo "ERROR: pkg payload did not contain bin/agent/agent. Expanded layout:" >&2; \
			find "$$EXPAND_DIR" -maxdepth 3 -type d >&2; \
			exit 1; \
		fi; \
		rm -rf $(MACOS_TEST_AGENT_INSTALL_DIR); \
		mkdir -p $$(dirname $(MACOS_TEST_AGENT_INSTALL_DIR)); \
		mv "$$PAYLOAD_DIR" $(MACOS_TEST_AGENT_INSTALL_DIR); \
		rm -rf "$$EXPAND_DIR"; \
		test -x $(MACOS_TEST_AGENT_INSTALL_DIR)/bin/agent/agent; \
	fi
	@if [ ! -f $(MACOS_TEST_AGENT_INSTALL_DIR)/etc/ipc_cert.pem ] || [ ! -f $(MACOS_TEST_AGENT_INSTALL_DIR)/etc/auth_token ]; then \
		echo "[*] Bootstrapping IPC cert + auth_token by running the Agent briefly..."; \
		mkdir -p $(MACOS_TEST_AGENT_INSTALL_DIR)/etc $(MACOS_TEST_AGENT_INSTALL_DIR)/run; \
		touch $(MACOS_TEST_AGENT_INSTALL_DIR)/etc/datadog.yaml; \
		DD_API_KEY=bootstrap DD_HOSTNAME=bootstrap \
			DD_RUN_PATH=$(MACOS_TEST_AGENT_INSTALL_DIR)/run \
			DD_AUTH_TOKEN_FILE_PATH=$(MACOS_TEST_AGENT_INSTALL_DIR)/etc/auth_token \
			DD_IPC_CERT_FILE_PATH=$(MACOS_TEST_AGENT_INSTALL_DIR)/etc/ipc_cert.pem \
			DD_CMD_PORT=55001 DD_GUI_PORT=-1 \
			DD_EXPVAR_PORT=55000 DD_APM_RECEIVER_PORT=58126 \
			DD_PROCESS_CONFIG_CMD_PORT=56062 DD_AGENT_IPC_PORT=55004 \
			DD_DOGSTATSD_PORT=58125 \
			$(MACOS_TEST_AGENT_INSTALL_DIR)/bin/agent/agent run -c $(MACOS_TEST_AGENT_INSTALL_DIR)/etc >/tmp/saluki-agent-bootstrap.log 2>&1 & \
		AGENT_PID=$$!; \
		for i in $$(seq 1 30); do \
			sleep 1; \
			if [ -f $(MACOS_TEST_AGENT_INSTALL_DIR)/etc/ipc_cert.pem ] && [ -f $(MACOS_TEST_AGENT_INSTALL_DIR)/etc/auth_token ]; then break; fi; \
		done; \
		kill $$AGENT_PID 2>/dev/null || true; \
		wait $$AGENT_PID 2>/dev/null || true; \
		if [ ! -f $(MACOS_TEST_AGENT_INSTALL_DIR)/etc/ipc_cert.pem ]; then \
			echo "ERROR: bootstrap Agent did not write the IPC cert. Bootstrap log:" >&2; \
			cat /tmp/saluki-agent-bootstrap.log >&2 2>/dev/null || true; \
			exit 1; \
		fi; \
	else \
		echo "[*] IPC cert already present at $(MACOS_TEST_AGENT_INSTALL_DIR)/etc/ipc_cert.pem"; \
	fi
	@echo "[*] macOS test environment ready."
	@echo "[*] Agent binary: $(MACOS_TEST_AGENT_INSTALL_DIR)/bin/agent/agent"

.PHONY: test-integration-macos-ci
test-integration-macos-ci: build-panoramic build-adp-host provision-macos-test-env test-integration-macos-run ## CI entry point: builds binaries, ensures Agent + cert are provisioned, then runs the `mac`-runtime integration tests

.PHONY: ensure-rust-miri
ensure-rust-miri:
ifeq ($(shell command -v rustup >/dev/null || echo not-found), not-found)
	$(error "Rustup must be present to install nightly toolchain/Miri component: https://www.rust-lang.org/tools/install")
endif
	@echo "[*] Installing/updating nightly Rust (2025-06-16) and Miri component..."
	@rustup toolchain install nightly-2025-06-16 --component miri
	@echo "[*] Ensuring Miri is setup..."
	@cargo +nightly-2025-06-16 miri setup

##@ Antithesis

ANTITHESIS_CONFIG_DIR := test/antithesis/deploy
ANTITHESIS_COMPOSE_FILE := $(ANTITHESIS_CONFIG_DIR)/docker-compose.yaml

.PHONY: check-antithesis-tools
check-antithesis-tools:
ifeq ($(shell command -v snouty >/dev/null || echo not-found), not-found)
	$(error "snouty must be present to validate the Antithesis harness, see https://github.com/antithesishq/snouty")
endif

.PHONY: antithesis-build
antithesis-build: ## Builds the Antithesis harness container images
	@echo "[*] Building Antithesis harness images..."
	@docker compose -f $(ANTITHESIS_COMPOSE_FILE) build

.PHONY: antithesis-validate
antithesis-validate: check-antithesis-tools antithesis-build
antithesis-validate: ## Validates the Antithesis harness: builds images, runs 'snouty validate'
	@echo "[*] Validating Antithesis harness with snouty..."
	@snouty validate $(ANTITHESIS_CONFIG_DIR)

##@ Profiling

.PHONY: profile-run-blackhole
profile-run-blackhole: cargo-install-dummyhttp
profile-run-blackhole: ## Runs a blackhole HTTP server for use by ADP
	@echo "[*] Running blackhole HTTP server at localhost:9095..."
	@dummyhttp -v -c 202 -p 9095

.PHONY: profile-run-adp
profile-run-adp: ensure-ddprof build-adp-release
profile-run-adp: ## Runs ADP locally for profiling (via ddprof)
ifeq ($(shell test -S /var/run/datadog/apm.socket || echo not-found), not-found)
	$(error "APM socket at /var/run/datadog/apm.socket not found. Is the Datadog Agent running?")
endif
	@echo "[*] Running ADP under ddprof (service: adp, environment: local, version: $(GIT_COMMIT))..."
	@DD_API_KEY=api-key-adp-profiling DD_HOSTNAME=adp-profiling DD_DD_URL=http://127.0.0.1:9095 \
	DD_DATA_PLANE_STANDALONE_MODE=true \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_ADP_OTLP_ENABLED=true DD_OTLP_CONFIG="{}" \
	./test/ddprof/bin/ddprof --service adp --environment local --service-version $(GIT_COMMIT) \
	--url unix:///var/run/datadog/apm.socket \
	--inlined-functions true --timeline --upload-period 10 --preset cpu_live_heap \
	target/release/agent-data-plane run

.PHONY: generate-smp-experiments
generate-smp-experiments: ## Generates SMP experiment configs from experiments.yaml
	@echo "[*] Generating SMP experiment configurations..."
	@python3 test/smp/regression/adp/generate_experiments.py

.PHONY: check-smp-experiments
check-smp-experiments: ## Verifies SMP experiment configs are up-to-date (CI)
	@echo "[*] Checking SMP experiment configurations..."
	@python3 test/smp/regression/adp/generate_experiments.py --check

.PHONY: profile-run-smp-experiment
profile-run-smp-experiment: ## Runs a specific SMP experiment for Saluki
ifeq ($(shell test -f test/smp/regression/adp/cases/$(EXPERIMENT)/lading/lading.yaml || echo not-found), not-found)
	$(error "Lading configuration for '$(EXPERIMENT)' not found. (test/smp/regression/adp/cases/$(EXPERIMENT)/lading/lading.yaml) ")
endif
	@echo "[*] Running '$(EXPERIMENT)' experiment (15 minutes)..."
	@docker run --rm --network host \
	    --mount type=bind,source=./test/smp/regression/adp/cases/$(EXPERIMENT)/lading/lading.yaml,target=/tmp/lading.yaml \
		--mount type=bind,source=/tmp/adp-dogstatsd-dgram.sock,target=/tmp/adp-dogstatsd-dgram.sock \
		--mount type=bind,source=/tmp/adp-dogstatsd-stream.sock,target=/tmp/adp-dogstatsd-stream.sock \
		ghcr.io/datadog/lading:$(LADING_VERSION) \
		--config-path /tmp/lading.yaml --no-target --warmup-duration-seconds 1 --experiment-duration-seconds 900 --prometheus-addr 127.0.0.1:9229

.PHONY: ensure-ddprof
ensure-ddprof:
ifeq ($(shell test -f test/ddprof/bin/ddprof || echo not-found), not-found)
	@echo "[*] Downloading ddprof v$(DDPROF_VERSION)..."
	@curl -q -L -o /tmp/ddprof.tar.xz https://github.com/DataDog/ddprof/releases/download/v$(DDPROF_VERSION)/ddprof-$(DDPROF_VERSION)-$(TARGET_ARCH)-linux.tar.xz
	@tar -C test -xf /tmp/ddprof.tar.xz
	@rm -f /tmp/ddprof.tar.xz
endif

##@ Development

.PHONY: fast-edit-test
fast-edit-test: fmt sync-licenses check-clippy check-deny check-licenses test-all
fast-edit-test: ## Runs a lightweight format/lint/test pass

##@ CI

.PHONY: emit-adp-build-metadata
emit-adp-build-metadata: ## Emits ADP build metadata shell variables suitable for use during image builds
	@echo "APP_FULL_NAME=${ADP_APP_FULL_NAME}"
	@echo "APP_SHORT_NAME=${ADP_APP_SHORT_NAME}"
	@echo "APP_IDENTIFIER=${ADP_APP_IDENTIFIER}"
	@echo "APP_GIT_HASH=${ADP_APP_GIT_HASH}"
	@echo "APP_VERSION=${ADP_APP_VERSION}"
	@echo "APP_BUILD_TIME=${ADP_APP_BUILD_TIME}"

.PHONY: bump-adp-version
bump-adp-version: ## Creates a PR branch that bumps the ADP patch version
	$(eval CURRENT_VERSION := $(shell grep -E '^version = "' bin/agent-data-plane/Cargo.toml | head -n 1 | cut -d '"' -f 2))
	$(eval MAJOR := $(shell echo $(CURRENT_VERSION) | cut -d '.' -f 1))
	$(eval MINOR := $(shell echo $(CURRENT_VERSION) | cut -d '.' -f 2))
	$(eval PATCH := $(shell echo $(CURRENT_VERSION) | cut -d '.' -f 3))
	$(eval NEW_PATCH := $(shell echo $$(($(PATCH) + 1))))
	$(eval NEW_VERSION := $(MAJOR).$(MINOR).$(NEW_PATCH))
	@echo "[*] Bumping ADP from $(CURRENT_VERSION) to $(NEW_VERSION)"
	@git fetch origin main
	@git checkout -b bump-adp-version-$(NEW_VERSION) origin/main
	@sed -i 's/^version = "$(CURRENT_VERSION)"/version = "$(NEW_VERSION)"/' bin/agent-data-plane/Cargo.toml
	@cargo update -p agent-data-plane --quiet
	@git add bin/agent-data-plane/Cargo.toml Cargo.lock
	@git commit -m "chore(agent-data-plane): bump version to $(NEW_VERSION)"
	@echo "[*] Created branch 'bump-adp-version-$(NEW_VERSION)' with version bump commit."

##@ Docs

.PHONY: check-js-build-tools
check-js-build-tools:
ifeq ($(shell command -v bun >/dev/null || echo not-found), not-found)
	$(error "Please install Bun: https://bun.sh/docs/installation")
endif

.PHONY: run-docs
run-docs: check-js-build-tools
run-docs: ## Runs a local development server for documentation
	@echo "[*] Running local development server for documentation..."
	@bun install
	@bun run docs:dev

##@ Utility

.PHONY: update-protos
update-protos: ## Updates all vendored Protocol Buffers definitions from their source repositories
	@DD_AGENT_GIT_TAG=$(PROTOBUF_SRC_REPO_DD_AGENT) \
	AGENT_PAYLOAD_GIT_TAG=$(PROTOBUF_SRC_REPO_AGENT_PAYLOAD) \
	CONTAINERD_GIT_TAG=$(PROTOBUF_SRC_REPO_CONTAINERD) \
	SKETCHES_GO_GIT_TAG=$(PROTOBUF_SRC_REPO_SKETCHES_GO) \
	./ci/tooling/update-protos.sh

.PHONY: update-pr-title-scopes
update-pr-title-scopes: ## Updates allowed PR title scopes in the CI workflow based on the codebase
	@./ci/tooling/update-pr-title-scopes.sh update

.PHONY: check-pr-title-scopes
check-pr-title-scopes: ## Checks that PR title scopes in the CI workflow are up-to-date
	@./ci/tooling/update-pr-title-scopes.sh check

.PHONY: clean
clean: check-rust-build-tools
clean: ## Clean all build artifacts (debug/release)
	@echo "[*] Cleaning Rust build artifacts..."
	@cargo clean

.PHONY: clean-docker
clean-docker: ## Cleans up Docker build cache
	@echo "[*] Cleaning Docker cache..."
	@docker builder prune --filter type=exec.cachemount --force

.PHONY: clean-airlock
clean-airlock: ## Cleans up Airlock-related resources in Docker (used for correctness tests)
	@echo "[*] Cleaning Airlock-related resources..."
	@docker container ls --filter label=created_by=airlock -q | xargs -r docker container rm -f
	@docker volume ls --filter label=created_by=airlock -q | xargs -r docker volume rm -f
	@docker network ls --filter label=created_by=airlock -q | xargs -r docker network rm -f

.PHONY: clean-kind
clean-kind: check-kind-tools ## Cleans up orphaned panoramic namespaces in the kind cluster (used for correctness tests)
	@echo "[*] Cleaning panoramic-kind namespaces..."
	@kubectl get namespace -l created-by=panoramic-kind -o name | xargs -r kubectl delete

.PHONY: clean-correctness
clean-correctness: clean-airlock clean-kind ## Cleans up all orphaned correctness test resources (Docker + kind)

.PHONY: fmt
fmt: check-rust-build-tools cargo-install-cargo-autoinherit cargo-install-cargo-sort
fmt: ## Format Rust source code
	@echo "[*] Formatting Rust source code..."
	@cargo +nightly fmt
	@echo "[*] Ensuring workspace dependencies are autoinherited..."
	@cargo autoinherit 2>/dev/null
	@echo "[*] Formatting Cargo.toml files..."
	@cargo sort --workspace >/dev/null

.PHONY: sync-licenses
sync-licenses: check-rust-build-tools cargo-install-dd-rust-license-tool
sync-licenses: ## Synchronizes the third-party license file with the current crate dependencies
	@echo "[*] Synchronizing third-party license file to current dependencies..."
	@$(HOME)/.cargo/bin/dd-rust-license-tool write

.PHONY: setup-hooks
setup-hooks: ## Configure Git to use the committed hooks in .githooks/
	@git config core.hooksPath .githooks
	@echo "[*] Git hooks configured. Pre-commit checks will run on each commit."
	@echo "[*] To skip hooks on a specific commit, use: git commit --no-verify"

.PHONY: cargo-preinstall
cargo-preinstall: cargo-install-dd-rust-license-tool cargo-install-cargo-deny cargo-install-cargo-hack
cargo-preinstall: cargo-install-cargo-nextest cargo-install-cargo-autoinherit cargo-install-cargo-sort
cargo-preinstall: cargo-install-dummyhttp cargo-install-cargo-machete cargo-install-rustfilt
cargo-preinstall: ## Pre-installs all necessary Cargo tools (used for CI)
	@echo "[*] Pre-installed all necessary Cargo tools!"

.PHONY: cargo-install-%
cargo-install-%: override TOOL = $(@:cargo-install-%=%)
cargo-install-%: override VERSIONED_TOOL = ${TOOL}@$(CARGO_TOOL_VERSION_$(TOOL))
cargo-install-%: check-rust-build-tools
	@$(if $(findstring true,$(AUTOINSTALL)),test -f ${CARGO_BIN_DIR}/${TOOL} || (echo "[*] Installing ${VERSIONED_TOOL}..." && cargo binstall --strategies ${CARGO_BINSTALL_STRATEGIES} ${VERSIONED_TOOL} --quiet -y),)
