#!/bin/sh
# Sequential specimen: persist a payment, then acknowledge its delivery.
set -eu
mode=$1
ledger=$2
event=$3
amount=$4

if [ "$mode" = idempotent ]; then
    if awk -F '\t' -v event="$event" '$1 == event { found=1 } END { exit !found }' "$ledger"; then
        printf 'ack\t%s\n' "$event"
        exit 0
    fi
fi
printf '%s\t%s\n' "$event" "$amount" >> "$ledger"
printf 'ack\t%s\n' "$event"
