#!/usr/bin/env python3
"""Summarise a results file: per op and busy, one row per mechanism and
thread count, each cell the range over the rounds."""
import re, sys
from collections import defaultdict
pat = re.compile(r'r\d (\S+)\s+t=(\d+) (\S+)\s+busy=\s*(\d+)us\s+(\d+) ops/s\s+p50\s+([\d.]+) us\s+p99\s+([\d.]+) us.*total\s+([\d.]+) us')
cells = defaultdict(list)
for line in open(sys.argv[1]):
    m = pat.search(line)
    if m:
        mech, t, op, busy, ops, p50, p99, cpu = m.groups()
        cells[(op, int(busy), mech, int(t))].append((int(ops), float(p50), float(p99), float(cpu)))
def rng(vals, fmt):
    lo, hi = min(vals), max(vals)
    a, b = fmt(lo), fmt(hi)
    return a if a == b else f"{a}–{b}"
mechs = ["m0", "m1", "m2", "m2r", "m3"]
for op in ["get", "cast", "call"]:
    for busy in [0, 1000]:
        print(f"\n### {op}, callback thread {'idle' if busy == 0 else 'busy (1 ms callbacks)'}\n")
        print("| mechanism | threads | thousand/s | p50 µs | p99 µs | CPU µs/op |")
        print("| --- | --- | --- | --- | --- | --- |")
        for mech in mechs:
            for t in [1, 2, 4, 8]:
                v = cells.get((op, busy, mech, t))
                if not v: continue
                print(f"| {mech} | {t} | {rng([x[0]/1000 for x in v], lambda x: f'{x:,.0f}')} | "
                      f"{rng([x[1] for x in v], lambda x: f'{x:.1f}' if x < 100 else f'{x:,.0f}')} | "
                      f"{rng([x[2] for x in v], lambda x: f'{x:.1f}' if x < 100 else f'{x:,.0f}')} | "
                      f"{rng([x[3] for x in v], lambda x: f'{x:.1f}')} |")
