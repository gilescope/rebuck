#!/usr/bin/env bash
# What CI will say, before CI says it.
#
# Written after breaking the `engine` job on `function seeder_for is never
# used` and not noticing: CI runs clippy with `-D warnings`, a local
# `cargo clippy` does not, so dead code passes here and fails there. Two
# commits later it was green again and nobody had looked.
#
# Same flags as .github/workflows/selftest.yml, in the same order, so a green
# run here means a green job there.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== fmt"
cargo fmt --check
echo "== clippy (-D warnings, as CI)"
cargo clippy --all-targets --locked -- -D warnings
echo "== workflow defaults"
bash scripts/check-workflow-defaults.sh

echo "== tests"
cargo test --locked
echo
echo "all green"
