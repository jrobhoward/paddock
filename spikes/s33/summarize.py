import re, sys, glob, os, statistics as st
d = sys.argv[1]
rows = {
 'ct: host seq p50 us': ('ct', r'one node: sequential calls \d+/s, p50 ([\d.]+)'),
 'ct: inst seq p50 us': ('ct', r'one node, from an instance: sequential calls \d+/s, p50 ([\d.]+)'),
 'ct: inst seq p99 us': ('ct', r'one node, from an instance: sequential calls \d+/s, p50 [\d.]+ us, p99 ([\d.]+)'),
 'ct: 16 inst calling, calls/s': ('ct', r'from instances, 16 callers: one node (\d+)'),
 'ct: host 16 callers calls/s': ('ct', r'16 callers: one node (\d+) calls/s, two'),
 'ct: host 64 callers calls/s': ('ct', r'64 callers: one node (\d+)'),
 'ct: inst casts/s': ('ct', r'one node, from instances: casts (\d+)'),
 'nc: request from callback us': ('nc', r'request from a callback \(its own pid\): ([\d.]+)'),
 'nc: bucket get us': ('nc', r'bucket get from a callback \(16 B, memory bucket\): ([\d.]+)'),
 'nc: host call p50 us': ('nc', r'call from the host, one at a time: p50 ([\d.]+)'),
 'nc: 16 in flight calls/s': ('nc', r'16 calls in flight: (\d+)'),
 'nc: 16 in flight CPU us/call': ('nc', r'16 calls in flight: .* ([\d.]+) µs of CPU'),
 'nc: 64 in flight calls/s': ('nc', r'64 calls in flight: (\d+)'),
 'nc: 64 in flight CPU us/call': ('nc', r'64 calls in flight: .* ([\d.]+) µs of CPU'),
 'nc: casts/s': ('nc', r'casts to one instance: (\d+)'),
}
cfgs = sys.argv[2].split(',') if len(sys.argv) > 2 else ['stable', 'guess', 'local', 'global']
print(f"{'row':34}" + ''.join(f"{c:>26}" for c in cfgs))
for name, (kind, rx) in rows.items():
    line = f"{name:34}"
    for c in cfgs:
        vals = []
        for f in sorted(glob.glob(f"{d}/{kind}-{c}-*.txt")):
            m = re.search(rx, open(f).read())
            if m: vals.append(float(m.group(1)))
        if vals:
            fmt = (lambda v: f"{v:.0f}") if max(vals) > 1000 else (lambda v: f"{v:.2f}")
            line += f"{fmt(st.median(vals)):>9} [{','.join(fmt(v) for v in vals)}]".rjust(26)
        else:
            line += f"{'-':>26}"
    print(line)
