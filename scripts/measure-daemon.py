#!/usr/bin/env python3
"""External measurement of a daemon, with no IPC client attached, so the act of
measuring cannot disable the idle sampling regime."""
import os, sys, time

pid, dur = int(sys.argv[1]), float(sys.argv[2] if len(sys.argv) > 2 else 180)
TCK = os.sysconf("SC_CLK_TCK")

def cpu_ticks():
    f = open(f"/proc/{pid}/stat").read()
    v = f[f.rindex(")") + 1:].split()
    return int(v[11]) + int(v[12])

def status(key):
    for line in open(f"/proc/{pid}/status"):
        if line.startswith(key):
            return int(line.split()[1])
    return 0

c0, v0, n0, r0 = cpu_ticks(), status("voluntary_ctxt_switches"), status("nonvoluntary_ctxt_switches"), status("VmRSS")
t0 = time.monotonic()
time.sleep(dur)
c1, v1, n1, r1 = cpu_ticks(), status("voluntary_ctxt_switches"), status("nonvoluntary_ctxt_switches"), status("VmRSS")
el = time.monotonic() - t0
cpu = (c1 - c0) / TCK
print(f"window              {el:.0f} s")
print(f"cpu time            {cpu:.3f} s   =>  {100*cpu/el:.4f}% of one core")
print(f"voluntary ctxsw     {v1-v0:6d}   =>  {(v1-v0)/el:.3f} wakeups/s")
print(f"involuntary ctxsw   {n1-n0:6d}")
print(f"rss                 {r1/1024:.2f} MB (start {r0/1024:.2f} MB)")
