//! End-to-end AC restore against a stub artifact API.
//!
//! What this pins is the thing that is easy to get wrong and expensive to
//! get wrong: rows are name-stable but content-MUTABLE, so the apply order
//! decides which result the driver serves.

mod common;
use common::*;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cold_bank_exits_three() {
    let api = serve(vec![]).await;
    let dir = tempfile::tempdir().unwrap();
    let out = bin()
        .args(["bank", "ac-restore"])
        .arg(dir.path().join("store"))
        .args(["driver", "all", "some-lineage", "-"])
        .env("GITHUB_API_URL", &api)
        .env("GITHUB_REPOSITORY", "o/r")
        .env("GH_TOKEN", "t")
        .env("BANK_WORK", dir.path().join("work"))
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3), "cold bank must exit 3");
    assert!(String::from_utf8_lossy(&out.stdout).contains("cold bank"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parent_rows_land_under_the_child_whatever_the_run_ids_say() {
    // The trap: the parent published LATER (run 1002) than the child
    // (run 900). Ordering by run alone serves the trunk's row to the
    // branch that was built on top of it.
    let row = "ac/aaaa";
    let parent_seg = "cas-seg-parent";
    let child_seg = "cas-seg-child";

    let api = serve(vec![
        Stub {
            name: "cas-manifest-trunk-ac-driver".into(),
            branch: "trunk".into(),
            created_at: "2026-07-30T00:00:00Z".into(),
            zip: manifest_zip(
                "trunk",
                "1002-1",
                1002,
                "driver",
                parent_seg,
                "container-parent",
                &[&format!("{row} parenthash")],
            ),
        },
        Stub {
            name: "cas-manifest-branch-ac-driver".into(),
            branch: "branch".into(),
            created_at: "2026-07-29T00:00:00Z".into(),
            zip: manifest_zip(
                "branch",
                "900-1",
                900,
                "driver",
                child_seg,
                "container-child",
                &[&format!("{row} childhash")],
            ),
        },
        Stub {
            name: "container-parent".into(),
            branch: "trunk".into(),
            created_at: "2026-07-30T00:00:00Z".into(),
            zip: container_zip(parent_seg, &[(row, b"from-the-trunk")]),
        },
        Stub {
            name: "container-child".into(),
            branch: "branch".into(),
            created_at: "2026-07-29T00:00:00Z".into(),
            zip: container_zip(child_seg, &[(row, b"from-the-branch")]),
        },
    ])
    .await;

    let dir = tempfile::tempdir().unwrap();
    let store = dir.path().join("store");
    let work = dir.path().join("work");
    let out = bin()
        .args(["bank", "ac-restore"])
        .arg(&store)
        .args(["driver", "all", "branch", "trunk"])
        .env("GITHUB_API_URL", &api)
        .env("GITHUB_REPOSITORY", "o/r")
        .env("GH_TOKEN", "t")
        .env("BANK_WORK", &work)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "restore failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        std::fs::read(store.join(row)).unwrap(),
        b"from-the-branch",
        "the child's row must win even though the trunk published later"
    );

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("(1 inherited)"),
        "the parent manifest should be reported as inherited: {stdout}"
    );

    // Both rows join the diff base, so neither is re-banked next publish.
    let banked = std::fs::read_to_string(work.join("ac-banked-rows.txt")).unwrap();
    assert!(
        banked.contains("parenthash"),
        "parent rows must be banked-known"
    );
    assert!(
        banked.contains("childhash"),
        "child rows must be banked-known"
    );

    // The own-role head is the CHILD's, never the parent's - publish
    // chains its generation from this.
    let head = std::fs::read_to_string(work.join("own-ac/manifest.json")).unwrap();
    assert!(
        head.contains(r#""lineage":"branch""#),
        "publish head must be this lineage's: {head}"
    );
}
