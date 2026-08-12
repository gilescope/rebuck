VERSION 0.8

# Test coverage for the rebuck2 engine, reproducibly.
#
# The engine's tests run in CI already (`selftest.yml`, job `engine`). What
# nothing measured is which parts of it they reach - and this project keeps
# finding bugs in exactly the places a test never went: a call site choosing
# `fs::write` over merge, a gate comparing two spellings of one digest, a
# timeout wrapped around the wrong span. Eight fixes last night were each
# accompanied by a unit test of a pure function, and not one of those tests
# would have caught its own bug.
#
# So the number worth having is not "is coverage high" but "did the line I
# just fixed have any coverage before I touched it".
#
#   earth +coverage                     # summary to stdout, lcov artifact out
#   earth +coverage-gate --FAIL_UNDER=45  # and fail below a floor
#   earth +check                        # fmt, clippy, tests - what CI runs
#
# Everything is pinned: the toolchain matches selftest.yml exactly, and
# cargo-llvm-cov is version-locked, because a coverage number that moves
# when a tool upgrades is a number nobody can act on.

# The toolchain selftest.yml uses. Two places is one too many, and the
# workflow cannot read this file - so when one moves, move both.
ARG --global RUST="1.92.0"
# Pinned, and `--locked` below pins its own dependency tree. An unpinned
# coverage tool makes the metric drift under you.
ARG --global LLVM_COV="0.6.16"
# Report-only by default. A threshold is a promise about a number nobody has
# measured yet; `+coverage-gate` is where a real floor goes once there is one.
ARG --global FAIL_UNDER="0"

deps:
    # `-slim-bookworm` and not alpine: the crate graph pulls in ring and
    # aws-lc-sys, which want a normal libc and a working cc.
    FROM rust:$RUST-slim-bookworm
    # llvm-tools-preview carries llvm-profdata, which is what does the work.
    # Installing it here rather than in +coverage keeps it out of the layer
    # that changes every commit.
    RUN rustup component add llvm-tools-preview
    # zstd and sqlite3 are TEST dependencies, and nothing declares them.
    # `bank::zstd` is `Command::new("zstd")` and `bank::dice` is
    # `Command::new("sqlite3")` - so five `bank::pack` tests fail on any
    # machine that happens not to have them, and pass on every machine that
    # does. A laptop has them from nix or homebrew; a GitHub runner ships
    # them; a clean bookworm-slim does not, which is how this surfaced.
    #
    # That is the argument for building tests in a container at all: the
    # suite was green everywhere it had ever run and still had an undeclared
    # dependency on the host.
    RUN apt-get update \
     && apt-get install -y --no-install-recommends \
          pkg-config libssl-dev protobuf-compiler zstd sqlite3 \
     && rm -rf /var/lib/apt/lists/*
    RUN cargo install cargo-llvm-cov --version $LLVM_COV --locked
    WORKDIR /w

# The dependency graph on its own layer, so editing src/ does not rebuild it.
vendor:
    FROM +deps
    COPY rebuck2/Cargo.toml rebuck2/Cargo.lock ./
    COPY rebuck2/buildkit-session/Cargo.toml buildkit-session/
    # Skeletons, purely to let cargo resolve and build the dependency tree.
    # `main.rs` because rebuck2 is a binary crate; a lib.rs skeleton would
    # build a different graph.
    RUN mkdir -p src buildkit-session/src \
     && echo 'fn main() {}' > src/main.rs \
     && echo '' > buildkit-session/src/lib.rs
    RUN cargo fetch --locked

src:
    FROM +vendor
    # `tests/fixtures` and `actions/` are NOT optional extras. The suite
    # `include_str!`s them, so they are needed at COMPILE time, not run
    # time - leaving them out fails the build with `couldn't read
    # src/bank/../../tests/fixtures/logstream-nested.jsonl`, which reads
    # like a missing file at runtime and is not.
    #
    # Found by running the target. A copy set assembled by looking at the
    # source tree would have missed all four, because nothing in `src/`
    # looks like it depends on them.
    COPY --dir rebuck2/src rebuck2/buildkit-session rebuck2/patches \
                rebuck2/tests rebuck2/actions ./
    COPY rebuck2/Cargo.toml rebuck2/Cargo.lock ./

# What CI runs, so a failure here is a failure there.
check:
    FROM +src
    RUN cargo fmt --all --check
    RUN cargo clippy --all-targets --locked -- -D warnings
    RUN cargo test --locked

coverage:
    FROM +src
    # `--summary-only` to stdout for a human, lcov for anything that reads
    # it. Both from ONE instrumented run - two runs would report two numbers
    # and invite the question of which is right.
    RUN cargo llvm-cov --locked --lcov --output-path lcov.info \
     && cargo llvm-cov --locked --summary-only report | tee coverage.txt
    SAVE ARTIFACT lcov.info AS LOCAL lcov.info
    SAVE ARTIFACT coverage.txt AS LOCAL coverage.txt

# The gate, separate from the report on purpose.
#
# `+coverage` should always be runnable and always succeed, because a report
# that can fail is a report people stop running. A floor belongs in its own
# target, so raising it is a deliberate edit rather than a surprise.
coverage-gate:
    FROM +src
    RUN cargo llvm-cov --locked --summary-only --fail-under-lines $FAIL_UNDER
