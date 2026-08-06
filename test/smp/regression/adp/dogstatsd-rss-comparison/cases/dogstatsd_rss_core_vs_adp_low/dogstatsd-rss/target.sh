#!/bin/sh

set -eu

TARGET_CONFIG_DIR=/etc/dogstatsd-rss

if [ -x /opt/datadog-agent/bin/agent/agent ]; then
    unset DD_DATA_PLANE_ENABLED
    unset DD_DATA_PLANE_DOGSTATSD_ENABLED
    unset DD_DATA_PLANE_STANDALONE_MODE
    exec /opt/datadog-agent/bin/agent/agent run -c "${TARGET_CONFIG_DIR}"
fi

if [ -x /usr/local/bin/agent-data-plane ]; then
    export DD_AGGREGATE_CONTEXT_LIMIT="${SMP_ADP_AGGREGATE_CONTEXT_LIMIT}"
    export DD_DATA_PLANE_DOGSTATSD_ENABLED=true
    export DD_DATA_PLANE_STANDALONE_MODE=true
    export DD_IPC_CERT_FILE_PATH="${TARGET_CONFIG_DIR}/cert.pem"
    export DD_SERIALIZER_EXPERIMENTAL_USE_V3_API_SERIES_ENDPOINTS=http://127.0.0.1:9091
    if [ -n "${SMP_ADP_DOGSTATSD_STRING_INTERNER_SIZE_BYTES:-}" ]; then
        export DD_DOGSTATSD_STRING_INTERNER_SIZE_BYTES="${SMP_ADP_DOGSTATSD_STRING_INTERNER_SIZE_BYTES}"
    fi
    exec /usr/local/bin/agent-data-plane --config "${TARGET_CONFIG_DIR}/datadog.yaml" run
fi

echo "No supported DogStatsD target binary found." >&2
exit 1
