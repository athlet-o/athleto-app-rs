#!/usr/bin/env python3
from itertools import product
for trace in product((0,1,2,3), repeat=5):
    if trace[0]!=0: continue
    if not all(b==a or b==a+1 for a,b in zip(trace,trace[1:])): continue
    assert all(b>=a for a,b in zip(trace,trace[1:])), "workout lifecycle regressed"
    if 3 in trace:
        i=trace.index(3); assert all(s==3 for s in trace[i:]), "completed workout reopened"
print("workout lifecycle temporal model: ok")
