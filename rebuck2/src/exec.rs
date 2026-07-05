//! Action execution: materialize the input tree, run the command, collect
//! outputs. Used by workers and by the driver's local fallback — the only
//! difference is where blobs come from (`Blobs` impl).

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
    /// Write blob `d` to `dest`. Default: fetch + write. Store-backed impls
    /// override with hardlink-from-store (copy fallback) — the
    /// write-amplification fix.
    async fn materialize_file(&self, d: &Dig, dest: &std::path::Path) -> Result<()> {
        let bytes = self.get(d).await?;
        tokio::fs::write(dest, &bytes).await?;
        Ok(())
    }
}

pub struct Outcome {
    pub action_result: re::ActionResult,
    pub do_not_cache: bool,
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
    let exec_dir = tempfile::tempdir_in(scratch).context("mk exec dir")?;
    let root = exec_dir.path();
    materialize(blobs, &root_dig, root).await?;

    let cwd = if command.working_directory.is_empty() {
        root.to_path_buf()
    } else {
        root.join(&command.working_directory)
    };
    tokio::fs::create_dir_all(&cwd).await?;

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
    for p in &out_paths {
        if let Some(parent) = cwd.join(p).parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
    }

    let (argv0, args) = command
        .arguments
        .split_first()
        .context("Command.arguments empty")?;
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
    // One start/finish line per action, always — makes a worker's CI log a
    // live view of what it's building. The first output path is the most
    // human-readable handle we have for an action.
    let label = out_paths
        .first()
        .map(String::as_str)
        .unwrap_or(argv0)
        .to_owned();
    println!("[action] start {label}");
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
            let bytes = tokio::fs::read(&abs).await?;
            let is_executable = is_exec(&meta);
            let digest = blobs.put(bytes).await?;
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
        std::mem::forget(exec_dir);
    }
    Ok(Outcome {
        action_result: result,
        do_not_cache: action.do_not_cache,
    })
}

async fn materialize(blobs: &dyn Blobs, dir_digest: &Dig, dest: &Path) -> Result<()> {
    // Iterative BFS; recursion in async fns needs boxing and trees are shallow-ish.
    let mut queue: Vec<(Dig, PathBuf)> = vec![(dir_digest.clone(), dest.to_path_buf())];
    let mut seen: HashMap<Dig, re::Directory> = HashMap::new();
    // Symlinks last: windows needs to know file-vs-dir at creation time, so
    // the targets must exist first (buck2's __srcs trees link to sibling dirs).
    let mut symlinks: Vec<(PathBuf, String)> = Vec::new();
    while let Some((dig, path)) = queue.pop() {
        tokio::fs::create_dir_all(&path).await?;
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
            let fp = path.join(&f.name);
            blobs.materialize_file(&fdig, &fp).await?;
            if f.is_executable {
                set_exec(&fp).await?;
            }
        }
        for s in &dir.symlinks {
            symlinks.push((path.join(&s.name), s.target.clone()));
        }
        for d in &dir.directories {
            let ddig: Dig = (&d.digest.clone().context("DirectoryNode.digest")?).into();
            queue.push((ddig, path.join(&d.name)));
        }
    }
    for (link, target) in symlinks {
        make_symlink(&link, &target)
            .with_context(|| format!("symlink {} -> {}", link.display(), target))?;
    }
    Ok(())
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

/// Build + upload a Tree proto for an output directory; returns the Tree digest.
async fn upload_tree(blobs: &dyn Blobs, dir: &Path) -> Result<Dig> {
    let mut children: Vec<re::Directory> = Vec::new();
    let root = build_dir(blobs, dir, &mut children).await?;
    let tree = re::Tree {
        root: Some(root),
        children,
    };
    blobs.put(tree.encode_to_vec()).await
}

/// Post-order directory build. Entries sorted by name — REAPI canonical form,
/// and tree digests must be deterministic.
async fn build_dir(
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
            let sub = Box::pin(build_dir(blobs, &p, children)).await?;
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
async fn set_exec(p: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    tokio::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).await?;
    Ok(())
}

#[cfg(not(unix))]
async fn set_exec(_p: &Path) -> Result<()> {
    Ok(())
}
