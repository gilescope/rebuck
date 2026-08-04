# Near-zero-cost input-tree materialization

Decision document for `exec.rs materialize()`. Audience: engine author.

---

## Problem

At sweep scale (~17k+ actions), each action's input tree is materialised by
copying every blob from the local CAS into a fresh temp exec dir. For rlib
inputs that reach 100s of MB per action, the write-amplification is a top-2
efficiency cost.

`Store::link_out` (hardlink with copy fallback) and `Blobs::materialize_file`
already exist and are wired up. Two correctness gaps block safe deployment at
full scale:

1. CAS blobs are not marked read-only after `Store::put` — hardlinks share the
   inode, so a careless action writing to an input path silently corrupts the
   blob for every consumer.
2. `exec.rs` `materialize()` calls `set_exec(&fp)` after materialising a hardlinked
   file, which `chmod 0o755`s the shared CAS inode — stomping the read-only
   protection and leaking to all concurrent actions sharing that blob.

---

## Measured on the actual runners (bench-materialize.yml, 2220 files / 385 MB)

| runner | copy | hardlink | speed-up | teardown copy->link |
| ------------------------ | ------- | ------ | ---- | ------------ |
| windows-latest (NTFS) | 1172 ms | 469 ms | 2.5x | 265 -> 187 ms |
| ubuntu-latest (ext4) | 306 ms | 68 ms | 4.5x | 75 -> 28 ms |
| macos-latest (APFS) | 1222 ms | 384 ms | 3.2x | 107 -> 81 ms |

Caveats: source files were page-cache-hot, so "copy" flatters itself — at
sweep scale copies dirty real disk (~850 GB churn per whole-tree run) while
links never touch data. The mac clonefile-via-`cp -c` row (5.9 s) was
process-spawn overhead, not clonefile; in-process cloning rivals hardlink.
`cp --reflink=always` FAILED on ubuntu-latest: ext4, as the table below says.

## Per-OS mechanism table

| Mechanism | GH runners | Mutation-safe | Rust access | Verdict |
| --------- | ---------- | ------------- | ----------- | ------- |
| Hardlink (`hard_link`) | Linux ext4, NTFS — same volume | No — shared inode; requires read-only CAS blobs | `std::fs::hard_link` | Primary on Linux and Windows |
| APFS CoW (`fclonefileat`) | macOS-latest = macOS 15 arm64 (APFS exclusive) | Yes — private inode, CoW on first write | `std::fs::copy` — already calls `fclonefileat` since Rust 1.36; no extra deps | Primary on macOS |
| FICLONE reflink | Linux btrfs/XFS with `reflink=1` only; ext4 returns `EOPNOTSUPP` | Yes | `reflink-copy::reflink_or_copy` | No benefit on ubuntu-latest (ext4); keep in fallback chain for non-GHA deployments |
| ReFS block clone | Windows Dev Drive only (VHDX, opt-in); NTFS default = no CoW | Practically yes, but conditional on ioctl succeeding and I/O going through the filesystem layer; ref-count cap = 8,175 per physical region | `reflink-copy::reflink_or_copy` — Windows path self-described as "untested and probably buggy" in both `reflink` and `reflink-copy` | Optional upgrade; not the default |
| `copy_file_range` | Linux kernel >= 4.5 | No (ext4 does no CoW; `std::io::copy` uses it internally on Rust >= 1.64) | `std::io::copy` | Hardlink fallback on Linux |
| Full copy | All | Yes (private inode) | `std::fs::copy` / `tokio::fs::write` | Always-on last resort |

Notes:

- `windows-latest` maps to windows-2025 as of 2025-09-30: C: only, ~33 GB
  free, NTFS, no native ReFS volume.
- `macos-latest` maps to macOS 15 arm64 (Apple Silicon) since 2025-09-01.
  `std::fs::copy` calls `fclonefileat` unconditionally since Rust 1.81 (Sierra
  runtime check removed); falls back to `fcopyfile` on `ENOTSUP`/`EXDEV`.
- ext4 hardlink ceiling is `EXT4_LINK_MAX = 65000` per inode. At 17k actions
  this is unlikely to fire for any single blob, but handle `EMLINK` anyway.

---

## Chosen fallback chain per OS

### macOS

```text
std::fs::copy(cas_blob, dest)
  // fclonefileat first (CoW, O(1), mutation-safe by construction)
  // falls back to fcopyfile on ENOTSUP / EXDEV / EEXIST
```

Do not use `hard_link` on macOS: the `set_exec` inode-mutation bug is the
failure mode, and `std::fs::copy` already gives CoW for free with zero extra
dependencies.

### Linux (ubuntu-latest = Ubuntu 24.04, ext4)

```text
hard_link(cas_blob, dest)          // near-free; EMLINK or EXDEV falls through
  -> std::io::copy(src_fd, dst_fd) // in-kernel copy_file_range, no heap bounce
       -> std::fs::copy()          // user-space safety net
```

**Exec-dir placement hazard**: Ubuntu 24.10 changed `/tmp` to tmpfs by
default (not 26.04 as the research dossier claimed -- the correct boundary is
24.10). Hardlinks from `/tmp` (tmpfs) to the CAS (ext4 root) fail with `EXDEV`.

Fix: change `main.rs:104/130` from `std::env::temp_dir().join("rebuck2-exec")`
to `<store_root>/exec/` so exec dirs are always on the same mount as the CAS.
Alternatively accept `$RUNNER_TEMP`, which GitHub guarantees to be on the root
filesystem on hosted runners.

> Shipped differently (61f9189): keyed commands use canonical run-stable dirs
> (`/tmp/rebuck2-exec` resp `C:\rb2x`, `REBUCK2_EXEC_BASE` overrides) because
> SVH stability requires the SAME path across workers and runs — a
> store-relative default would vary with `--store`. tmpfs `/tmp` therefore
> degrades to the copy fallback (all link sites already catch
> `CrossesDevices`), it does not break. Unkeyed commands still use
> per-action temp dirs under the worker scratch.

### Windows (NTFS, windows-2025)

```text
hard_link(cas_blob, dest)          // FILE_ATTRIBUTE_READONLY on CAS blob (advisory)
  -> on ERROR_TOO_MANY_LINKS (1142) or ERROR_NOT_SAME_DEVICE (17):
    std::fs::copy()                // always correct
```

Current `link_out` catches all `hard_link` errors and falls back to copy.
Tighten: only silently fall back on `ErrorKind::TooManyLinks` and
`ErrorKind::CrossesDevices`; surface other error kinds so filesystem bugs do
not hide.

**Dev Drive (opt-in)**: add `samypr100/setup-dev-drive@v3` to CI, point both
the store (`--store`) and `REBUCK2_EXEC_BASE` at the mounted ReFS drive. With both
on the same ReFS volume, `reflink_or_copy` uses `FSCTL_DUPLICATE_EXTENTS_TO_FILE`
-- CoW with no link count ceiling and Defender performance mode. Treat as
experimental until the `reflink-copy` Windows path exits its "probably buggy"
self-description.

---

## Mutation-hazard policy

### Threat model

The target is careless actions (compiler writes to a declared output that
happens to share a name with an input) rather than adversarial same-user code.
`chmod 0o444` / `FILE_ATTRIBUTE_READONLY` stops `open(O_WRONLY)` with `EACCES`
for non-root. It does not stop a same-UID process from calling `chmod` to clear
it, nor does it stop a rename into the CAS directory -- both require the action
to know the CAS path and deliberately subvert it. That is out of scope.

`fs.protected_hardlinks=1` (Linux default since kernel ~3.6) prevents
cross-user hardlink creation; relevant for multi-tenant deployments but moot on
single-user GHA runners.

### Required change: CAS blob read-only enforcement

In `Store::put` (store.rs:104), after `tokio::fs::write(&tmp, bytes)` and
before `tokio::fs::rename(&tmp, &dest)`, set permissions on `tmp`:

- Unix: `std::fs::set_permissions(&tmp, PermissionsExt::from_mode(0o444))`
  (or `0o555` for executable blobs -- see below). Setting on `tmp` before
  rename means `dest` is read-only from first visibility; no race window.
- Windows: `std::fs::set_permissions(&dest, readonly_perms)` after rename
  (`FILE_ATTRIBUTE_READONLY`; advisory but stops accidental writes).

Cost: one extra syscall per unique blob written. Zero cost per subsequent link.

Cleanup (exec-dir teardown): on Unix, exec-dir hardlinks inherit the CAS
inode's read-only bit; `TempDir`'s `remove_dir_all` will fail on read-only
files. Either clear read-only on exec-dir files before drop, or use
`FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE` on Windows 10+. Simplest: a
custom drop that does `chmod 0o644` on all files in the exec dir before
`remove_dir_all`. On macOS the CoW clone has its own inode, so no issue.

### Executable-bit fix (existing bug, store.rs + exec.rs)

`exec.rs` `materialize()` calls `set_exec(&fp)` -- `chmod 0o755` -- on the file at
`fp` after materialising it. If `fp` is a hardlink to a CAS blob:

- The chmod mutates the shared inode, overwriting the `0o444` protection.
- All other actions hardlinked to the same blob are now `0o755` (writable).

**Fix**: encode the executable bit in the CAS blob's permissions at store time.

1. Add `is_executable: bool` parameter to `Store::put`. After
   `tokio::fs::write(&tmp, bytes)`, set mode `0o555` (executable, not writable)
   if `is_executable`, else `0o444`.
2. In `materialize()` (exec.rs): skip `set_exec(&fp)` when the blob
   was materialised via `link_out` (hardlink or CoW clone). The permissions are
   already correct on the shared inode. Call `set_exec` only when the file is
   a private copy (i.e. link_out returned the copy-fallback path).
3. For output collection, `build_dir` determines `is_executable` from the
   output file's metadata -- pass this through to `blobs.put`.

On macOS, `std::fs::copy` clones source permissions; if the CAS blob is `0o555`
the CoW clone is `0o555`. No `set_exec` call needed.

### Action category risk matrix

| Category | Writes to input paths? | Risk with hardlinks |
| -------- | ---------------------- | ------------------- |
| `rustc` compile | No (outputs to `--out-dir`) | None |
| `cl.exe` compile | No | None |
| `build.rs` run | No (outputs to `OUT_DIR`) | None |
| Unpack / archive actions | No | None |
| Any action declaring output == input path | Yes (REAPI violation) | Blocked by `0o444` -> `EACCES`; loud failure, no silent corruption |

---

## Output-collection path

Current (`run_action` output collection, exec.rs): `tokio::fs::read` -> `blobs.put(bytes)` ->
`Store::put` writes a tmp file and renames. The read is unavoidable (SHA-256
hash requires it). The write is avoidable.

After hashing the output:

```rust
// Instead of writing bytes back to a tmp file:
hard_link(exec_output_path, store_tmp_path)
rename(store_tmp_path, cas_dest)
set_permissions(cas_dest, readonly)
```

This saves one full-file write per output file. Safety conditions:

- Read-only enforcement (Priority 1) must already be in place; otherwise a
  CAS input could have been mutated in place and the hardlink would store the
  corrupted content.
- The output file must be on the same mount as the CAS (guaranteed if exec dirs
  are under `<store_root>/exec/`).
- On macOS the output file is a fresh inode (the CoW path does not hardlink
  inputs), so hardlinking output into the store is straightforward.

This is Priority 2; implement after the read-only + exec-bit fix is green in CI.

---

## Implementation sketch

### Files to change

| File | Change | ~lines |
| ---- | ------ | ------ |
| `store.rs` | `put()`: chmod blob to `0o444`/`0o555` after tmp write; add `is_executable: bool` param; tighten `link_out` fallback to only match `TooManyLinks`/`CrossesDevices`/`PermissionDenied` | ~25 |
| `exec.rs` | macOS: use `std::fs::copy` in `materialize_file` default or override; skip `set_exec` when hardlinked; pass `is_executable` to `put` in `build_dir` | ~30 |
| `worker.rs`, `driver.rs` | Thread `is_executable` through call sites; update `StoreBlobs::put` and `RemoteBlobs::put` signatures | ~20 |
| `main.rs` | Scratch default: superseded for keyed commands by canonical dirs (`REBUCK2_EXEC_BASE`, 61f9189); `<store_root>/exec/` remains an option for unkeyed ones | ~5 |

### Crates

- No new crate needed for macOS or Linux (stdlib + existing `reflink-copy` dep
  if wanted for btrfs/XFS detection).
- `reflink-copy` crate: keep in `Cargo.toml` behind a feature flag for
  btrfs/XFS non-GHA deployments and the Windows Dev Drive experimental path.
- Windows exec-dir cleanup of read-only files: use `std::fs::set_permissions`
  to clear read-only before `remove_dir_all`; avoids pulling in `windows-sys`
  for `FILE_DISPOSITION_FLAG_IGNORE_READONLY_ATTRIBUTE`.

### Suggested test

```rust
// In store.rs #[cfg(test)]:
// 1. put a blob; verify mode is 0o444
// 2. link_out to a tmp path; verify both paths have mode 0o444
// 3. attempt tokio::fs::write to the linked path; expect EACCES/PermissionDenied
// 4. put same blob with is_executable=true; verify mode is 0o555
```

Write the test before the fix (red -> green proves the bug existed).

---

## Rollout

1. **Measure first**: expose a `materialized_bytes_copied` counter in
   `Store::link_out`; log it in the worker heartbeat alongside
   `stored_bytes`/`read_bytes`. Establish the baseline copy volume before
   landing the fix.

2. **Priority 1 - read-only + exec-bit fix**: land together (the exec-bit fix
   depends on read-only being in place). Keep `--no-hardlinks` as the rollback
   escape hatch. The flag already exists; no new risk surface.

3. **macOS CoW path**: low risk -- `std::fs::copy` is always correct; CoW is
   transparent. Can land in the same PR as Priority 1.

4. **Exec-dir placement fix** (`temp_dir` -> `<store_root>/exec/`): partially
   superseded — keyed commands now use canonical run-stable dirs (61f9189)
   and every link site copy-falls-back on `CrossesDevices`, so tmpfs `/tmp`
   costs copies, not correctness. `<store_root>/exec/` stays open as a perf
   option for unkeyed commands only.

5. **Priority 2 - output-collection hardlink**: after Priority 1 is green in
   CI for at least one sweep run. Measure `stored_bytes` reduction.

6. **Dev Drive opt-in (Windows)**: CI YAML change; add `setup-dev-drive@v3`
   step, set `--store` and `REBUCK2_EXEC_BASE` to the ReFS drive letter.
   Gated behind its own CI job variant; does not affect the default Windows
   path. Treat `reflink-copy` Windows support as experimental until confirmed
   with an actual Dev Drive workload.

### Risks

- Read-only enforcement may break actions that `chmod` their own inputs (an
  explicit REAPI violation; fail-fast is the right behaviour).
- `set_permissions` on the `tmp` file before rename: on NTFS, the read-only
  attribute is advisory, so the rename itself succeeds. No issue.
- Exec-dir cleanup of read-only files on Windows: test on windows-2025; the
  `set_permissions` + `remove_dir_all` approach is simple and portable.
- Dev Drive VHDX cold-setup adds 5-10 s per CI run; mitigated by VHDX caching
  via `actions/cache`.
