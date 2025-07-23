# .PHONY: $(MAKECMDGOALS) all
.DEFAULT_GOAL := help

mkfile_path := $(abspath $(lastword $(MAKEFILE_LIST)))
mkfile_dir := $(dir $(mkfile_path))

export TARGET_ARCH ?= $(shell uname -m | sed s/x86_64/amd64/ | sed s/aarch64/arm64/)
export TARGET_TRIPLE ?= $(shell command -v rustc 1>/dev/null && rustc -vV | sed -n 's|host: ||p')

# High-level settings that ultimately get passed down to build-specific targets.
export APP_FULL_NAME ?= Agent Data Plane
export CHECKS_FULL_NAME ?= Checks Agent
export APP_SHORT_NAME ?= data-plane
export APP_IDENTIFIER ?= adp
export CHECKS_SHORT_NAME ?= checks-agent
export CHECKS_IDENTIFIER ?= checks-agent
export APP_GIT_HASH ?= $(shell git rev-parse --short HEAD 2>/dev/null || echo not-in-git)
export APP_VERSION ?= $(shell cat bin/agent-data-plane/Cargo.toml | grep -E "^version = \"" | head -n 1 | cut -d '"' -f 2)

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
export ADP_BUILD_IMAGE ?= rust:$(RUST_VERSION)-bookworm
export ADP_APP_IMAGE ?= ubuntu:24.04
export GO_BUILD_IMAGE ?= golang:1.23-bullseye
export GO_APP_IMAGE ?= ubuntu:24.04
export CARGO_BIN_DIR ?= $(shell echo "${HOME}/.cargo/bin")
export GIT_COMMIT ?= $(shell git rev-parse --short HEAD 2>/dev/null || echo not-in-git)

# Specific versions of various tools we use.
export CARGO_TOOL_VERSION_dd-rust-license-tool ?= 1.0.3
export CARGO_TOOL_VERSION_cargo-deny ?= 0.18.3
export CARGO_TOOL_VERSION_cargo-hack ?= 0.6.30
export CARGO_TOOL_VERSION_cargo-nextest ?= 0.9.99
export CARGO_TOOL_VERSION_cargo-autoinherit ?= 0.1.5
export CARGO_TOOL_VERSION_cargo-sort ?= 1.0.9
export CARGO_TOOL_VERSION_dummyhttp ?= 1.1.0
export DDPROF_VERSION ?= 0.19.0
export LADING_VERSION ?= 0.23.3

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
build-adp: ## Builds the ADP binary in debug mode
	@echo "[*] Building ADP locally..."
	@cargo build --profile dev --package agent-data-plane

.PHONY: build-checks-agent
build-checks-agent: check-rust-build-tools
build-checks-agent: ## Builds the Checks Agent binary in debug mode
	@echo "[*] Building Checks Agent locally..."
	@cargo build --profile dev --package checks-agent

.PHONY: build-adp-release
build-adp-release: check-rust-build-tools
build-adp-release: ## Builds the ADP binary in release mode
	@echo "[*] Building ADP locally..."
	@cargo build --profile release --package agent-data-plane

.PHONY: build-adp-image
build-adp-image: ## Builds the ADP container image in release mode ('latest' tag)
	@echo "[*] Building ADP image..."
	@$(CONTAINER_TOOL) build \
		--tag saluki-images/agent-data-plane:latest \
		--tag local.dev/saluki-images/agent-data-plane:testing \
		--build-arg "BUILD_IMAGE=$(ADP_BUILD_IMAGE)" \
		--build-arg "APP_IMAGE=$(ADP_APP_IMAGE)" \
		--build-arg "APP_FULL_NAME=$(APP_FULL_NAME)" \
		--build-arg "APP_SHORT_NAME=$(APP_SHORT_NAME)" \
		--build-arg "APP_IDENTIFIER=$(APP_IDENTIFIER)" \
		--build-arg "APP_VERSION=$(APP_VERSION)" \
		--build-arg "APP_GIT_HASH=$(APP_GIT_HASH)" \
		--file ./docker/Dockerfile.agent-data-plane \
		.

.PHONY: build-checks-agent-image
build-checks-agent-image: ## Builds the ADP container image in release mode ('latest' tag)
	@echo "[*] Building Check Agent image..."
	@$(CONTAINER_TOOL) build \
		--tag saluki-images/check-agent:latest \
		--tag local.dev/saluki-images/check-agent:testing \
		--build-arg "BUILD_IMAGE=$(ADP_BUILD_IMAGE)" \
		--build-arg "APP_IMAGE=$(ADP_APP_IMAGE)" \
		--build-arg "APP_FULL_NAME=$(CHECKS_FULL_NAME)" \
		--build-arg "APP_SHORT_NAME=$(CHECKS_SHORT_NAME)" \
		--build-arg "APP_IDENTIFIER=$(CHECKS_IDENTIFIER)" \
		--build-arg "APP_VERSION=$(APP_VERSION)" \
		--build-arg "APP_GIT_HASH=$(APP_GIT_HASH)" \
		--file ./docker/Dockerfile.checks-agent \
		.

.PHONY: build-datadog-agent-image
build-datadog-agent-image: build-adp-image ## Builds a converged Datadog Agent container image containing ADP ('latest' tag)
	@echo "[*] Building converged Datadog Agent image..."
	@$(CONTAINER_TOOL) build \
		--tag saluki-images/datadog-agent:latest \
		--tag local.dev/saluki-images/datadog-agent:testing \
		--file ./docker/Dockerfile.datadog-agent \
		.

.PHONY: build-gen-statsd-image
build-gen-statsd-image: ## Builds the gen-statsd container image ('latest' tag)
	@echo "[*] Building gen-statsd image..."
	@$(CONTAINER_TOOL) build \
		--tag saluki-images/gen-statsd:latest \
		--tag local.dev/saluki-images/gen-statsd:testing \
		--build-arg BUILD_IMAGE=$(GO_BUILD_IMAGE) \
		--build-arg APP_IMAGE=$(GO_APP_IMAGE) \
		--file ./docker/Dockerfile.gen-statsd \
		.

.PHONY: build-ground-truth
build-ground-truth: check-rust-build-tools
build-ground-truth: ## Builds the ground-truth binary in debug mode
	@echo "[*] Building ground-truth locally..."
	@cargo build --profile dev --package ground-truth

.PHONY: build-metrics-intake-image
build-metrics-intake-image: ## Builds the metrics-intake container image in release mode ('latest' tag)
	@echo "[*] Building metrics-intake image..."
	@$(CONTAINER_TOOL) build \
		--tag saluki-images/metrics-intake:latest \
		--tag local.dev/saluki-images/metrics-intake:testing \
		--build-arg BUILD_IMAGE=$(ADP_BUILD_IMAGE) \
		--build-arg APP_IMAGE=$(ADP_APP_IMAGE) \
		--file ./docker/Dockerfile.metrics-intake \
		.

.PHONY: build-millstone-image
build-millstone-image: ## Builds the millstone container image in release mode ('latest' tag)
	@echo "[*] Building millstone image..."
	@$(CONTAINER_TOOL) build \
		--tag saluki-images/millstone:latest \
		--tag local.dev/saluki-images/millstone:testing \
		--build-arg BUILD_IMAGE=$(ADP_BUILD_IMAGE) \
		--build-arg APP_IMAGE=$(ADP_APP_IMAGE) \
		--file ./docker/Dockerfile.millstone \
		.

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
	@$(CONTAINER_TOOL) build \
		--tag saluki-images/proxy-dumper:latest \
		--tag local.dev/saluki-images/proxy-dumper:testing \
		--build-arg BUILD_IMAGE=$(GO_BUILD_IMAGE) \
		--build-arg APP_IMAGE=$(GO_APP_IMAGE) \
		--file ./docker/Dockerfile.proxy-dumper \
		test/build/dd-agent-benchmarks/docker/proxy-dumper

.PHONY: build-dsd-client
build-dsd-client: ## Builds the Dogstatsd client (used for sending DSD payloads)
	@echo "[*] Building Dogstatsd client..."
	@go build -C tooling/dogstatsd_client -o ../bin/dogstatsd_client .

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

##@ Running

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
	@DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_TELEMETRY_ENABLED=true DD_PROMETHEUS_LISTEN_ADDR=tcp://127.0.0.1:5102 \
	DD_AUTH_TOKEN_FILE_PATH=/etc/datadog-agent/auth_token \
	target/debug/agent-data-plane

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
	@DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_TELEMETRY_ENABLED=true DD_PROMETHEUS_LISTEN_ADDR=tcp://127.0.0.1:5102 \
	DD_AUTH_TOKEN_FILE_PATH=/etc/datadog-agent/auth_token \
	target/release/agent-data-plane

.PHONY: run-adp-standalone
run-adp-standalone: build-adp
run-adp-standalone: ## Runs ADP locally in standalone mode (debug)
	@echo "[*] Running ADP..."
	@DD_ADP_STANDALONE_MODE=true \
	DD_API_KEY=api-key-adp-standalone DD_HOSTNAME=adp-standalone \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_TELEMETRY_ENABLED=true DD_PROMETHEUS_LISTEN_ADDR=tcp://127.0.0.1:5102 \
	target/debug/agent-data-plane

.PHONY: run-checks-agent-standalone
run-checks-agent-standalone: build-checks-agent
run-checks-agent-standalone: ## Runs Checks Agent locally in standalone mode (debug)
	@echo "[*] Running Checks Agent..."
	@DD_ADP_STANDALONE_MODE=true \
	DD_API_KEY=api-key-adp-standalone DD_HOSTNAME=check-agent-standalone \
	DD_CHECKS_CONFIG_DIR=./dist/conf.d \
	DD_TELEMETRY_ENABLED=true DD_PROMETHEUS_LISTEN_ADDR=tcp://127.0.0.1:5102 \
	target/debug/checks-agent

.PHONY: run-checks-agent
run-checks-agent: build-checks-agent
run-checks-agent: ## Runs Checks Agent alongside the core Agent (debug)
	@echo "[*] Running Checks Agent..."
	@DD_ADP_STANDALONE_MODE=false \
	DD_AUTH_TOKEN_FILE_PATH=../datadog-agent/bin/agent/dist/auth_token \
	DD_API_KEY=api-key-adp-standalone DD_HOSTNAME=check-agent-standalone \
	DD_TELEMETRY_ENABLED=true DD_PROMETHEUS_LISTEN_ADDR=tcp://127.0.0.1:5102 \
	target/debug/checks-agent

.PHONY: run-adp-standalone-release
run-adp-standalone-release: build-adp-release
run-adp-standalone-release: ## Runs ADP locally in standalone mode (release)
	@echo "[*] Running ADP..."
	@DD_ADP_STANDALONE_MODE=true \
	DD_API_KEY=api-key-adp-standalone DD_HOSTNAME=adp-standalone \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_TELEMETRY_ENABLED=true DD_PROMETHEUS_LISTEN_ADDR=tcp://127.0.0.1:5102 \
	target/release/agent-data-plane

.PHONY: run-dsd-basic-udp
run-dsd-basic-udp: build-dsd-client ## Runs a basic set of metrics via the Dogstatsd client (UDP)
	@echo "[*] Sending basic metrics via Dogstatsd (UDP, 127.0.0.1:9191)..."
	@./tooling/bin/dogstatsd_client 127.0.0.1:9191 count:1,gauge:2,histogram:3,distribution:4,set:five

.PHONY: run-dsd-basic-uds
run-dsd-basic-uds: build-dsd-client ## Runs a basic set of metrics via the Dogstatsd client (UDS)
	@echo "[*] Sending basic metrics via Dogstatsd (unixgram:///tmp/adp-dogstatsd-dgram.sock)..."
	@./tooling/bin/dogstatsd_client unixgram:///tmp/adp-dogstatsd-dgram.sock count:1,gauge:2,histogram:3,distribution:4,set:five

.PHONY: run-dsd-basic-uds-stream
run-dsd-basic-uds-stream: build-dsd-client ## Runs a basic set of metrics via the Dogstatsd client (UDS Stream)
	@echo "[*] Sending basic metrics via Dogstatsd (unix:///tmp/adp-dogstatsd-stream.sock)..."
	@./tooling/bin/dogstatsd_client unix:///tmp/adp-dogstatsd-stream.sock count:1,gauge:2,histogram:3,distribution:4,set:five

##@ Kubernetes

.PHONY: k8s-create-cluster
k8s-create-cluster: check-k8s-tools ## Creates a dedicated Kubernetes cluster (minikube)
	@echo "[*] Creating Kubernetes cluster via minikube..."
	@minikube start --profile=adp-local --container-runtime=containerd --keep-context=true

.PHONY: k8s-install-datadog-agent
k8s-install-datadog-agent: check-k8s-tools k8s-ensure-ns-datadog ## Installs the Datadog Agent (minikube)
ifeq ($(shell test -d test/k8s/charts || echo not-found), not-found)
	@echo "[*] Downloading Datadog Agent Helm chart locally..."
	@git -C test/k8s clone --single-branch --branch=saluki/adp-container \
		https://github.com/DataDog/helm-charts.git charts
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

.PHONY: check-proxy-dumper-tools
check-proxy-dumper-tools:
ifeq ($(shell command -v go >/dev/null || echo not-found), not-found)
	$(error "Please install Go.")
endif
ifeq ($(shell timeout 5s git remote show https://github.com/DataDog/dd-source 2>&1 >/dev/null || echo not-available), not-available)
	$(error "Please ensure Git is configured correctly to accessing private/internal Datadog repositories.")
endif

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

##@ Testing

.PHONY: test
test: check-rust-build-tools cargo-install-cargo-nextest
test: ## Runs all unit tests
	@echo "[*] Running unit tests..."
	cargo nextest run --lib -E 'not test(/property_test_*/)'

.PHONY: test-property
test-property: check-rust-build-tools cargo-install-cargo-nextest
test-property: ## Runs all property tests
	@echo "[*] Running property tests..."
	cargo nextest run --lib --release -E 'test(/property_test_*/)'

.PHONY: test-docs
test-docs: check-rust-build-tools
test-docs: ## Runs all doctests
	@echo "[*] Running doctests..."
	cargo test --workspace --exclude datadog-protos --doc

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
test-correctness: build-ground-truth
test-correctness: ## Runs the metrics correctness (ground-truth) suite
	@echo "[*] Running correctness suite..."
	@echo "[*] Running 'no-origin-detection' test case..."
	@target/debug/ground-truth \
		--millstone-image saluki-images/millstone:latest \
		--millstone-config-path $(shell pwd)/test/correctness/millstone.yaml \
		--metrics-intake-image saluki-images/metrics-intake:latest \
		--metrics-intake-config-path $(shell pwd)/test/correctness/metrics-intake.yaml \
		--dsd-image docker.io/datadog/dogstatsd:7.67.1 \
		--dsd-config-path $(shell pwd)/test/correctness/datadog-no-origin-detection.yaml \
		--adp-image saluki-images/agent-data-plane:latest \
		--adp-config-path $(shell pwd)/test/correctness/datadog-no-origin-detection.yaml

.PHONY: test-correctness-origin-detection
test-correctness-origin-detection: build-ground-truth
test-correctness-origin-detection: ## Runs the metrics correctness (ground-truth) suite (origin detection)
	@echo "[*] Running correctness suite..."
	@echo "[*] Running 'origin-detection' test case..."
	@target/debug/ground-truth \
		--millstone-image saluki-images/millstone:latest \
		--millstone-config-path $(shell pwd)/test/correctness/millstone.yaml \
		--metrics-intake-image saluki-images/metrics-intake:latest \
		--metrics-intake-config-path $(shell pwd)/test/correctness/metrics-intake.yaml \
		--dsd-image saluki-images/datadog-agent:latest \
		--dsd-entrypoint /bin/entrypoint.sh \
		--dsd-command /init \
		--dsd-config-path $(shell pwd)/test/correctness/datadog-origin-detection.yaml \
		--adp-image saluki-images/datadog-agent:latest \
		--adp-command /init \
		--adp-config-path $(shell pwd)/test/correctness/datadog-origin-detection.yaml \
		--adp-env-arg DD_ADP_ENABLED=true \
		--adp-env-arg DD_AGGREGATE_CONTEXT_LIMIT=500000

.PHONY: ensure-rust-miri
ensure-rust-miri:
ifeq ($(shell command -v rustup >/dev/null || echo not-found), not-found)
	$(error "Rustup must be present to install nightly toolchain/Miri component: https://www.rust-lang.org/tools/install")
endif
	@echo "[*] Installing/updating nightly Rust (2025-06-16) and Miri component..."
	@rustup toolchain install nightly-2025-06-16 --component miri
	@echo "[*] Ensuring Miri is setup..."
	@cargo +nightly-2025-06-16 miri setup

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
	DD_ADP_STANDALONE_MODE=true \
	DD_DOGSTATSD_PORT=9191 DD_DOGSTATSD_SOCKET=/tmp/adp-dogstatsd-dgram.sock DD_DOGSTATSD_STREAM_SOCKET=/tmp/adp-dogstatsd-stream.sock \
	DD_TELEMETRY_ENABLED=true DD_PROMETHEUS_LISTEN_ADDR=tcp://127.0.0.1:5102 \
	./test/ddprof/bin/ddprof --service adp --environment local --service-version $(GIT_COMMIT) \
	--url unix:///var/run/datadog/apm.socket \
	--inlined-functions true --timeline --upload-period 10 --preset cpu_live_heap \
	target/release/agent-data-plane

.PHONY: profile-run-smp-experiment
profile-run-smp-experiment: ensure-lading
profile-run-smp-experiment: ## Runs a specific SMP experiment for Saluki
ifeq ($(shell test -f test/smp/regression/saluki/cases/$(EXPERIMENT)/lading/lading.yaml || echo not-found), not-found)
	$(error "Lading configuration for '$(EXPERIMENT)' not found. (test/smp/regression/saluki/cases/$(EXPERIMENT)/lading/lading.yaml) ")
endif
	@echo "[*] Running '$(EXPERIMENT)' experiment (15 minutes)..."
	@./test/lading/bin/lading --config-path test/smp/regression/saluki/cases/$(EXPERIMENT)/lading/lading.yaml \
		--no-target --warmup-duration-seconds 1 --experiment-duration-seconds 900 --prometheus-addr 127.0.0.1:9229

.PHONY: ensure-ddprof
ensure-ddprof:
ifeq ($(shell test -f test/ddprof/bin/ddprof || echo not-found), not-found)
	@echo "[*] Downloading ddprof v$(DDPROF_VERSION)..."
	@curl -q -L -o /tmp/ddprof.tar.xz https://github.com/DataDog/ddprof/releases/download/v$(DDPROF_VERSION)/ddprof-$(DDPROF_VERSION)-$(TARGET_ARCH)-linux.tar.xz
	@tar -C test -xf /tmp/ddprof.tar.xz
	@rm -f /tmp/ddprof.tar.xz
endif

.PHONY: ensure-lading
ensure-lading:
ifeq ($(shell test -f test/lading/bin/lading || echo not-found), not-found)
	@echo "[*] Downloading lading v$(LADING_VERSION)..."
	@curl -q -L -o /tmp/lading.tar.gz https://github.com/DataDog/lading/releases/download/v$(LADING_VERSION)/lading-$(TARGET_TRIPLE).tar.gz
	@mkdir -p test/lading/bin
	@tar -C test/lading/bin -xf /tmp/lading.tar.gz
	@rm -f /tmp/lading.tar.gz
endif

##@ Development

.PHONY: fast-edit-test
fast-edit-test: fmt sync-licenses check-clippy check-deny check-licenses test-all
fast-edit-test: ## Runs a lightweight format/lint/test pass

##@ CI

.PHONY: emit-build-metadata
emit-build-metadata: override APP_BUILD_TIME = $(shell date -u +'%Y-%m-%dT%H:%M:%SZ')
emit-build-metadata: ## Emits build metadata shell variables suitable for use during image builds
	@echo "APP_FULL_NAME=${APP_FULL_NAME}"
	@echo "APP_SHORT_NAME=${APP_SHORT_NAME}"
	@echo "APP_IDENTIFIER=${APP_IDENTIFIER}"
	@echo "APP_GIT_HASH=${APP_GIT_HASH}"
	@echo "APP_VERSION=${APP_VERSION}"
	@echo "APP_BUILD_TIME=${APP_BUILD_TIME}"
	@echo "CHECKS_FULL_NAME=${CHECKS_FULL_NAME}"
	@echo "CHECKS_SHORT_NAME=${CHECKS_SHORT_NAME}"
	@echo "CHECKS_IDENTIFIER=${CHECKS_IDENTIFIER}"

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
	@$(CONTAINER_TOOL) container ls --filter label=created_by=airlock -q | xargs -r $(CONTAINER_TOOL) container rm -f
	@$(CONTAINER_TOOL) volume ls --filter label=created_by=airlock -q | xargs -r $(CONTAINER_TOOL) volume rm -f
	@$(CONTAINER_TOOL) network ls --filter label=created_by=airlock -q | xargs -r $(CONTAINER_TOOL) network rm -f

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

.PHONY: cargo-preinstall
cargo-preinstall: cargo-install-dd-rust-license-tool cargo-install-cargo-deny cargo-install-cargo-hack
cargo-preinstall: cargo-install-cargo-nextest cargo-install-cargo-autoinherit cargo-install-cargo-sort
cargo-preinstall: ## Pre-installs all necessary Cargo tools (used for CI)
	@echo "[*] Pre-installed all necessary Cargo tools!"

.PHONY: cargo-install-%
cargo-install-%: override TOOL = $(@:cargo-install-%=%)
cargo-install-%: override VERSIONED_TOOL = ${TOOL}@$(CARGO_TOOL_VERSION_$(TOOL))
cargo-install-%: check-rust-build-tools
	@$(if $(findstring true,$(AUTOINSTALL)),test -f ${CARGO_BIN_DIR}/${TOOL} || (echo "[*] Installing ${VERSIONED_TOOL}..." && cargo install ${VERSIONED_TOOL} --quiet),)
