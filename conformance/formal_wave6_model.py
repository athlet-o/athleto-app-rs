#!/usr/bin/env python3
from itertools import product

PLANNED, STARTED, PAUSED, COMPLETED = range(4)
EVENTS = ("start", "pause", "resume", "complete", "noop")


def step(state, event):
    if event == "start" and state == PLANNED:
        return STARTED
    if event == "pause" and state == STARTED:
        return PAUSED
    if event == "resume" and state == PAUSED:
        return STARTED
    if event == "complete" and state in (STARTED, PAUSED):
        return COMPLETED
    if event == "noop":
        return state
    return None


reached_completed = False
rejected_illegal = False
for events in product(EVENTS, repeat=5):
    state = PLANNED
    history = [state]
    legal = True
    for event in events:
        nxt = step(state, event)
        if nxt is None:
            rejected_illegal = True
            legal = False
            break
        state = nxt
        history.append(state)
    if not legal:
        continue
    if COMPLETED in history:
        reached_completed = True
        i = history.index(COMPLETED)
        assert all(s == COMPLETED for s in history[i:]), "completed workout reopened"
    assert history[0] == PLANNED

assert reached_completed, "completed workout is unreachable"
assert rejected_illegal, "model never exercised an illegal workout transition"
print("workout lifecycle transition-system model: ok")
