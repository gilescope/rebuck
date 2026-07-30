//! End-to-end AC restore against a stub artifact API.
//!
//! The choreography used to live in bash and was tested by a fake `gh` on
//! `PATH`. Now that it is in the engine, the seam is `GITHUB_API_URL` -
//! which GitHub sets on every runner anyway - and the test can drive the
//! real client, the real zip reader and the real ordering rules.
//!
//! What this pins is the thing that is easy to get wrong and expensive to
//! get wrong: rows are name-stable but content-MUTABLE, so the apply order
//! decides which result the driver serves.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One artifact the stub will serve: its metadata row and its zip bytes.
struct Stub {
    name: String,
    branch: String,
    created_at: String,
    zip: Vec<u8>,
}

/// Zip a set of (path, bytes) with no compression - the same envelope
/// upload-artifact writes at compression-level 0.
fn zip_of(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
        let opts: zip::write::FileOptions<'_, ()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in entries {
            w.start_file(*name, opts).unwrap();
            w.write_all(bytes).unwrap();
        }
        w.finish().unwrap();
    }
    buf
}

/// Minimal HTTP/1.1 server speaking just enough of the artifacts API.
async fn serve(stubs: Vec<Stub>) -> String {
    let by_id: Arc<HashMap<usize, Vec<u8>>> = Arc::new(
        stubs
            .iter()
            .enumerate()
            .map(|(i, s)| (i + 1, s.zip.clone()))
            .collect(),
    );
    let rows: Arc<Vec<String>> = Arc::new(
        stubs
            .iter()
            .enumerate()
            .map(|(i, s)| {
                format!(
                    r#"{{"id":{},"name":"{}","expired":false,"created_at":"{}",
                        "workflow_run":{{"head_branch":"{}","head_repository_id":1,
                        "repository_id":1}}}}"#,
                    i + 1,
                    s.name,
                    s.created_at,
                    s.branch
                )
            })
            .collect(),
    );

    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = l.accept().await else {
                return;
            };
            let (by_id, rows) = (by_id.clone(), rows.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]).to_string();
                let path = req.split_whitespace().nth(1).unwrap_or("/").to_owned();

                let (ctype, body): (&str, Vec<u8>) = if path.contains("/zip") {
                    let id: usize = path
                        .rsplit('/')
                        .nth(1)
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0);
                    (
                        "application/zip",
                        by_id.get(&id).cloned().unwrap_or_default(),
                    )
                } else {
                    // ?name=X filters; bare listing returns everything.
                    let want = path
                        .split("name=")
                        .nth(1)
                        .map(|s| s.split('&').next().unwrap_or("").to_owned());
                    let sel: Vec<&String> = rows
                        .iter()
                        .filter(|r| match &want {
                            Some(w) => r.contains(&format!(r#""name":"{w}""#)),
                            None => true,
                        })
                        .collect();
                    let json = format!(
                        r#"{{"artifacts":[{}]}}"#,
                        sel.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(",")
                    );
                    ("application/json", json.into_bytes())
                };
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = sock.write_all(head.as_bytes()).await;
                let _ = sock.write_all(&body).await;
                let _ = sock.shutdown().await;
            });
        }
    });
    format!("http://{addr}")
}

/// Build a manifest artifact's zip: manifest.json + the row list.
fn manifest_zip(
    lineage: &str,
    generation: &str,
    run: u64,
    role: &str,
    segment: &str,
    container: &str,
    rows: &[&str],
) -> Vec<u8> {
    let manifest = format!(
        r#"{{"version":1,"lineage":"{lineage}","generation":"{generation}",
            "parent_lineage":null,"parent_generation":null,"created_by_run":{run},
            "segments":[{{"name":"{segment}","bytes":1,"blobs":{},"prefixes":"*",
            "artifact":"{container}","run":{run},"role":"{role}"}}]}}"#,
        rows.len()
    );
    let tmp = tempfile::tempdir().unwrap();
    let list = tmp.path().join("blobs.txt.zst");
    let owned: Vec<String> = rows.iter().map(|s| (*s).to_owned()).collect();
    rebuck2_zstd_write(&list, &owned);
    zip_of(&[
        ("manifest.json", manifest.into_bytes()),
        ("blobs.txt.zst", std::fs::read(&list).unwrap()),
    ])
}

/// A container zip holding one segment dir with a tar.zst of AC rows.
fn container_zip(segment: &str, rows: &[(&str, &[u8])]) -> Vec<u8> {
    let tmp = tempfile::tempdir().unwrap();
    let store = tmp.path().join("s");
    let mut paths = Vec::new();
    for (rel, body) in rows {
        let p = store.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, body).unwrap();
        paths.push((*rel).to_owned());
    }
    let raw = tmp.path().join("bulk.tar");
    rebuck2_tar(&store, &paths, &raw);
    let zst = tmp.path().join("bulk.tar.zst");
    assert!(std::process::Command::new("zstd")
        .args(["-q", "-f"])
        .arg(&raw)
        .arg("-o")
        .arg(&zst)
        .status()
        .unwrap()
        .success());
    zip_of(&[(
        &format!("{segment}/bulk.tar.zst"),
        std::fs::read(&zst).unwrap(),
    )])
}

// The crate is a binary, so the test drives it through its CLI - which is
// also what CI does, so the surface under test is the real one.
fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_rebuck2"))
}

fn rebuck2_tar(store: &Path, paths: &[String], out: &Path) {
    let batch = out.with_extension("batch");
    std::fs::write(&batch, paths.join("\n") + "\n").unwrap();
    assert!(bin()
        .args(["bank", "tar"])
        .arg(store)
        .arg(&batch)
        .arg(out)
        .status()
        .unwrap()
        .success());
}

fn rebuck2_zstd_write(path: &Path, lines: &[String]) {
    let plain = path.with_extension("plain");
    std::fs::write(&plain, lines.join("\n") + "\n").unwrap();
    assert!(std::process::Command::new("zstd")
        .args(["-q", "-f"])
        .arg(&plain)
        .arg("-o")
        .arg(path)
        .status()
        .unwrap()
        .success());
}

// multi_thread, deliberately: the test blocks on a subprocess that HTTP
// requests the stub server. On the default current-thread runtime the
// accept loop never gets polled while Command::output() blocks, and the
// whole thing deadlocks.
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
