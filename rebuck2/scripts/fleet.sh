#!/usr/bin/env bash
# Stand up a distributed buildkit and measure it.
#
# One registry (the peer-to-peer mirror), N buildkitd daemons, one rebuck2
# proxy in front of daemon 0, and a fan-out of plain-LLB builds pushed
# through the proxy with buildctl. Prints the wire report, which is the
# measurement: how many solves were routed to another daemon, how many were
# peer 0's own share, and the reason for every one that stayed put.
#
# This existed as twenty-five hand-typed shell lines and twenty-five log
# files with names like reg9.log before it existed as a script, and every
# re-run risked measuring a slightly different thing. Reproducibility is
# the point: same inputs, same fleet, comparable numbers.
#
#   rebuck2/scripts/fleet.sh            # 2 daemons, 4 builds
#   DAEMONS=3 BUILDS=6 rebuck2/scripts/fleet.sh
#   NOPROXY=1 rebuck2/scripts/fleet.sh  # baseline: one daemon, client talks direct
#
# Correctness, not just speed - build the baseline, then check the fleet
# returns the same bytes:
#   NOPROXY=1 DAEMONS=1 RUN=~/data/base rebuck2/scripts/fleet.sh
#   EXPECT=~/data/base/digests.txt rebuck2/scripts/fleet.sh
#
# Needs docker. buildctl is used from the host if present and borrowed from
# the buildkit image otherwise. Leaves nothing running.
set -euo pipefail

DAEMONS=${DAEMONS:-2}
BUILDS=${BUILDS:-4}
NOPROXY=${NOPROXY:-}
IMAGE=${IMAGE:-moby/buildkit:latest}
# The daemon's ARCHITECTURE, pinned. Without this the fleet's architecture is
# whatever happens to be in the local image cache: pulling the same tag once
# with `--platform linux/amd64` leaves an amd64 image under it, and every
# later `docker run` silently reuses it under QEMU. That is a 5-10x slowdown
# that looks like a slow fleet, and it is invisible except for one warning
# line docker prints to stderr.
PLATFORM=${PLATFORM:-linux/$(uname -m | sed 's/x86_64/amd64/; s/aarch64/arm64/')}
RUN=${RUN:-${TMPDIR:-/tmp}/rebuck2-fleet}
BASE_PORT=${BASE_PORT:-18372}
REG_PORT=${REG_PORT:-15000}
# The base image the fixtures build on.
#
# Fresh daemons pull it every run, and a day of that earns a 429 from Docker
# Hub that looks exactly like a product failure. Overridable so a site with
# its own mirror can avoid that; there is no built-in answer, because
# rebuck2's own pull-through registry serves BLOBS by digest and not
# manifests, so it cannot stand in for Hub without being extended.
export REBUCK2_LLB_BASE="${REBUCK2_LLB_BASE:-docker-image://docker.io/library/alpine:3.20}"
PROXY_PORT=${PROXY_PORT:-11234}
# CPU quota for the LAST daemon, e.g. SLOW=0.25. A real fleet is never
# uniform, and placement that ignores capacity is invisible until one machine
# is slower than the rest.
SLOW=${SLOW:-}
# How many times to run the build set through the SAME proxy. One round can
# only ever measure a cold fleet: every solve of a fan-out is placed before
# any of them has finished, so nothing learned during a round can affect it.
ROUNDS=${ROUNDS:-1}
# Build a Dockerfile instead of raw LLB. The canonical client for any project
# that is not earthbuild, and the only shape that exercises a build CONTEXT -
# every LLB run reports "contexts published: 0", so that whole path was
# untested.
DOCKERFILE=${DOCKERFILE:-}
# LLB that reads a real build CONTEXT from the client's disk. The only mode
# that exercises context publishing, which every other run reports as 0.
CONTEXT=${CONTEXT:-}
# LLB whose exec mounts a SECRET. Undispatchable by construction until a peer
# could be handed a session, so this is the fixture that proves it can.
SECRET=${SECRET:-}
# LLB whose exec asks for PRIVILEGED mode. The one exclusion with no lift:
# cache mounts, secrets and agents each have a knob, but granting a peer
# privileged exec is a trust decision and no session service makes the peer's
# `--privileged` mean what this machine's would have meant.
INSECURE=${INSECURE:-}
# Same fixture, asking for HOST NETWORKING instead. Excluded for the same
# reason: host networking means THIS host's network, and a peer honouring it
# would silently mean a different one.
HOSTNET=${HOSTNET:-}
# LLB whose exec mounts a CACHE. Excluded from dispatch until a peer was
# allowed its own, which is what REBUCK2_PEER_CACHE_MOUNTS=1 permits.
CACHE=${CACHE:-}
# LLB whose exec mounts an SSH AGENT. Needs a real agent on this machine.
SSHM=${SSHM:-}
# A peer on ANOTHER MACHINE. Everything else here runs several daemons on one
# host, which can measure overhead and placement but never capacity: the fleet
# has no more CPU than the single daemon did.
#   REMOTE=user@host  (or just host)
# The mirror must then be named by an address BOTH machines can reach, so all
# daemons are given the same LAN address rather than host.docker.internal -
# the mirror name is baked into the rewritten graph, so it has to be one name.
REMOTE=${REMOTE:-}
MIRROR_HOST=${MIRROR_HOST:-host.docker.internal}
REMOTE_PORT=${REMOTE_PORT:-18400}
# How many daemons to start on the remote host. More than one gives
# `least_loaded` an actual choice between away peers - with a single one it is
# trivially the answer and its ranking has never been exercised for real.
REMOTE_DAEMONS=${REMOTE_DAEMONS:-1}
# The remote's share, relative to this machine. Buildkit does not report core
# counts, so somebody has to say. 2 means "twice the turns".
REMOTE_WEIGHT=${REMOTE_WEIGHT:-1}
# Kill the LAST daemon this many seconds into the build. Fail-open is a
# stated principle and had never been tested by actually breaking something:
# a build must still finish, with the right bytes, when a peer dies holding
# work.
KILL_AFTER=${KILL_AFTER:-}
# Kill the REGISTRY this many seconds into the build. Everything flows
# through it - published contexts, mirrored bases, adopted results - so it is
# the single point the fleet actually depends on.
KILL_REGISTRY_AFTER=${KILL_REGISTRY_AFTER:-}

crate=$(cd "$(dirname "$0")/.." && pwd)
# Keyless rendezvous: proxy and workers derive the driver's iroh identity from
# this string, so nothing has to learn an address. Per-run, or two concurrent
# rigs would join each other's fleet.
SESSION=${SESSION:-"fleet-$$-$(date +%s 2>/dev/null || echo 0)"}
rm -rf "$RUN"
mkdir -p "$RUN/llb" "$RUN/store"

pids=()
containers=()
# shellcheck disable=SC2329  # invoked by the EXIT trap, not by name
cleanup() {
  for p in "${pids[@]}"; do kill "$p" 2>/dev/null || true; done
  for c in "${containers[@]}"; do docker rm -f "$c" >/dev/null 2>&1 || true; done
  if [ -n "${REMOTE:-}" ]; then
    SSH_AUTH_SOCK="" ssh "$REMOTE" \
      "docker ps -aq --filter name=rebuck2-fleet-remote | xargs -r docker rm -f" \
      >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

say() { printf '\n=== %s\n' "$*"; }

# sha256, wherever it lives. macOS ships `shasum`, Linux ships `sha256sum`,
# and a harness that only runs on the machine it was written on cannot be the
# thing CI uses to check the claims.
if command -v sha256sum >/dev/null 2>&1; then
  sha256() { sha256sum; }
else
  sha256() { shasum -a 256; }
fi

# buildctl, from wherever it can be had. The buildkit image ships it, so a
# host without it is not a blocker - but `--network host` on Docker Desktop
# does NOT reach the host's loopback, so borrowed buildctl must dial
# host.docker.internal. Rewriting the address here rather than at every call
# site is the whole reason this is a function.
#
# It also decides how far the fleet has to be published. A host buildctl
# reaches loopback, so ports stay on 127.0.0.1. A borrowed one runs in a
# container, and `host-gateway` resolves to the host's routable address -
# which a service bound to loopback will not answer. So borrowing costs an
# all-interfaces bind on the daemons. Say so rather than defaulting to it:
# these are PRIVILEGED daemons that will run anything they are handed.
if command -v buildctl >/dev/null 2>&1; then
  BIND=127.0.0.1
  bctl() { buildctl "$@"; }
else
  BIND=0.0.0.0
  echo "note: no host buildctl; publishing daemons on $BIND so a borrowed one can reach them"
  # An array, because the two halves of `-v path:path` must stay two
  # arguments and shellcheck is right that unquoted expansion to achieve
  # that is a trap waiting for a path with a space in it.
  agent_mount=()
  if [ -n "${SSH_AUTH_SOCK:-}" ]; then
    agent_mount=(-v "$SSH_AUTH_SOCK:$SSH_AUTH_SOCK")
  fi
  bctl() {
    local args=()
    for a in "$@"; do args+=("${a//127.0.0.1/host.docker.internal}"); done
    # $RUN is mounted at the same path so `--output dest=$RUN/...` lands on
    # the host and not inside a container that is about to be deleted.
    docker run --rm -i --add-host host.docker.internal:host-gateway \
      -e "rebuck2_probe=${rebuck2_probe:-}" \
      -e "SSH_AUTH_SOCK=${SSH_AUTH_SOCK:-}" \
      "${agent_mount[@]}" \
      -v "$RUN:$RUN" --entrypoint buildctl "$IMAGE" "${args[@]}"
  }
fi

say "build"
(cd "$crate" && cargo build --quiet)
bin="$crate/target/debug/rebuck2"

# The LLB. Four independent CPU-bound execs on a common base: the shape
# where a fleet can win and a single daemon cannot fake it. A sleep-based
# workload measures nothing - one daemon serves four sleeps as fast as four
# daemons do, so the fleet looks free when it is not.
if [ -n "$DOCKERFILE" ]; then
  say "generate dockerfile context"
  mkdir -p "$RUN/ctx"
  # One file per build so the builds differ, as the LLB fixtures do. Same
  # CPU-bound shape: a fleet cannot fake parallelism on it.
  for i in $(seq 0 $((BUILDS - 1))); do
    mkdir -p "$RUN/ctx/$i"
    echo "task-$i" >"$RUN/ctx/$i/marker"
    cat >"$RUN/ctx/$i/Dockerfile" <<DF
FROM alpine:3.20
COPY marker /marker
RUN i=0; while [ \$i -lt 90 ]; do dd if=/dev/zero bs=1M count=20 2>/dev/null | sha256sum >/dev/null; i=\$((i+1)); done
RUN mkdir -p /result && cp /marker /result/task
FROM scratch
COPY --from=0 /result /
DF
  done
  ls "$RUN/ctx"
fi

say "generate llb"
fixture=write_fanout_llb
if [ -n "$CACHE" ]; then fixture=write_cache_llb; fi
if [ -n "$SSHM" ]; then fixture=write_ssh_llb; fi
if [ -n "$INSECURE" ]; then fixture=write_insecure_llb; fi
if [ -n "$HOSTNET" ]; then
  fixture=write_insecure_llb
  export REBUCK2_LLB_HOSTNET=1
fi
if [ -n "$SECRET" ]; then
  fixture=write_secret_llb
  export rebuck2_probe=the-value
fi
if [ -n "$CONTEXT" ]; then
  fixture=write_context_llb
  for i in $(seq 0 $((BUILDS - 1))); do
    mkdir -p "$RUN/ctx/$i"
    echo "task-$i" >"$RUN/ctx/$i/marker"
  done
fi
(cd "$crate" && REBUCK2_LLB_OUT="$RUN/llb" REBUCK2_LLB_N="$BUILDS" \
  cargo test --quiet --bin rebuck2 "$fixture" -- --ignored >/dev/null)
printf "%s\n" "$RUN"/llb/*.llb

say "daemons: $IMAGE on $PLATFORM"
docker pull --platform "$PLATFORM" -q "$IMAGE" >/dev/null
echo "image arch: $(docker image inspect "$IMAGE" --format '{{.Architecture}}')"

# The registry is served BY THE PROXY, over its driver, so it is mesh-backed:
# a layer built on one worker is served to another over iroh. A separate
# process here would hold no provider index and would be a third copy of a job
# the mesh already does. The baseline needs one anyway, having no proxy.
if [ -n "$NOPROXY" ]; then
  say "registry on 0.0.0.0:$REG_PORT (baseline has no proxy to host it)"
  "$bin" registry --bind "0.0.0.0:$REG_PORT" --store "$RUN/store" >"$RUN/registry.log" 2>&1 &
  reg_pid=$!
  pids+=("$reg_pid")
fi

# Daemons trust the mirror over plain HTTP. Without this a peer with no
# session cannot pull what another daemon published, and the failure looks
# like a portability bug rather than a config one.
cat >"$RUN/buildkitd.toml" <<TOML
root = "/var/lib/buildkit"
[grpc]
  address = [ "tcp://0.0.0.0:8372" ]
[worker.oci]
  enabled = true
  max-parallelism = 20
TOML
# NO_REGISTRY_TRUST=1 leaves this stanza out, which is the single most
# likely misconfiguration and the one that fails SILENTLY: publishing is
# insecure per-solve so the mirror fills, pulling is not, so every peer
# refuses and the build quietly falls back to home.
if [ -z "${NO_REGISTRY_TRUST:-}" ]; then
  # The coordinator's, plus one per worker when they serve their own. A
  # daemon that does not trust the registry it is told to pull from fails the
  # silent way described above, so the trust list has to cover every address
  # any daemon might be handed - not just the shared one.
  for r in $(seq 0 $((DAEMONS - 1))); do
    cat >>"$RUN/buildkitd.toml" <<TOML
[registry."$MIRROR_HOST:$((REG_PORT + r))"]
  http = true
  insecure = true
TOML
  done
fi


for i in $(seq 0 $((DAEMONS - 1))); do
  port=$((BASE_PORT + i))
  name="rebuck2-fleet-$i"
  docker rm -f "$name" >/dev/null 2>&1 || true
  # FOREIGN=linux/amd64 makes the LAST daemon a foreign-architecture one, so
  # native-vs-emulated placement has something to choose between.
  plat="$PLATFORM"
  if [ -n "${FOREIGN:-}" ] && [ "$i" -eq $((DAEMONS - 1)) ]; then
    plat="$FOREIGN"
    docker pull --platform "$plat" -q "$IMAGE" >/dev/null
    say "daemon $i is FOREIGN ($plat)"
  fi
  limit=()
  if [ -n "$SLOW" ] && [ "$i" -eq $((DAEMONS - 1)) ]; then
    limit=(--cpus "$SLOW")
    say "daemon $i is the SLOW one (--cpus $SLOW)"
  fi
  # `"${limit[@]}"`, NOT `"${limit[@]:-}"`: the `:-` form expands an empty
  # array to one EMPTY ARGUMENT, and docker reads that as the image name and
  # fails with "invalid reference format". Bash 5 handles an empty `[@]` under
  # `set -u` correctly on its own.
  docker run -d --name "$name" --privileged \
    --platform "$plat" \
    "${limit[@]}" \
    -p "$BIND:$port:8372" \
    -v "$RUN/buildkitd.toml:/etc/buildkit/buildkitd.toml:ro" \
    --add-host host.docker.internal:host-gateway \
    "$IMAGE" \
    ${INSECURE:+--allow-insecure-entitlement security.insecure} \
    ${HOSTNET:+--allow-insecure-entitlement network.host} >/dev/null
  containers+=("$name")
  say "daemon $i on 127.0.0.1:$port ($name)"
done

if [ -n "$REMOTE" ]; then
  # SSH_AUTH_SOCK is cleared because a GPG agent holding the socket refuses
  # ED25519 signing and the connection dies with a misleading auth error.
  scp -q "$RUN/buildkitd.toml" "$REMOTE:/tmp/rebuck2-buildkitd.toml"
  remote_host=${REMOTE##*@}
  for r in $(seq 0 $((REMOTE_DAEMONS - 1))); do
    rport=$((REMOTE_PORT + r))
    # shellcheck disable=SC2029  # client-side expansion is intended: the port
    # and image are this harness's choice, not the remote host's.
    SSH_AUTH_SOCK="" ssh "$REMOTE" "docker rm -f rebuck2-fleet-remote-$r >/dev/null 2>&1;
      docker run -d --name rebuck2-fleet-remote-$r --privileged \
        -p 0.0.0.0:$rport:8372 \
        -v /tmp/rebuck2-buildkitd.toml:/etc/buildkit/buildkitd.toml:ro \
        $IMAGE" >/dev/null
    # No --peer any more. A remote daemon joins by running a WORKER against
    # it, the same way a local one does; the mesh finds the driver by session
    # and needs no address here.
    say "remote daemon $r on $remote_host:$rport (start a worker against it)"
  done
fi

# Daemons are not ready when `docker run` returns; ListWorkers is the only
# honest readiness signal.
for i in $(seq 0 $((DAEMONS - 1))); do
  port=$((BASE_PORT + i))
  for _ in $(seq 1 60); do
    bctl --addr "tcp://127.0.0.1:$port" debug workers >/dev/null 2>&1 && break
    sleep 1
  done
done

if [ -n "$NOPROXY" ]; then
  addr="tcp://127.0.0.1:$BASE_PORT"
  say "baseline: client -> daemon 0 direct"
else
  addr="tcp://127.0.0.1:$PROXY_PORT"
  say "proxy on $addr -> daemon 0, plus $((DAEMONS - 1)) worker(s) on the mesh"
  # UNRESOLVABLE=1 keeps the secret from the PROXY while leaving it with the
  # client. That is the earthly shape: a secret the build legitimately uses
  # and nothing outside the client process can produce.
  proxy_secret="${rebuck2_probe:-}"
  if [ -n "${UNRESOLVABLE:-}" ]; then proxy_secret=""; fi
  rebuck2_probe="$proxy_secret" \
    REBUCK2_MIRROR="$MIRROR_HOST:$REG_PORT" \
    "$bin" buildkit-proxy --listen "$BIND:$PROXY_PORT" \
    --upstream "http://127.0.0.1:$BASE_PORT" \
    --registry-bind "0.0.0.0:$REG_PORT" \
    --session "$SESSION" --store "$RUN/store" >"$RUN/proxy.log" 2>&1 &
  proxy_pid=$!
  pids+=("$proxy_pid")

  # One worker per AWAY daemon. Daemon 0 is the proxy's own upstream and is
  # not a worker: the gateway builds its share there by falling through.
  # Rendezvous is keyless - both sides derive the driver's identity from
  # $SESSION - so there is no address to configure and no ordering to get
  # right.
  # PER_WORKER_REGISTRY=1 gives each worker its own, backed by the fleet: its
  # daemon pulls from localhost and a miss is answered by whoever built the
  # layer. That is the configuration a second MACHINE needs, because the
  # coordinator's registry is not reachable from one - so it is worth being
  # able to run it here, where a failure is a log away rather than a CI run.
  #
  # Off by default until it has earned it: every measurement to date used the
  # shared registry, and switching the default would silently re-baseline all
  # of them.
  for i in $(seq 1 $((DAEMONS - 1))); do
    if [ -n "${PER_WORKER_REGISTRY:-}" ]; then
      wreg="127.0.0.1:$((REG_PORT + i))"
      bind=(--registry-bind "0.0.0.0:$((REG_PORT + i))")
      # The daemon is a container, so it reaches its worker's registry the
      # same way it reaches the coordinator's - via the host, not localhost.
      seen="$MIRROR_HOST:$((REG_PORT + i))"
    else
      wreg=""
      bind=()
      seen="$MIRROR_HOST:$REG_PORT"
    fi
    "$bin" worker --session "$SESSION" \
      --store "$RUN/worker-$i" \
      --buildkit-addr "http://127.0.0.1:$((BASE_PORT + i))" \
      "${bind[@]}" \
      --registry-addr "$seen" >"$RUN/worker-$i.log" 2>&1 &
    pids+=("$!")
    [ -n "$wreg" ] && say "worker $i serves its own registry on $wreg"
  done
  for _ in $(seq 1 30); do
    bctl --addr "$addr" debug workers >/dev/null 2>&1 && break
    sleep 1
  done
  # BARRIER on the mesh, not just on the gateway. A worker joins over iroh
  # some hundreds of ms after its process starts, and the proxy asks
  # `worker_count() > 0` per solve - so without this the first builds see an
  # empty fleet, dispatch nothing, and the run reports a placement failure
  # that is really a startup race. re-e2e.yml waits the same way.
  want=$((DAEMONS - 1))
  if [ "$want" -gt 0 ]; then
    # Bounded at 20s, not 60. A worker that has not joined in twenty seconds
    # is not joining, and waiting the full minute in every scenario that
    # cannot satisfy this is what turned a six-minute suite into a
    # hundred-and-eight-minute one. Say so rather than proceeding quietly:
    # a short fleet is a real finding, and every placement assertion
    # downstream depends on it.
    for _ in $(seq 1 20); do
      joined=$(grep -c "^\[driver\] worker .* joined" "$RUN/proxy.log" 2>/dev/null || true)
      [ "${joined:-0}" -ge "$want" ] && break
      sleep 1
    done
    say "${joined:-0}/$want worker(s) on the mesh"
    if [ "${joined:-0}" -lt "$want" ]; then
      say "WARNING: fleet is short - placement below is not what was asked for"
    fi
  fi
fi

fail=0
walls=()
for round in $(seq 1 "$ROUNDS"); do
  say "round $round/$ROUNDS: $BUILDS builds through $addr"
  start=$(date +%s)
  n=0
  builds=()
  for f in "$RUN"/llb/*.llb; do
    if [ "$n" -ge "$BUILDS" ]; then break; fi
    # Export the result. Exit 0 says a build ran; it says nothing about what
    # came back, and a distributed buildkit that returns the wrong bytes is
    # worse than a slow one. The exported tree is what gets hashed below.
    rm -rf "$RUN/out-$n"
    if [ -n "$DOCKERFILE" ]; then
      bctl --addr "$addr" build --no-cache \
        --frontend dockerfile.v0 \
        --local "context=$RUN/ctx/$n" --local "dockerfile=$RUN/ctx/$n" \
        --output "type=local,dest=$RUN/out-$n" >"$RUN/build-$n.log" 2>&1 &
    elif [ -n "$CONTEXT" ]; then
      # The name must match the fixture's `local://` identifier exactly, and
      # it deliberately contains a slash: see write_context_llb.
      bctl --addr "$addr" build --no-cache \
        --local "${REBUCK2_LLB_CONTEXT:-./ctx/sub}=$RUN/ctx/$n" \
        --output "type=local,dest=$RUN/out-$n" <"$f" >"$RUN/build-$n.log" 2>&1 &
    elif [ -n "$SSHM" ]; then
      # The CLIENT forwards its own agent for the home builds, exactly as it
      # always could; only the dispatched ones need the proxy to forward one
      # to the peer.
      bctl --addr "$addr" build --no-cache --ssh default \
        --output "type=local,dest=$RUN/out-$n" <"$f" >"$RUN/build-$n.log" 2>&1 &
    elif [ -n "$SECRET" ]; then
      # The CLIENT serves the secret too. Home builds resolve it through the
      # client's own session exactly as they always did; only the dispatched
      # ones need the proxy to serve a second session to the peer.
      bctl --addr "$addr" build --no-cache \
        --secret id=rebuck2_probe,env=rebuck2_probe \
        --output "type=local,dest=$RUN/out-$n" <"$f" >"$RUN/build-$n.log" 2>&1 &
    elif [ -n "$HOSTNET" ]; then
      bctl --addr "$addr" build --no-cache --allow network.host \
        --output "type=local,dest=$RUN/out-$n" <"$f" >"$RUN/build-$n.log" 2>&1 &
    elif [ -n "$INSECURE" ]; then
      # The daemon needs the entitlement too, granted at launch. Without
      # both halves every build fails with "insecure is not allowed" and the
      # scenario degenerates into proving a broken build does not travel.
      bctl --addr "$addr" build --no-cache --allow security.insecure \
        --output "type=local,dest=$RUN/out-$n" <"$f" >"$RUN/build-$n.log" 2>&1 &
    else
      bctl --addr "$addr" build --no-cache \
        --output "type=local,dest=$RUN/out-$n" <"$f" >"$RUN/build-$n.log" 2>&1 &
    fi
    builds+=($!)
    n=$((n + 1))
  done
  if [ -n "$KILL_REGISTRY_AFTER" ]; then
    ( sleep "$KILL_REGISTRY_AFTER"
      echo "=== killing the registry mid-build"
      kill "${reg_pid:-$proxy_pid}" 2>/dev/null || true ) &
  fi
  if [ -n "$KILL_AFTER" ]; then
    victim="rebuck2-fleet-$((DAEMONS - 1))"
    ( sleep "$KILL_AFTER"
      echo "=== killing $victim mid-build"
      docker rm -f "$victim" >/dev/null 2>&1 || true ) &
  fi
  # Wait on the BUILD pids only. A bare `wait` also waits on the registry and
  # the proxy, neither of which ever exits, so the harness hangs forever
  # having already finished the measurement.
  for p in "${builds[@]}"; do
    wait "$p" || fail=1
  done
  walls+=($(($(date +%s) - start)))
done

# Docker Hub rate limiting looks exactly like a product failure: builds fail,
# `failed` is non-zero, and the wire report shows a fleet that did nothing.
# It is neither. Say so before anyone reads the rest.
if grep -qE "429 Too Many Requests" "$RUN"/build-*.log 2>/dev/null; then
  echo
  echo "!! RATE LIMITED by Docker Hub - these results say nothing about the fleet."
  echo "!! Every run starts fresh daemons that pull the base image; wait, or"
  echo "!! authenticate, or point IMAGE/the fixture at a local mirror."
  ratelimited=1
fi

say "result"
echo "daemons : $DAEMONS"
echo "builds  : $BUILDS"
echo "wall    : ${walls[*]}s (per round)"
echo "failed  : $fail"
echo "ratelimited: ${ratelimited:-0}"

if [ -z "$NOPROXY" ]; then
  # The wire report prints on SIGINT, so ask for it before the trap kills
  # everything with SIGTERM.
  kill -INT "$proxy_pid" 2>/dev/null || true
  # NOT `kill -INT "${reg_pid:-0}"`. PID 0 means "every process in my process
  # group", so the fallback signalled the script itself and the run ended
  # right after printing a clean summary - exit 1 with nothing to explain it.
  [ -n "${reg_pid:-}" ] && kill -INT "$reg_pid" 2>/dev/null
  true
  # Wait for the report to LAND, not for a guessed interval. The proxy now
  # also winds down a driver, a mesh endpoint and a registry, and two seconds
  # stopped being enough - the report was cut off mid-line and every
  # placement assertion failed against output that simply had not been
  # written yet.
  # Stop as soon as the proxy is GONE, not only when the report lands. A
  # scenario that kills the proxy can never print `placed`, and waiting the
  # full timeout for it in every such run was half the suite's slowdown.
  for _ in $(seq 1 15); do
    grep -q "^\[wire\] placed" "$RUN/proxy.log" 2>/dev/null && break
    kill -0 "$proxy_pid" 2>/dev/null || break
    sleep 1
  done
  # The registry lives in the proxy now, so its tally is in the proxy log.
  # Baseline runs still have a standalone one.
  grep -hE "^\[registry\] served" "$RUN/proxy.log" "$RUN/registry.log" 2>/dev/null || true
  # `[driver]` lines are now part of the evidence, not just noise: WHICH
  # machine took a subtree, who declined one, and who joined are only visible
  # there since the gateway stopped choosing.
  grep -E '^\[wire\]|^\[driver\]|^\[proxy\] +(adopted|peer|taking|frontend|what|fleet)' \
    "$RUN/proxy.log" || true
fi

# The identity of the result, per build. Written to a file so a proxied run
# and a NOPROXY run can be diffed - a distributed build that is one byte
# different from a local one has broken cache identity (principle 4) even
# when every exit code is zero.
say "output digests"
: >"$RUN/digests.txt"
for i in $(seq 0 $((BUILDS - 1))); do
  # `|| true` on the find, and it is load-bearing.
  #
  # A build that FAILED leaves no out-$i, find exits non-zero, and under
  # `set -euo pipefail` the whole script dies here - before printing a single
  # digest. On CI that turned every "outputs identical" assertion into a
  # failure whose real cause was three sections earlier, and the digest
  # section into something that had apparently never run.
  #
  # A missing output directory is not an error to this loop. It is a build
  # that failed, which the failure count already reports.
  d=$({ find "$RUN/out-$i" -type f -exec sh -c 'for f; do sha256sum "$f" 2>/dev/null \
    || shasum -a 256 "$f"; done' _ {} + 2>/dev/null || true; } |
    sed "s|$RUN/out-$i||" | sort | sha256 | cut -d' ' -f1)
  echo "build $i: $d" | tee -a "$RUN/digests.txt"
done
if [ -n "${EXPECT:-}" ]; then
  # A baseline recorded with a different BUILDS is not a mismatch, it is a
  # mis-comparison - and it reads identically in a diff. Caught it claiming
  # "outputs DIFFER" when builds 0-3 were byte-identical and 4-5 simply did
  # not exist in the baseline.
  want=$(wc -l <"$EXPECT" | tr -d ' ')
  if [ "$want" != "$BUILDS" ]; then
    echo "✗ baseline $EXPECT has $want builds, this run has $BUILDS - re-record it"
    fail=1
  elif diff -u "$EXPECT" "$RUN/digests.txt"; then
    echo "◈ outputs identical to $EXPECT"
  else
    echo "✗ outputs DIFFER from $EXPECT"
    fail=1
  fi
fi

say "per-daemon cache (work leaves a mark where it ran)"
for i in $(seq 0 $((DAEMONS - 1))); do
  port=$((BASE_PORT + i))
  printf 'daemon %s: ' "$i"
  bctl --addr "tcp://127.0.0.1:$port" du 2>/dev/null | tail -1 || echo "?"
done

# What the shared mirror is holding. Reported as a COUNT rather than a size
# because `du` rounds every 400-byte manifest up to a block and makes the
# store look an order of magnitude heavier than it is.
#
# The number is what makes "repeating a build costs nothing" checkable: run
# the same builds for more rounds and it must not move. Before the exporter
# was made reproducible, 8 rounds of 4 builds left 75 blobs where 1 round
# left 19.
printf 'store blobs: %s in %s tags\n' \
  "$(find "$RUN/store/cas" -type f 2>/dev/null | wc -l | tr -d ' ')" \
  "$(find "$RUN/store/tags" -type f 2>/dev/null | wc -l | tr -d ' ')"

echo
echo "logs in $RUN"
exit "$fail"
