<p align="center"><img src="assets/banner.svg" alt="Rustis — drop-in replacement for MOTIS with a fraction of the RAM" width="100%"></p>

# Rustis

A lean public-transport journey planner written in Rust, built as a drop-in
replacement for [MOTIS](https://github.com/motis-project/motis) where RAM is scarce.

* **Small footprint:** GTFS and NeTEx feeds are compiled offline into an immutable
  snapshot of flat arrays. The server only maps it read-only (`mmap`), so the timetable
  lives in the page cache, not on the heap. All of Europe (1,086 feeds, 3.9 M stops,
  16 M trips) is served within a 512 MiB memory budget.
* **Fast:** its own RAPTOR implementation with exact lower-bound pruning. Typical queries
  take tens of milliseconds.
* **Self-updating:** `rustis update` downloads changed feeds (conditional GET), rebuilds
  the snapshot in parallel, publishes it atomically and the server hot-reloads it.

> Status: early. The MOTIS-compatible subset (`/api/v6/plan` GET/POST incl. offsets,
> `radius`, `transitModes`, and `/api/v6/map/stops`) is contract-tested against MOTIS
> 2.11; other MOTIS endpoints are not provided.

## Commands

```sh
cargo build --release

# Build a snapshot from a MOTIS config (dataset name = id namespace) …
rustis build --motis-config config.yml --feed-dir gtfs/ --out snap/ \
  --first-day 2026-11-14 --horizon-days 366
# … or from single feeds (one --gtfs: ids without prefix)
rustis build --gtfs feed.zip --out snap/ --first-day 2026-11-16
#   --link-radius-m 300          walking links between nearby stops (default 300 m)
#   --default-change-seconds N   minimum change time without a feed rule (default 120 s)
#   --exclude-booking-stops      drop on-demand stops (pickup/drop_off 2/3) instead of flagging
#   --max-footpath-secs 720      cap of the transitive footpath closure (12 min)
#   --jobs N                     parse feeds in parallel (result identical to serial)

# Download/refresh feeds and publish a new snapshot (root/{feeds,snapshots,current})
rustis update --root data/ --motis-config config.yml --mirror https://example.org/gtfs

rustis plan   --snapshot snap/ --date 2026-11-18 --at 07:30 --from 'ds:STOP@60' --to 'ds:STOP'
rustis stops  --snapshot snap/ --bbox 48.13,11.55,48.15,11.57 --served-only
rustis stats  --snapshot snap/
rustis verify --snapshot snap/        # checksums + range/order checks
rustis serve  --snapshot data/current --listen 127.0.0.1:8088 --max-concurrency 2 \
  --reload-interval 60                # also reloads on SIGHUP
```

Feed scripts: a dataset's `script:` in the MOTIS config (path relative to the config) is
run at import with nigiri's Lua API (`process_agency`/`process_route`/`process_trip`;
e.g. Transitous' scripts set train numbers such as `ICE 119`, route types and colours).
Scripts are not bundled; put them next to the config (`scripts/*.lua`). A script that
fails to load is reported and the feed is imported without it.

Deutschlandticket (Rustis extension of `/api/v6/plan`): `deutschlandticket=true` only uses
rides valid with the Deutschlandticket (local transport of German feeds; DB Fernverkehr only
within the released sections Rostock–Stralsund, Gäubahn, Bremen–Norddeich, Dresden–Chemnitz;
released cross-border stations; rules and sources in `src/dticket.rs`). Every transit leg and
itinerary carries `deutschlandticket: true|false`. `transitModes` is applied in the search
(as in MOTIS), not to finished journeys.

## HTTP API (loopback by default)

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/health` | status, horizon, reload state, feed warnings |
| GET | `/v1/stats` | snapshot figures and process memory (RSS anon/file, PSS, swap) |
| GET | `/v1/feeds` | imported feeds, validity and update status |
| GET | `/v1/stops?min_lat=&min_lon=&max_lat=&max_lon=&limit=&served_only=` | stop catalogue in a bounding box |
| POST | `/v1/plan` | journeys |

```json
POST /v1/plan
{"date":"2026-11-18","departure":27000,
 "from":[{"stop_id":"de-DELFI:de:09162:6:51","walk_seconds":60}],
 "to":[{"stop_id":"de-DELFI:de:09162:2:52"}],
 "max_transfers":3,"max_results":3,
 "max_travel_seconds":86400,"alternatives_within_seconds":3600}
```

* `date` + `departure`: local date and seconds since local midnight in the time zone of
  the first origin stop; every time in the response uses the same reference (DST: real
  seconds). Dates outside the horizon → `400`.
* Response: `itineraries[]` with `departure`/`arrival` (seconds and RFC 3339 with the stop's
  offset), `transfers`, `legs[]` (`access|transit|walk|egress`) with trip, route, line, mode,
  headsign, agency, colour and `geometry` (coordinates of the served stops, not a vehicle
  track).
* `booking_required` / `pickup_booking_required` / `drop_off_booking_required`: boarding or
  alighting needs a booking (GTFS pickup/drop_off 2/3), plus a `BOOKING:` warning.
* `warnings`: every known coverage or semantics limit of the snapshot (`INCOMPLETE: …`,
  e.g. feeds not imported, Flex, unexploited transfer rules, search limit reached).
* Per-request bounds: ≤ 64 origin/destination stops, `max_transfers` ≤ 8,
  `max_results` ≤ 10, `max_travel_seconds` ≤ 4 days; `503` when all query slots are busy.

## Container

Prebuilt images: `ghcr.io/isntfunny/rustis:latest` and `:<commit>` (GitHub Actions on every push to `main`).

```sh
docker build -t rustis .
# serve: /data/current is the snapshot symlink, reloaded every 60 s
docker run -d -p 127.0.0.1:8080:8080 -v /srv/rustis:/data -v ./config.yml:/config/motis-config.yml:ro rustis
# daily refresh inside the running container (downloads changed feeds, publishes, hot reload)
docker exec <container> rustis update --root /data --motis-config /config/motis-config.yml --jobs 1
```

Serving stays well below 512 MiB; a full rebuild of all of Europe peaks at about 4.5 GiB,
so the container limit has to cover the update run, not the server.

## Architecture

* **Import** (`src/build.rs`, `src/feed.rs`, `src/netex.rs`): each feed is streamed
  (CSV straight from the ZIP member; NeTEx with a streaming XML parser), validated as a
  whole and only then committed; a broken feed is reported with its reason in
  `snapshot.json`. Feeds parse in parallel and commit in input order. Service days are
  deduplicated bitfields over the horizon, times are GTFS-local from "noon − 12 h" (exact
  across DST), frequency trips are expanded, routes are split by stop sequence and
  boarding flags without overtaking. Footpaths come from `transfers.txt` plus generated
  links between nearby stops (also through unserved stops and stations), closed
  transitively up to 12 min.
* **Snapshot** (`src/store.rs`, `src/tt.rs`, `src/catalog.rs`): one little-endian array per
  file, a manifest with length and FNV checksum, publication via staging + rename,
  read-only files. Full stop catalogue with sorted id and latitude indices.
* **Routing** (`src/raptor.rs`, `src/engine.rs`): RAPTOR with sparse labels per round,
  arrival and boarding readiness kept apart (minimum change times are exact; a walk after
  a ride takes at least the stop's change time), trip/route-specific rules, stay-seated
  continuations (`block_id`, transfer type 4), a lazy backward lower-bound Dijkstra from the
  destination and a label budget; search windows grow 2 h → 4 h → … up to
  `max_travel_seconds`.

## Limits

GTFS-Flex groups and Polygon/MultiPolygon zones serve the feed's stops within each
area. Rides use pickup/drop-off windows, calendars and a conservative estimate:
maximum stop-to-stop distance × 1.5 at 30 km/h + 5 minutes (minimum 5 minutes).
This is not an operator travel-time guarantee; booking contact/notice is reported
in `BOOKING:` warnings. MOTIS clients must explicitly request `FLEX` or `ODM`.
Mixed timed/area trips, empty areas and unsupported flex shapes are reported with
skip reasons. Zone routing does not offer arbitrary door-to-door endpoints.
Transfer types 1/5 are not exploited; no de-duplication of
the same service across feeds. NeTEx: the French (SNCF) and Italian (Trenitalia)
profiles are imported; other profiles, reservation flags and NeTEx interchanges are not
yet supported. Walks are straight-line at 1.2 m/s (no street network).

## Tests

```sh
cargo fmt --check && cargo clippy --all-targets && cargo test --release
```

Routing, transfers, feeds, NeTEx, booking, HTTP, update/reload and a property test with
36,000 random queries on random networks against an exact brute-force reference.

## License

No license has been chosen yet; all rights reserved for now.
