//! `rustis update`: downloads (200/304/404), fallback to previous files,
//! publishing/pruning, parallel builds identical to serial ones, hot reload.
use rustis::update::{DownloadStatus, UpdateConfig, run};
use rustis::{BuildOptions, FeedSpec, Snapshot, build_snapshot_jobs};
use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tempfile::TempDir;

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny")
}

/// The tiny fixture as a ZIP; `extra` is appended to stops.txt (a change).
fn feed_zip(extra: &str) -> Vec<u8> {
    let mut buf = std::io::Cursor::new(Vec::new());
    {
        let mut z = zip::ZipWriter::new(&mut buf);
        let opts = zip::write::SimpleFileOptions::default();
        let mut names: Vec<_> = std::fs::read_dir(fixture())
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        names.sort();
        for p in names {
            let name = p.file_name().unwrap().to_str().unwrap().to_owned();
            let mut body = std::fs::read_to_string(&p).unwrap();
            if name == "stops.txt" {
                body += extra;
            }
            z.start_file(name, opts).unwrap();
            z.write_all(body.as_bytes()).unwrap();
        }
        z.finish().unwrap();
    }
    buf.into_inner()
}

type Log = Arc<Mutex<Vec<(String, u16)>>>;
type Files = Arc<Mutex<HashMap<String, (Vec<u8>, String)>>>;

/// Minimal mirror: `GET /<name>` → 200 with ETag, 304 on a matching
/// If-None-Match, 404 for unknown names. Returns base URL and request log.
fn mirror(files: Files) -> (String, Log) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", l.local_addr().unwrap());
    let log = Arc::new(Mutex::new(Vec::new()));
    let log2 = log.clone();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let mut s = s.unwrap();
            let mut r = BufReader::new(s.try_clone().unwrap());
            let mut line = String::new();
            r.read_line(&mut line).unwrap();
            let path = line.split_whitespace().nth(1).unwrap_or("/").to_owned();
            let mut inm = None;
            loop {
                let mut h = String::new();
                if r.read_line(&mut h).unwrap() == 0 || h == "\r\n" {
                    break;
                }
                if let Some((k, v)) = h.split_once(':')
                    && k.eq_ignore_ascii_case("if-none-match")
                {
                    inm = Some(v.trim().to_owned());
                }
            }
            let name = path.trim_start_matches('/').to_owned();
            let f = files.lock().unwrap().get(&name).cloned();
            let code = match &f {
                None => {
                    write!(
                        s,
                        "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    404
                }
                Some((_, etag)) if inm.as_deref() == Some(etag.as_str()) => {
                    write!(
                        s,
                        "HTTP/1.1 304 Not Modified\r\nETag: {etag}\r\nConnection: close\r\n\r\n"
                    )
                    .unwrap();
                    304
                }
                Some((body, etag)) => {
                    write!(
                        s,
                        "HTTP/1.1 200 OK\r\nETag: {etag}\r\nLast-Modified: Fri, 13 Nov 2026 10:00:00 GMT\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    )
                    .unwrap();
                    s.write_all(body).unwrap();
                    200
                }
            };
            let _ = r.read(&mut [0u8; 0]);
            log2.lock().unwrap().push((name, code));
        }
    });
    (url, log)
}

fn cfg(root: &Path, mirror: Option<String>, feeds: &[(&str, &str)]) -> UpdateConfig {
    UpdateConfig {
        root: root.to_owned(),
        feeds: feeds
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
        mirror,
        user_agent: "rustis-test".into(),
        download_jobs: 2,
        parse_jobs: 2,
        rate_limit: 0,
        opts: BuildOptions::for_date("2026-11-16"),
        keep: 2,
        force: false,
    }
}

fn status(r: &rustis::update::UpdateReport, name: &str) -> DownloadStatus {
    r.feeds
        .iter()
        .find(|f| f.name == name)
        .unwrap()
        .status
        .clone()
}

#[test]
fn update_downloads_reuses_and_falls_back() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("root");
    let files: Files = Arc::new(Mutex::new(HashMap::new()));
    files
        .lock()
        .unwrap()
        .insert("a.zip".into(), (feed_zip(""), "\"a1\"".into()));
    files
        .lock()
        .unwrap()
        .insert("b.zip".into(), (feed_zip(""), "\"b1\"".into()));
    let (url, log) = mirror(files.clone());
    let feeds = [("a", "a.zip"), ("b", "b.zip"), ("gone", "gone.zip")];

    // 1st run: both downloaded, the missing one fails, snapshot published.
    let r = run(&cfg(&root, Some(url.clone()), &feeds)).unwrap();
    assert_eq!(status(&r, "a"), DownloadStatus::Downloaded);
    assert_eq!(status(&r, "b"), DownloadStatus::Downloaded);
    assert_eq!(status(&r, "gone"), DownloadStatus::Failed);
    let first = r.snapshot.clone().expect("published");
    let cur = rustis::update::current_snapshot(&root).unwrap();
    assert!(cur.ends_with(&first));
    let s = Snapshot::open(&cur).unwrap();
    let f = s.feeds();
    assert_eq!(f.len(), 3);
    assert_eq!(f[2]["imported"], false);
    assert!(
        s.feed_warnings(7)
            .iter()
            .any(|w| w.contains("not imported: gone"))
    );
    // Last-Modified is applied to the file (wget -N semantics)
    let mt = std::fs::metadata(root.join("feeds/a.zip"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(
        jiff::Timestamp::try_from(mt).unwrap().to_string(),
        "2026-11-13T10:00:00Z"
    );

    // 2nd run, same day, nothing changed: conditional requests, no rebuild.
    let r = run(&cfg(&root, Some(url.clone()), &feeds)).unwrap();
    assert_eq!(status(&r, "a"), DownloadStatus::Unchanged);
    assert!(r.snapshot.is_none() && r.skipped.is_some());
    assert!(
        log.lock()
            .unwrap()
            .iter()
            .filter(|(_, c)| *c == 304)
            .count()
            >= 2
    );

    // 3rd run: b changed, a's mirror is down (404) → previous file is used.
    {
        let mut f = files.lock().unwrap();
        f.insert(
            "b.zip".into(),
            (feed_zip("Z,Zed,50.1,8.1,\n"), "\"b2\"".into()),
        );
        f.remove("a.zip");
    }
    std::thread::sleep(std::time::Duration::from_millis(1100)); // new snapshot name
    let r = run(&cfg(&root, Some(url.clone()), &feeds)).unwrap();
    assert_eq!(status(&r, "a"), DownloadStatus::Stale);
    assert_eq!(status(&r, "b"), DownloadStatus::Downloaded);
    let second = r.snapshot.clone().expect("rebuilt because b changed");
    assert_ne!(first, second);
    let s = Snapshot::open(&rustis::update::current_snapshot(&root).unwrap()).unwrap();
    assert_eq!(s.feeds()[0]["imported"], true, "stale file still imported");
    assert!(
        s.feed_warnings(7)
            .iter()
            .any(|w| w.contains("could not be refreshed"))
    );
    assert!(
        s.stops_in_bbox([50.09, 8.09], [50.11, 8.11], 10, false)
            .unwrap()
            .iter()
            .any(|x| x.stop_id == "b:Z")
    );

    // 4th run (forced): pruning keeps two snapshots, never the current one.
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut c = cfg(&root, Some(url), &feeds);
    c.force = true;
    let r = run(&c).unwrap();
    assert_eq!(r.removed_snapshots, vec![first.clone()]);
    let left: Vec<_> = std::fs::read_dir(root.join("snapshots")).unwrap().collect();
    assert_eq!(left.len(), 2);
    assert!(root.join("last-update.json").exists());
}

#[test]
fn offline_update_without_files_publishes_nothing() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("root");
    let err = run(&cfg(&root, None, &[("a", "a.zip")])).unwrap_err();
    assert!(format!("{err:#}").contains("no feed could be imported"));
    assert!(rustis::update::current_snapshot(&root).is_none());
}

#[test]
fn parallel_build_is_identical_to_serial() {
    let tmp = TempDir::new().unwrap();
    let mut feeds = Vec::new();
    for i in 0..5 {
        let p = tmp.path().join(format!("f{i}.zip"));
        std::fs::write(&p, feed_zip(&format!("X{i},X {i},50.{i},8.{i},\n"))).unwrap();
        feeds.push(FeedSpec {
            name: format!("f{i}"),
            path: p,
        });
    }
    let mut opts = BuildOptions::for_date("2026-11-16");
    opts.link_radius_m = 200.0;
    let a = build_snapshot_jobs(&feeds, &tmp.path().join("serial"), opts.clone(), 1, &|_| {
        Ok(())
    })
    .unwrap();
    let b = build_snapshot_jobs(&feeds, &tmp.path().join("par"), opts, 3, &|i| {
        // later feeds become ready first
        std::thread::sleep(std::time::Duration::from_millis(50 * (5 - i as u64)));
        if i == 3 {
            Err("download failed".into())
        } else {
            Ok(())
        }
    })
    .unwrap();
    assert!(!b.feeds[3].imported);
    // apart from feed 3, every array matches a serial build without it
    let feeds_wo: Vec<_> = feeds
        .iter()
        .enumerate()
        .filter(|(i, _)| *i != 3)
        .map(|(_, f)| f.clone())
        .collect();
    let mut opts = BuildOptions::for_date("2026-11-16");
    opts.link_radius_m = 200.0;
    let c = build_snapshot_jobs(&feeds_wo, &tmp.path().join("serial-wo"), opts, 1, &|_| {
        Ok(())
    })
    .unwrap();
    let sums = |m: &rustis::SnapshotMeta| serde_json::to_string(&m.manifest).unwrap();
    assert_eq!(sums(&b), sums(&c));
    assert_ne!(sums(&a), sums(&c));
    let all3 = build_snapshot_jobs(
        &feeds,
        &tmp.path().join("par-ok"),
        {
            let mut o = BuildOptions::for_date("2026-11-16");
            o.link_radius_m = 200.0;
            o
        },
        3,
        &|_| Ok(()),
    )
    .unwrap();
    assert_eq!(sums(&a), sums(&all3));
}

#[test]
fn server_reloads_a_switched_snapshot_and_keeps_it_on_failure() {
    let tmp = TempDir::new().unwrap();
    let root = tmp.path().join("root");
    std::fs::create_dir_all(root.join("feeds")).unwrap();
    std::fs::write(root.join("feeds/a.zip"), feed_zip("")).unwrap();
    let r1 = run(&cfg(&root, None, &[("a", "a.zip")])).unwrap();
    let current = root.join("current");
    let h = rustis::http::SnapshotHandle::new(
        Snapshot::open(&current).unwrap(),
        Some(current.clone()),
        1000,
    );
    assert!(!h.reload_if_changed().unwrap());
    let held = h.get(); // a request still running on the old snapshot

    std::fs::write(root.join("feeds/a.zip"), feed_zip("Q,Que,50.2,8.2,\n")).unwrap();
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut c = cfg(&root, None, &[("a", "a.zip")]);
    c.force = true;
    let r2 = run(&c).unwrap();
    assert_ne!(r1.snapshot, r2.snapshot);
    assert!(h.reload_if_changed().unwrap());
    assert_eq!(h.get().stats().stops, held.stats().stops + 1);

    assert_eq!(held.stats().stops + 1, h.get().stats().stops);

    // a broken target: the served snapshot stays
    let broken = root.join("snapshots/broken");
    std::fs::create_dir_all(&broken).unwrap();
    let _ = std::fs::remove_file(&current);
    std::os::unix::fs::symlink("snapshots/broken", &current).unwrap();
    assert!(h.reload_if_changed().is_err());
    assert_eq!(h.get().stats().stops, held.stats().stops + 1);
}
