#!/bin/sh
# Redirects Ubuntu package mirrors to a reliable US mirror.
# Targets both sources.list (Ubuntu <=22.04) and ubuntu.sources (Ubuntu 24.04+).
# To disable redirection (e.g. when upstream mirrors recover), set MIRROR to an empty string.
MIRROR=http://mirrors.edge.kernel.org

[ -z "$MIRROR" ] && exit 0
sed -i \
    "s|http://archive.ubuntu.com|${MIRROR}|g; s|http://security.ubuntu.com|${MIRROR}|g" \
    /etc/apt/sources.list /etc/apt/sources.list.d/ubuntu.sources 2>/dev/null || true
