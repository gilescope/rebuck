import * as core from "@actions/core";
import * as cache from "@actions/cache";
import * as fs from "fs";

// Runs at job end: stop the sidecar (flush), report stats, then persist its
// blob dir to the GHA cache under the run-unique key written in main.
async function post(): Promise<void> {
  const httpPort = core.getState("httpPort");
  const pid = core.getState("pid");
  const cacheDir = core.getState("cacheDir");
  const primaryKey = core.getState("primaryKey");
  const logFile = core.getState("logFile");

  // Stats before we kill it — cheap signal of hit rate / cache growth.
  if (httpPort) {
    try {
      const r = await fetch(`http://127.0.0.1:${httpPort}/status`);
      if (r.ok) core.info(`rebuck: bazel-remote status ${await r.text()}`);
    } catch {
      core.info("rebuck: could not read bazel-remote status");
    }
  }

  // Stop the sidecar BEFORE saving so blobs are flushed and the dir is quiescent.
  if (pid) {
    try {
      process.kill(parseInt(pid, 10), "SIGTERM");
      await new Promise((res) => setTimeout(res, 1000));
      core.info(`rebuck: stopped bazel-remote (pid ${pid})`);
    } catch (e) {
      core.info(`rebuck: bazel-remote already stopped (${e instanceof Error ? e.message : e})`);
    }
  }

  if (logFile && fs.existsSync(logFile)) {
    const log = fs.readFileSync(logFile, "utf8").trim().split("\n");
    core.info(`rebuck: bazel-remote log tail:\n${log.slice(-5).join("\n")}`);
  }

  if (!cacheDir || !primaryKey) {
    core.info("rebuck: nothing to save (no state)");
    return;
  }
  try {
    await cache.saveCache([cacheDir], primaryKey);
    core.info(`rebuck: saved cache ${primaryKey}`);
  } catch (e) {
    // ReserveCacheError (key already exists) is benign — another run won the race.
    core.warning(`rebuck: cache save skipped: ${e instanceof Error ? e.message : e}`);
  }
}

post().catch((e) => core.warning(e instanceof Error ? e.message : String(e)));
