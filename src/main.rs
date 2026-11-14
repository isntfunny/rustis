use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rustis::{BuildOptions, Endpoint, FeedSpec, PlanRequest, Snapshot, build_snapshot};
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
        /// First service day YYYY-MM-DD (default: today).
        #[arg(long)]
        first_day: Option<String>,
        #[arg(long, default_value_t = 366)]
        horizon_days: u32,
        /// Generate footpaths between served stops closer than this (0 = off).
        #[arg(long, default_value_t = 0.0)]
        link_radius_m: f64,
        #[arg(long, default_value_t = 1.2)]
        walk_speed: f64,
        #[arg(long, default_value_t = 720)]
        max_footpath_secs: u32,
        /// Minimum change time (s) at stops without a same-stop transfers.txt rule.
        #[arg(long, default_value_t = 0)]
        default_change_seconds: u32,
        /// Treat pickup/drop_off 2/3 (booking) as not available.
        #[arg(long)]
        exclude_booking_stops: bool,
        /// Comparison mode: footpaths/change times built like MOTIS (nigiri).
        #[arg(long)]
        motis_footpaths: bool,
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
        /// Per-search label budget (bounds per-query memory/time).
        #[arg(long, default_value_t = rustis::DEFAULT_MAX_LABELS)]
        max_labels: usize,
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
            first_day,
            horizon_days,
            link_radius_m,
            walk_speed,
            max_footpath_secs,
            default_change_seconds,
            exclude_booking_stops,
            motis_footpaths,
        } => {
            let mut feeds: Vec<FeedSpec> = Vec::new();
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
            let first_day = match first_day {
                Some(d) => jiff::civil::Date::strptime("%Y-%m-%d", &d)?,
                None => jiff::Zoned::now().date(),
            };
            let mut opts = BuildOptions::new(first_day);
            opts.horizon_days = horizon_days;
            opts.link_radius_m = link_radius_m;
            opts.walk_speed_m_per_s = walk_speed;
            opts.max_footpath_secs = max_footpath_secs;
            opts.default_change_seconds = default_change_seconds;
            opts.exclude_booking_stops = exclude_booking_stops;
            opts.motis_footpaths = motis_footpaths;
            let m = build_snapshot(&feeds, &out, opts)?;
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
        } => {
            let mut s = Snapshot::open(&snapshot)?;
            s.set_max_labels(max_labels);
            let s = Arc::new(s);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(max_concurrency.max(1))
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(listen).await?;
                eprintln!(
                    "rustis serving {} (+{} days) on http://{listen}",
                    s.first_day(),
                    s.meta().horizon_days
                );
                axum::serve(listener, rustis::http::router(s, max_concurrency))
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
