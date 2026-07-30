//! GitHub Actions concerns, kept in one place.
//!
//! The bank persists to the Actions artifact pool: cross-workflow,
//! cross-branch, 90-day retention. Everything that knows about that API
//! lives here so the rest of the bank stays store-shaped - see
//! `ci/cas-bank-design.md`.
//!
//! Two credentials, and they are not interchangeable:
//! - **listing and download** use `GITHUB_TOKEN` against the REST API,
//!   which is what the workflow's `gh api` calls have always done.
//! - **upload** uses `ACTIONS_RUNTIME_TOKEN` against the results service.
//!   The runner injects that only into JavaScript actions (probed:
//!   buck2-fixups run 30514479915), hence `actions/runtime-env`, which
//!   re-exports it into the job env.

use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

/// One artifact as the REST API reports it.
#[derive(Debug, Clone, Deserialize)]
pub struct Artifact {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub expired: bool,
    pub created_at: String,
    #[serde(default)]
    pub workflow_run: Option<WorkflowRun>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct WorkflowRun {
    #[serde(default)]
    pub head_branch: Option<String>,
    #[serde(default)]
    pub head_repository_id: Option<u64>,
    #[serde(default)]
    pub repository_id: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ArtifactPage {
    #[serde(default)]
    artifacts: Vec<Artifact>,
}

impl Artifact {
    /// Was this published by a run of `lineage`'s own branch, in this repo?
    ///
    /// The lineage is part of an artifact's NAME, and names are not
    /// namespaced - so without this a hostile branch could publish
    /// `cas-manifest-main-r0` and have the trunk seed from it. Requiring
    /// `head_repository_id == repository_id` also drops fork PRs, which
    /// have no write token and must not be able to publish under any
    /// lineage.
    /// `lineage == "-"` means "any": CONTAINERS are looked up that way,
    /// because the trust anchor is the manifest that names them, not the
    /// container artifact itself. That is the pre-existing behaviour and
    /// this port keeps it deliberately - see the nits file for why it is
    /// nonetheless worth tightening.
    pub fn provenance_ok(&self, lineage: &str) -> bool {
        if self.expired {
            return false;
        }
        if lineage == "-" {
            return true;
        }
        let Some(run) = &self.workflow_run else {
            return false;
        };
        run.head_branch.as_deref() == Some(lineage)
            && run.head_repository_id.is_some()
            && run.head_repository_id == run.repository_id
    }
}

/// Newest-first by `created_at`. The pool returns several generations of
/// the same name; the newest that passes provenance is HEAD.
pub fn newest_first(mut v: Vec<Artifact>) -> Vec<Artifact> {
    v.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    v
}

/// The REST client. One per process is plenty.
pub struct Client {
    http: reqwest::Client,
    repo: String,
    token: String,
}

impl Client {
    /// `repo` is `owner/name`; both it and the token default to the
    /// environment the runner already provides.
    pub fn from_env() -> Result<Self> {
        let repo = std::env::var("GITHUB_REPOSITORY")
            .context("GITHUB_REPOSITORY is unset - not running in Actions?")?;
        let token = std::env::var("GH_TOKEN")
            .or_else(|_| std::env::var("GITHUB_TOKEN"))
            .context("GH_TOKEN/GITHUB_TOKEN is unset")?;
        Ok(Self {
            http: reqwest::Client::builder()
                .user_agent("rebuck2-bank")
                .build()?,
            repo,
            token,
        })
    }

    async fn get(&self, url: &str) -> Result<reqwest::Response> {
        let r = self
            .http
            .get(url)
            .bearer_auth(&self.token)
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await?;
        if !r.status().is_success() {
            bail!("{url}: HTTP {}", r.status());
        }
        Ok(r)
    }

    /// Artifacts with this exact name, newest first, provenance-checked.
    pub async fn by_name(&self, name: &str, lineage: &str) -> Result<Vec<Artifact>> {
        let url = format!(
            "https://api.github.com/repos/{}/actions/artifacts?name={name}&per_page=100",
            self.repo
        );
        let page: ArtifactPage = self.get(&url).await?.json().await?;
        Ok(newest_first(
            page.artifacts
                .into_iter()
                .filter(|a| a.provenance_ok(lineage))
                .collect(),
        ))
    }

    /// Every artifact whose name starts with `prefix`, newest per name.
    ///
    /// One listing call rather than one per name: roles need no
    /// enumeration, so a matrix change cannot silently drop a slice.
    pub async fn by_prefix(&self, prefix: &str, lineage: &str) -> Result<Vec<Artifact>> {
        let url = format!(
            "https://api.github.com/repos/{}/actions/artifacts?per_page=100",
            self.repo
        );
        let page: ArtifactPage = self.get(&url).await?.json().await?;
        let mut newest: std::collections::BTreeMap<String, Artifact> = Default::default();
        for a in page
            .artifacts
            .into_iter()
            .filter(|a| a.name.starts_with(prefix) && a.provenance_ok(lineage))
        {
            match newest.get(&a.name) {
                Some(seen) if seen.created_at >= a.created_at => {}
                _ => {
                    newest.insert(a.name.clone(), a);
                }
            }
        }
        Ok(newest.into_values().collect())
    }

    /// Download an artifact and unzip it into `dest`.
    ///
    /// The REST endpoint 302s to blob storage; reqwest follows it. The zip
    /// is only ever an envelope - our payloads are already `tar.zst`, which
    /// is why the uploads set `compression-level: 0`.
    pub async fn download_to(&self, id: u64, dest: &Path) -> Result<()> {
        let url = format!(
            "https://api.github.com/repos/{}/actions/artifacts/{id}/zip",
            self.repo
        );
        let bytes = self.get(&url).await?.bytes().await?;
        if dest.exists() {
            std::fs::remove_dir_all(dest)?;
        }
        std::fs::create_dir_all(dest)?;
        unzip(&bytes, dest).with_context(|| format!("artifact {id}"))
    }
}

/// Extract a zip archive into `dest`, refusing anything that escapes it.
fn unzip(bytes: &[u8], dest: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes))?;
    for i in 0..zip.len() {
        let mut f = zip.by_index(i)?;
        // enclosed_name() rejects absolute paths and `..` - a zip is
        // remote input, and ours are not the only ones in the pool.
        let Some(rel) = f.enclosed_name() else {
            bail!("zip entry {} escapes the destination", f.name());
        };
        let out = dest.join(rel);
        if f.is_dir() {
            std::fs::create_dir_all(&out)?;
            continue;
        }
        if let Some(p) = out.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut w = std::fs::File::create(&out)?;
        std::io::copy(&mut f, &mut w)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(name: &str, branch: Option<&str>, same_repo: bool, expired: bool, at: &str) -> Artifact {
        Artifact {
            id: 1,
            name: name.into(),
            expired,
            created_at: at.into(),
            workflow_run: Some(WorkflowRun {
                head_branch: branch.map(Into::into),
                head_repository_id: Some(1),
                repository_id: Some(if same_repo { 1 } else { 2 }),
            }),
        }
    }

    #[test]
    fn provenance_demands_the_lineages_own_branch_in_this_repo() {
        assert!(art("m", Some("main"), true, false, "t").provenance_ok("main"));
        assert!(
            !art("m", Some("evil"), true, false, "t").provenance_ok("main"),
            "another branch must not publish under this lineage's name"
        );
        assert!(
            !art("m", Some("main"), false, false, "t").provenance_ok("main"),
            "a fork PR has no business publishing a lineage"
        );
        assert!(
            !art("m", Some("main"), true, true, "t").provenance_ok("main"),
            "expired artifacts are not HEAD"
        );
        let mut orphan = art("m", Some("main"), true, false, "t");
        orphan.workflow_run = None;
        assert!(!orphan.provenance_ok("main"), "no run, no provenance");
    }

    #[test]
    fn a_dash_lineage_skips_provenance_but_not_expiry() {
        // Containers are named by a manifest that IS provenance-checked.
        assert!(art("c", Some("other"), false, false, "t").provenance_ok("-"));
        assert!(
            !art("c", Some("other"), true, true, "t").provenance_ok("-"),
            "expired is still expired"
        );
    }

    #[test]
    fn newest_first_is_by_created_at() {
        let v = newest_first(vec![
            art("m", Some("main"), true, false, "2026-07-01T00:00:00Z"),
            art("m", Some("main"), true, false, "2026-07-30T00:00:00Z"),
            art("m", Some("main"), true, false, "2026-07-15T00:00:00Z"),
        ]);
        assert_eq!(v[0].created_at, "2026-07-30T00:00:00Z", "HEAD is newest");
    }

    #[test]
    fn unzip_refuses_to_escape_the_destination() {
        // A crafted entry must not write outside dest. zip's
        // enclosed_name() is the guard; assert we actually consult it.
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("../escaped.txt", opts).unwrap();
            use std::io::Write;
            w.write_all(b"nope").unwrap();
            w.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("d");
        std::fs::create_dir_all(&dest).unwrap();
        assert!(unzip(&buf, &dest).is_err(), "traversal must be refused");
        assert!(
            !dir.path().join("escaped.txt").exists(),
            "nothing may be written outside dest"
        );
    }

    #[test]
    fn unzip_round_trips_a_normal_entry() {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(std::io::Cursor::new(&mut buf));
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            w.start_file("cas-seg-x/bulk.tar.zst", opts).unwrap();
            use std::io::Write;
            w.write_all(b"payload").unwrap();
            w.finish().unwrap();
        }
        let dir = tempfile::tempdir().unwrap();
        unzip(&buf, dir.path()).unwrap();
        assert_eq!(
            std::fs::read(dir.path().join("cas-seg-x/bulk.tar.zst")).unwrap(),
            b"payload"
        );
    }
}
