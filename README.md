# Rustis

Lokaler, sparsamer ÖPNV-Routingdienst in Rust – **Prototyp** (RTP-75).
Ein GTFS-Feed wird offline in einen unveränderlichen Snapshot auf Disk
übersetzt; der Dienst öffnet nur diesen Snapshot (kein GTFS-Parsing beim
Serving) und routet mit dem unveränderten RAPTOR-Kern von
[Vulture](https://github.com/urschrei/vulture) (vendort unter `vendor/vulture`).

Kein MOTIS-Ersatz für Europa in diesem Stand – siehe [Grenzen](#grenzen).

## Befehle

```sh
cargo build --release -j2          # Mars ist speicherknapp: Parallelität begrenzen

# Import: GTFS-Verzeichnis oder ZIP → neues Snapshot-Verzeichnis (darf nicht existieren)
target/release/rustis import --gtfs feed.zip --out data/linz-2026-11-16 --date 2026-11-16
#   --default-change-seconds 120     Mindestumsteigezeit an Haltestellen ohne eigene Regel (Std. 0)
#   --exclude-booking-stops          Bedarfshalte (pickup/drop_off 2/3) ausschließen statt markieren
#   --drop-relaxing-transfers        nur *lockernde* Transferregeln verwerfen (siehe unten);
#                                    Snapshot + jede Antwort werden als UNVOLLSTÄNDIG markiert
#   --walk-radius-m 300 [--walk-speed 1.2]   Luftlinien-Fußwege zwischen nahen Haltestellen

target/release/rustis verify --snapshot data/linz-2026-11-16   # Prüfsumme + Sortierung
target/release/rustis stats  --snapshot data/linz-2026-11-16   # Größen + Prozessspeicher
target/release/rustis stops  --snapshot data/linz-2026-11-16 --bbox 48.28,14.27,48.34,14.33
target/release/rustis plan   --snapshot data/linz-2026-11-16 --date 2026-11-16 --at 08:00 \
    --from 'at:44:41164:0:56' --from 'at:44:41164:0:0@120' --to 'at:44:41101:0:3'
target/release/rustis serve  --snapshot data/linz-2026-11-16 --listen 127.0.0.1:8088 --max-concurrency 2
```

`--from/--to` sind wiederholbar; `STOP_ID@sekunden` setzt die Fußwegzeit zwischen
Nutzerposition und Haltestelle. `--at` akzeptiert Stunden ≥ 24 (Folgenacht).

## HTTP-API (Standard: nur Loopback)

| Methode | Pfad | Zweck |
| --- | --- | --- |
| GET | `/health` | `{status, service_date, transfer_rules, warnings}` |
| GET | `/v1/stats` | Snapshotgrößen und Prozessspeicher (RSS anon/file, PSS, Swap) |
| GET | `/v1/stops?min_lat=&min_lon=&max_lat=&max_lon=&limit=` | bediente Haltestellen in der BBox (limit ≤ 1000) |
| POST | `/v1/plan` | Verbindungen |

```json
POST /v1/plan
{"date":"2026-11-16","departure":28800,
 "from":[{"stop_id":"A","walk_seconds":60}],"to":[{"stop_id":"C"}],
 "max_transfers":3,"max_results":3}
```

* `departure`: Sekunden ab lokaler Mitternacht des Betriebstags; < 172800.
* `date` muss dem Snapshot-Betriebstag entsprechen; sonst `400` (kein stilles Ersatzdatum).
* Antwort: `itineraries[]` mit `departure`/`arrival` (Sekunden **und** RFC 3339 mit Offset),
  `transfers`, `legs[]` (`access` | `transit` | `walk` | `egress`) mit Haltestellen,
  `trip_id`, `route_id`, `line`, `mode` und `geometry` = Koordinaten der bedienten
  Haltestellen (`geometry_source: "stops"`, keine Fahrzeugspur).
* `itinerary.departure` ist der **tatsächliche Aufbruch** (spätestmöglicher Start inkl.
  Zugangsweg), nicht die Abfragezeit.
* `omitted_unreconstructable`: von RAPTOR gefundene Journeys, deren Abschnitte nicht
  konsistent rekonstruierbar waren; sie werden weggelassen statt kaputt ausgeliefert.
* `booking_required` (Itinerary) bzw. `pickup_booking_required`/`drop_off_booking_required`
  (Leg): Ein-/Ausstieg nur nach Anmeldung (GTFS pickup/drop_off 2/3); zusätzlich `BOOKING:`-Warnung.
* `warnings`: nicht leer bei Snapshots im konservativen Transfermodus (`INCOMPLETE: …`).
  Die gelieferten Verbindungen halten alle behaltenen Regeln ein, aber Verbindungen über
  verworfene Regeln fehlen – „keine Verbindung“ und Reihenfolge sind dann nicht maßgeblich.
* Fehler: `400 {"error": …}` für Validierung, `503` wenn alle Abfrageslots belegt sind.
* Grenzen pro Anfrage: ≤ 64 Start-/Zielhaltestellen, `max_transfers` ≤ 8, `max_results` ≤ 10,
  Body ≤ 64 KiB, Fußweg-Offset ≤ 3 h.

## Architektur

```
GTFS (ZIP/Dir) ──import──▶ Vulture GtfsTimetable (Kalender, Muster, Nichtüberholen, +1 Tag)
                              │ eigene transfers.txt-Auswertung (siehe unten)
                              ▼
                  snapshot/ timetable.bin (LE, sektioniert, read-only)  metadata.json
                              │
                 serve/plan ──┤ Topologie (Routen↔Haltestellen, Fußwege) → Heap
                              │ Zeiten/Flags/Trip-IDs → read-only mmap (nie in Heap kopiert)
                              ▼
                 MappedTimetable: impl vulture::Timetable → RAPTOR → Leg-Rekonstruktion
```

* **Import** (`src/import.rs`): Kalender/Pattern-Logik vollständig von Vulture. Rustis prüft
  vorher die unterstützte Teilmenge und ersetzt die Transferlisten: `gtfs-structures` verwirft
  `from_trip_id`/… und Vulture würde **jede** Nicht-Typ-3-Zeile (auch garantierte/In-Seat-
  Anschlüsse) als 300-s-Fußweg übernehmen und Mindestumsteigezeiten an derselben Haltestelle
  nie erzwingen. Rustis liest `transfers.txt` selbst (siehe [Transferregeln](#transferregeln-und-korrektheitsgarantien)).
  Snapshot wird in einem Staging-Verzeichnis gebaut, `fsync`t, read-only gesetzt und per
  `rename` veröffentlicht; ein vorhandenes Ziel wird nie angefasst.
* **Format** (`src/format.rs`, Version 2): 17 Sektionen mit Verzeichnis; Werte nur per
  `from_le_bytes` gelesen, keine Typ-Reinterpretation (Vultures Index-Newtypes sind nicht
  `repr(transparent)`). Beim Öffnen werden alle Offsets/Bereiche/Querverweise geprüft;
  `verify` prüft zusätzlich FNV-1a-Prüfsumme und Abfahrtssortierung.
* **Serving** (`src/storage.rs`, `src/routing.rs`): Vultures Trait verlangt Slices seiner
  Index-Typen – daher liegt die Topologie im Heap; Ankunft/Abfahrt/Pickup-Flags/Trip-IDs
  werden je Zugriff aus dem Mapping gelesen. Scratch-Puffer kommen aus einem
  `RaptorCachePool`, begrenzt durch `--max-concurrency`.
* **Rekonstruktion**: Vulture liefert nur `(Route, Ausstieg)`; `Journey::with_timing` folgt
  nur einem Fußweg-Hop. Rustis sucht Einstiegshaltestelle und Fußwege selbst (Dijkstra über
  denselben Fußweggraphen, bei geschlossener Relation nur Direktkanten), berücksichtigt
  Pickup/Drop-off und prüft das Ergebnis gegen RAPTORs Ankunftslabel.

## Transferregeln und Korrektheitsgarantien

RAPTOR (Vulture) ist unverändert. Umsteigeregeln werden ausschließlich im Import und im
`Timetable`-Adapter umgesetzt:

| Regelklasse in `transfers.txt` | Standardimport | `--drop-relaxing-transfers` |
| --- | --- | --- |
| Typ 0/2 zwischen verschiedenen Haltestellen | Fußweg, strengste Zeit je Paar (Typ 0 ohne Zeit: 300 s) | gleich |
| Typ 2 an derselben Haltestelle | Mindestumsteigezeit, im Adapter erzwungen | gleich |
| Typ 2 ohne `min_transfer_time` | **abgelehnt** | **abgelehnt** |
| Typ 3 zwischen verschiedenen Haltestellen | kein Fußweg (auch nicht per `--walk-radius-m`); über verkettete Fußwege erreichbar → **abgelehnt** | gleich |
| Typ 3 an derselben Haltestelle | **abgelehnt** (Adapter kann Wiedereinstieg nicht verbieten) | **abgelehnt** |
| trip-/routen-spezifisch Typ 2/3 (einschränkend) | **abgelehnt** | **abgelehnt** – Weglassen könnte Unzulässiges erlauben |
| trip-/routen-spezifisch Typ 0/1/4/5, stop-level Typ 1/4/5 (lockernd) | **abgelehnt** | verworfen, Snapshot `conservative` |

Warum die lockernden Klassen gefahrlos verworfen werden können: Typ 0 (empfohlen), 1
(garantierter Anschluss) und 4 (Sitzenbleiben erlaubt) erweitern nur die Möglichkeiten
gegenüber den stop-level Regeln; Rustis erzeugt aus ihnen keine Kante. Typ 5 verbietet nur
das Sitzenbleiben, das Rustis nie modelliert. Ohne sie gelten weiterhin alle behaltenen
stop-level Regeln – es fallen also nur Verbindungen weg. Belegt durch
`tests/transfers.rs` (u. a. kein B→D-Fußweg aus verworfenem Typ 1, behaltene 300-s-Regel greift weiter).

**Garantie (für strikte und konservative Snapshots):** Jede ausgegebene Verbindung hält ein:
Kalender, Pickup/Drop-off-Verbote (unbekannte Werte gelten als verboten), Bedarfshalte nur
markiert (`booking_required`) oder mit `--exclude-booking-stops` ausgeschlossen, Mindestumsteigezeit beim Wiedereinstieg an derselben
Haltestelle (Regel oder `--default-change-seconds`), Fußwegzeiten aus `transfers.txt` bzw.
Radius, Typ-3-Verbote zwischen Haltestellen. Die Rekonstruktion prüft dieselben Regeln noch
einmal unabhängig; Abweichungen landen in `omitted_unreconstructable` (Tests verlangen 0).

**Nachweis:** neben den Szenario-Tests prüft `tests/property.rs` 600 zufällige Netze (je
6 Haltestellen, 4 Linien, Pickup/Drop-off-Verbote, Fußwege; 36 000 Abfragen). Jede Verbindung
wird von einem unabhängigen Prüfer gegen den erzeugten Fahrplan nachgerechnet; eine
Brute-Force-Suche mit exakten Regeln liefert die frühestmögliche Ankunft. Ergebnis: 0
Regelverletzungen; ohne Umsteigezeiten exakt optimal (0 Abweichungen); mit Umsteigezeiten
165 konservative Lücken bei ≈15 000 verglichenen Abfragen (≈1,1 %), Ziele, die zu Fuß
erreichbar sind, ausgenommen.

**Vulture-Unvollständigkeit, behoben im Export:** RAPTOR prüft pro Route nur die früheste
erreichbare Fahrt. Verbietet diese den Ausstieg am Ziel, wurde eine spätere Fahrt derselben
Route nie betrachtet (vom Eigenschaftstest gefunden). Rustis teilt RAPTOR-Routen daher
zusätzlich nach Ein-/Ausstiegsmuster auf (mehr Routen, gleicher Algorithmus).

**Keine Garantie für Vollständigkeit/Optimalität:**
* RAPTOR-Labels kennen ihre Herkunft nicht. Die Umsteigezeit wird daher auf die Ankunft
  aufgeschlagen; ein *Fußweg nach dem Aussteigen* an einer Haltestelle mit Umsteigezeit
  beginnt erst danach (z. B. 300 s + 120 s statt 120 s). Das verwirft gültige Verbindungen,
  erlaubt aber nie zu kurze. Start- und zu Fuß erreichte Haltestellen sind nicht betroffen.
* Bei mehreren Zielhaltestellen kann die interne Reihung um die Umsteigezeit der
  Zielhaltestelle verzerrt sein; gemeldete Zeiten sind die echten Fahrplanzeiten.
* Ohne Regeln und mit `--default-change-seconds 0` ist Umsteigen an derselben Haltestelle in
  0 s erlaubt (feed-konform, aber unrealistisch). Linz/KVV haben keine solchen Regeln.
* Konservativer Modus: verworfene Anschlüsse fehlen (Antwort-`warnings`).

## Unterstützter Umfang und Grenzen

Unterstützt: ein Feed, eine Agency-Zeitzone, ein Betriebstag + Folgetag (Übernacht-Fahrten,
Abfragen bis 47:59:59), `calendar`/`calendar_dates`, Pickup/Drop-off `1` (nicht verfügbar),
gleichzeitig abfahrende Fahrten, Multi-Source/-Target mit Fußweg-Offsets, Pareto-Alternativen
(Ankunft vs. Umstiege), mehrstufige Fußwege aus `transfers.txt`.

Explizit abgelehnt (Import bricht ab): `frequencies.txt`, gemischte Zeitzonen (Agency/Stop),
Betriebstage mit DST-Wechsel im Fenster, nicht durchsetzbare Transferregeln (Tabelle oben).

Bekannte Grenzen:
* `continuous_pickup/drop_off` (Ein-/Ausstieg zwischen Halten) wird ignoriert – nur fehlende
  Optionen. GTFS-Flex (Zeitfenster/Zonen) nicht unterstützt; ohne Abfahrtszeiten bricht der
  Import ab. `timepoint=0` (Näherungszeiten) wird wie exakt behandelt. Wheelchair-Routing
  nicht exponiert.
* Ohne `transfers.txt` und ohne `--walk-radius-m` gibt es keine Fußwege zwischen Haltestellen;
  reine Fußweg-Verbindungen ohne Fahrzeug liefert RAPTOR nicht (Valhalla beim Verbraucher).
* BBox-Suche ist ein linearer Scan; Haltestellen-Metadaten liegen im Heap.
* Topologie im Heap skaliert mit Haltestellen/Routen/Fußwegen; Europa-Skalierung ist offen.
* Der **Import** lädt den ganzen Feed via `gtfs-structures` in den Speicher (KVV: ~740 MiB
  Spitze für 20 MB ZIP). Für DELFI/Europa ungeprüft und auf Mars derzeit nicht vertretbar.
* Snapshot-Dateien dürfen nicht verändert/abgeschnitten werden, solange ein Prozess sie
  mappt (SIGBUS). Neue Daten → neues Verzeichnis, Dienst umstellen, altes später löschen.
* Keine MOTIS-JSON-Kompatibilität, kein Realtime, keine Shapes.

Messwerte: [docs/measurements/2026-11-14-first-probe.md](docs/measurements/2026-11-14-first-probe.md).
Der Disk-/RAM-Limit-Spike folgt in RTP-76.

## Tests

```sh
cargo fmt --check && cargo clippy -j2 --all-targets -- -D warnings && cargo test -j2
```

Fixture `tests/fixtures/tiny` (handgeschrieben): Umstieg mit Fußweg vs. langsamere
Direktfahrt, Zugangs-/Abgangswege, Multi-Origin, Umstiegslimit, Pickup/Drop-off-Verbote,
Übernacht-Fahrt und `calendar_dates`-Ausnahme, Folgetag, mehrstufige Fußwege, gleichzeitige
Fahrten, ungültige Anfragen, Korruption/Abschneiden, Unveränderlichkeit, abgelehnte Feeds,
DST, konservativer Modus ohne erfundene Fußwege, Typ 3 vs. Radius-Fußwege; HTTP-Tests.
`tests/transfers.rs`: Mindestumsteigezeit (zu kurz/ausreichend/Standardwert/Duplikate),
Fußweg-Mindestzeiten, Start nicht verzögert, echte Ankunftszeiten, verbotene Transfers
(gleiche Haltestelle, verkettet, fahrtbezogen), Ablehnung im Standardmodus, Warnungen.
Ein Regelprüfer kontrolliert jeden Umstieg jeder gelieferten Verbindung; Mutationsproben
(Adapter-Aufschlag entfernt, Ablehnung entfernt) lassen die Tests scheitern.

## Lizenz

Für den eigenen Code ist noch keine Lizenz festgelegt. `vendor/vulture` steht unter
Apache-2.0 OR BlueOak-1.0.0 (Lizenztexte dort); lokale Änderung siehe `vendor/UPSTREAM.md`.
