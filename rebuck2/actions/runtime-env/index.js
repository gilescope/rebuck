// Re-export the Actions *runtime* credentials so ordinary `run:` steps
// (and rebuck2 itself) can speak the artifact API directly.
//
// Why this exists at all: artifact upload authenticates with
// ACTIONS_RUNTIME_TOKEN against ACTIONS_RESULTS_URL, not GITHUB_TOKEN.
// The runner injects those ONLY into JavaScript actions - probed on
// 2026-07-30 (run 30514479915): a plain `run:` step sees none of them,
// and neither does a composite action. So the smallest possible JS
// action reads them and hands them on.
//
// Vanilla node, no dependencies, no bundler: the whole thing is
// process.env in and $GITHUB_ENV out, and a build step for 30 lines
// would cost more than it carries.

const fs = require("fs");

// Names worth forwarding. RESULTS_URL is the v4 artifact service;
// RUNTIME_URL and CACHE_URL are the older v2/cache endpoints, still
// present on some runner versions and harmless to pass through.
const NAMES = [
  "ACTIONS_RUNTIME_TOKEN",
  "ACTIONS_RESULTS_URL",
  "ACTIONS_RUNTIME_URL",
  "ACTIONS_CACHE_URL",
];

// Secret-ish values are masked BEFORE they are exported, so a later
// step that echoes one by accident prints ***, not a credential that
// can write artifacts to this repo.
const SECRET = /TOKEN$/;

function exportVar(name, value) {
  const file = process.env.GITHUB_ENV;
  if (!file) throw new Error("GITHUB_ENV is unset - not running in Actions?");
  // Multiline values need the delimiter form; a stray newline in a
  // NAME=value line would let a value forge further assignments.
  if (value.includes("\n")) {
    const delim = `__rebuck2_${Math.random().toString(36).slice(2)}__`;
    if (value.includes(delim)) throw new Error(`${name}: delimiter collision`);
    fs.appendFileSync(file, `${name}<<${delim}\n${value}\n${delim}\n`);
  } else {
    fs.appendFileSync(file, `${name}=${value}\n`);
  }
}

let exported = 0;
for (const name of NAMES) {
  const value = process.env[name];
  if (!value) {
    console.log(`${name}: unset on this runner - skipped`);
    continue;
  }
  if (SECRET.test(name)) {
    console.log(`::add-mask::${value}`);
    console.log(`${name}: exported (masked, len ${value.length})`);
  } else {
    console.log(`${name}: exported (${value})`);
  }
  exportVar(name, value);
  exported += 1;
}

if (exported === 0) {
  // Not fatal by default: a workflow may use this action defensively on
  // a runner that never had the credentials. Fail loudly only when the
  // caller says the upload path depends on it.
  const msg = "no runtime credentials found to export";
  if (process.env.INPUT_REQUIRED === "true") {
    console.log(`::error::${msg}`);
    process.exit(1);
  }
  console.log(`::warning::${msg}`);
}
