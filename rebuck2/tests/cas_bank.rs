//! End-to-end CAS bank: restore, then publish, against a stub artifact API.
//!
//! This is the coverage that used to live in
//! `ci/cas-bank-integration-test.sh`'s laps 1-6. It moved here with the
//! choreography: the shell can no longer intercept an API call made from
//! inside the binary, and the properties are worth keeping.
//!
//! The federated invariant these guard: a node banks ONLY its own range
//! and spills everything else, so a blob is never both banked and spilled,
//! and a node whose own-manifest lookup FLAKED must publish nothing at all
//! rather than a thin manifest that newest-wins would put over the fat one.

mod common;
use common::*;

use std::path::Path;

/// A store with one blob per given hash.
fn store_with(dir: &Path, blobs: &[(&str, &[u8])]) {
    for (hash, body) in blobs {
        let d = dir.join("cas").join(&hash[..2]);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(hash), body).unwrap();
    }
}

fn run_bank(api: &str, work: &Path, args: &[&str]) -> std::process::Output {
    bin()
        .arg("bank")
        .args(args)
        .env("GITHUB_API_URL", api)
        .env("GITHUB_REPOSITORY", "o/r")
        .env("GH_TOKEN", "t")
        .env("BANK_WORK", work)
        .output()
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_bank_exits_three() {
    let api = serve(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let out = run_bank(
        &api,
        &dir.path().join("work"),
        &[
            "cas-restore",
            dir.path().join("store").to_str().unwrap(),
            "0",
            "some-lineage",
            "-",
        ],
    );
    assert_eq!(out.status.code(), Some(3), "cold bank must exit 3");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_banks_its_range_and_spills_the_rest() {
    // Range 0 owns prefixes 0 and 1. Everything else must spill - and a
    // blob must never appear in both, or two owners bank the same bytes.
    let api = serve(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let work = dir.path().join("work");
    store_with(
        &store,
        &[
            ("0aaa0001", b"in-range"),
            ("1bbb0002", b"in-range-too"),
            ("2ccc0003", b"foreign"),
            ("edee0004", b"foreign-too"),
        ],
    );

    let out = run_bank(
        &api,
        &work,
        &[
            "cas-publish",
            store.to_str().unwrap(),
            "w1",
            "0",
            "lin",
            "100",
            "-",
        ],
    );
    assert!(
        out.status.success(),
        "publish failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let banked = banked_blobs(&work);
    let spilled = spilled_blobs(&work);
    assert_eq!(banked, ["0aaa0001", "1bbb0002"], "own range only");
    assert_eq!(spilled, ["2ccc0003", "edee0004"], "everything else spills");
    assert!(
        banked.iter().all(|b| !spilled.contains(b)),
        "a blob must never be both banked and spilled"
    );
    assert!(
        work.join("bank-manifest-out/manifest.json").is_file(),
        "a range owner with new blobs stages a manifest"
    );
}

/// Read a zstd'd id list.
fn zst_lines(path: &Path) -> Vec<String> {
    if !path.is_file() {
        return Vec::new();
    }
    let raw = std::process::Command::new("zstd")
        .arg("-dqc")
        .arg(path)
        .output()
        .unwrap();
    let mut v: Vec<String> = String::from_utf8_lossy(&raw.stdout)
        .lines()
        .map(str::to_owned)
        .collect();
    v.sort();
    v
}

/// What this lap BANKED: publish moves the payload into the container and
/// drops the staging dir, so the manifest's own list is the durable
/// record - and it is what the next restore will read.
fn banked_blobs(work: &Path) -> Vec<String> {
    zst_lines(&work.join("bank-manifest-out/blobs.txt.zst"))
}

/// What this lap SPILLED: spill segments are moved whole, sidecar and all.
fn spilled_blobs(work: &Path) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(rd) = std::fs::read_dir(work.join("bank-spill")) {
        for d in rd.flatten() {
            out.extend(zst_lines(&d.path().join("blobs.txt.zst")));
        }
    }
    out.sort();
    out.dedup();
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_flaked_own_lookup_publishes_nothing_but_spill() {
    // The monotonicity trap: if the own-range manifest lookup ERRORS, the
    // state is UNKNOWN, not absent. Staging a manifest anyway would let
    // newest-wins put a thin generation over the fat one and lose the
    // range's history. The lap must demote to spill-only.
    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let work = dir.path().join("work");
    store_with(&store, &[("0aaa0001", b"mine"), ("2ccc0003", b"foreign")]);

    // The restore's flag is what publish reads; write it directly rather
    // than simulating a network failure, so the assertion is about the
    // publish rule and nothing else.
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join(".own-range-unknown"), b"").unwrap();
    std::fs::write(work.join("bank-blobs.txt"), "").unwrap();

    let api = serve(vec![]).await;
    let out = run_bank(
        &api,
        &work,
        &[
            "cas-publish",
            store.to_str().unwrap(),
            "w1",
            "0",
            "lin",
            "200",
            "-",
        ],
    );
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stdout).contains("spill-only lap"),
        "the demotion should be announced: {}",
        String::from_utf8_lossy(&out.stdout)
    );
    assert!(
        !work.join("bank-manifest-out/manifest.json").is_file(),
        "an unknown own state must stage NO manifest"
    );
    let spilled = spilled_blobs(&work);
    assert!(
        spilled.contains(&"0aaa0001".to_string()) && spilled.contains(&"2ccc0003".to_string()),
        "demoted to spill-only, everything spills: {spilled:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_child_lineage_inherits_the_trunks_range() {
    // A branch must start warm from the trunk and bank only its own new
    // blobs - under its OWN manifest, never the parent's.
    let seg = "cas-seg-parent";
    let api = serve(vec![
        Stub {
            name: "cas-manifest-trunk-r0".into(),
            branch: "trunk".into(),
            created_at: "2026-07-30T00:00:00Z".into(),
            zip: manifest_zip_with_prefixes(
                "trunk",
                "1-1",
                1,
                "w0",
                seg,
                "container-parent",
                &["0aaa0001"],
                "0",
            ),
        },
        Stub {
            name: "container-parent".into(),
            branch: "trunk".into(),
            created_at: "2026-07-30T00:00:00Z".into(),
            zip: container_zip(seg, &[("cas/0a/0aaa0001", b"from-the-trunk")]),
        },
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let work = dir.path().join("work");
    let out = run_bank(
        &api,
        &work,
        &[
            "cas-restore",
            store.to_str().unwrap(),
            "0",
            "branch",
            "trunk",
        ],
    );
    assert!(
        out.status.success(),
        "inherit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(
        std::fs::read(store.join("cas/0a/0aaa0001")).unwrap(),
        b"from-the-trunk",
        "the trunk's blob should have seeded the branch"
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("0 own + 1 inherited"),
        "inheritance should be reported: {stdout}"
    );

    // Now publish: the inherited blob is in the union, so only the new one
    // banks, and it goes to the CHILD's manifest.
    store_with(&store, &[("0ccc0009", b"branch-only")]);
    let out = run_bank(
        &api,
        &work,
        &[
            "cas-publish",
            store.to_str().unwrap(),
            "w0",
            "0",
            "branch",
            "900",
            "trunk",
        ],
    );
    assert!(out.status.success());
    let banked = banked_blobs(&work);
    assert_eq!(
        banked,
        ["0ccc0009"],
        "the inherited blob must not be re-banked"
    );
    let m = std::fs::read_to_string(work.join("bank-manifest-out/manifest.json")).unwrap();
    assert!(
        m.contains(r#""lineage":"branch""#),
        "published under the child"
    );
    assert!(m.contains(r#""parent_lineage":"trunk""#), "parent recorded");
}
