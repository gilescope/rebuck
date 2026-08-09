# Running a fleet

A distributed BuildKit. Point a client at it and work spreads across
machines; point it at a fleet of one and nothing changes.

Every claim below is checked by `rebuck2/scripts/fleet-check.sh`; the
measurements behind them are in [fleet-findings.md](fleet-findings.md).

## The smallest thing that works

One process is the fleet's coordinator: it holds the client's connection,
arbitrates placement, and serves the shared registry.

```sh
# the coordinator: gateway + driver + registry in one
REBUCK2_MIRROR=<addr-the-workers-see>:15000 \
  rebuck2 buildkit-proxy --listen 127.0.0.1:1234 \
    --upstream http://127.0.0.1:8372 \
    --registry-bind 0.0.0.0:15000 \
    --session my-fleet

export BUILDKIT_HOST=tcp://127.0.0.1:1234
```

Then a worker per machine that should build, each against its own buildkitd:

```sh
rebuck2 worker --session my-fleet \
  --buildkit-addr http://127.0.0.1:8372 \
  --registry-addr <addr-the-workers-see>:15000
```

**There is no `--peer`.** Workers find the coordinator over the iroh mesh by
deriving its identity from `--session`, so there is no address to configure
and no order to start them in. A worker that joins mid-build is simply
available for the next placement.

`REBUCK2_MIRROR` is the address the **workers' daemons** use, not the one you
use. It is baked into every rewritten graph, so all of them must resolve the
same string: on one host that is `host.docker.internal:15000`, across
machines a LAN address.

### Every daemon must trust the mirror

The mirror has no TLS. Each buildkitd therefore needs, in
`/etc/buildkit/buildkitd.toml`:

```toml
[registry."<the same addr as REBUCK2_MIRROR>"]
  http = true
  insecure = true
```

**This is not optional and its absence is silent.** Publishing is told to be
insecure per-solve, so the mirror fills up and the log says
`base ... mirrored as ...`; but a PULL is governed by daemon config alone, so
every peer then fails with

```text
Head "https://<mirror>/v2/...": http: server gave HTTP response to HTTPS client
```

The worker declines, the driver runs out of candidates, the build SUCCEEDS at
home, and a fleet that placed nothing looks exactly like a fleet that had
nothing to place. The driver names this one when it sees it - `LIKELY CAUSE:
a worker cannot PULL from the mirror` - but check `-> worker` in the log
before believing any fleet distributed anything.

## Which clients this can distribute

The rule is about where the graph is BUILT, not which tool builds it.

| client | dispatches |
| --------------------------------- | ---------- |
| `buildctl build < graph.llb` | yes |
| anything driving the gateway with LLB | yes |
| earthbuild | partly - see [earthly-dispatch.md](earthly-dispatch.md) |
| `--frontend dockerfile.v0` | no |
| `docker build`, `buildx` | no |

A named frontend is resolved **inside** the daemon, so its LLB never crosses
the proxy and there is nothing to place. That is structural, not a gap.

## Placement

One rule, no tuning: **work is only sent away once the local machine is
full.** Slots default to the local core count.

That happens to land on the best split a hand sweep could find at the build
size where a fleet matters, and it needs no estimate of how fast anyone is.
Attempts to do better by ranking machines all measured worse - see the
findings doc before trying again.

**Which** worker takes it is the driver's decision, not the gateway's, and it
is the same arbitration a worker gets when it subdivides: candidates filtered
to those that can actually run the subtree, emptiest first, ties on worker id
so the choice is deterministic. There are no weights - `--peer url*N` went
with the transport that had a peer list.

A worker's load counts the subtree leads it is already holding, not only its
REAPI jobs. Without that every worker priced as idle and the first candidate
took everything: measured four leads, all to one worker, its neighbour never
offered anything. Read `-> worker N` in the driver's log to see the spread;
`placed` only says home-or-fleet.

## Failure

Everything fails open: a build that cannot be distributed is a build that
runs locally, at ordinary speed.

- a worker that refuses or dies: declines, the driver offers the next, and
  the requester builds it itself if nobody takes it
- a worker that cannot build the architecture natively: not offered it
- nobody takes it at all: built at home, which is what would have happened
  without a fleet

Verified by destroying machines mid-build. Outputs stayed byte-identical to a
one-machine baseline every time.

**A worker that goes SLOW is not handled.** The proxy used to withdraw an
adoption past three times the observed median and rebuild at home, leaving
the peer running so whoever published first won. That lived in the transport
the mesh replaced, and there is no equivalent yet: a machine that crawls
holds its lead until it finishes. Slowness costs time, not correctness - the
bytes are still right - but one bad machine can pace a build.

## The three permissions

A subtree is grounded by anything a peer cannot service. Three of those can
be lifted, and all three are **off by default** because each hands something
to another machine.

| variable | what it permits | what it costs you |
| -------- | --------------- | ----------------- |
| `REBUCK2_PEER_CACHE_MOUNTS=1` | a worker uses its own cache mount | nothing leaves your machine; a colder cache, which the cache-mount contract already allows |

**Secrets and the ssh agent no longer travel, and the two flags for them do
nothing.** A dispatched subtree used to be built by a peer over a connection
this process had opened, so it could attach a session and answer the peer's
secret lookups itself. A worker builds on its own connection with no session
to answer, so a graph naming a secret stays home - which is what principle 10
said all along: one secret anywhere excludes the whole subtree.

`REBUCK2_SERVE_SECRETS` and `REBUCK2_FORWARD_AGENT` are still read and still
lift the exclusion, so the graph is offered and every worker declines it. The
build is correct and the round trip is wasted. They should either go or grow
a session on the worker side; until one of those happens, leave them off.

**Never lifted:** insecure/privileged exec and host networking. Granting a
privilege is a trust decision, and no session service makes a peer's
`--privileged` mean what yours would have meant.

## Every knob

| | |
| ---------------------------- | --------------------------------------- |
| `--listen` | where clients connect (default `127.0.0.1:1234`) |
| `--upstream` | your own buildkitd |
| `--registry-bind` | serve the mesh-backed registry here |
| `--session` | fleet name; workers derive the coordinator's identity from it |
| `worker --buildkit-addr` | the daemon that worker builds on |
| `worker --registry-addr` | the coordinator's registry, as that worker sees it |
| `REBUCK2_MIRROR` | registry address **as the workers' daemons see it** |
| `REBUCK2_HOME_SLOTS` | local concurrency before work is sent away (default: cores) |
| `REBUCK2_PEER_CACHE_MOUNTS` | see above |
| `REBUCK2_ADAPT=1` | derive weights from service times. **Unstable** - it chases itself, see the findings doc |
| `REBUCK2_GATE=1` | skip subtrees smaller than their transfer. Does not fire in practice |

The last two are opt-in experiments that did not work, kept with their
measurements rather than deleted, so nobody re-derives them.

## The mirror grows and nothing prunes it

Known gap, stated plainly because the failure arrives weeks after the
decision to run this.

The mirror keeps one copy of each base image per architecture, plus **every
adopted result, forever**. There is no gc, no size cap and no expiry. When the
disk fills, pushes fail, peers refuse, and the fleet goes idle - the report
will say it did no distributed work, but it will blame the peer rather than
the disk.

### What it actually costs

Soaked: 32 builds over 8 rounds, 24 of them adopted, 4 distinct graphs, one
persistent store.

| | blobs | orphaned |
| ------------------------------------ | ----- | -------- |
| before the exporter was made reproducible | 75 | 60 |
| now | 15 | 0 |

**Repeating a build costs nothing.** 24 adoptions of 4 graphs leave exactly
the 4 results and their shared base. That is not free by default - buildkit
stamps wall clock into the image config and real mtimes into the layer, so an
unchanged input used to republish as fresh bytes every time and orphan
whatever the tag previously named. Both `source-date-epoch` and
`rewrite-timestamp` are set on every push this fleet makes, which is what
collapses 75 blobs to 15.

So the store grows with the number of **distinct** results, not with the
number of builds. A CI fleet rebuilding one commit all day adds nothing after
the first round.

It still grows without bound across distinct results, and there is still no
gc. Size it against your result payload: 24 adoptions of a graph exporting
300 MB would be one 300 MB copy now, but a hundred distinct such graphs are a
hundred copies.

Until that is fixed, treat the store as something you watch:

```sh
du -sh <the --store path>
```

and delete it when convenient. Losing it costs nothing but re-mirroring: the
content is a cache, and every tag in it is derivable from a graph someone
still has.

Not fixed in this pass on purpose. Evicting blobs correctly means walking
tags to manifests to blobs and removing only what nothing references; doing
it approximately means a mirror that serves images with missing layers,
which is silent corruption and the one failure this design works hardest to
avoid.

## Reading the report

`SIGINT` the proxy and it prints what the build looked like. The line that
matters is `placed`:

```text
[wire] placed         : {0: 16, 1: 8} (0 = home, 1 = the fleet)
[driver] subtree job 7 -> worker 2
```

`placed` says how much left this machine. It does NOT say where it went, and
reading it as though it did hid a fleet running entirely on one worker - the
suite's own spread check passed throughout. `-> worker N` is the line that
names a machine. Wall clock moves for
unrelated reasons, and a control run has twice overturned a conclusion drawn
from it.

The other line to read when a fleet has gone lopsided:

```text
[wire] struck         : {1: 4}
```

A strike is this proxy deciding a machine is the problem, and it is the one
judgement that outlives the solve that made it - a struck peer stays
deprioritised for the rest of the run. Printed even when empty, so `{}` is
evidence rather than silence: a mirror that dies takes the fleet down with it
but must cost no peer its standing.
