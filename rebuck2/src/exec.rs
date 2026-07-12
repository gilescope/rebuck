//! Action execution: materialize the input tree, run the command, collect
//! outputs. Used by workers and by the driver's local fallback — the only
//! difference is where blobs come from (`Blobs` impl).
//!
//! Keyed commands (a target's emit flavours) run in canonical run-stable
//! dirs (`REBUCK2_EXEC_BASE` overrides the base) so absolutized path-envs
//! agree across pipelined twins — see [`crate_affinity_key`]. Unkeyed
//! commands get per-action temp dirs. `REBUCK2_KEEP_SCRATCH=1` keeps exec
//! dirs and logs argv/cwd per action.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use prost::Message;

use crate::mesh::Dig;

#[async_trait::async_trait]
pub trait Blobs: Send + Sync {
    async fn get(&self, d: &Dig) -> Result<Vec<u8>>;
    /// Store bytes, returning their digest.
    async fn put(&self, bytes: Vec<u8>) -> Result<Dig>;
    /// Ingest an output file, returning its digest. Default: read + put.
    /// Store-backed impls adopt the file into the CAS by link/clone instead
    /// of rewriting it (same --no-hardlinks opt-out as materialization).
    async fn put_file(&self, path: &std::path::Path) -> Result<Dig> {
        let bytes = tokio::fs::read(path).await?;
        self.put(bytes).await
    }

    /// Write blob `d` to `dest`. Default: fetch + write (+exec bit). Store-
    /// backed impls override with link/clone-from-store — the write-
    /// amplification fix — where store perms (0o555) already cover exec, and
    /// chmod on a shared inode would be a cross-action mutation.
    async fn materialize_file(
        &self,
        d: &Dig,
        dest: &std::path::Path,
        is_executable: bool,
    ) -> Result<()> {
        let bytes = self.get(d).await?;
        tokio::fs::write(dest, &bytes).await?;
        if is_executable {
            set_exec(dest).await?;
        }
        Ok(())
    }

    /// Best-effort bulk warm-up: make `digs` locally available so subsequent
    /// per-blob calls don't each pay a network round-trip. Never fails the
    /// action — a blob it couldn't obtain surfaces later through `get`'s
    /// per-blob error path, which names the digest. Default: no-op (store-
    /// backed impls are already local). Wrapper impls MUST delegate or the
    /// batching silently vanishes.
    async fn prefetch(&self, _digs: &[Dig]) -> Result<()> {
        Ok(())
    }
}

pub struct Outcome {
    pub action_result: re::ActionResult,
    pub do_not_cache: bool,
}

/// Affinity key for a command: the crate output prefix (buck2 rules place
/// every emit flavour of one target under `.../__<name>__/`). A crate's
/// pipelined metadata compile and its rlib compile MUST run on one machine
/// AND in one directory path — the prelude wrapper absolutizes `--path-env`
/// values (CARGO_MANIFEST_DIR & co) against the action cwd and rustc tracks
/// `env!`-read values into the crate hash (SVH), so twins in per-action temp
/// dirs hash differently and every downstream link dies with E0460. The
/// input root can't pair them (argsfiles differ); the output prefix can.
pub fn crate_affinity_key(cmd: &re::Command) -> Option<String> {
    #[allow(deprecated)] // pre-v2.1 clients send output_files/output_directories
    let first = cmd
        .output_paths
        .first()
        .or_else(|| cmd.output_files.first())
        .or_else(|| cmd.output_directories.first())?;
    let end = first.find("__/")? + 3;
    Some(first[..end].to_string())
}

/// Canonical exec dir for a crate key: identical across actions, workers
/// and RUNS (a cached metadata result must agree with a later fresh rlib
/// compile). Hence a fixed OS-family base, not the worker store or a temp
/// dir (mac's $TMPDIR is per-boot random). REBUCK2_EXEC_BASE overrides —
/// every worker in a fleet must then agree on it. Leaf is a short hash:
/// windows MAX_PATH is part of the budget.
fn canonical_exec_dir(key: &str) -> PathBuf {
    let base = std::env::var_os("REBUCK2_EXEC_BASE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if cfg!(windows) {
                PathBuf::from(r"C:\rb2x")
            } else {
                PathBuf::from("/tmp/rebuck2-exec")
            }
        });
    base.join(&crate::store::sha256_hex(key.as_bytes())[..16])
}

/// Canonical dirs are shared mutable state: flavours of one key serialize
/// within the process. Affinity routing pins a key to one process, so an
/// in-process lock suffices (a usurped owner's old action is already dead).
fn crate_lock(key: &str) -> std::sync::Arc<tokio::sync::Mutex<()>> {
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex, OnceLock};
    static LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut map = LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .expect("crate lock table poisoned");
    map.entry(key.to_string()).or_default().clone()
}

pub async fn run_action(blobs: &dyn Blobs, action_digest: &Dig, scratch: &Path) -> Result<Outcome> {
    let action =
        re::Action::decode(blobs.get(action_digest).await?.as_slice()).context("decode Action")?;
    let cmd_dig: Dig = (&action
        .command_digest
        .clone()
        .context("Action.command_digest")?)
        .into();
    let command =
        re::Command::decode(blobs.get(&cmd_dig).await?.as_slice()).context("decode Command")?;
    let root_dig: Dig = (&action
        .input_root_digest
        .clone()
        .context("Action.input_root_digest")?)
        .into();

    // REBUCK2_KEEP_SCRATCH=1 keeps exec dirs and logs each action's argv/cwd
    // — the debug lever for "worked locally, failed on the worker".
    let keep_scratch = std::env::var_os("REBUCK2_KEEP_SCRATCH").is_some();
    // Keyed commands (all of a target's emit flavours) run in a canonical
    // run-stable dir so absolutized path-envs agree across the flavours —
    // see crate_affinity_key. Unkeyed commands keep per-action temp dirs.
    let (root_buf, exec_dir, _key_guard) = match crate_affinity_key(&command) {
        Some(key) => {
            let guard = crate_lock(&key).lock_owned().await;
            let dir = canonical_exec_dir(&key);
            // Clear stale content (crash leftovers or the previous emit
            // flavour) by renaming aside and deleting in the background:
            // an inline remove_dir_all of a dereferenced __srcs forest is
            // a minute of windows fs latency on the action's critical
            // path, and invisible to the staging timer.
            static TRASH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let trash = dir.with_extension(format!(
                "trash{}",
                TRASH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            match tokio::fs::rename(&dir, &trash).await {
                Ok(()) => {
                    tokio::spawn(async move {
                        let _ = tokio::fs::remove_dir_all(&trash).await;
                    });
                }
                // NotFound = nothing to clear; anything else (open handle,
                // cross-device oddity) falls back to the inline delete.
                Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                    let _ = tokio::fs::remove_dir_all(&dir).await;
                }
                Err(_) => {}
            }
            tokio::fs::create_dir_all(&dir)
                .await
                .context("mk canonical exec dir")?;
            (dir, None, Some(guard))
        }
        None => {
            let td = tempfile::tempdir_in(scratch).context("mk exec dir")?;
            (td.path().to_path_buf(), Some(td), None)
        }
    };
    let root = root_buf.as_path();

    // REAPI: the worker creates parent dirs of every declared output path.
    #[allow(deprecated)] // pre-v2.1 clients send output_files/output_directories
    let out_paths: Vec<String> = if !command.output_paths.is_empty() {
        command.output_paths.clone()
    } else {
        command
            .output_files
            .iter()
            .chain(command.output_directories.iter())
            .cloned()
            .collect()
    };
    let (argv0, args) = command
        .arguments
        .split_first()
        .context("Command.arguments empty")?;
    // The first output path is the most human-readable handle we have for an
    // action; needed before materialize so staging announces itself.
    let label = out_paths
        .first()
        .map(String::as_str)
        .unwrap_or(argv0)
        .to_owned();

    let staging = std::time::Instant::now();
    materialize(blobs, &root_dig, root, &label).await?;
    let staged_secs = staging.elapsed().as_secs_f64();

    let cwd = if command.working_directory.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&command.working_directory)
    };
    tokio::fs::create_dir_all(&cwd).await?;
    for p in &out_paths {
        if let Some(parent) = cwd.join(p).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }
    // Windows resolves a relative argv0 against the PARENT process's cwd,
    // not `current_dir` (documented std::process behaviour) — project-relative
    // scripts (buck-out\...\foo.bat) then miss. Absolutize against the action
    // cwd when the file exists there; bare tool names keep PATH resolution.
    let argv0_abs = {
        let p = std::path::Path::new(argv0);
        let joined = cwd.join(p);
        if p.is_relative() && joined.is_file() {
            joined
        } else {
            p.to_path_buf()
        }
    };
    let mut proc = tokio::process::Command::new(&argv0_abs);
    proc.args(args).current_dir(&cwd).env_clear();
    let mut saw: std::collections::HashSet<String> = std::collections::HashSet::new();
    for ev in &command.environment_variables {
        saw.insert(ev.name.to_ascii_uppercase());
        proc.env(&ev.name, &ev.value);
    }
    // System toolchains (rustc, cl.exe) are PATH-resolved and buck2 doesn't
    // ship a PATH in the action env. Runner images are uniform per-OS, so
    // inheriting the worker's PATH is the pragmatic v0 hermeticity trade.
    if !saw.contains("PATH") {
        if let Ok(path) = std::env::var("PATH") {
            proc.env("PATH", path);
        }
    }
    // Windows actions shell out via cmd.exe, whose scripts fail with "The
    // system cannot find the path specified." without the core system env
    // (SystemRoot, ComSpec, TEMP, ...). buck2 doesn't put these in the
    // action env; inherit any the action didn't set itself.
    #[cfg(windows)]
    for name in [
        "SYSTEMROOT",
        "SYSTEMDRIVE",
        "COMSPEC",
        "PATHEXT",
        "WINDIR",
        "TEMP",
        "TMP",
        "USERPROFILE",
        "HOMEDRIVE",
        "HOMEPATH",
        "LOCALAPPDATA",
        "APPDATA",
        "PROGRAMDATA",
        "ALLUSERSPROFILE",
        "PROGRAMFILES",
        "PROGRAMFILES(X86)",
        "COMMONPROGRAMFILES",
        "COMMONPROGRAMFILES(X86)",
        "NUMBER_OF_PROCESSORS",
        "PROCESSOR_ARCHITECTURE",
        "OS",
    ] {
        if !saw.contains(name) {
            // Lookup is case-insensitive on windows, so the canonical-case
            // names above find e.g. `ProgramFiles(x86)` too.
            if let Ok(v) = std::env::var(name) {
                proc.env(name, v);
            }
        }
    }
    // One staging/start/finish line per action, always — makes a worker's CI
    // log a live view of what it's building AND what it's waiting for
    // (staging used to be silent, and 20-minute input-forest fetches read as
    // a dead scheduler — run 29160244348).
    println!("[action] start {label} (staged in {staged_secs:.1}s)");
    if keep_scratch {
        eprintln!(
            "[exec] action {} argv={:?} cwd={} outs={:?}",
            action_digest.hash,
            command.arguments,
            cwd.display(),
            out_paths
        );
    }
    let started = std::time::SystemTime::now();
    let output = proc
        .output()
        .await
        .with_context(|| format!("spawn {argv0}"))?;
    let finished = std::time::SystemTime::now();
    let secs = finished
        .duration_since(started)
        .unwrap_or_default()
        .as_secs_f64();
    if output.status.success() {
        println!("[action] ok    {label} ({secs:.1}s)");
    } else {
        println!(
            "[action] FAIL  {label} exit={:?} ({secs:.1}s)",
            output.status.code()
        );
        // Raw compiler output, where the failure actually explains itself.
        // (It also travels back to buck2 as a blob; this is the live copy.)
        let excerpt =
            |bytes: &[u8]| String::from_utf8_lossy(&bytes[..bytes.len().min(4096)]).into_owned();
        if !output.stderr.is_empty() {
            eprintln!("--- stderr {label}\n{}", excerpt(&output.stderr));
        }
        if !output.stdout.is_empty() {
            eprintln!("--- stdout {label}\n{}", excerpt(&output.stdout));
        }
    }
    if keep_scratch {
        eprintln!(
            "[exec] action {} exit={:?} scratch kept at {}",
            action_digest.hash,
            output.status.code(),
            root.display()
        );
    }

    let stdout_digest = blobs.put(output.stdout).await?;
    let stderr_digest = blobs.put(output.stderr).await?;

    let mut result = re::ActionResult {
        exit_code: output.status.code().unwrap_or(-1),
        stdout_digest: Some(stdout_digest.to_proto()),
        stderr_digest: Some(stderr_digest.to_proto()),
        execution_metadata: Some(re::ExecutedActionMetadata {
            worker: hostname(),
            execution_start_timestamp: Some(ts(started)),
            execution_completed_timestamp: Some(ts(finished)),
            ..Default::default()
        }),
        ..Default::default()
    };

    for p in &out_paths {
        let abs = cwd.join(p);
        let Ok(meta) = tokio::fs::symlink_metadata(&abs).await else {
            continue; // action declared but didn't produce it; buck2 will complain if it matters
        };
        if meta.file_type().is_symlink() {
            let target = tokio::fs::read_link(&abs).await?;
            result.output_symlinks.push(re::OutputSymlink {
                path: p.clone(),
                target: target.to_string_lossy().into_owned(),
                node_properties: None,
            });
        } else if meta.is_dir() {
            let tree_digest = upload_tree(blobs, &abs).await?;
            result.output_directories.push(re::OutputDirectory {
                path: p.clone(),
                tree_digest: Some(tree_digest.to_proto()),
                is_topologically_sorted: false,
                root_directory_digest: None,
            });
        } else {
            // Exec bit read BEFORE adoption chmods the inode to 0o555.
            let is_executable = is_exec(&meta);
            let digest = blobs.put_file(&abs).await?;
            result.output_files.push(re::OutputFile {
                path: p.clone(),
                digest: Some(digest.to_proto()),
                is_executable,
                contents: Vec::new(),
                node_properties: None,
            });
        }
    }

    if keep_scratch {
        // Leak the tempdir so the workflow can inspect scripts post-mortem.
        // (Canonical dirs persist anyway until the next same-key action.)
        if let Some(td) = exec_dir {
            std::mem::forget(td);
        }
    } else if exec_dir.is_none() {
        // Canonical dir: no TempDir RAII, reclaim the disk ourselves.
        let _ = tokio::fs::remove_dir_all(root).await;
    }
    Ok(Outcome {
        action_result: result,
        do_not_cache: action.do_not_cache,
    })
}

/// Stage the input tree under `dest`, returning the file count. Staging was
/// invisible and strictly sequential (one awaited fetch per file, one per
/// directory) — at RTT-bound peer-fetch rates the big substrate crate
/// forests spent 10-22 min here before rustc even started, and the whole
/// mac leg of run 29160244348 was exactly these gaps. Now: walk the tree
/// level by level (one `prefetch` batch per depth), then batch-fetch every
/// file blob and materialize with bounded fan-out.
async fn materialize(
    blobs: &dyn Blobs,
    dir_digest: &Dig,
    dest: &Path,
    label: &str,
) -> Result<usize> {
    // Phase 1 — BFS the Directory forest collecting work; NO fs ops here.
    // Iterative: recursion in async fns needs boxing. A sequential awaited
    // create_dir_all per directory put ~15ms of windows fs latency on every
    // dir of a dereferenced __srcs forest (windows buck2 ships those as full
    // file lists, ~4k dirs) — 60s+ of the win leg's per-action staging.
    let mut files: Vec<(Dig, PathBuf, bool)> = Vec::new();
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashMap<Dig, re::Directory> = HashMap::new();
    // Symlinks last: windows needs to know file-vs-dir at creation time, so
    // the targets must exist first (buck2's __srcs trees link to sibling dirs).
    let mut symlinks: Vec<(PathBuf, String)> = Vec::new();
    let mut frontier: Vec<(Dig, PathBuf)> = vec![(dir_digest.clone(), dest.to_path_buf())];
    while !frontier.is_empty() {
        let unseen: Vec<Dig> = frontier
            .iter()
            .filter(|(d, _)| !seen.contains_key(d))
            .map(|(d, _)| d.clone())
            .collect();
        blobs.prefetch(&unseen).await?;
        let mut next: Vec<(Dig, PathBuf)> = Vec::new();
        for (dig, path) in frontier {
            let dir = match seen.get(&dig) {
                Some(d) => d.clone(),
                None => {
                    let d = re::Directory::decode(blobs.get(&dig).await?.as_slice())
                        .context("decode Directory")?;
                    seen.insert(dig.clone(), d.clone());
                    d
                }
            };
            for f in &dir.files {
                let fdig: Dig = (&f.digest.clone().context("FileNode.digest")?).into();
                files.push((fdig, path.join(&f.name), f.is_executable));
            }
            for s in &dir.symlinks {
                symlinks.push((path.join(&s.name), s.target.clone()));
            }
            for d in &dir.directories {
                let ddig: Dig = (&d.digest.clone().context("DirectoryNode.digest")?).into();
                next.push((ddig, path.join(&d.name)));
            }
            dirs.push(path);
        }
        frontier = next;
    }

    // Phase 2 — one concurrent mkdir pass. create_dir_all is idempotent and
    // makes parents, so ordering and same-path races don't matter.
    println!("[action] staging {label} ({} files)", files.len());
    use futures::TryStreamExt;
    futures::stream::iter(dirs.iter().map(anyhow::Ok))
        .try_for_each_concurrent(64, |p| async move {
            tokio::fs::create_dir_all(p)
                .await
                .map_err(anyhow::Error::from)
        })
        .await?;

    // Phase 3 — warm the local store in batches, then fan out the
    // filesystem writes (post-prefetch these are local link/copy, so the
    // bound is about syscall parallelism, not network).
    let digs: Vec<Dig> = files.iter().map(|(d, _, _)| d.clone()).collect();
    blobs.prefetch(&digs).await?;
    futures::stream::iter(files.iter().map(Ok))
        .try_for_each_concurrent(64, |(d, p, x)| async move {
            blobs.materialize_file(d, p, *x).await
        })
        .await?;
    for (link, target) in symlinks {
        // Canonical topology: buck2 ships the SAME logical tree sometimes as
        // SymlinkNodes and sometimes dereferenced FileNodes (observed on the
        // rust dep forests: one compile of a pipelined pair got each form,
        // rustc's crate hash is topology-sensitive, and every downstream
        // link died with E0460). Dereference in-tree relative symlinks into
        // hardlinks so every materialization is file-topology; external or
        // dangling targets keep the symlink.
        let resolved = link
            .parent()
            .map(|p| p.join(&target))
            .filter(|r| r.starts_with(dest) || target_within(dest, r));
        let deref_ok = match &resolved {
            Some(r) => dereference_into(r, &link).await.unwrap_or(false),
            None => false,
        };
        if !deref_ok {
            make_symlink(&link, &target)
                .with_context(|| format!("symlink {} -> {}", link.display(), target))?;
        }
    }
    Ok(files.len())
}

/// Lexically normalize `r` (resolving `..`) and check it stays inside `root`.
fn target_within(root: &Path, r: &Path) -> bool {
    let mut norm = PathBuf::new();
    for c in r.components() {
        match c {
            std::path::Component::ParentDir => {
                if !norm.pop() {
                    return false;
                }
            }
            std::path::Component::CurDir => {}
            other => norm.push(other),
        }
    }
    norm.starts_with(root)
}

/// Hardlink (or copy) `target` to `link`; directories link file-by-file.
/// Ok(false) = target missing/unsupported, caller falls back to a symlink.
async fn dereference_into(target: &Path, link: &Path) -> Result<bool> {
    let Ok(meta) = tokio::fs::metadata(target).await else {
        return Ok(false); // dangling — keep the symlink
    };
    if meta.is_file() {
        if tokio::fs::hard_link(target, link).await.is_err() {
            tokio::fs::copy(target, link).await?;
        }
        return Ok(true);
    }
    if meta.is_dir() {
        let mut stack = vec![(target.to_path_buf(), link.to_path_buf())];
        while let Some((src, dst)) = stack.pop() {
            tokio::fs::create_dir_all(&dst).await?;
            let mut rd = tokio::fs::read_dir(&src).await?;
            while let Some(e) = rd.next_entry().await? {
                let (s, d) = (e.path(), dst.join(e.file_name()));
                if e.file_type().await?.is_dir() {
                    stack.push((s, d));
                } else if tokio::fs::hard_link(&s, &d).await.is_err() {
                    tokio::fs::copy(&s, &d).await?;
                }
            }
        }
        return Ok(true);
    }
    Ok(false)
}

#[cfg(unix)]
fn make_symlink(link: &Path, target: &str) -> Result<()> {
    std::os::unix::fs::symlink(target, link)?;
    Ok(())
}

#[cfg(windows)]
fn make_symlink(link: &Path, target: &str) -> Result<()> {
    // Forward slashes don't resolve in NT symlink targets.
    let target = target.replace('/', "\\");
    let resolved = link.parent().context("link has parent")?.join(&target);
    // Windows distinguishes file and directory symlinks; stat the (already
    // materialized) target to pick. Dangling targets default to file links.
    if resolved.is_dir() {
        std::os::windows::fs::symlink_dir(&target, link)?;
    } else {
        std::os::windows::fs::symlink_file(&target, link)?;
    }
    Ok(())
}

/// Build + upload a Tree proto for an output directory; returns the Tree
/// digest. Ingestion was one awaited put per file — the 10-15k-file crate
/// unpacks (windows-sys, openssl-src) took 60-90s here and sat on BOTH
/// legs' critical paths. Now: sync walk (one spawn_blocking, no per-op
/// executor hops), concurrent file ingestion, then a deterministic
/// assembly pass that reproduces the original post-order children layout
/// byte-for-byte (tree digests must not shift across engine versions).
async fn upload_tree(blobs: &dyn Blobs, dir: &Path) -> Result<Dig> {
    let walked = {
        let dir = dir.to_path_buf();
        tokio::task::spawn_blocking(move || walk_out_dir(&dir))
            .await
            .context("walk join")??
    };
    // Ingest every file concurrently; digests keyed by path for assembly.
    // Futures built eagerly then streamed: mapping async closures over
    // borrowed items trips HRTB inference (same fix as rpc.rs).
    let mut file_paths: Vec<(PathBuf, bool)> = Vec::new();
    collect_walk_files(&walked, &mut file_paths);
    use futures::{StreamExt, TryStreamExt};
    let futs: Vec<_> = file_paths
        .iter()
        .map(|(p, _)| async move { Ok::<_, anyhow::Error>((p.clone(), blobs.put_file(p).await?)) })
        .collect();
    let digests: HashMap<PathBuf, Dig> = futures::stream::iter(futs)
        .buffer_unordered(64)
        .try_collect()
        .await?;
    let mut children: Vec<re::Directory> = Vec::new();
    let root = assemble_dir(blobs, &walked, &digests, &mut children).await?;
    let tree = re::Tree {
        root: Some(root),
        children,
    };
    blobs.put(tree.encode_to_vec()).await
}

/// One output directory's entries, name-sorted (REAPI canonical form).
struct WalkedDir {
    files: Vec<(String, PathBuf, bool)>,
    symlinks: Vec<(String, String)>,
    dirs: Vec<(String, WalkedDir)>,
}

/// Sync recursive walk — a single blocking task beats one executor
/// round-trip per metadata call on 15k-entry forests.
fn walk_out_dir(dir: &Path) -> Result<WalkedDir> {
    let mut out = WalkedDir {
        files: Vec::new(),
        symlinks: Vec::new(),
        dirs: Vec::new(),
    };
    let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let meta = e.metadata()?;
        let p = e.path();
        if meta.file_type().is_symlink() {
            out.symlinks
                .push((name, std::fs::read_link(&p)?.to_string_lossy().into_owned()));
        } else if meta.is_dir() {
            out.dirs.push((name, walk_out_dir(&p)?));
        } else {
            out.files.push((name, p, is_exec(&meta)));
        }
    }
    Ok(out)
}

fn collect_walk_files(w: &WalkedDir, into: &mut Vec<(PathBuf, bool)>) {
    for (_, p, x) in &w.files {
        into.push((p.clone(), *x));
    }
    for (_, d) in &w.dirs {
        collect_walk_files(d, into);
    }
}

/// Post-order assembly from the walk + digest map. Children push order and
/// entry sorting reproduce the sequential builder exactly — same bytes,
/// same tree digest.
async fn assemble_dir(
    blobs: &dyn Blobs,
    walked: &WalkedDir,
    digests: &HashMap<PathBuf, Dig>,
    children: &mut Vec<re::Directory>,
) -> Result<re::Directory> {
    let mut out = re::Directory::default();
    for (name, target) in &walked.symlinks {
        out.symlinks.push(re::SymlinkNode {
            name: name.clone(),
            target: target.clone(),
            node_properties: None,
        });
    }
    for (name, sub_walked) in &walked.dirs {
        let sub = Box::pin(assemble_dir(blobs, sub_walked, digests, children)).await?;
        let digest = blobs.put(sub.encode_to_vec()).await?;
        children.push(sub);
        out.directories.push(re::DirectoryNode {
            name: name.clone(),
            digest: Some(digest.to_proto()),
        });
    }
    for (name, p, is_executable) in &walked.files {
        let digest = digests
            .get(p)
            .with_context(|| format!("digest missing for {}", p.display()))?;
        out.files.push(re::FileNode {
            name: name.clone(),
            digest: Some(digest.to_proto()),
            is_executable: *is_executable,
            node_properties: None,
        });
    }
    Ok(out)
}

fn ts(t: std::time::SystemTime) -> bazel_remote_apis::google::protobuf::Timestamp {
    let d = t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    bazel_remote_apis::google::protobuf::Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

fn hostname() -> String {
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .unwrap_or_else(|_| "rebuck2-worker".into())
}

#[cfg(unix)]
fn is_exec(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_exec(_meta: &std::fs::Metadata) -> bool {
    false
}

#[cfg(unix)]
pub(crate) async fn set_exec(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).await?;
    Ok(())
}

#[cfg(not(unix))]
pub(crate) async fn set_exec(_p: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Twin pipelined compiles (metadata-full / rlib) of one crate must see
    // the SAME absolutized --path-env values or their SVHs diverge (E0460).
    // The exec dir is the only variable: same key -> same canonical dir,
    // stable across actions, workers and runs.
    #[test]
    fn canonical_exec_dirs_stable_per_crate_key() {
        let a1 = canonical_exec_dir("gen/root/1a2b/__polkavm-0.21.0__/");
        let a2 = canonical_exec_dir("gen/root/1a2b/__polkavm-0.21.0__/");
        let b = canonical_exec_dir("gen/root/1a2b/__serde-1.0.228__/");
        assert_eq!(a1, a2);
        assert_ne!(a1, b);
        // Same crate under a different buck configuration is a different key
        // (twins always share a configuration; cross-config reuse would leak).
        let c = canonical_exec_dir("gen/root/9f8e/__polkavm-0.21.0__/");
        assert_ne!(a1, c);
        // Short leaf: windows MAX_PATH is part of the budget.
        assert!(a1.file_name().unwrap().len() <= 16);
    }

    // Canonical dirs are shared mutable state: flavours of one key must
    // serialize within the process (affinity pins them to one process).
    #[test]
    fn same_key_shares_one_lock() {
        let l1 = crate_lock("__k__/");
        let l2 = crate_lock("__k__/");
        let other = crate_lock("__other__/");
        assert!(std::sync::Arc::ptr_eq(&l1, &l2));
        assert!(!std::sync::Arc::ptr_eq(&l1, &other));
    }

    /// In-memory Blobs: records every digest handed to `prefetch` so tests
    /// can assert the batching contract, serves blobs from a map.
    struct MemBlobs {
        blobs: HashMap<String, Vec<u8>>,
        prefetched: std::sync::Mutex<Vec<Vec<String>>>,
    }

    impl MemBlobs {
        fn dig(bytes: &[u8]) -> Dig {
            Dig {
                hash: crate::store::sha256_hex(bytes),
                size: bytes.len() as i64,
            }
        }
        fn insert(&mut self, bytes: &[u8]) -> Dig {
            let d = Self::dig(bytes);
            self.blobs.insert(d.hash.clone(), bytes.to_vec());
            d
        }
    }

    #[async_trait::async_trait]
    impl Blobs for MemBlobs {
        async fn get(&self, d: &Dig) -> Result<Vec<u8>> {
            self.blobs
                .get(&d.hash)
                .cloned()
                .with_context(|| format!("blob {} missing", d.hash))
        }
        async fn put(&self, bytes: Vec<u8>) -> Result<Dig> {
            Ok(Self::dig(&bytes))
        }
        async fn prefetch(&self, digs: &[Dig]) -> Result<()> {
            self.prefetched
                .lock()
                .unwrap()
                .push(digs.iter().map(|d| d.hash.clone()).collect());
            Ok(())
        }
    }

    fn file_node(name: &str, d: &Dig, exec: bool) -> re::FileNode {
        re::FileNode {
            name: name.into(),
            digest: Some(re::Digest {
                hash: d.hash.clone(),
                size_bytes: d.size,
            }),
            is_executable: exec,
            ..Default::default()
        }
    }

    // The parallel rewrite must materialize the same tree the sequential
    // walk did: nested dirs, duplicate digests fanned to distinct paths,
    // exec bits — and hand every file digest to ONE batched prefetch.
    #[tokio::test]
    async fn materialize_stages_tree_and_batches_prefetch() {
        let mut mem = MemBlobs {
            blobs: HashMap::new(),
            prefetched: std::sync::Mutex::new(Vec::new()),
        };
        let body = mem.insert(b"fn main() {}");
        let tool = mem.insert(b"#!/bin/sh\n");
        let sub = re::Directory {
            files: vec![file_node("dup.rs", &body, false)],
            ..Default::default()
        };
        let sub_bytes = sub.encode_to_vec();
        let sub_dig = mem.insert(&sub_bytes);
        let root = re::Directory {
            files: vec![
                file_node("main.rs", &body, false),
                file_node("run.sh", &tool, true),
            ],
            directories: vec![re::DirectoryNode {
                name: "nested".into(),
                digest: Some(re::Digest {
                    hash: sub_dig.hash.clone(),
                    size_bytes: sub_dig.size,
                }),
            }],
            ..Default::default()
        };
        let root_bytes = root.encode_to_vec();
        let root_dig = mem.insert(&root_bytes);

        let dest = tempfile::tempdir().unwrap();
        let n = materialize(&mem, &root_dig, dest.path(), "test")
            .await
            .unwrap();

        assert_eq!(n, 3);
        let read = |p: &str| std::fs::read(dest.path().join(p)).unwrap();
        assert_eq!(read("main.rs"), b"fn main() {}");
        assert_eq!(read("nested/dup.rs"), b"fn main() {}");
        assert_eq!(read("run.sh"), b"#!/bin/sh\n");
        #[cfg(unix)]
        assert!(is_exec(
            &std::fs::metadata(dest.path().join("run.sh")).unwrap()
        ));
        // Last prefetch batch = the file phase: every file digest, one call.
        let batches = mem.prefetched.lock().unwrap();
        let files = batches.last().unwrap();
        assert_eq!(files.len(), 3);
        assert!(files.contains(&body.hash) && files.contains(&tool.hash));
    }

    /// The old, strictly sequential tree builder — kept as the reference
    /// the concurrent upload_tree must byte-match: a shifted tree digest
    /// silently invalidates every cached directory output across laps.
    async fn reference_build_dir(
        blobs: &dyn Blobs,
        dir: &Path,
        children: &mut Vec<re::Directory>,
    ) -> Result<re::Directory> {
        let mut out = re::Directory::default();
        let mut entries: Vec<_> = std::fs::read_dir(dir)?.collect::<std::io::Result<_>>()?;
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let name = e.file_name().to_string_lossy().into_owned();
            let meta = e.metadata()?;
            let p = e.path();
            if meta.file_type().is_symlink() {
                out.symlinks.push(re::SymlinkNode {
                    name,
                    target: std::fs::read_link(&p)?.to_string_lossy().into_owned(),
                    node_properties: None,
                });
            } else if meta.is_dir() {
                let sub = Box::pin(reference_build_dir(blobs, &p, children)).await?;
                let digest = blobs.put(sub.encode_to_vec()).await?;
                children.push(sub);
                out.directories.push(re::DirectoryNode {
                    name,
                    digest: Some(digest.to_proto()),
                });
            } else {
                let bytes = tokio::fs::read(&p).await?;
                let is_executable = is_exec(&meta);
                let digest = blobs.put(bytes).await?;
                out.files.push(re::FileNode {
                    name,
                    digest: Some(digest.to_proto()),
                    is_executable,
                    node_properties: None,
                });
            }
        }
        Ok(out)
    }

    #[tokio::test]
    async fn upload_tree_matches_sequential_reference() {
        let mem = MemBlobs {
            blobs: HashMap::new(),
            prefetched: std::sync::Mutex::new(Vec::new()),
        };
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/nested")).unwrap();
        std::fs::create_dir_all(root.join("aaa")).unwrap(); // sorts before src
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::write(root.join("Cargo.toml"), b"[package]").unwrap();
        std::fs::write(root.join("src/lib.rs"), b"pub fn f() {}").unwrap();
        std::fs::write(root.join("src/nested/deep.rs"), b"mod deep;").unwrap();
        std::fs::write(root.join("aaa/z.txt"), b"z").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("../Cargo.toml", root.join("src/link.toml")).unwrap();

        let got = upload_tree(&mem, root).await.unwrap();
        let mut children = Vec::new();
        let root_dir = reference_build_dir(&mem, root, &mut children)
            .await
            .unwrap();
        let want = mem
            .put(
                re::Tree {
                    root: Some(root_dir),
                    children,
                }
                .encode_to_vec(),
            )
            .await
            .unwrap();
        assert_eq!(
            got, want,
            "concurrent tree digest diverged from the sequential reference"
        );
    }
}
