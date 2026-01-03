#!/bin/sh
rev="$(git rev-parse HEAD)"
note="$1"
binary=target/release/iosock

if [ -z "$note" ]; then
    echo "Usage: $0 NOTE"
    exit 1
fi

cargo clean
cargo build -r
strip "$binary"
size="$(du "$binary")"
printf '%s %s %s\n' "$rev" "$size $note" >> sizes.txt
