#!/bin/bash
# Median of 3 KV_ONLY runs (8 s each) per loop, 1 worker; label in $1.
bin=$(cd "$(dirname "$0")/../.." && pwd)/target/release/examples/kv_cost
for loop in miss:b1k:1000:16 get:b1k:1000:16 get:b100k:100000:16 set:b1k:1000:16 set:b1k:1000:65536 many100:b1k:1000:16; do
  vals=$(for r in 1 2 3; do KV_ONLY=$loop:8 $bin 1 | sed -E 's/.*: ([0-9]+) ns.*/\1/'; done | sort -n | tr '\n' ' ')
  echo "$1 $loop $(echo $vals | cut -d' ' -f2) ns ($vals)"
done
