# aarch64 statically-linked musl build.
#
# musl lacks `__getauxval`, which "outline atomics" (runtime detection of LSE atomic instructions)
# relies on. Without disabling it, jemalloc fails to compile. `-mno-outline-atomics` turns it off for
# this target -- this is why the flag lives here, next to the target it applies to, rather than in CI.

rustup target add aarch64-unknown-linux-musl
copy_kernel_headers aarch64-linux-gnu aarch64-linux-musl

TARGET_CARGO_ARGS="--target aarch64-unknown-linux-musl"
TARGET_CFLAGS="-mno-outline-atomics"
TARGET_OUTPUT_DIR="/adp/target/aarch64-unknown-linux-musl/${BUILD_PROFILE}"
