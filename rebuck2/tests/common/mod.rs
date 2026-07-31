//! Shared stub for the artifact API, plus the fixture builders that go
//! with it.
//!
//! The bank's choreography lives in the engine now, so its tests drive the
//! real client, the real zip reader and the real ordering - only the
//! network is stubbed. `GITHUB_API_URL` is the seam, and GitHub sets it on
//! every runner anyway.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// One artifact the stub will serve: its metadata row and its zip bytes.
pub struct Stub {
    pub name: String,
    pub branch: String,
    pub created_at: String,
    pub zip: Vec<u8>,
}

/// Zip a set of (path, bytes) with no compression - the same envelope
/// upload-artifact writes at compression-level 0.
pub fn zip_of(entries: &[(&str, Vec<u8>)]) -> Vec<u8> {
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
pub async fn serve(stubs: Vec<Stub>) -> String {
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
/// `prefixes` is the segment's bitmap: the first hex char of every blob
/// it holds. The CAS restore matches on it, so a segment claiming "*"
/// (what the AC uses, since its restore is whole-fetch) is invisible to a
/// range owner asking for "01".
#[allow(clippy::too_many_arguments)]
pub fn manifest_zip_with_prefixes(
    lineage: &str,
    generation: &str,
    run: u64,
    role: &str,
    segment: &str,
    container: &str,
    rows: &[&str],
    prefixes: &str,
) -> Vec<u8> {
    let manifest = format!(
        r#"{{"version":1,"lineage":"{lineage}","generation":"{generation}",
            "parent_lineage":null,"parent_generation":null,"created_by_run":{run},
            "segments":[{{"name":"{segment}","bytes":1,"blobs":{},"prefixes":"{prefixes}",
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
pub fn container_zip(segment: &str, rows: &[(&str, &[u8])]) -> Vec<u8> {
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
pub fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_rebuck2"))
}

pub fn rebuck2_tar(store: &Path, paths: &[String], out: &Path) {
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

pub fn rebuck2_zstd_write(path: &Path, lines: &[String]) {
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

/// The AC's shape: whole-fetch, so its segments claim every prefix.
#[allow(clippy::too_many_arguments)]
pub fn manifest_zip(
    lineage: &str,
    generation: &str,
    run: u64,
    role: &str,
    segment: &str,
    container: &str,
    rows: &[&str],
) -> Vec<u8> {
    manifest_zip_with_prefixes(
        lineage, generation, run, role, segment, container, rows, "*",
    )
}
