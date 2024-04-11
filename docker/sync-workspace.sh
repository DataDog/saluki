#!/usr/bin/env bash

# Make sure each library and binary crate have their respective src/lib.rs and src/main.rs, and touch them to ensure the
# file modification time is updated.
for d in "./lib/"*; do
  if [ -d "$d" ]; then
    mkdir "${d}/src"
    touch "${d}/src/lib.rs"
  fi
done

for d in "./bin/"*; do
  if [ -d "$d" ]; then
    mkdir "${d}/src"
    if [ ! -f "${d}/src/main.rs" ]; then
      echo "fn main() {}" > "${d}/src/main.rs"
    fi
    touch "${d}/src/main.rs"
  fi
done
