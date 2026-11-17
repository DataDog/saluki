#!/bin/sh
set -eu
curl --fail --silent --show-error "$(cat /etc/fetch/url)/index.html"
