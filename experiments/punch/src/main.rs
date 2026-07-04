//! Does it punch? Two GitHub runners, iroh, one 256 MB transfer.
//! Answers: can two ubuntu-latest runners hole-punch a *direct* iroh path,
//! or do they fall back to relay -- and how fast either way. That single
//! bit decides the CAS-transport design for the RE engine.
//!
//! No rendezvous service: both jobs derive their keypairs from the shared
//! GITHUB_RUN_ID, so each knows the other's EndpointId a priori; iroh's N0
//! discovery resolves the address.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use iroh::{
    endpoint::presets,
    Endpoint, EndpointId, SecretKey,
};

const ALPN: &[u8] = b"iroh-re/punch/0";
const BYTES: usize = 256 * 1024 * 1024;
const CHUNK: usize = 1024 * 1024;

fn key(run_id: &str, role: &str) -> SecretKey {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"iroh-re-punch-v1\0");
    h.update(run_id.as_bytes());
    h.update(b"\0");
    h.update(role.as_bytes());
    let seed: [u8; 32] = h.finalize().into();
    SecretKey::from_bytes(&seed)
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let role = std::env::args().nth(1).context("usage: punch <serve|fetch>")?;
    let run_id = std::env::var("GITHUB_RUN_ID").unwrap_or_else(|_| "local".into());
    match role.as_str() {
        "serve" => serve(&run_id).await,
        "fetch" => fetch(&run_id).await,
        other => anyhow::bail!("role must be serve|fetch, got {other}"),
    }
}

async fn serve(run_id: &str) -> Result<()> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(key(run_id, "A"))
        .alpns(vec![ALPN.to_vec()])
        .bind()
        .await?;
    println!("[serve] endpoint_id={}", ep.id());

    let deadline = Instant::now() + Duration::from_secs(240);
    while Instant::now() < deadline {
        let accept = tokio::time::timeout(Duration::from_secs(10), ep.accept()).await;
        let Ok(Some(incoming)) = accept else { continue };
        let conn = incoming.await?;
        println!("[serve] accepted, sending {BYTES} bytes");
        let mut send = conn.open_uni().await?;
        let buf = vec![0u8; CHUNK];
        let mut sent = 0;
        while sent < BYTES {
            send.write_all(&buf).await?;
            sent += CHUNK;
        }
        send.finish()?;
        // hold open so the receiver drains and paths can upgrade
        tokio::time::sleep(Duration::from_secs(5)).await;
        println!("[serve] done, sent {sent}");
        break;
    }
    ep.close().await;
    Ok(())
}

async fn fetch(run_id: &str) -> Result<()> {
    let ep = Endpoint::builder(presets::N0)
        .secret_key(key(run_id, "B"))
        .bind()
        .await?;
    let target: EndpointId = key(run_id, "A").public();
    println!("[fetch] endpoint_id={} target={}", ep.id(), target);

    // retry until the serve endpoint is discoverable
    let conn = {
        let deadline = Instant::now() + Duration::from_secs(180);
        loop {
            match ep.connect(target, ALPN).await {
                Ok(c) => break c,
                Err(e) if Instant::now() < deadline => {
                    println!("[fetch] connect retry: {e}");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
                Err(e) => return Err(e.into()),
            }
        }
    };
    println!("[fetch] connected, draining");
    let mut recv = conn.accept_uni().await?;
    let start = Instant::now();
    let mut total = 0usize;
    let mut buf = vec![0u8; CHUNK];
    while let Some(n) = recv.read(&mut buf).await? {
        total += n;
    }
    let secs = start.elapsed().as_secs_f64();
    let mbps = (total as f64 / 1e6) / secs;

    // path readout: direct (Ip) vs relayed
    let mut kind = "unknown";
    if let Some(info) = ep.remote_info(target).await {
        for a in info.addrs() {
            println!("[fetch] addr {:?} usage={:?}", a.addr(), a.usage());
        }
        kind = if info.addrs().any(|a| a.addr().is_ip()) {
            "has-direct"
        } else {
            "relay-only"
        };
    }
    println!("RESULT bytes={total} secs={secs:.2} MBps={mbps:.1} path={kind}");
    ep.close().await;
    Ok(())
}
