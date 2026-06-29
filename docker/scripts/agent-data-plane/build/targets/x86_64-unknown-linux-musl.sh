# x86_64 statically-linked musl build.

rustup target add x86_64-unknown-linux-musl
copy_kernel_headers x86_64-linux-gnu x86_64-linux-musl

TARGET_CARGO_ARGS="--target x86_64-unknown-linux-musl"
TARGET_CFLAGS=""
TARGET_OUTPUT_DIR="/adp/target/x86_64-unknown-linux-musl/${BUILD_PROFILE}"
