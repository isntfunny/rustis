//! `rustis update`: fetch the feeds of a MOTIS config from a mirror (conditional
//! requests, bounded parallelism, global rate limit) and build a new snapshot
//! while downloading — every feed is parsed as soon as its download finished.
//!
//! Layout under `root`:
//! * `feeds/<file>` + `feeds/<file>.http.json` (ETag/Last-Modified, last check)
//! * `snapshots/<YYYYMMDDTHHMMSSZ>/` immutable snapshots (+ `update.json`)
//! * `current` → symlink to the published snapshot (switched atomically);
//!   `rustis serve --snapshot <root>/current` reloads it without a restart.
//! * `update.lock` prevents concurrent runs.
//!
//! A feed whose download fails keeps its previous file (reported as stale);
//! without any file it is reported as failed. The previous snapshot stays
//! published until the new one is complete.
use crate::build::{BuildOptions, FeedSpec};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

pub struct UpdateConfig {
    pub root: PathBuf,
    /// (dataset name, file name) in import order.
    pub feeds: Vec<(String, String)>,
    /// Base URL; the file name is appended. `None` = no downloads (offline).
    pub mirror: Option<String>,
    pub user_agent: String,
    pub download_jobs: usize,
    pub parse_jobs: usize,
    /// Total download rate over all connections (bytes/s; 0 = unlimited).
    pub rate_limit: u64,
    pub opts: BuildOptions,
    /// Snapshots kept (published one included).
    pub keep: usize,
    /// Rebuild even if no feed changed and the snapshot is from today.
    pub force: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct HttpMeta {
    pub etag: Option<String>,
    pub last_modified: Option<String>,
    pub checked_at: Option<String>,
    pub downloaded_at: Option<String>,
    pub bytes: Option<u64>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DownloadStatus {
    Downloaded,
    Unchanged,
    /// Download failed; the previous file is used.
    Stale,
    /// Download failed and there is no previous file.
    Failed,
    /// No mirror configured; the existing file is used.
    Offline,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FeedStatus {
    pub name: String,
    pub file: String,
    pub status: DownloadStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Last-Modified of the file in use (mirror header or file mtime).
    pub last_modified: Option<String>,
    pub bytes: u64,
    pub seconds: f64,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct UpdateReport {
    pub started_at: String,
    pub snapshot: Option<String>,
    /// Why no snapshot was built (`up to date`), if so.
    pub skipped: Option<String>,
    pub download_seconds: f64,
    pub total_seconds: f64,
    pub downloaded_bytes: u64,
    pub counts: std::collections::BTreeMap<String, usize>,
    pub feeds: Vec<FeedStatus>,
    pub removed_snapshots: Vec<String>,
}

/// Global leaky-bucket limiter shared by all download threads.
struct RateLimit {
    rate: u64,
    next: Mutex<Instant>,
}

impl RateLimit {
    fn take(&self, n: usize) {
        if self.rate == 0 {
            return;
        }
        let wait = {
            let mut next = self.next.lock().unwrap();
            let now = Instant::now();
            // allow a burst of at most 0.5 s worth of bytes
            let floor = now.checked_sub(Duration::from_millis(500)).unwrap_or(now);
            let start = (*next).max(floor);
            *next = start + Duration::from_secs_f64(n as f64 / self.rate as f64);
            next.saturating_duration_since(now)
        };
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
    }
}

fn now_rfc3339() -> String {
    jiff::Timestamp::now()
        .round(jiff::Unit::Second)
        .unwrap()
        .to_string()
}

fn http_date(t: std::time::SystemTime) -> Option<String> {
    let ts = jiff::Timestamp::try_from(t).ok()?;
    Some(ts.strftime("%a, %d %b %Y %H:%M:%S GMT").to_string())
}

fn parse_http_date(s: &str) -> Option<std::time::SystemTime> {
    let t = jiff::fmt::strtime::parse("%a, %d %b %Y %H:%M:%S GMT", s).ok()?;
    let dt = t.to_datetime().ok()?;
    let ts = dt.to_zoned(jiff::tz::TimeZone::UTC).ok()?.timestamp();
    Some(ts.into())
}

fn meta_path(file: &Path) -> PathBuf {
    let mut p = file.as_os_str().to_owned();
    p.push(".http.json");
    p.into()
}

fn read_meta(file: &Path) -> HttpMeta {
    std::fs::read(meta_path(file))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

fn write_meta(file: &Path, m: &HttpMeta) -> Result<()> {
    let p = meta_path(file);
    let tmp = p.with_extension("tmp");
    std::fs::write(&tmp, serde_json::to_vec_pretty(m)?)?;
    std::fs::rename(tmp, p)?;
    Ok(())
}

fn mtime_header(file: &Path) -> Option<String> {
    http_date(std::fs::metadata(file).ok()?.modified().ok()?)
}

/// One conditional GET; returns (downloaded?, bytes).
fn fetch(
    agent: &ureq::Agent,
    url: &str,
    file: &Path,
    limit: &RateLimit,
) -> Result<(bool, u64, HttpMeta)> {
    let mut meta = read_meta(file);
    let exists = file.exists();
    let mut req = agent.get(url);
    if exists {
        if let Some(e) = &meta.etag {
            req = req.header("If-None-Match", e);
        }
        // like `wget -N`: the file mtime is the mirror's Last-Modified
        if let Some(lm) = meta.last_modified.clone().or_else(|| mtime_header(file)) {
            req = req.header("If-Modified-Since", lm);
        }
    }
    let resp = req.call().context("request")?;
    let status = resp.status().as_u16();
    meta.checked_at = Some(now_rfc3339());
    if status == 304 && exists {
        if meta.last_modified.is_none() {
            meta.last_modified = mtime_header(file);
        }
        return Ok((false, 0, meta));
    }
    if status != 200 {
        bail!("HTTP {status}");
    }
    let header = |k: &str| {
        resp.headers()
            .get(k)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    let (etag, lm) = (header("etag"), header("last-modified"));
    let part = file.with_extension("part");
    let mut out = std::io::BufWriter::new(std::fs::File::create(&part)?);
    let mut body = resp.into_body().into_with_config().limit(u64::MAX).reader();
    let mut buf = vec![0u8; 256 * 1024];
    let mut bytes = 0u64;
    loop {
        let n = body.read(&mut buf).context("reading body")?;
        if n == 0 {
            break;
        }
        limit.take(n);
        out.write_all(&buf[..n])?;
        bytes += n as u64;
    }
    let f = out.into_inner().map_err(|e| e.into_error())?;
    f.sync_all()?;
    if let Some(t) = lm.as_deref().and_then(parse_http_date) {
        f.set_modified(t)?;
    }
    drop(f);
    std::fs::rename(&part, file)?;
    meta.etag = etag;
    meta.last_modified = lm.or_else(|| mtime_header(file));
    meta.downloaded_at = Some(now_rfc3339());
    meta.bytes = Some(bytes);
    Ok((true, bytes, meta))
}

fn download(
    agent: &ureq::Agent,
    mirror: &str,
    name: &str,
    file_name: &str,
    file: &Path,
    limit: &RateLimit,
) -> FeedStatus {
    let t = Instant::now();
    let url = format!("{}/{}", mirror.trim_end_matches('/'), file_name);
    let mut last_err = String::new();
    for attempt in 0..3 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_secs(5 * attempt));
        }
        match fetch(agent, &url, file, limit) {
            Ok((changed, bytes, meta)) => {
                let _ = write_meta(file, &meta);
                return FeedStatus {
                    name: name.into(),
                    file: file_name.into(),
                    status: if changed {
                        DownloadStatus::Downloaded
                    } else {
                        DownloadStatus::Unchanged
                    },
                    error: None,
                    last_modified: meta.last_modified,
                    bytes,
                    seconds: t.elapsed().as_secs_f64(),
                };
            }
            Err(e) => {
                last_err = format!("{e:#}");
                // a 404 will not go away by retrying
                if last_err.contains("HTTP 404") {
                    break;
                }
            }
        }
    }
    let _ = std::fs::remove_file(file.with_extension("part"));
    let exists = file.exists();
    FeedStatus {
        name: name.into(),
        file: file_name.into(),
        status: if exists {
            DownloadStatus::Stale
        } else {
            DownloadStatus::Failed
        },
        error: Some(last_err),
        last_modified: read_meta(file).last_modified.or_else(|| mtime_header(file)),
        bytes: 0,
        seconds: t.elapsed().as_secs_f64(),
    }
}

fn offline_status(name: &str, file_name: &str, file: &Path) -> FeedStatus {
    let exists = file.exists();
    FeedStatus {
        name: name.into(),
        file: file_name.into(),
        status: if exists {
            DownloadStatus::Offline
        } else {
            DownloadStatus::Failed
        },
        error: (!exists).then(|| "file missing (offline)".to_owned()),
        last_modified: read_meta(file).last_modified.or_else(|| mtime_header(file)),
        bytes: 0,
        seconds: 0.0,
    }
}

/// The snapshot `root/current` points to, if any.
pub fn current_snapshot(root: &Path) -> Option<PathBuf> {
    std::fs::canonicalize(root.join("current")).ok()
}

fn publish(root: &Path, snap: &Path) -> Result<()> {
    let rel = Path::new("snapshots").join(snap.file_name().context("snapshot name")?);
    let tmp = root.join("current.tmp");
    let _ = std::fs::remove_file(&tmp);
    std::os::unix::fs::symlink(&rel, &tmp)?;
    std::fs::rename(&tmp, root.join("current")).context("switching current")?;
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

/// Removes the oldest snapshots so that `keep` remain (never the current one).
fn prune(root: &Path, keep: usize) -> Result<Vec<String>> {
    let current = current_snapshot(root);
    let mut snaps: Vec<PathBuf> = std::fs::read_dir(root.join("snapshots"))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.join("snapshot.json").exists())
        .collect();
    snaps.sort();
    let mut removed = Vec::new();
    while snaps.len() > keep.max(1) {
        let p = snaps.remove(0);
        if Some(&p) == current.as_ref() {
            continue;
        }
        make_writable(&p)?;
        std::fs::remove_dir_all(&p).with_context(|| format!("removing {}", p.display()))?;
        removed.push(p.file_name().unwrap().to_string_lossy().into_owned());
    }
    Ok(removed)
}

fn make_writable(dir: &Path) -> Result<()> {
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        let mut perm = std::fs::metadata(&p)?.permissions();
        #[allow(clippy::permissions_set_readonly_false)]
        perm.set_readonly(false);
        std::fs::set_permissions(&p, perm)?;
    }
    Ok(())
}

/// Runs one update. Returns the report (also written to the new snapshot as
/// `update.json` and to `root/last-update.json`).
pub fn run(cfg: &UpdateConfig) -> Result<UpdateReport> {
    let t_start = Instant::now();
    let started_at = now_rfc3339();
    let root = &cfg.root;
    let feed_dir = root.join("feeds");
    let snap_dir = root.join("snapshots");
    std::fs::create_dir_all(&feed_dir)?;
    std::fs::create_dir_all(&snap_dir)?;
    let lock = std::fs::File::create(root.join("update.lock"))?;
    if lock.try_lock().is_err() {
        bail!(
            "another update is running ({})",
            root.join("update.lock").display()
        );
    }
    // remove leftovers of an interrupted run
    for e in std::fs::read_dir(&snap_dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "staging") {
            make_writable(&p)?;
            std::fs::remove_dir_all(&p)?;
        }
    }
    let n = cfg.feeds.len();
    let files: Vec<PathBuf> = cfg.feeds.iter().map(|(_, f)| feed_dir.join(f)).collect();
    let specs: Vec<FeedSpec> = cfg
        .feeds
        .iter()
        .zip(&files)
        .map(|((name, _), path)| FeedSpec {
            name: name.clone(),
            path: path.clone(),
        })
        .collect();

    // Downloads run on their own threads; the build waits per feed.
    let state: (Mutex<Vec<Option<FeedStatus>>>, Condvar) =
        (Mutex::new(vec![None; n]), Condvar::new());
    let limit = RateLimit {
        rate: cfg.rate_limit,
        next: Mutex::new(Instant::now()),
    };
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .user_agent(cfg.user_agent.as_str())
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_recv_body(Some(Duration::from_secs(3600)))
        .build()
        .into();
    let wait = |i: usize| -> FeedStatus {
        let mut s = state.0.lock().unwrap();
        loop {
            if let Some(st) = &s[i] {
                return st.clone();
            }
            s = state.1.wait(s).unwrap();
        }
    };
    let current = current_snapshot(root);
    let current_first_day = current.as_ref().and_then(|c| {
        let m: serde_json::Value =
            serde_json::from_slice(&std::fs::read(c.join("snapshot.json")).ok()?).ok()?;
        Some(m.get("first_day")?.as_str()?.to_owned())
    });
    // Same day and same snapshot date: rebuild only if a feed changed, so the
    // downloads have to finish before deciding.
    let decide_later =
        !cfg.force && current_first_day.as_deref() == Some(cfg.opts.first_day.to_string().as_str());
    let next_dl = std::sync::atomic::AtomicUsize::new(0);
    let download_done = Mutex::new(None::<f64>);
    let finished = std::sync::atomic::AtomicUsize::new(0);
    let mut result: Result<Option<(PathBuf, crate::SnapshotMeta)>> = Ok(None);
    let mut skipped = None;
    std::thread::scope(|sc| {
        let jobs = if cfg.mirror.is_some() {
            cfg.download_jobs.max(1)
        } else {
            1
        };
        for _ in 0..jobs {
            let (state, limit, agent, files, next_dl, finished, download_done) = (
                &state,
                &limit,
                &agent,
                &files,
                &next_dl,
                &finished,
                &download_done,
            );
            sc.spawn(move || {
                loop {
                    let i = next_dl.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if i >= n {
                        break;
                    }
                    let (name, file_name) = &cfg.feeds[i];
                    let st = match &cfg.mirror {
                        Some(m) => download(agent, m, name, file_name, &files[i], limit),
                        None => offline_status(name, file_name, &files[i]),
                    };
                    match st.status {
                        DownloadStatus::Downloaded => eprintln!(
                            "fetch {name}: downloaded {:.1} MB in {:.1}s",
                            st.bytes as f64 / 1e6,
                            st.seconds
                        ),
                        DownloadStatus::Stale | DownloadStatus::Failed => eprintln!(
                            "fetch {name}: {:?} ({})",
                            st.status,
                            st.error.as_deref().unwrap_or("")
                        ),
                        _ => {}
                    }
                    state.0.lock().unwrap()[i] = Some(st);
                    state.1.notify_all();
                    if finished.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1 == n {
                        *download_done.lock().unwrap() = Some(t_start.elapsed().as_secs_f64());
                    }
                }
            });
        }
        if decide_later {
            let changed = (0..n).any(|i| {
                let st = wait(i);
                st.status == DownloadStatus::Downloaded
            });
            if !changed {
                skipped = Some(format!(
                    "up to date: no feed changed and the published snapshot starts {}",
                    cfg.opts.first_day
                ));
                return;
            }
        }
        let name = jiff::Timestamp::now()
            .strftime("%Y%m%dT%H%M%SZ")
            .to_string();
        let dest = snap_dir.join(&name);
        let ready = |i: usize| -> Result<(), String> {
            let st = wait(i);
            match st.status {
                DownloadStatus::Failed => {
                    Err(format!("no feed file: {}", st.error.unwrap_or_default()))
                }
                _ => Ok(()),
            }
        };
        result = crate::engine::build_snapshot_jobs(
            &specs,
            &dest,
            cfg.opts.clone(),
            cfg.parse_jobs,
            &ready,
        )
        .map(|m| Some((dest, m)));
    });
    let feeds: Vec<FeedStatus> = state
        .0
        .into_inner()
        .unwrap()
        .into_iter()
        .map(|s| s.expect("all downloads finished"))
        .collect();
    let mut counts = std::collections::BTreeMap::new();
    for f in &feeds {
        *counts
            .entry(
                serde_json::to_value(&f.status)?
                    .as_str()
                    .unwrap()
                    .to_owned(),
            )
            .or_insert(0) += 1;
    }
    let mut report = UpdateReport {
        started_at,
        snapshot: None,
        skipped,
        download_seconds: download_done.into_inner().unwrap().unwrap_or(0.0),
        total_seconds: 0.0,
        downloaded_bytes: feeds.iter().map(|f| f.bytes).sum(),
        counts,
        feeds,
        removed_snapshots: Vec::new(),
    };
    if let Some((dest, meta)) = result? {
        let imported = meta.feeds.iter().filter(|f| f.imported).count();
        if imported == 0 {
            bail!(
                "no feed could be imported; {} stays published",
                root.join("current").display()
            );
        }
        report.snapshot = Some(dest.file_name().unwrap().to_string_lossy().into_owned());
        // sanity check before switching: the server must be able to open it
        crate::Snapshot::open(&dest).context("opening the new snapshot")?;
        report.total_seconds = t_start.elapsed().as_secs_f64();
        std::fs::write(
            dest.join("update.json"),
            serde_json::to_vec_pretty(&report)?,
        )?;
        publish(root, &dest)?;
        report.removed_snapshots = prune(root, cfg.keep)?;
    }
    report.total_seconds = t_start.elapsed().as_secs_f64();
    std::fs::write(
        root.join("last-update.json"),
        serde_json::to_vec_pretty(&report)?,
    )?;
    Ok(report)
}
