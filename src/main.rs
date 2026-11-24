use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rustis::{BuildOptions, Endpoint, FeedSpec, PlanRequest, Snapshot, build_snapshot_jobs};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(
    version,
    about = "Disk-backed GTFS routing (RAPTOR with lower-bound pruning over mmap snapshots)"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(clap::Args)]
struct BuildArgs {
    /// First service day YYYY-MM-DD (default: today).
    #[arg(long)]
    first_day: Option<String>,
    #[arg(long, default_value_t = 366)]
    horizon_days: u32,
    /// Generate footpaths between stops closer than this (0 = off); longer
    /// walks are chained over intermediate stops up to --max-footpath-secs.
    #[arg(long, default_value_t = 300.0)]
    link_radius_m: f64,
    #[arg(long, default_value_t = 1.2)]
    walk_speed: f64,
    #[arg(long, default_value_t = 720)]
    max_footpath_secs: u32,
    /// Minimum change time (s) at stops without a same-stop transfers.txt
    /// rule; walking on to another stop takes at least this long, too.
    #[arg(long, default_value_t = 120)]
    default_change_seconds: u32,
    /// Treat pickup/drop_off 2/3 (booking) as not available.
    #[arg(long)]
    exclude_booking_stops: bool,
    /// Comparison mode: footpaths/change times built like MOTIS (nigiri).
    #[arg(long)]
    motis_footpaths: bool,
}

impl BuildArgs {
    fn options(&self) -> Result<BuildOptions> {
        let first_day = match &self.first_day {
            Some(d) => jiff::civil::Date::strptime("%Y-%m-%d", d)?,
            None => jiff::Zoned::now().date(),
        };
        let mut opts = BuildOptions::new(first_day);
        opts.horizon_days = self.horizon_days;
        opts.link_radius_m = self.link_radius_m;
        opts.walk_speed_m_per_s = self.walk_speed;
        opts.max_footpath_secs = self.max_footpath_secs;
        opts.default_change_seconds = self.default_change_seconds;
        opts.exclude_booking_stops = self.exclude_booking_stops;
        opts.motis_footpaths = self.motis_footpaths;
        Ok(opts)
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Build a snapshot from GTFS feeds (directories or ZIPs).
    Build {
        /// A feed; repeatable. With a single --gtfs, ids stay unprefixed;
        /// otherwise they are namespaced by file stem.
        #[arg(long)]
        gtfs: Vec<PathBuf>,
        /// MOTIS config.yml: import all of its datasets (namespace = dataset name).
        #[arg(long)]
        motis_config: Option<PathBuf>,
        /// Directory holding the dataset files (with --motis-config).
        #[arg(long)]
        feed_dir: Option<PathBuf>,
        /// Only datasets whose name matches one of these prefixes (development).
        #[arg(long)]
        only: Vec<String>,
        #[arg(long)]
        out: PathBuf,
        /// Feeds parsed in parallel (committed in order; more jobs = more RAM).
        #[arg(long, default_value_t = 1)]
        jobs: usize,
        #[command(flatten)]
        build: BuildArgs,
    },
    /// Fetch the feeds of a MOTIS config from a mirror and publish a new
    /// snapshot under ROOT/current (served ones reload it without restart).
    Update {
        /// Data root: feeds/, snapshots/, current -> snapshots/<id>.
        #[arg(long)]
        root: PathBuf,
        /// MOTIS config.yml listing the datasets (file names are taken from `path`).
        #[arg(long)]
        motis_config: PathBuf,
        /// Mirror base URL (file name appended).
        #[arg(long, default_value = "https://api.transitous.org/gtfs")]
        mirror: String,
        /// Do not download; build from the files present in ROOT/feeds.
        #[arg(long)]
        offline: bool,
        #[arg(long, default_value = concat!("rustis/", env!("CARGO_PKG_VERSION")))]
        user_agent: String,
        #[arg(long, default_value_t = 4)]
        download_jobs: usize,
        /// Feeds parsed in parallel while downloading.
        #[arg(long, default_value_t = 2)]
        jobs: usize,
        /// Total download rate in MB/s over all connections (Transitous asks for 30).
        #[arg(long, default_value_t = 30.0)]
        rate_limit_mb: f64,
        /// Snapshots kept including the published one.
        #[arg(long, default_value_t = 2)]
        keep: usize,
        /// Build even if nothing changed today.
        #[arg(long)]
        force: bool,
        /// Exit with code 3 (after publishing) if more feeds than this could not
        /// be downloaded or imported.
        #[arg(long, default_value_t = 100)]
        max_failed: usize,
        #[command(flatten)]
        build: BuildArgs,
    },
    /// Plan a journey against a snapshot.
    Plan {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long)]
        date: String,
        /// Earliest departure HH:MM[:SS]; hours ≥ 24 address the next morning.
        #[arg(long)]
        at: String,
        /// Origin stop, optionally `STOP_ID@walk_seconds`; repeatable.
        #[arg(long, required = true)]
        from: Vec<String>,
        /// Destination stop, optionally `STOP_ID@walk_seconds`; repeatable.
        #[arg(long, required = true)]
        to: Vec<String>,
        #[arg(long, default_value_t = 3)]
        max_transfers: u8,
        #[arg(long, default_value_t = 3)]
        max_results: usize,
        #[arg(long, default_value_t = 86_400)]
        max_travel_seconds: u32,
    },
    /// List served stops inside a bounding box.
    Stops {
        #[arg(long)]
        snapshot: PathBuf,
        /// min_lat,min_lon,max_lat,max_lon
        #[arg(long)]
        bbox: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        served_only: bool,
    },
    /// Print snapshot statistics and this process's memory.
    Stats {
        #[arg(long)]
        snapshot: PathBuf,
    },
    /// Full checksum and ordering check of a snapshot.
    Verify {
        #[arg(long)]
        snapshot: PathBuf,
    },
    /// Read-only inventory of all datasets of a MOTIS config (JSON lines).
    Inventory {
        #[arg(long)]
        config: PathBuf,
        #[arg(long)]
        feeds: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[arg(long, default_value_t = 2)]
        jobs: usize,
    },
    /// Serve the HTTP API.
    Serve {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8088")]
        listen: std::net::SocketAddr,
        #[arg(long, default_value_t = 2)]
        max_concurrency: usize,
        /// Optional per-search label cap (default: none). Only for operators who
        /// must bound per-query memory/time; capped searches may miss journeys.
        #[arg(long, default_value_t = rustis::DEFAULT_MAX_LABELS)]
        max_labels: usize,
        /// Check every N seconds whether --snapshot (e.g. ROOT/current, a
        /// symlink switched by `rustis update`) points to a new snapshot and
        /// switch to it without restart; SIGHUP checks immediately. 0 = off.
        #[arg(long, default_value_t = 60)]
        reload_interval: u64,
    },
}

fn parse_time(s: &str) -> Result<u32> {
    let parts: Vec<u32> = s
        .split(':')
        .map(str::parse)
        .collect::<Result<_, _>>()
        .context("time must be HH:MM[:SS]")?;
    match parts[..] {
        [h, m] if m < 60 => Ok(h * 3600 + m * 60),
        [h, m, sec] if m < 60 && sec < 60 => Ok(h * 3600 + m * 60 + sec),
        _ => bail!("time must be HH:MM[:SS]"),
    }
}

fn endpoint(s: &str) -> Result<Endpoint> {
    Ok(match s.rsplit_once('@') {
        Some((id, w)) => Endpoint {
            stop_id: id.into(),
            walk_seconds: w.parse().context("walk seconds")?,
        },
        None => Endpoint {
            stop_id: s.into(),
            walk_seconds: 0,
        },
    })
}

fn print(v: &impl serde::Serialize) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Build {
            gtfs,
            motis_config,
            feed_dir,
            only,
            out,
            jobs,
            build,
        } => {
            let mut feeds: Vec<FeedSpec> = Vec::new();
            let mut opts = build.options()?;
            if let Some(cfg) = &motis_config {
                opts.scripts = rustis::inventory::motis_scripts(
                    &std::fs::read_to_string(cfg)?,
                    cfg.parent().unwrap_or(std::path::Path::new(".")),
                );
            }
            if let Some(cfg) = motis_config {
                let dir = feed_dir.context("--feed-dir is required with --motis-config")?;
                for (name, path) in
                    rustis::inventory::motis_datasets(&std::fs::read_to_string(cfg)?)
                {
                    if only.is_empty() || only.iter().any(|o| name.starts_with(o.as_str())) {
                        let file = std::path::Path::new(&path)
                            .file_name()
                            .context("dataset path")?
                            .to_owned();
                        feeds.push(FeedSpec {
                            name,
                            path: dir.join(file),
                        });
                    }
                }
            }
            let single = gtfs.len() == 1 && feeds.is_empty();
            for g in gtfs {
                let name = if single {
                    String::new()
                } else {
                    g.file_stem()
                        .map(|s| s.to_string_lossy().trim_end_matches(".gtfs").to_owned())
                        .unwrap_or_default()
                };
                feeds.push(FeedSpec { name, path: g });
            }
            if feeds.is_empty() {
                bail!("no feeds given (--gtfs or --motis-config)");
            }
            let m = build_snapshot_jobs(&feeds, &out, opts, jobs, &|_| Ok(()))?;
            let failed: Vec<_> = m.feeds.iter().filter(|f| !f.imported).collect();
            println!(
                "built {}: {} feeds ({} failed), {} stops, {} routes, {} trips, {} bitfields, {} footpaths, {} specific rules in {:.0}s",
                out.display(),
                m.feeds.len(),
                failed.len(),
                m.n_stops,
                m.n_routes,
                m.n_trips,
                m.n_bitfields,
                m.n_footpaths,
                m.n_specific_rules,
                m.build_seconds
            );
            for f in failed {
                println!("failed: {} ({})", f.name, f.error.as_deref().unwrap_or(""));
            }
            for w in &m.warnings {
                println!("warning: {w}");
            }
            if let Some(mem) = rustis::memory::ProcessMemory::current() {
                println!("build peak RSS (VmHWM): {} MiB", mem.hwm_kib / 1024);
            }
        }
        Cmd::Update {
            root,
            motis_config,
            mirror,
            offline,
            user_agent,
            download_jobs,
            jobs,
            rate_limit_mb,
            keep,
            force,
            max_failed,
            build,
        } => {
            let feeds = rustis::inventory::motis_datasets(&std::fs::read_to_string(&motis_config)?)
                .into_iter()
                .map(|(name, path)| {
                    let file = std::path::Path::new(&path)
                        .file_name()
                        .context("dataset path")?
                        .to_string_lossy()
                        .into_owned();
                    Ok((name, file))
                })
                .collect::<Result<Vec<_>>>()?;
            if feeds.is_empty() {
                bail!("no datasets in {}", motis_config.display());
            }
            let mut opts = build.options()?;
            opts.scripts = rustis::inventory::motis_scripts(
                &std::fs::read_to_string(&motis_config)?,
                motis_config.parent().unwrap_or(std::path::Path::new(".")),
            );
            let cfg = rustis::update::UpdateConfig {
                root,
                feeds,
                mirror: (!offline).then_some(mirror),
                user_agent,
                download_jobs,
                parse_jobs: jobs,
                rate_limit: (rate_limit_mb * 1e6) as u64,
                opts,
                keep,
                force,
            };
            let r = rustis::update::run(&cfg)?;
            let failed_dl = r.feeds.iter().filter(|f| {
                matches!(
                    f.status,
                    rustis::update::DownloadStatus::Stale | rustis::update::DownloadStatus::Failed
                )
            });
            let failed_dl: Vec<_> = failed_dl.collect();
            println!(
                "update: {:?}, {:.1} MB downloaded, downloads {:.0}s, total {:.0}s",
                r.counts,
                r.downloaded_bytes as f64 / 1e6,
                r.download_seconds,
                r.total_seconds
            );
            for f in &failed_dl {
                println!(
                    "  download {:?}: {} ({})",
                    f.status,
                    f.name,
                    f.error.as_deref().unwrap_or("")
                );
            }
            let mut failed_import = 0;
            match (&r.snapshot, &r.skipped) {
                (Some(s), _) => {
                    let m: rustis::SnapshotMeta = serde_json::from_slice(&std::fs::read(
                        cfg.root.join("snapshots").join(s).join("snapshot.json"),
                    )?)?;
                    for f in m.feeds.iter().filter(|f| !f.imported) {
                        failed_import += 1;
                        println!(
                            "  import failed: {} ({})",
                            f.name,
                            f.error.as_deref().unwrap_or("")
                        );
                    }
                    for w in &m.warnings {
                        println!("  warning: {w}");
                    }
                    println!(
                        "published snapshots/{s}: {} stops, {} trips, build {:.0}s; removed {:?}",
                        m.n_stops, m.n_trips, m.build_seconds, r.removed_snapshots
                    );
                }
                (None, Some(why)) => println!("{why}"),
                (None, None) => {}
            }
            if let Some(mem) = rustis::memory::ProcessMemory::current() {
                println!("update peak RSS (VmHWM): {} MiB", mem.hwm_kib / 1024);
            }
            if failed_dl.len() + failed_import > max_failed {
                eprintln!("more than {max_failed} feeds failed");
                std::process::exit(3);
            }
        }
        Cmd::Plan {
            snapshot,
            date,
            at,
            from,
            to,
            max_transfers,
            max_results,
            max_travel_seconds,
        } => {
            let s = Snapshot::open(&snapshot)?;
            let req = PlanRequest {
                date,
                departure: parse_time(&at)?,
                from: from.iter().map(|e| endpoint(e)).collect::<Result<_>>()?,
                to: to.iter().map(|e| endpoint(e)).collect::<Result<_>>()?,
                max_transfers,
                max_results,
                max_travel_seconds,
                alternatives_within_seconds: 3600,
            };
            let t = std::time::Instant::now();
            let resp = s.plan(&req)?;
            eprintln!("query time: {:?}", t.elapsed());
            print(&resp)?;
        }
        Cmd::Stops {
            snapshot,
            bbox,
            limit,
            served_only,
        } => {
            let v: Vec<f64> = bbox
                .split(',')
                .map(|x| x.trim().parse())
                .collect::<Result<_, _>>()
                .context("bbox numbers")?;
            let [a, b, c, d] = v[..] else {
                bail!("bbox needs four numbers")
            };
            print(&Snapshot::open(&snapshot)?.stops_in_bbox(
                [a, b],
                [c, d],
                limit,
                served_only,
            )?)?;
        }
        Cmd::Inventory {
            config,
            feeds,
            out,
            jobs,
        } => rustis::inventory::run(&config, &feeds, &out, jobs)?,
        Cmd::Stats { snapshot } => print(&Snapshot::open(&snapshot)?.stats())?,
        Cmd::Verify { snapshot } => {
            Snapshot::open(&snapshot)?.verify()?;
            println!("ok");
        }
        Cmd::Serve {
            snapshot,
            listen,
            max_concurrency,
            max_labels,
            reload_interval,
        } => {
            let handle = Arc::new(rustis::http::SnapshotHandle::new(
                Snapshot::open(&snapshot)?,
                Some(snapshot.clone()),
                max_labels,
            ));
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(max_concurrency.max(1) + 1)
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(listen).await?;
                let s = handle.get();
                eprintln!(
                    "rustis serving {} ({} +{} days) on http://{listen}",
                    s.dir().display(),
                    s.first_day(),
                    s.meta().horizon_days
                );
                drop(s);
                if reload_interval > 0 {
                    let h = handle.clone();
                    tokio::spawn(async move {
                        use tokio::signal::unix::{SignalKind, signal};
                        let mut hup = signal(SignalKind::hangup()).ok();
                        let mut tick =
                            tokio::time::interval(std::time::Duration::from_secs(reload_interval));
                        loop {
                            match &mut hup {
                                Some(hup) => tokio::select! {
                                    _ = tick.tick() => {}
                                    _ = hup.recv() => {}
                                },
                                None => {
                                    tick.tick().await;
                                }
                            }
                            let h = h.clone();
                            let _ =
                                tokio::task::spawn_blocking(move || h.reload_if_changed()).await;
                        }
                    });
                }
                axum::serve(listener, rustis::http::router_with(handle, max_concurrency))
                    .with_graceful_shutdown(async {
                        let _ = tokio::signal::ctrl_c().await;
                    })
                    .await?;
                anyhow::Ok(())
            })?;
        }
    }
    Ok(())
}
