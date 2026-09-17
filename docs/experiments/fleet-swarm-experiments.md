# Experiments 23–34: a swarm of agents that runs in Rooms

Added at Michael's request on 2026-09-17. All are planned; none is built or
measured. The workbench `swarm` tool coordinates a flat fleet of coding agents:
seats are branches, and nobody manages. Code lives only in git: a landing is a
branch tip a seat pushed to the shared remote, pinned by the head it names, and
the board is derived from git. A store carries everything else: requests, claims,
rulings, resource leases and work units. In the store's contract "commit" means
committing an item's result under its lease epoch, not a git commit. That
contract already passed inside Rooms clones, with a clone killed and Redis
restarted mid-run. The entries below were corrected against the swarm session's
review of the first draft. These entries ask what Rooms changes about
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

**Prototype:** `swarm admit` only answers yes, no or wait; the runner starts
seats. Give the runner a two-command seam with a worktree implementation and a
Rooms one: `start-seat --seat NAME --remote URL --base REF --store SPEC --prompt
FILE` prints a handle, and `wait-seat HANDLE` returns the exit code and the
session's JSON. There is no "collect a landing": the seat pushes its own branch
from inside the room to a remote on the host, because a patch applied by anyone
else changes the head and invalidates the pin. Collected patches are a backup
only. The seat gets `SWARM_SEAT`, `SWARM_STORE`, an incarnation minted after
restore, and its credential through `rooms clone --secret`. Record a one-session
baseline on the same goal. Measure seat start, time to first edit, disk per seat,
and whether one seat can read another's files.
**Fails if** seats start slower than worktrees once the base is warm, or a room
cannot be re-entered after its command exits: the runner's wake path resumes a
stopped session in the same room or a clone of it, and that is the first thing
this experiment has to find out.

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

**Prototype:** worth doing only after 24 says what a seat costs. Swarm admits on
a fixed seat count and a disk floor today, kept in a local file. Feed it
summed VMM PSS and `MemAvailable` from the 2b sampler and admit while headroom
holds. Ramp seats until the first admission refusal or the first OOM kill.
**Fails if** measured admission admits fewer useful seats than a fixed count, or
lets the host reach an OOM kill.

### 26. Split is a fork (*local, agents*)

**Question:** when a seat discovers its task is really three, is cloning the seat
better than starting three cold ones?

**Prototype:** `swarm split` spawns nothing: it validates the children, records
a ruling and writes task rows, and admission picks the children up later. So this
is runner behaviour triggered by new rows. When a split lands, snapshot the
splitting seat and start each child from a clone of it, so each child inherits the checkout, caches and whatever the parent
already built. Compare child time to first edit and tokens spent re-reading.
**Fails if** inherited state misleads children more often than it saves them.

## Deciding by running

### 27. Fork instead of asking (*local, agents*)

**Question:** when two seats contend for the same package, is it cheaper to run
both approaches and let the tests rule than to ask a peer for a ruling?

**Prototype:** `swarm check` reads contention from git (each seat's declared
intent plus the paths its branch touched); `ask` and `rule` are the ledger. On a
contended check, clone the room, apply each approach
in its own clone, run the suite, and record the ruling from the result. Compare
with `swarm ask` on minutes to ruling, tokens, and how often the ruling is later
reversed. **Fails if** most contention is about intent, which no test can rule on.
Expect that: in the swarm session's runs so far no seat used `swarm ask` at all,
because an exact spec leaves nothing to rule on. Run this one last.

### 28. Clean-room verification (*local*)

**Question:** does verifying each landed head in a fresh clone catch what a
long-lived checkout hides?

**Prototype:** `swarm watch --verify` runs a command at each landed head and
writes a receipt keyed by that head; red heads block consolidation. Where the
command runs is the swappable part. Make the executor a new clone at the pinned
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
commit, pause one past its lease, restart the store, reboot the host. Also seed
the failure kill schedules never produce: a headless seat that ends its turn and
exits 0 while still holding a claim. That cost one of the swarm session's runs a
quarter of its tests, and the fix was a wake path. The plane's checker already
judges the store; add a second check that joins store to git: every work unit
committed done corresponds to exactly one commit reachable from the remote's
main, and no seat whose lease was fenced has a commit there. The join needs
`work done` to record the head it landed, which the swarm session is adding.
Extends experiment 21. **Fails if** any seed leaves a done unit without its
commit, a commit from a fenced seat on main, or a held claim nobody wakes.

### 31. The hostile seat (*local*)

**Question:** does the fleet hold when one seat is an adversary?

**Prototype:** one seat tries to read other seats' work, write outside its
declared paths, forge a ruling, replay an old epoch, and reach the network. Run
seats with `--egress` limited to the store and the remote. Score each attempt as
blocked, detected or missed. Declared before running: forging a ruling is
expected to be **missed** today. Seat identity is self-asserted, and store
credentials scoped by key prefix cannot fix it because every seat legitimately
writes the same kinds of key; it needs per-seat signing or a broker in front of
the store. Epoch replay should be blocked, since the contract fences it.
**Fails if** anything other than the declared gap is missed. This is also the honest test of the
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
follow-up lists, run four ways: one session working the whole list, a flat swarm
in worktrees, in Rooms seats, and in Rooms seats on a warm base. The one-session
arm is required. On an exact-spec greenfield goal the swarm session measured one
session at 86 of 86 hidden tests for $2.89, against $8.88 to $10.59 for three
team shapes that scored no better; independent follow-up tasks with no dependency
chain are where a fleet should win, so without that arm this experiment cannot
fail in the way that matters. Score landed, red, reverted, operator requests, wall time
and cost. **Fails if** long installs, flaky tests or wrongly scoped cards sink
the fleet in ways the synthetic runs never showed; record which.

### 34. Store bake-off on one contract (*local*)

**Question:** which store should a fleet of Rooms seats actually use?

**Prototype:** the plane contract and its independent checker already exist, and
memory, file and Redis backends pass it with mutants that prove the checker can
fail. Add Postgres (one transaction on the server clock, expected easy) and NATS
JetStream (compare-and-set by revision but no server-side time check, so lease
expiry is the step expected not to be atomic); run the same fault
matrix and the same in-Room runs against each. Compare claim cost, behaviour
during an outage, recovery time and what an operator has to run.
**Fails if** the contract cannot be expressed atomically on a backend; say which
step and why.

## Suggested order

1. **24**: local, no agents; it says what a seat costs. Then 25.
2. **23**, with the one-session baseline recorded.
3. **30**, including the seat that exits while holding a claim.
4. **33**: the real test. Do it before renting hosts for 32.
5. **31**, with the forged ruling pre-declared as a known gap.
6. **28 and 26**, the ideas only Rooms makes possible; **34 and 29** as the need
   appears; **27** last.
