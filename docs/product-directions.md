# Portfolio experiments enabled by rooms

This note evaluates rooms as one component of a personal tool portfolio, not as
a standalone product looking for a market. The objective is **portfolio
compounding**: how much a direction makes ship, dossier, the verification
ladder, gate, and the observability views more capable together.

The portfolio is organized as five planes coupled by typed artifacts, never by
call stacks:

- **State — dossier:** durable project memory and links to the artifacts that
  explain what happened.
- **Execution — rooms and ship:** do work and emit evidence; they never judge
  their own output.
- **Verification:** an escalate-only ladder that turns evidence, including four
  AI PR reviewers, into one reduced verdict.
- **Capability — gate:** requires a live, scoped, tiered, time-boxed grant and a
  supporting verdict for every sanctioned effect; records decisions as
  hash-chained artifacts.
- **Observability:** read-only projections built from recorded State.

That changes the question from “what could become a product?” to:

> What evidence can rooms manufacture that another plane cannot produce today,
> and which existing plane can act on that evidence immediately?

## Current capability boundary

The shipped primitive is already substantial:

- A proven concurrent room pool with isolated network slots and structured
  backpressure at a cap
- Disposable Firecracker guests with zero-leak teardown
- A git patch plus `changeset.json` attribution of files changed during a run
- A forensic lane-escape signal for persistent writes outside the workspace

It is not yet a fully pinned experimental universe:

- Guests see the real clock.
- Entropy and model randomness are not pinned.
- Network traffic is live; there is no request record/replay layer.
- `changeset.json` is enumerated from inside the guest by an identity with root.
  A careless agent is observable, but an adversarial guest can forge or suppress
  that report. Host-side overlay enumeration is not built.

Consequently, rooms can run **isolated, clean, attributable trials today**. It
cannot yet promise byte-for-byte replay or causal control over clock, entropy,
network responses, or remote-model behavior. The proposals below distinguish
current experiments from capabilities they would first have to add.

## Portfolio ranking

| Rank | Direction | Portfolio reason | Readiness |
| ---: | --- | --- | --- |
| 1 | Sting Ops | Calibrates the reviewers and gate that govern every other direction | High for a shadow campaign |
| 2 | Agent Airworthiness | Gives ship and gate measured evidence about which agent may attempt which class of work | Medium-high; statistical, not replayable |
| 3 | Counterfactual CI | Adds controlled failure evidence, but initially serves fewer existing decisions | Medium with today's controllable inputs |
| 4 | Action Escrow | Is the purest evidence → verdict → grant → action composition | Low-medium; replicas and applicators do not exist |
| 5 | Speculative Engineering | Compounds ship's fan-out only after selection quality is measured | Medium mechanically; low confidence in selection |
| 6 | Software Customs | Could feed gate, but the evidence needed for an adversarial claim is not trustworthy yet | Low as a security instrument |

## 1. Sting Ops

**Pitch:** Seed known, realistic defects into plausible diffs, send them blindly
through the four-reviewer plus gate pipeline, and measure who actually catches
what.

**Mechanism:** Maintain a small catalog of defect operators by class and
severity: missing authorization check, race window, error swallowed, boundary
condition, stale documentation contract, unsafe cleanup, and so on. For each
campaign, start from a real clean commit and create a matched pair:

- A control diff with no seeded defect
- A natural-looking mutant diff containing exactly one known defect

Rooms runs control and mutant heads in parallel and records their exact patches,
test results, changesets, escape status, and run provenance. Ship carries each
head through the normal review path without telling the reviewers which is the
mutant. The Verification plane evaluates reviewer comments and the consolidated
gate outcome against the hidden sting manifest, then emits per-reviewer catch
rates, false-positive rates on controls, catch rate by defect class, and the
reducer's end-to-end result.

This is a shadow exercise: no synthetic head is eligible to merge. The point is
to replace beliefs such as “four reviewers should be enough” with measured
evidence about when each rung is useful.

**What rooms enables that the portfolio cannot do today:** Rooms can manufacture
many clean control/mutant pairs concurrently, prove the injected patch was the
only workspace difference, bound each run, and remove every trial afterward.
The current lack of clock, entropy, and network replay does not invalidate the
campaign because the result is a repeated empirical rate, not a claim of
deterministic replay. Model, prompt, provider, time, and reviewer version must be
recorded as provenance.

**Most likely killer:** Synthetic bugs are recognizable or unrepresentative.
Reviewers learn the mutation style, catch rates rise, and the campaign measures
sting familiarity rather than real review ability.

**Smallest experiment:** Select ten historical PR bases. Produce ten matched
control/mutant pairs across at least four defect classes, keeping the sting
manifest hidden from the review path. Run all four reviewers and gate twice.
The experiment earns a second iteration only if it produces useful separation:
stable differences among reviewers or defect classes, a tolerable control
false-positive rate, and a consolidated gate that catches materially more
mutants than controls. If every reviewer moves together or repeat-to-repeat
variance dominates, the fixture set is not yet a calibrator.

**Reality:** **High for a shadow trial.** The pool, clean checkout, diff capture,
teardown, reviewer panel, and gate already exist. New work is mostly campaign
schema, mutation fixtures, blinded evaluation, and aggregation. It requires no
claim that remote reviewers are hermetic.

## 2. Agent Airworthiness

**Pitch:** Qualify an agent configuration for a bounded class of portfolio work
before ship dispatches real tasks to it.

**Mechanism:** Turn historical tasks into versioned task capsules containing:

- A base commit and task instructions
- The allowed tools and capability policy
- Agent, model, prompt, and runner identities
- Time, token, CPU, and memory limits
- Hidden validation supplied only to Verification

Run candidate configurations several times in clean rooms. Compare task success,
diff footprint, unrelated edits, escape signals, test regressions, cleanup, cost,
and variance. Verification emits an `airworthiness-verdict` scoped to an agent
identity, repository or task class, evaluator version, and expiry—not a vague
global score.

### Does this directly mint a gate tier?

It should inform one, but it must not mint permission to merge an exact head by
itself. A measured pass rate is an **actor prior**: evidence that an agent has
been reliable on a bounded task distribution. Gate authorizes an **exact action
on an exact code head**. Conflating those two would break the Capability plane's
supporting-verdict rule.

The sound composition is:

1. Airworthiness supports a time-boxed capability ceiling for an agent and task
   class: what ship may dispatch, which tools it may receive, and the highest
   risk band it may attempt without operator intervention.
2. The resulting PR still needs the normal exact-head reviewer and gate verdict.

Under that rule, Airworthiness ranks above Counterfactual CI. Its artifact has
immediate consumers: ship can select or refuse a runner; gate can bound a grant;
dossier can retain qualification history; observability can show drift. A
generic failure-localization result has a less direct action path today.

**What rooms enables that the portfolio cannot do today:** Safe, repeated,
private-repository trials with identical filesystem starts, bounded resources,
and attributable mutations. Without rooms, qualification runs can contaminate
the operator's checkout and one another, and “unrelated edits” are difficult to
measure consistently.

**Most likely killer:** The capsule suite becomes stale or gets mistaken for the
real task distribution. A precise score over yesterday's tasks can be worse than
no score if it silently grants broad authority tomorrow.

**Smallest experiment:** Build ten capsules from five previously accepted and
five rejected or heavily reworked changes. Compare two agent configurations with
three repetitions each. Ask whether the verdict predicts human accept/reject and
whether its variance is small enough to support a narrow dispatch ceiling. If
not, retain the run records but do not mint a capability policy from them.

**Reality:** **Medium-high, statistically.** Rooms lacks clock/entropy pinning and
network/model replay, so repeated remote-agent runs are independent samples, not
replays. An honest v1 records the ambient inputs and confidence interval. A
future record/replay layer could turn some tool interactions into deterministic
capsules, but that is not a current property.

## 3. Counterfactual CI

**Pitch:** Localize a failure to the declared environmental inputs rooms can
actually vary.

**Mechanism:** Define a bounded matrix over controllable inputs available today:

- Source revision
- Toolchain version
- Dependency or lockfile version
- Feature flags and configuration
- CPU and memory limits

Run the same command across the matrix, then use covering arrays and delta
debugging to identify the smallest observed failing combination. Emit an
`experiment-result` containing every input vector, exit result, output hashes,
git diff, changeset, escape status, and the reduced failure predicate.

Clock values, entropy seeds, filesystem scheduling, and recorded network
responses do **not** belong in the v1 matrix: rooms cannot control them today.
They become legitimate axes only after explicit clock injection, entropy
control, or network record/replay exists. Until then the result must say
“minimal predicate among controlled inputs,” not “the cause.”

**What rooms enables that the portfolio cannot do today:** The concurrent pool
can create clean, isolated variants cheaply, enforce a cap, and attach each
outcome to the exact controlled input vector and resulting diff. Ship can use
that evidence to distinguish a task failure from a toolchain/configuration
failure; dossier can retain the result for later recurrence; Verification can
escalate when the observed predicate is incomplete.

**Most likely killer:** An uncontrolled variable is causal. The reducer then
produces a precise but incomplete predicate, especially when live network or
remote-model behavior changes between trials.

**Smallest experiment:** Seed one failure caused by an unknown two-variable
interaction chosen from toolchain, dependency version, feature flag, CPU cap,
and memory cap. Run no more than 32 variants and recover the minimal controlled
predicate. Repeat on one real compatibility failure. If either requires clock,
entropy, or live-network explanations, record the experiment as indeterminate;
do not smuggle those axes into a hermeticity claim.

**Reality:** **Medium, not High.** The useful controlled-input core is close to
the shipped pool. Full Counterfactual CI—including time, randomness, filesystem
ordering, and external responses—requires input controls rooms does not have.

## 4. Action Escrow

**Pitch:** Let an agent prepare an external action without ever possessing the
authority to perform it.

**Mechanism:** Snapshot an external system into typed, versioned local state:
repository metadata, issues, deployment configuration, cloud resources, or
access rules. The agent works only inside a room and expresses desired changes
as mutations or an operations artifact. Rooms records the proposal and its
workspace effects.

Then:

1. Verification checks the proposed transaction and its simulated effects.
2. Gate requires a live grant whose scope and tier cover that exact operation.
3. A small trusted applicator executes exactly the authorized operations.
4. Compare-and-swap preconditions reject the transaction if live state changed
   after the snapshot.

An agent could propose issue labels, comments, branch changes, or deployment
edits without receiving the credential that performs them.

**What rooms enables that the portfolio cannot do today:** Isolation makes the
proposal boundary structural rather than prompt-based. The agent gets inputs but
not authority; its attributable output becomes the evidence consumed by
Verification and gate. This is the cleanest expression of the portfolio's typed
artifact law.

**Most likely killer:** Replica fidelity and time-of-check/time-of-use drift.
External systems have semantics the local snapshot does not capture, and the
trusted applicator can become a second implementation of every external API.

**Smallest experiment:** Use a disposable repository with 20 issues. Snapshot
their versioned state, have an agent propose labels and comments, mutate two
issues after the snapshot, and apply through compare-and-swap operations. Valid
operations should apply, both stale operations should be rejected, and nothing
omitted from the proposal should occur.

**Reality:** **Low-medium.** Rooms can isolate the proposer today, but typed
external snapshots, versioned operation schemas, policy simulators, and trusted
applicators do not exist. The fit is excellent; the prerequisite surface is
large. It ranks above more buildable ideas because the ranking is portfolio fit,
not shortest implementation.

## 5. Speculative Engineering

**Pitch:** Execute several plausible implementations of a task and retain only
the candidate supported by the strongest verification evidence.

**Mechanism:** Ship dispatches several candidates into separate rooms:

- A minimal patch
- A conservative refactor
- Different implementation strategies
- Different agents or tool policies
- An adversarial candidate trying to satisfy tests cheaply

Each receives the same declared repository inputs and emits an attributable
changeset. Verification applies public tests, hidden tests, invariants, mutation
testing, and review. Ship advances one evidence bundle; every losing room
evaporates.

**What rooms enables that the portfolio cannot do today:** The pool makes N
simultaneous attempts safe and clean. Candidate workspaces cannot contaminate
one another, resource caps provide structured backpressure, and exact diffs let
Verification compare collateral change rather than only test outcomes.

The comparison is not fully hermetic when candidates use live networks or
remote models. That is acceptable for a practical best-of-N trial if provenance
and variance are recorded; it is not acceptable to describe the candidates as
identical deterministic executions.

**Most likely killer:** The selection oracle. Weak tests and review do not become
stronger because there are more candidates; best-of-N instead selects the patch
that most effectively exploits the verifier.

**Smallest experiment:** Take one medium, well-specified issue and generate six
deliberately different candidates. Keep several acceptance tests hidden. Rank
the candidates before human inspection. If the top verdict does not match the
human-preferred patch, or the operator must inspect all six, fan-out has created
work rather than leverage.

**Reality:** **Medium mechanically.** The room pool and ship fan-out provide most
of the execution path. The direction stays fifth because the portfolio has not
yet measured its selector. Sting Ops is the prerequisite evidence: do not scale
candidate production before knowing the review ladder can rank candidates.

## 6. Software Customs

**Pitch:** Pass a dependency, generated artifact, or untrusted build step through
an instrumented room and emit a behavioral manifest for gate.

**Mechanism:** Give a room a package or patch plus a synthetic environment with
canary files, fake credentials, simulated internal endpoints, resource limits,
and an allowed-write manifest. Exercise install, build, test, and representative
runtime paths. The intended evidence includes:

- Files created, modified, or deleted
- Writes outside permitted roots
- Network destinations and attempted exfiltration
- Persistence attempts
- Processes and resource consumption
- Conformance to the declared behavior manifest

Verification—not rooms—decides whether that behavior is acceptable.

**What rooms enables that the portfolio cannot do today:** Firecracker contains
the exercised code, the room disappears after the trial, and the overlay can
show accidental persistent writes. Cheap parallelism makes it possible to run
several trigger environments without contaminating the host.

But the current evidence is insufficient for the adversarial version of this
idea. `changeset.json` is reported from inside the guest by a root-capable
identity and can be forged. Rooms does not log syscall attempts or network
destinations, and a filesystem diff cannot observe an attempted read or blocked
exfiltration. Today this is a useful forensic signal for buggy or careless code,
not an airlock that can attest hostile code stayed within policy.

**Most likely killer:** False confidence from incomplete telemetry. Dormant
behavior, laboratory detection, and forged guest reports can all produce a
clean-looking manifest.

**Smallest experiment:** First add an adversarial control that modifies a
persistent path and then suppresses or forges the in-guest changeset. It is
expected to evade the current reporter; that result establishes host-side
overlay enumeration as a prerequisite. After that closes, run ten hostile
fixtures and 25 ordinary packages, including credential reads, outbound
exfiltration, `/etc` mutation, delayed children, and resource exhaustion.
Attempt-level syscall and network telemetry is a further prerequisite before
any verdict covers reads or exfiltration rather than persistent writes.

**Reality:** **Low as a security instrument; medium as non-adversarial
forensics.** This falls furthest under the portfolio lens because gate can only
compound evidence it can trust.

## Where the product and portfolio rankings disagree

The earlier product-oriented ranking was:

1. Counterfactual CI
2. Agent Airworthiness
3. Software Customs
4. Speculative Engineering
5. Action Escrow

The portfolio ranking adds Sting Ops at number one and moves the original five
to Airworthiness, Counterfactual CI, Action Escrow, Speculative Engineering, and
Software Customs.

The disagreements are substantive:

- **Sting Ops enters at #1** because it calibrates the Verification and
  Capability planes that every other autonomous path depends on. Improving the
  shared judge has greater portfolio leverage than improving one kind of trial.
- **Agent Airworthiness beats Counterfactual CI** because its bounded
  qualification verdict can immediately change ship dispatch policy and support
  a gate capability ceiling. Counterfactual results are valuable evidence, but
  the current portfolio has fewer decisions wired to consume them.
- **Counterfactual CI falls from #1 to #3** because its broad promise relied on
  clock, entropy, and network controls rooms does not have. Its honest v1 is a
  narrower controlled-input matrix.
- **Action Escrow rises despite low readiness** because it directly exercises
  the portfolio's core contract: proposal evidence → exact verdict → scoped
  grant → effect → hash-chained receipt.
- **Speculative Engineering falls** because it multiplies candidates before the
  portfolio has calibrated the selector. More Execution is not compounding when
  Verification is the bottleneck.
- **Software Customs falls from #3 to #6** because its interesting claim is
  adversarial, while the current evidence collector is guest-controlled and has
  no attempt-level network or syscall view.

## Provenance and cross-lineage convergence

Two other from-scratch passes, produced by different model lineages and blind to
this pass and to each other, independently recovered three of the same shapes:

- **License to Merge** converged with Agent Airworthiness.
- **The Airlock** converged with Software Customs.
- **Best-of-N speculative execution** converged with Speculative Engineering.

That convergence is the strongest evidence in this note that the core thesis is
real: cheap, isolated, attributable rooms naturally become a measurement
instrument for agent behavior, review reliability, and bounded authority. The
same affordances were discovered independently rather than suggested by shared
wording.

Convergence does not settle the implementation or rank:

- A “License to Merge” is unsound if aggregate agent performance directly
  authorizes an exact PR. Airworthiness should bound the agent's capability
  ceiling; the exact head still needs a supporting gate verdict.
- An “Airlock” overstates current rooms if it treats the guest-reported
  changeset as adversarial proof. Host-side reporting and attempt-level telemetry
  are prerequisites.
- Best-of-N is not leverage until the selection oracle is empirically reliable.
  Independent rediscovery does not remove that bottleneck.

The convergence strengthens the categories, not every version of the proposed
solution.

## A second axis deliberately out of frame

This note covers **rooms as a measurement instrument**: manufacture controlled
or bounded trials and emit attributable evidence.

A second axis is **rooms as a live execution fabric**: multi-host fleet control,
snapshot/restore warm pools, forking a running agent mid-task, and an MCP surface
agents call directly. Those directions could reduce trial latency and make
experiments cheaper, but they solve a different problem—where and how live work
runs rather than what evidence a trial produces. They are out of frame here, not
rejected. Snapshot/fork is already identified in the operational roadmap; the
other fabric ideas deserve their own comparison when a concrete consumer asks
for them.

## Revised recommendation

Build **Sting Ops first**, as a shadow calibration campaign with no merge
authority. Do not start by extending rooms with clock pinning, network replay,
or a general experiment language. The existing pool, resource caps, diff
attribution, and teardown are enough to learn whether the portfolio's reviewer
ladder is measurable.

The artifact path should be explicit:

| Step | Plane | Emits | Consumed by |
| ---: | --- | --- | --- |
| 1 | State / dossier | `sting-campaign.v1`: base heads, hidden mutation manifests, defect classes, reviewer set, repetitions, success bar | ship campaign driver |
| 2 | Execution / ship | Per-trial dispatch record binding agent/reviewer versions, prompt, model, time, control or mutant head, and room ID | rooms and later evaluation |
| 3 | Execution / rooms | `room-evidence.v1`: exact patch hash, test result, `changeset.json`, escape status, limits, teardown receipt | Verification; linked from dossier |
| 4 | Verification | `review-calibration-verdict.v1`: blind detection matrix, per-reviewer catch and false-positive rates, defect-class coverage, consolidated gate outcome, confidence, exact head hashes | gate policy and Observability |
| 5 | Capability / gate | A hash-chained shadow decision showing what scoped tier would have been allowed; later, only after an operator policy decision, a live time-boxed grant ceiling supported by the calibration verdict | sanctioned dispatch or merge path |
| 6 | State + Observability | Campaign record and read-only trend over reviewer/model versions | operator and later campaigns |

Gate must not turn a population-level calibration result into permission for a
specific synthetic or real head. The calibration verdict supports **policy about
ceilings**; the exact-head verdict supports **the action**.

If Sting Ops produces stable signal, the same campaign and scoring substrate
unlocks **Agent Airworthiness** next: replace “which reviewer caught this known
defect?” with “which agent configuration completed this known task safely?” The
two artifacts together then make **Speculative Engineering** defensible: ship
may fan out candidates only after the portfolio has measured both the workers
and the selector. Counterfactual matrices can reuse the same campaign runner
later, without pretending today's live clock and network are pinned inputs.

The revised thesis is:

> Rooms manufactures bounded, attributable execution evidence that lets the
> rest of the portfolio measure its workers, calibrate its judges, and grant
> authority on evidence rather than intuition.

