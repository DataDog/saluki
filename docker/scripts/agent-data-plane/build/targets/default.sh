# Implicit host target: build natively for the build image's own architecture.
#
# No `rustup target add`, no kernel-header copying, and no `--target` flag -- cargo builds for the
# host triple and writes to target/<profile>/. Used by local Makefile builds (BUILD_TARGET=default).

TARGET_CARGO_ARGS=""
TARGET_CFLAGS=""
TARGET_OUTPUT_DIR="/adp/target/${BUILD_PROFILE}"
