# Running a fleet

A distributed BuildKit. Point a client at it and work spreads across
machines; point it at a fleet of one and nothing changes.

Every claim below is checked by `rebuck2/scripts/fleet-check.sh`; the
measurements behind them are in [fleet-findings.md](fleet-findings.md).

## The smallest thing that works

```sh
# a peer-to-peer mirror the daemons can all reach
rebuck2 registry --bind 0.0.0.0:15000 --store ~/data/rebuck2-mirror

# the fleet, in front of your own buildkitd
REBUCK2_MIRROR=<addr-the-daemons-see>:15000 \
  rebuck2 buildkit-proxy --listen 127.0.0.1:1234 \
    --upstream http://127.0.0.1:8372 \
    --peer http://other-machine:8372

export BUILDKIT_HOST=tcp://127.0.0.1:1234
```

`REBUCK2_MIRROR` is the address the **daemons** use, not the one you use. It
is baked into every rewritten graph, so all daemons must resolve the same
string: on one host that is `host.docker.internal:15000`, across machines a
LAN address.

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

Dispatch falls back to building at home, the build SUCCEEDS, and a fleet that
placed nothing looks exactly like a fleet that had nothing to place. Check
`placed` in the report before believing otherwise.

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
Attempts to do better by ranking peers all measured worse - see the findings
doc before trying again.

Peers may be weighted (`--peer http://host:port*2`), but a weight is a claim
about observed end-to-end throughput, **not** about hardware. Setting one
from core counts measured worse than leaving it alone.

## Failure

Everything fails open: a build that cannot be distributed is a build that
runs locally, at ordinary speed.

- a peer that refuses or dies: struck, avoided afterwards, work rebuilt at home
- a peer that goes slow: withdrawn after 3x the observed median
- the mirror down: the fleet stops offering entirely until it returns
- a peer that cannot build the architecture natively: not offered it

Verified by destroying each of them mid-build. Outputs stayed byte-identical
to a one-machine baseline every time.

## The three permissions

A subtree is grounded by anything a peer cannot service. Three of those can
be lifted, and all three are **off by default** because each hands something
to another machine.

| variable | what it permits | what it costs you |
| -------- | --------------- | ----------------- |
| `REBUCK2_PEER_CACHE_MOUNTS=1` | a peer uses its own cache mount | nothing leaves your machine; a colder cache, which the cache-mount contract already allows |
| `REBUCK2_SERVE_SECRETS=1` | this process answers the peer's secret lookups from its own environment | the secret's VALUE reaches the peer |
| `REBUCK2_FORWARD_AGENT=1` | this process forwards its ssh agent | a CAPABILITY reaches the peer: it can sign anything, for as long as the build runs |

Read that last row twice. A secret is a string; an agent is the ability to
use your key. Enable it only on a fleet whose machines you would already
trust with the key itself.

Secrets are lifted per GRAPH, not per capability: if any secret a graph names
cannot be resolved here, the whole subtree stays home rather than failing on
the peer.

**Never lifted:** insecure/privileged exec and host networking. Granting a
privilege is a trust decision, and no session service makes a peer's
`--privileged` mean what yours would have meant.

## Every knob

| | |
| ---------------------------- | --------------------------------------- |
| `--listen` | where clients connect (default `127.0.0.1:1234`) |
| `registry --bind` | mirror listen address (default `127.0.0.1:5000`) |
| `--upstream` | your own buildkitd |
| `--peer URL[*N]` | repeatable; `*N` is a relative share |
| `REBUCK2_MIRROR` | registry address **as the daemons see it** |
| `REBUCK2_HOME_SLOTS` | local concurrency before work is sent away (default: cores) |
| `REBUCK2_HOME_WEIGHT` | home's share, for a weighted split |
| `REBUCK2_PEER_CACHE_MOUNTS` | see above |
| `REBUCK2_SERVE_SECRETS` | see above |
| `REBUCK2_FORWARD_AGENT` | see above |
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
[wire] placed         : {0: 16, 1: 8} (0 = home)
```

That is the only honest evidence of distribution. Wall clock moves for
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
