#!/bin/bash
RUSTFLAGS='--cfg tokio_unstable' cargo fuzz run apd -- -max_len=16384 -rss_limit_mb=0 "$@"

