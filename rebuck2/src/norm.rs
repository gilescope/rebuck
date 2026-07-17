//! Name-independent (canonical) action keys — the caching dream: two
//! actions performing byte-identical work under different target labels
//! share one cache entry.
//!
//! Mechanism (general, not rust-specific): canonical key =
//! SHA-256(normalize(Command) ∥ normalize(each @-argsfile blob reachable
//! from the arguments) ∥ sorted source-input content hash). Normalization
//! rewrites every label-derived token — output-path prefixes, content-hash
//! segments, `-Cmetadata`, `--buck-target`, path-env absolutizations — to
//! fixed placeholders, while leaving work-relevant tokens (crate versions,
//! `--target` triples, feature flags) untouched.
//!
//! Soundness: a canonical hit serves the ORIGINAL result bytes (CAS
//! digests unchanged) with only the result's path strings rewritten to the
//! requesting action's declared outputs. Because consumers then ingest
//! byte-identical dep artifacts, their own source-input hashes converge and
//! the dedupe fixpoint propagates up the graph one honest level at a time —
//! a parent only ever hits after all its inputs are already byte-shared.
//! Two same-named-but-different crates can never collide: any divergence in
//! source bytes, flags, or dep bytes changes the key.

use bazel_remote_apis::build::bazel::remote::execution::v2 as re;
use sha2::{Digest as _, Sha256};

/// Gate for the first validated action category. Other categories are
/// normalized identically but not yet probed — widen as each is proven.
pub fn is_rustc_action(cmd: &re::Command) -> bool {
    cmd.arguments.iter().any(|a| a.contains("rustc_action.py"))
}

fn is_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Rewrite `<prefix>/__<name>__/<cfg-or-content-hash|output_artifacts|aquery_placeholder>/<rest>`
/// to `__norm__/__<name>__/<rest>`. Strips BOTH the label package prefix
/// and the config/content-hash segment — the hash segment is itself
/// label-contaminated (it hashes argsfile content that contains the label),
/// so keeping it would make canonical keys permanently diverge.
fn normalize_path_like(tok: &str) -> Option<String> {
    let start = tok.find("__")?;
    // Find a `/__name__/` segment: search each segment boundary.
    let bytes = tok.as_bytes();
    let mut seg_start = 0usize;
    let mut i = 0usize;
    let _ = start;
    while i <= bytes.len() {
        if i == bytes.len() || bytes[i] == b'/' {
            let seg = &tok[seg_start..i];
            if seg.len() > 4 && seg.starts_with("__") && seg.ends_with("__") {
                // Next segment is the hash/placeholder to strip (if present).
                let rest_start = if i < bytes.len() { i + 1 } else { i };
                let rest = &tok[rest_start..];
                let (next, after) = match rest.find('/') {
                    Some(p) => (&rest[..p], &rest[p + 1..]),
                    None => (rest, ""),
                };
                let strip_next =
                    is_hex(next, 16) || next == "output_artifacts" || next == "aquery_placeholder";
                let tail = if strip_next { after } else { rest };
                return Some(if tail.is_empty() {
                    format!("__norm__/{seg}")
                } else {
                    format!("__norm__/{seg}/{tail}")
                });
            }
            seg_start = i + 1;
        }
        i += 1;
    }
    None
}

/// Normalize `lib<name>-<8hex>.<ext>` filename suffixes (rustc extra
/// filename hashes) anywhere in the token.
fn normalize_lib_suffix(tok: &str) -> String {
    let mut out = String::with_capacity(tok.len());
    let mut rest = tok;
    loop {
        // find "-<8hex>." after an alnum run
        let mut found = None;
        let b = rest.as_bytes();
        for i in 0..b.len() {
            if b[i] == b'-' && i + 9 < b.len() && b[i + 9] == b'.' && is_hex(&rest[i + 1..i + 9], 8)
            {
                found = Some(i);
                break;
            }
        }
        match found {
            Some(i) => {
                out.push_str(&rest[..i]);
                out.push_str("-00000000");
                rest = &rest[i + 9..];
            }
            None => {
                out.push_str(rest);
                return out;
            }
        }
    }
}

/// Normalize one command/argsfile token. First-match-wins for the
/// flag-shaped rewrites; path-shaped rewrites apply to whatever remains.
pub fn normalize_token(tok: &str) -> String {
    if let Some(rest) = tok.strip_prefix('@') {
        return format!("@{}", normalize_token(rest));
    }
    if tok.starts_with("-Cmetadata=") {
        return "-Cmetadata=__CKEY__".to_owned();
    }
    if tok.starts_with("-Cextra-filename=") {
        return "-Cextra-filename=-00000000".to_owned();
    }
    if tok.starts_with("--buck-target=") {
        return "--buck-target=__CKEY__".to_owned();
    }
    if let Some(rest) = tok.strip_prefix("--remap-path-prefix=") {
        // Normalize LHS like a path; RHS collapses entirely.
        let lhs = rest.split('=').next().unwrap_or(rest);
        let norm = normalize_path_like(lhs).unwrap_or_else(|| lhs.to_owned());
        return format!("--remap-path-prefix={norm}=__norm__/");
    }
    if let Some(rest) = tok.strip_prefix("--path-env=") {
        // NAME=value: keep the name (work-relevant), normalize the value.
        if let Some(eq) = rest.find('=') {
            let (name, val) = rest.split_at(eq);
            let val = &val[1..];
            let norm = normalize_path_like(val).unwrap_or_else(|| val.to_owned());
            return format!("--path-env={name}={}", normalize_lib_suffix(&norm));
        }
    }
    let path_normed = normalize_path_like(tok).unwrap_or_else(|| tok.to_owned());
    normalize_lib_suffix(&path_normed)
}

/// Normalize argsfile content line-by-line (CRLF-tolerant).
pub fn normalize_argsfile(content: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(content);
    let mut out = String::with_capacity(text.len());
    for line in text.split('\n') {
        let (line, cr) = match line.strip_suffix('\r') {
            Some(l) => (l, "\r"),
            None => (line, ""),
        };
        out.push_str(&normalize_token(line));
        out.push_str(cr);
        out.push('\n');
    }
    out.into_bytes()
}

/// Canonical key over the normalized parts, 0x00-separated.
pub fn canonical_key_from_parts(
    norm_cmd: &[u8],
    norm_argsfiles: &[Vec<u8>],
    src_content_hash: &[u8; 32],
) -> String {
    let mut h = Sha256::new();
    h.update(norm_cmd);
    h.update([0u8]);
    for f in norm_argsfiles {
        h.update(f);
        h.update([0u8]);
    }
    h.update(src_content_hash);
    let d = h.finalize();
    let mut s = String::with_capacity(64);
    for b in d {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Normalized byte form of a Command proto: every argument, env value and
/// output path through normalize_token, joined with newlines. (A fresh
/// proto encode would also work; a flat text form is easier to eyeball in
/// forensics dumps.)
pub fn normalize_command(cmd: &re::Command) -> Vec<u8> {
    let mut out = String::new();
    for a in &cmd.arguments {
        out.push_str(&normalize_token(a));
        out.push('\n');
    }
    for e in &cmd.environment_variables {
        out.push_str(&e.name);
        out.push('=');
        out.push_str(&normalize_token(&e.value));
        out.push('\n');
    }
    #[allow(deprecated)]
    let paths: Vec<&String> = if cmd.output_paths.is_empty() {
        cmd.output_files
            .iter()
            .chain(cmd.output_directories.iter())
            .collect()
    } else {
        cmd.output_paths.iter().collect()
    };
    for p in paths {
        out.push_str(&normalize_token(p));
        out.push('\n');
    }
    out.into_bytes()
}

/// Store form of a result: path strings normalized (digests untouched),
/// timing metadata cleared. The canonical namespace must never leak one
/// requester's concrete paths to another.
pub fn normalize_result(r: &re::ActionResult) -> re::ActionResult {
    let mut n = r.clone();
    for f in &mut n.output_files {
        f.path = normalize_token(&f.path);
    }
    for d in &mut n.output_directories {
        d.path = normalize_token(&d.path);
    }
    for s in &mut n.output_symlinks {
        s.path = normalize_token(&s.path);
    }
    n.execution_metadata = None;
    n
}

/// buck2's OSS RE client hard-rejects any ActionResult without
/// execution_metadata ("The execution metadata are not defined.").
/// Call at every serve boundary; rows banked by pre-fix code lack it.
pub fn ensure_execution_metadata(r: &mut re::ActionResult) {
    if r.execution_metadata.is_none() {
        r.execution_metadata = Some(re::ExecutedActionMetadata {
            worker: "rebuck2-cached".into(),
            ..Default::default()
        });
    }
}

/// Serve a canonical result under the requesting action's paths.
/// Positional: normalization preserves order, and both sides of a canonical
/// match declared the same output shape (same normalized Command).
pub fn rewrite_result(mut canonical: re::ActionResult, cmd: &re::Command) -> re::ActionResult {
    // Presence is contractual (see ensure_execution_metadata): the store
    // form strips it so one requester's timing never leaks to another.
    ensure_execution_metadata(&mut canonical);
    #[allow(deprecated)]
    let declared: Vec<String> = if cmd.output_paths.is_empty() {
        cmd.output_files
            .iter()
            .chain(cmd.output_directories.iter())
            .cloned()
            .collect()
    } else {
        cmd.output_paths.clone()
    };
    // Map normalized-path -> declared-path for exact assignment.
    let mut by_norm: std::collections::HashMap<String, &String> = std::collections::HashMap::new();
    for p in &declared {
        by_norm.insert(normalize_token(p), p);
    }
    for f in &mut canonical.output_files {
        if let Some(p) = by_norm.get(&f.path) {
            f.path = (*p).clone();
        }
    }
    for d in &mut canonical.output_directories {
        if let Some(p) = by_norm.get(&d.path) {
            d.path = (*p).clone();
        }
    }
    for s in &mut canonical.output_symlinks {
        if let Some(p) = by_norm.get(&s.path) {
            s.path = (*p).clone();
        }
    }
    canonical
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAIN: &str = "buck-out/sweep/art/fixups/third-party/__adler-1__/a1b2c3d4e5f60718/LPPMD/libadler-12ab34cd.rmeta";
    const SNAP: &str = "buck-out/sweep/art/fixups/third-party/snapshots/2024-11/__adler-1__/ffee00112233aabb/LPPMD/libadler-98dc76ba.rmeta";

    #[test]
    fn labels_and_hashes_normalize_identically() {
        assert_eq!(normalize_token(MAIN), normalize_token(SNAP));
        assert_eq!(
            normalize_token(MAIN),
            "__norm__/__adler-1__/LPPMD/libadler-00000000.rmeta"
        );
    }

    #[test]
    fn cmetadata_and_buck_target_collapse() {
        assert_eq!(
            normalize_token("-Cmetadata=fixups//third-party:adler-1#b9dc2001455688d3"),
            normalize_token(
                "-Cmetadata=fixups//third-party/snapshots/2024-11:adler-1#deadbeef00112233"
            ),
        );
        assert_eq!(
            normalize_token("--buck-target=fixups//third-party:adler-1"),
            "--buck-target=__CKEY__"
        );
    }

    #[test]
    fn version_sensitivity_survives() {
        // The crate version lives in a plain path segment, not a label
        // token — different versions must stay distinct.
        let v2 = normalize_token(
            "buck-out/sweep/art/fixups/third-party/__adler-1.0.2.crate__/aabbccdd00112233/adler-1.0.2.crate/src/lib.rs",
        );
        let v3 = normalize_token(
            "buck-out/sweep/art/fixups/third-party/__adler-1.0.3.crate__/aabbccdd00112233/adler-1.0.3.crate/src/lib.rs",
        );
        assert_ne!(v2, v3);
    }

    #[test]
    fn placeholder_segments_strip() {
        assert_eq!(
            normalize_token("buck-out/x/art/p/__t__/output_artifacts/out/lib.rmeta"),
            "__norm__/__t__/out/lib.rmeta"
        );
        assert_eq!(
            normalize_token("@buck-out/x/art/p/__t__/aquery_placeholder/f.args"),
            "@__norm__/__t__/f.args"
        );
    }

    #[test]
    fn canonical_key_equality_across_labels() {
        let args_main =
            format!("--crate-name=adler\n-Cmetadata=fixups//third-party:adler-1#b9dc\n{MAIN}\n");
        let args_snap = format!("--crate-name=adler\n-Cmetadata=fixups//third-party/snapshots/2024-11:adler-1#ffee\n{SNAP}\n");
        let src = [7u8; 32];
        let k1 =
            canonical_key_from_parts(b"cmd", &[normalize_argsfile(args_main.as_bytes())], &src);
        let k2 =
            canonical_key_from_parts(b"cmd", &[normalize_argsfile(args_snap.as_bytes())], &src);
        assert_eq!(k1, k2);
        // ...and a source change breaks it.
        let k3 = canonical_key_from_parts(
            b"cmd",
            &[normalize_argsfile(args_main.as_bytes())],
            &[8u8; 32],
        );
        assert_ne!(k1, k3);
    }

    #[test]
    fn rewrite_result_restores_execution_metadata() {
        // The canonical store form strips execution_metadata (timing must
        // not leak between requesters), but buck2's OSS RE client REJECTS
        // any ActionResult without it ("The execution metadata are not
        // defined.") - every canonical hit served bare was converted into
        // an internal error (run 29524645875, atk-sys rustc diag).
        let out = rewrite_result(re::ActionResult::default(), &re::Command::default());
        assert!(out.execution_metadata.is_some());
    }

    #[test]
    fn rewrite_result_maps_paths_keeps_digests() {
        let cmd = re::Command {
            output_paths: vec![SNAP.to_owned()],
            ..Default::default()
        };
        let mut canonical = re::ActionResult::default();
        canonical.output_files.push(re::OutputFile {
            path: normalize_token(MAIN),
            digest: Some(re::Digest {
                hash: "aa".repeat(32),
                size_bytes: 42,
            }),
            is_executable: false,
            contents: Vec::new(),
            node_properties: None,
        });
        let out = rewrite_result(canonical, &cmd);
        assert_eq!(out.output_files[0].path, SNAP);
        assert_eq!(out.output_files[0].digest.as_ref().unwrap().size_bytes, 42);
    }
}
