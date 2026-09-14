#!/bin/sh
# Lose a reply after commit, then redeliver through a fresh handler process.
set -eu
mode=$1
scenario=$2
out=$3
handler=$4
mkdir -p "$out"
: > "$out/ledger.tsv"
: > "$out/trace.tsv"

invoke() {
    printf 'deliver\t%s\n' "$1" >> "$out/trace.tsv"
    sh "$handler" "$mode" "$out/ledger.tsv" "$1" 2500 > "$out/reply.tmp"
}
acknowledge() {
    cat "$out/reply.tmp" >> "$out/trace.tsv"
}

invoke payment-001
if [ "$scenario" = lost-ack ]; then
    printf 'reply-lost\tpayment-001\n' >> "$out/trace.tsv"
    invoke payment-001
fi
acknowledge
if [ "$scenario" = distinct-events ]; then
    invoke payment-002
    acknowledge
fi
rm "$out/reply.tmp"
