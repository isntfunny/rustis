use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use rustis::{Endpoint, ImportOptions, PlanRequest, Snapshot, import_gtfs_with};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Parser)]
#[command(version, about = "Disk-backed GTFS routing prototype (Vulture RAPTOR)")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Import one GTFS feed (directory or ZIP) into a new snapshot directory.
    Import {
        #[arg(long)]
        gtfs: PathBuf,
        #[arg(long)]
        out: PathBuf,
        /// Service date YYYY-MM-DD (the following day is included for overnight trips).
        #[arg(long)]
        date: String,
        /// Add straight-line footpaths between stops within this many metres.
        #[arg(long)]
        walk_radius_m: Option<f64>,
        #[arg(long, default_value_t = 1.2)]
        walk_speed: f64,
        /// Drop transfer rules whose omission can only remove connections
        /// (timed/in-seat/recommended, also trip/route-specific). Restricting
        /// rules (specific min-time or forbidden) are always rejected. The
        /// snapshot and every response are marked as possibly incomplete.
        #[arg(long)]
        drop_relaxing_transfers: bool,
        /// Apply trip/route-specific min-time/forbidden rules to all trips at
        /// those stops (only tightens; marks the snapshot conservative).
        #[arg(long)]
        generalize_restricting_transfers: bool,
        /// Minimum change time (s) at stops without a same-stop transfers.txt rule.
        #[arg(long, default_value_t = 0)]
        default_change_seconds: u32,
        /// Exclude boarding/alighting that needs booking (pickup/drop_off 2/3)
        /// instead of returning it marked `booking_required`.
        #[arg(long)]
        exclude_booking_stops: bool,
        /// Parse the full feed instead of stream-filtering it to the snapshot days.
        #[arg(long)]
        no_prefilter: bool,
        /// Remove GTFS-Flex trips (on-demand zones/windows) instead of failing;
        /// the snapshot and every response are marked incomplete.
        #[arg(long)]
        exclude_flex_trips: bool,
        /// Remove frequency-based trips (frequencies.txt) instead of failing;
        /// the snapshot and every response are marked incomplete.
        #[arg(long)]
        exclude_frequency_trips: bool,
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
    /// Serve the HTTP API.
    Serve {
        #[arg(long)]
        snapshot: PathBuf,
        #[arg(long, default_value = "127.0.0.1:8088")]
        listen: std::net::SocketAddr,
        #[arg(long, default_value_t = 2)]
        max_concurrency: usize,
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
        Cmd::Import {
            gtfs,
            out,
            date,
            walk_radius_m,
            walk_speed,
            drop_relaxing_transfers,
            generalize_restricting_transfers,
            default_change_seconds,
            exclude_booking_stops,
            no_prefilter,
            exclude_flex_trips,
            exclude_frequency_trips,
        } => {
            let opts = ImportOptions {
                date,
                walk_radius_m,
                walk_speed_m_per_s: walk_speed,
                drop_relaxing_transfers,
                generalize_restricting_transfers,
                default_change_seconds,
                exclude_booking_stops,
                prefilter: !no_prefilter,
                exclude_flex_trips,
                exclude_frequency_trips,
            };
            let r = import_gtfs_with(&gtfs, &out, &opts)?;
            println!(
                "imported {} stops, {} RAPTOR routes, {} trips; timetable.bin {} bytes, metadata.json {} bytes",
                r.stops, r.raptor_routes, r.trips, r.timetable_bytes, r.metadata_bytes
            );
            for n in r.notes {
                println!("note: {n}");
            }
            if let Some(m) = rustis::memory::ProcessMemory::current() {
                println!("import peak RSS (VmHWM): {} KiB", m.hwm_kib);
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
        } => {
            let s = Snapshot::open(&snapshot)?;
            let req = PlanRequest {
                date,
                departure: parse_time(&at)?,
                from: from.iter().map(|e| endpoint(e)).collect::<Result<_>>()?,
                to: to.iter().map(|e| endpoint(e)).collect::<Result<_>>()?,
                max_transfers,
                max_results,
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
        } => {
            let v: Vec<f64> = bbox
                .split(',')
                .map(|x| x.trim().parse())
                .collect::<Result<_, _>>()
                .context("bbox numbers")?;
            let [a, b, c, d] = v[..] else {
                bail!("bbox needs four numbers")
            };
            print(&Snapshot::open(&snapshot)?.stops_in_bbox([a, b], [c, d], limit)?)?;
        }
        Cmd::Stats { snapshot } => print(&Snapshot::open(&snapshot)?.stats())?,
        Cmd::Verify { snapshot } => {
            Snapshot::open(&snapshot)?.verify()?;
            println!("ok");
        }
        Cmd::Serve {
            snapshot,
            listen,
            max_concurrency,
        } => {
            let s = Arc::new(Snapshot::open(&snapshot)?);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .max_blocking_threads(max_concurrency.max(1))
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let listener = tokio::net::TcpListener::bind(listen).await?;
                eprintln!("rustis serving {} on http://{listen}", s.service_date());
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
