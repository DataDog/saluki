#!/bin/bash
set -euo pipefail

ip=$(hostname -i | awk '{print $1}') || ip=''
if [[ -z "$ip" ]]; then
    echo 'panoramic: could not determine the participant IP address' >&2
    exit 1
fi

placeholder='@IP_REFERENCE@'
mkdir -p /run/adp/env /run/agent/env
while IFS= read -r -d '' entry; do
    key=${entry%%=*}
    value=${entry#*=}
    if [[ "$value" == *"$placeholder"* ]]; then
        resolved=${value//"$placeholder"/$ip}
        printf '%s' "$resolved" > "/run/adp/env/$key"
        printf '%s' "$resolved" > "/run/agent/env/$key"
    fi
done < <(env -0)
