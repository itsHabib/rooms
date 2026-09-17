# Experiments 23–34: a swarm of agents that runs in Rooms

Added at Michael's request on 2026-09-17. All are planned; none is built or
measured. The workbench `swarm` tool coordinates a flat fleet of coding agents:
seats are branches, a store carries claims, commits and rulings, and nobody
manages. Its store contract already passed inside Rooms clones, with a clone
killed and Redis restarted mid-run. These entries ask what Rooms changes about
such a fleet, and try to break it with real work and real faults.

Rooms stays substrate. Swarm logic lives in workbench; what lands here is
recipes, results and any small primitive the evidence justifies. Each entry names
what would count as the idea *failing*, so a negative result is a result.
Entries marked *local* run on the Lima host; *agents* need model time; *cloud*
needs rented hosts.

## Seats in Rooms

### 23. A seat is a clone (*local, agents*)

**Question:** what does a fleet gain and lose when every seat is a microVM clone
and not a git worktree?

**Prototype:** `swarm admit` hands out clones of one warmed base. The seat's
branch is fetched inside the room; a landing is the patch Rooms collects. Run the
swarm proof-of-concept workload both ways. Measure seat start, time to first
edit, disk per seat, and whether one seat can read another's files.
**Fails if** seats start slower than worktrees once the base is warm, or landing
through a collected patch loses work that a worktree would have kept.

### 24. The warm seat (*local*)

**Question:** how much of a builder's first ten minutes is setup that a snapshot
could hold?

**Prototype:** a base with the repository cloned, dependencies installed and the
build cache hot, warmed by running the test suite once. Compare time to first
green test for a cold worktree, a cold room and a clone of the warm base, and
host memory for 2, 4 and 6 seats, using the sharing result from 2b.
**Fails if** the warm base saves less than the snapshot costs to build and keep.

### 25. Admit by memory, not by seat count (*local*)

**Question:** how many seats fit on one machine when admission reads measured
headroom?

**Prototype:** swarm admits on a fixed seat count and a disk floor today. Feed it
summed VMM PSS and `MemAvailable` from the 2b sampler and admit while headroom
holds. Ramp seats until the first admission refusal or the first OOM kill.
**Fails if** measured admission admits fewer useful seats than a fixed count, or
lets the host reach an OOM kill.

### 26. Split is a fork (*local, agents*)

**Question:** when a seat discovers its task is really three, is cloning the seat
better than starting three cold ones?

**Prototype:** on `swarm split`, snapshot the splitting seat and clone it per
child, so each child inherits the checkout, caches and whatever the parent
already built. Compare child time to first edit and tokens spent re-reading.
**Fails if** inherited state misleads children more often than it saves them.

## Deciding by running

### 27. Fork instead of asking (*local, agents*)

**Question:** when two seats contend for the same package, is it cheaper to run
both approaches and let the tests rule than to ask a peer for a ruling?

**Prototype:** on a contended `swarm check`, clone the room, apply each approach
in its own clone, run the suite, and record the ruling from the result. Compare
with `swarm ask` on minutes to ruling, tokens, and how often the ruling is later
reversed. **Fails if** most contention is about intent, which no test can rule on.

### 28. Clean-room verification (*local*)

**Question:** does verifying each landed head in a fresh clone catch what a
long-lived checkout hides?

**Prototype:** `swarm watch --verify` runs the suite in a new clone at the pinned
head, several heads at once. Plant a landing that passes only with a stale
artifact from an earlier build. Measure verifications per minute and whether the
planted landing goes red. **Fails if** clean rooms find nothing a shared checkout
misses, at several times the cost.

### 29. Rewind a red landing (*local, agents*)

**Question:** can a failed landing be debugged from the moment before it went
wrong?

**Prototype:** snapshot a seat at every commit to the store. When verification
goes red, restore the seat one step earlier and hand that room to a second agent
with the failing output. Extends experiment 20. **Fails if** snapshot overhead
slows seats more than the rewind saves, or the second agent does no better than
one given only the diff.

## Breaking it

### 30. Kill everything (*local*)

**Question:** after arbitrary deaths, do the store's history and the git history
still agree?

**Prototype:** run the seeded fault schedules inside Rooms: kill clones mid
commit, pause one past its lease, restart the store, reboot the host. The plane's
checker already judges the store; add a second check that every accepted commit
has exactly one landed patch and no landed patch lacks one. Extends experiment
21. **Fails if** any seed leaves an accepted commit without its patch, or a patch
landed twice.

### 31. The hostile seat (*local*)

**Question:** does the fleet hold when one seat is an adversary?

**Prototype:** one seat tries to read other seats' work, write outside its
declared paths, forge a ruling, replay an old epoch, and reach the network. Give
each seat a store credential scoped to its own keys and run seats with
`--egress` limited to the store. Score each attempt as blocked, detected or
missed. **Fails if** any attempt is missed. This is also the honest test of the
follow-up that a guest can reach services on the host's LAN address.

### 32. Two hosts, one store (*cloud*)

**Question:** does the flat fleet survive losing half its machines?

**Prototype:** seats in Rooms on two hosts against one store. Cut one host off
with the egress controls, then delete it. Survivors should take over its leases
and finish; no commit may be accepted twice. Merges experiment 18.
**Fails if** work is lost, duplicated, or waits on a human.

## What works in practice

### 33. A real repository for a day (*agents, then cloud*)

**Question:** everything above uses a synthetic app with small, similar tasks.
What breaks on real work?

**Prototype:** twenty to thirty real tasks from one of our own repositories'
follow-up lists, run as a flat swarm three ways: worktrees, Rooms seats, Rooms
seats on a warm base. Score landed, red, reverted, operator requests, wall time
and cost. **Fails if** long installs, flaky tests or wrongly scoped cards sink
the fleet in ways the synthetic runs never showed; record which.

### 34. Store bake-off on one contract (*local*)

**Question:** which store should a fleet of Rooms seats actually use?

**Prototype:** the plane contract and its independent checker already exist.
Implement the contract on Redis, NATS JetStream and Postgres; run the same fault
matrix and the same in-Room runs against each. Compare claim cost, behaviour
during an outage, recovery time and what an operator has to run.
**Fails if** the contract cannot be expressed atomically on a backend; say which
step and why.

## Suggested order

1. **24 and 25**: local, no agents, and they tell us what a seat costs.
2. **23**: the integration itself, once a warm seat exists.
3. **30 and 31**: try to break it before trusting it with real work.
4. **28, then 27 and 26**: the ideas only Rooms makes possible.
5. **33**: the real test. Do it before renting hosts for 32.
6. **34 and 29** as the need appears.
