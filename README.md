# Rustis

Lokaler, sparsamer ÖPNV-Routingdienst in Rust (RTP-75/RTP-76), Ziel: MOTIS auf Mars
für Roadtrip-Planner und Lulu ersetzen. GTFS-Feeds werden offline in einen
unveränderlichen Snapshot aus flachen Arrays übersetzt; der Dienst bildet sie nur
read-only per `mmap` ab (Page-Cache statt Heap) und routet mit einem eigenen RAPTOR mit
exakter Untergrenzen-Beschneidung.

* Anforderungen und offene Abnahmegrenzen: [Spec](docs/specs/2026-11-14-rustis.md)
* Architekturentscheidung mit Belegen (inkl. Vulture, Solari, CSA, TB):
  [ADR](docs/decisions/2026-11-14-architecture.md)
* Messungen: [docs/measurements](docs/measurements/)

**Kein vollständiger MOTIS-Ersatz** in diesem Stand – siehe [Grenzen](#grenzen).

## Befehle

```sh
cargo build --release -j2

# Read-only-Inventur aller Datensätze einer MOTIS-Konfiguration (JSON-Zeilen)
rustis inventory --config config.yml --feeds gtfs/ --out feeds.jsonl --jobs 3

# Snapshot bauen: alle MOTIS-Datensätze (Namensraum = Datensatzname) …
rustis build --motis-config config.yml --feed-dir gtfs/ --out snap/ \
  --first-day 2026-11-14 --horizon-days 366 --link-radius-m 200
# … oder einzelne Feeds (ein --gtfs: IDs ohne Präfix)
rustis build --gtfs feed.zip --out snap/ --first-day 2026-11-16
#   --default-change-seconds N   Mindestumstiegszeit ohne eigene Regel (Std. 0)
#   --exclude-booking-stops      Bedarfshalte (pickup/drop_off 2/3) ausschließen statt markieren
#   --max-footpath-secs 720      Obergrenze der transitiven Fußweg-Hülle (MOTIS: 12 min)

rustis plan --snapshot snap/ --date 2026-11-18 --at 07:30 --from 'ds:STOP@60' --to 'ds:STOP'
rustis stops --snapshot snap/ --bbox 48.13,11.55,48.15,11.57 --served-only
rustis stats  --snapshot snap/
rustis verify --snapshot snap/        # Prüfsummen + Bereichs-/Sortierprüfungen
rustis serve  --snapshot snap/ --listen 127.0.0.1:8088 --max-concurrency 2
```

## HTTP-API (Standard: nur Loopback)

| Methode | Pfad | Zweck |
| --- | --- | --- |
| GET | `/health` | `{status, first_day, horizon_days, warnings}` |
| GET | `/v1/stats` | Snapshot-Kennzahlen und Prozessspeicher (RSS anon/file, PSS, Swap) |
| GET | `/v1/stops?min_lat=&min_lon=&max_lat=&max_lon=&limit=&served_only=` | Haltestellenkatalog in der BBox (`served`-Flag) |
| POST | `/v1/plan` | Verbindungen |

```json
POST /v1/plan
{"date":"2026-11-18","departure":27000,
 "from":[{"stop_id":"de-DELFI:de:09162:6:51","walk_seconds":60}],
 "to":[{"stop_id":"de-DELFI:de:09162:2:52"}],
 "max_transfers":3,"max_results":3,
 "max_travel_seconds":86400,"alternatives_within_seconds":3600}
```

* `date` + `departure`: lokales Datum und Sekunden ab lokaler Mitternacht in der Zeitzone
  der ersten Start-Haltestelle; alle Sekundenangaben der Antwort beziehen sich darauf
  (DST: echte Sekunden, 08:00 am 25-Stunden-Tag = 32400). Datum außerhalb des Horizonts → `400`.
* Antwort: `itineraries[]` mit `departure`/`arrival` (Sekunden und RFC 3339 mit Offset der
  jeweiligen Haltestelle), `transfers`, `legs[]` (`access|transit|walk|egress`), `trip_id`,
  `route_id`, `line`, `mode`, `geometry` = Koordinaten der bedienten Halte (keine Fahrzeugspur).
* `booking_required` / `pickup_booking_required` / `drop_off_booking_required`: Ein-/Ausstieg
  nur nach Anmeldung (GTFS pickup/drop_off 2/3) plus `BOOKING:`-Warnung.
* `warnings`: jede bekannte Abdeckungs-/Semantikgrenze des Snapshots (`INCOMPLETE: …`, z. B.
  nicht importierte Feeds, Flex, nicht ausgenutzte Anschlussregeln).
* Grenzen pro Anfrage: ≤ 64 Start-/Zielhalte, `max_transfers` ≤ 8, `max_results` ≤ 10,
  `max_travel_seconds` ≤ 4 Tage, Fußweg-Offset ≤ 3 h; `503` wenn alle Abfrageslots belegt.

## Architektur

* **Import** (`src/build.rs`, `src/feed.rs`): pro Feed streamend (`csv` über das ZIP-Member,
  Fahrt für Fahrt), komplett im Speicher geprüft und erst dann übernommen; ein kaputter oder
  Nicht-GTFS-Feed wird mit Grund in `snapshot.json` (`feeds[].error`, `warnings`) geführt.
  Verkehrstage als deduplizierte Bitfelder über den Horizont, Zeiten GTFS-lokal ab
  „Mittag − 12 h“ (DST exakt), Frequenzfahrten expandiert, Routen nach Haltefolge +
  Ein-/Ausstiegsflags und ohne Überholen geteilt, Fußwege aus transfers.txt plus
  generierte Verknüpfung naher bedienter Halte (Radius), transitiv geschlossen bis 12 min.
* **Snapshot** (`src/store.rs`, `src/tt.rs`, `src/catalog.rs`): ein Little-Endian-Array pro Datei,
  `manifest` mit Länge und FNV-Prüfsumme, Veröffentlichung per Staging + Rename, Dateien
  read-only. Vollständiger Haltestellenkatalog mit sortiertem ID- und Breitengrad-Index.
* **Routing** (`src/raptor.rs`, `src/engine.rs`): RAPTOR mit spärlichen Labels pro Runde,
  getrennt nach Ankunft und Einstiegsbereitschaft (Mindestumstiegszeiten exakt),
  fahrt-/linienspezifische Regeln, träge rückwärts laufende Untergrenzen-Dijkstra vom Ziel;
  Suchfenster wachsen 2 h → 4 h → … bis `max_travel_seconds`.

## Grenzen

Siehe [Spec](docs/specs/2026-11-14-rustis.md#open-acceptance-limits-must-stay-visible-until-resolved):
NeTEx-Datensätze (SNCF, Trenitalia) nicht importiert; GTFS-Flex nicht geroutet; Anschluss-
Typen 1/4/5 nicht ausgenutzt; keine Duplikat-Zusammenführung über Feeds; MOTIS-Lua-Skripte
nicht angewandt; Horizont ist eine benannte Importeinstellung (Kürzungen werden gezählt).

## Tests

```sh
cargo fmt --check && cargo clippy -j2 --all-targets && cargo test -j2 --release
```

`tests/routing.rs` (Umstieg vs. Direktfahrt, Zugang/Abgang, Kalender, Übernacht, DST,
Horizont, Korruption, Unveränderlichkeit), `tests/transfers.rs` (Mindestzeiten, verbotene,
spezifische und nicht ausgenutzte Regeln), `tests/feeds.rs` (ZIP-Layouts, Flex, Frequenzen,
Katalog inkl. unbedienter Halte, mehrere Feeds mit Namensräumen und Verknüpfung, Nicht-GTFS-Feed),
`tests/booking.rs`, `tests/http.rs` und `tests/property.rs`: 36 000 Zufallsanfragen auf
Zufallsnetzen gegen eine exakte Brute-Force-Referenz (0 Abweichungen mit und ohne Umstiegszeiten).

## Lizenz

Für den eigenen Code ist noch keine Lizenz festgelegt.
