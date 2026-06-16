import * as core from "@actions/core";
import * as cache from "@actions/cache";
import * as tc from "@actions/tool-cache";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import * as crypto from "crypto";
import { spawn } from "child_process";
import { platformArch, downloadUrl, checksum } from "./constants";
import { writeBuckconfigLocal } from "./buckconfig";

function sha256(file: string): string {
  return crypto.createHash("sha256").update(fs.readFileSync(file)).digest("hex");
}

async function waitForHealth(httpPort: number, timeoutMs: number): Promise<void> {
  const deadline = Date.now() + timeoutMs;
  const url = `http://127.0.0.1:${httpPort}/status`;
  while (Date.now() < deadline) {
    try {
      const r = await fetch(url);
      if (r.ok) return;
    } catch {
      // not up yet
    }
    await new Promise((res) => setTimeout(res, 200));
  }
  throw new Error(`rebuck: bazel-remote did not become healthy on :${httpPort} within ${timeoutMs}ms`);
}

async function run(): Promise<void> {
  const version = core.getInput("bazel-remote-version");
  const maxSizeGb = core.getInput("max-size-gb");
  const grpcPort = parseInt(core.getInput("grpc-port"), 10);
  const httpPort = parseInt(core.getInput("http-port"), 10);
  const cacheDir = core.getInput("cache-dir") || path.join(os.homedir(), ".cache", "rebuck");
  const keyPrefix = core.getInput("cache-key-prefix");
  const execPlatform = core.getInput("execution-platform");
  const writeLocal = core.getBooleanInput("write-buckconfig-local");
  const forceUpload = core.getBooleanInput("force-cache-upload");

  const pa = platformArch();
  const runId = process.env.GITHUB_RUN_ID ?? "0";
  const runAttempt = process.env.GITHUB_RUN_ATTEMPT ?? "0";
  const workspace = process.env.GITHUB_WORKSPACE ?? process.cwd();

  fs.mkdirSync(cacheDir, { recursive: true });

  // Monotonic key so every run saves a fresh entry (GHA cache keys are
  // write-once); the prefix restore-key pulls the most recent prior entry,
  // giving partial hits across dependency changes. GHA LRU prunes old entries
  // under the 10GB budget.
  const primaryKey = `${keyPrefix}-${pa}-${runId}-${runAttempt}`;
  const restoreKeys = [`${keyPrefix}-${pa}-`];
  core.saveState("primaryKey", primaryKey);
  core.saveState("cacheDir", cacheDir);
  core.saveState("httpPort", String(httpPort));

  const hit = await cache.restoreCache([cacheDir], primaryKey, restoreKeys);
  core.info(`rebuck: cache ${hit ? `restored from ${hit}` : "miss (cold start)"}`);

  // Download + verify bazel-remote.
  const url = downloadUrl(version, pa);
  core.info(`rebuck: downloading bazel-remote ${version} (${pa})`);
  const dl = await tc.downloadTool(url);
  const want = checksum(version, pa);
  const got = sha256(dl);
  if (got !== want) {
    throw new Error(`rebuck: bazel-remote checksum mismatch\n  expected ${want}\n  got      ${got}`);
  }
  const bin = path.join(process.env.RUNNER_TEMP ?? os.tmpdir(), "bazel-remote");
  fs.copyFileSync(dl, bin);
  fs.chmodSync(bin, 0o755);

  // Start the sidecar detached, logging to a file we can tail in post.
  const logFile = path.join(process.env.RUNNER_TEMP ?? os.tmpdir(), "bazel-remote.log");
  core.saveState("logFile", logFile);
  const logFd = fs.openSync(logFile, "a");
  const child = spawn(
    bin,
    [
      `--dir=${cacheDir}`,
      `--max_size=${maxSizeGb}`,
      `--grpc_address=127.0.0.1:${grpcPort}`,
      `--http_address=127.0.0.1:${httpPort}`,
      "--idle_timeout=30m",
    ],
    { detached: true, stdio: ["ignore", logFd, logFd] },
  );
  child.unref();
  if (child.pid) core.saveState("pid", String(child.pid));
  core.info(`rebuck: bazel-remote started (pid ${child.pid}), waiting for health…`);
  await waitForHealth(httpPort, 30_000);
  core.info("rebuck: bazel-remote healthy");

  const grpcAddr = `grpc://127.0.0.1:${grpcPort}`;
  core.setOutput("grpc-address", grpcAddr);

  if (writeLocal) {
    const dest = writeBuckconfigLocal(workspace, grpcAddr, execPlatform);
    core.info(`rebuck: wrote ${dest} (execution_platforms=${execPlatform})`);
  } else {
    core.info("rebuck: write-buckconfig-local=false — configure buck2 yourself with:");
    core.info(`  [build] execution_platforms = ${execPlatform}`);
    core.info(`  [buck2_re_client] action_cache_address/cas_address/engine_address = ${grpcAddr}, tls=false, capabilities=false`);
  }

  if (forceUpload) {
    core.exportVariable("BUCK2_TEST_FORCE_CACHE_UPLOAD", "1");
    core.info("rebuck: exported BUCK2_TEST_FORCE_CACHE_UPLOAD=1");
  }
}

run().catch((e) => core.setFailed(e instanceof Error ? e.message : String(e)));
