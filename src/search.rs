//! Lookup indexes written after the timetable (`write`) and read through
//! memory maps (`Index`):
//!
//! * **Stations** – served stops merged into one entry per station: stops of
//!   one `parent_station`, and stops with the same normalised name within
//!   `STATION_RADIUS_M` (platforms, duplicate feeds). A station is named and
//!   identified by a representative stop (preferably the parent station), so
//!   its id is a normal stop id. `geo_entry` lists every word start of every
//!   distinct normalised name, sorted by the text from there (prefix search).
//! * **Trip ids** – GTFS trip id → snapshot trips (hash-sorted).
//! * **Trip names** – normalised display name (`ICE 930` → `ice930`) and bare
//!   trip number (`930`) → GTFS trips (hash-sorted).
use crate::build::NONE;
use crate::catalog::Catalog;
use crate::store::{Array, ArrayWriter, Manifest, write_all};
use crate::tt::Strs;
use anyhow::Result;
use std::cmp::Ordering;
use std::collections::HashMap;
use std::path::Path;

/// Same-name stops closer than this are one station.
pub const STATION_RADIUS_M: f64 = 500.0;
/// Stops whose names match only after dropping leading words or adding the
/// town are one station if closer than this.
pub const MERGE_RADIUS_M: f64 = 300.0;

pub struct Index {
    pub stop_group: Array<u32>,
    pub group_off: Array<u32>,
    pub group_members: Array<u32>,
    pub group_rep: Array<u32>,
    pub group_weight: Array<u32>,
    /// MOTIS modes serving a station (bit `i` = `MODES[i]`).
    pub group_modes: Array<u32>,
    /// Town of a station (index into `area_name`, `NONE` = unknown).
    pub group_area: Array<u32>,
    pub area_name: Strs,
    pub geo_key: Strs,
    pub geo_key_group: Array<u32>,
    pub geo_entry: Array<u64>,
    pub trip_id_hash: Array<u64>,
    pub trip_id_gtfs: Array<u32>,
    pub gtfs_trips_off: Array<u32>,
    pub gtfs_trips: Array<u32>,
    pub trip_name_hash: Array<u64>,
    pub trip_name_gtfs: Array<u32>,
}

impl Index {
    pub fn open(dir: &Path, m: &Manifest) -> Result<Self> {
        Ok(Self {
            stop_group: Array::open(dir, "stop_group", m)?,
            group_off: Array::open(dir, "group_off", m)?,
            group_members: Array::open(dir, "group_members", m)?,
            group_rep: Array::open(dir, "group_rep", m)?,
            group_weight: Array::open(dir, "group_weight", m)?,
            group_modes: Array::open(dir, "group_modes", m)?,
            group_area: Array::open(dir, "group_area", m)?,
            area_name: Strs::open(dir, "area_name", m)?,
            geo_key: Strs::open(dir, "geo_key", m)?,
            geo_key_group: Array::open(dir, "geo_key_group", m)?,
            geo_entry: Array::open(dir, "geo_entry", m)?,
            trip_id_hash: Array::open(dir, "trip_id_hash", m)?,
            trip_id_gtfs: Array::open(dir, "trip_id_gtfs", m)?,
            gtfs_trips_off: Array::open(dir, "gtfs_trips_off", m)?,
            gtfs_trips: Array::open(dir, "gtfs_trips", m)?,
            trip_name_hash: Array::open(dir, "trip_name_hash", m)?,
            trip_name_gtfs: Array::open(dir, "trip_name_gtfs", m)?,
        })
    }

    pub fn members(&self, g: u32) -> &[u32] {
        &self.group_members
            [self.group_off[g as usize] as usize..self.group_off[g as usize + 1] as usize]
    }

    /// Snapshot trips of a GTFS trip (several for frequency-based trips).
    pub fn trips_of(&self, gtfs: u32) -> &[u32] {
        &self.gtfs_trips[self.gtfs_trips_off[gtfs as usize] as usize
            ..self.gtfs_trips_off[gtfs as usize + 1] as usize]
    }

    /// GTFS trip index for a (namespaced) GTFS trip id.
    pub fn gtfs_trip(&self, ids: &Strs, id: &str) -> Option<u32> {
        let h = hash(id.as_bytes());
        let lo = self.trip_id_hash.partition_point(|&x| x < h);
        (lo..self.trip_id_hash.len())
            .take_while(|&k| self.trip_id_hash[k] == h)
            .map(|k| self.trip_id_gtfs[k])
            .find(|&g| ids.get(g as usize) == id)
    }

    /// GTFS trips whose normalised name key equals `key` (unverified hash
    /// matches are filtered by the caller).
    pub fn trips_named(&self, key: &str) -> &[u32] {
        let h = hash(key.as_bytes());
        let lo = self.trip_name_hash.partition_point(|&x| x < h);
        let hi = self.trip_name_hash.partition_point(|&x| x <= h);
        &self.trip_name_gtfs[lo..hi]
    }

    /// Stations matching a free-text query, best first: `(group, score)`.
    /// Only stations served by one of the modes in `modes` (bit set over
    /// `MODES`) are returned.
    pub fn geocode(&self, query: &str, limit: usize, modes: u32) -> Vec<(u32, f64)> {
        let mut best: HashMap<u32, f64> = HashMap::new();
        let rail = mode_mask(|m| crate::motis::RAIL.contains(&m));
        let mut town_is: HashMap<u32, bool> = HashMap::new();
        for q in query_variants(query) {
            let tokens: Vec<&str> = q.split(' ').filter(|t| !t.is_empty()).collect();
            let Some(primary) = tokens.iter().max_by_key(|t| t.len()) else {
                continue;
            };
            let key_at = |e: u64| {
                let k = (e >> 16) as usize;
                &self.geo_key.get(k).as_bytes()[(e & 0xffff) as usize..]
            };
            let p = primary.as_bytes();
            let lo = self.geo_entry.partition_point(|&e| key_at(e) < p);
            for k in (lo..self.geo_entry.len()).take(MAX_SCAN) {
                let e = self.geo_entry[k];
                if !key_at(e).starts_with(p) {
                    break;
                }
                let key_id = (e >> 16) as usize;
                let key = self.geo_key.get(key_id);
                let words: Vec<&str> = key.split(' ').collect();
                if !tokens
                    .iter()
                    .all(|t| words.iter().any(|w| word_matches(w, t)))
                {
                    continue;
                }
                let g = self.geo_key_group[key_id];
                let gm = self.group_modes[g as usize];
                if gm & modes == 0 {
                    continue;
                }
                // busy stations and train stations first
                let mut score = 2.0 * f64::from(self.group_weight[g as usize]).ln_1p();
                if gm & rail != 0 {
                    score += 15.0;
                }
                // the query is the station's town (`Ulm` → `Ulm Hbf` before a
                // bus stop `Ulm` in France)
                let a = self.group_area[g as usize];
                if a != NONE
                    && *town_is
                        .entry(a)
                        .or_insert_with(|| norm(self.area_name.get(a as usize)) == q)
                {
                    score += 60.0;
                }
                if key == q {
                    score += 100.0;
                } else if key.starts_with(q.as_str()) {
                    score += 50.0;
                } else if key.ends_with(&format!(" {q}")) {
                    // `S+U Berlin Hauptbahnhof` for `berlin hbf`
                    score += 45.0;
                } else if key.contains(&format!(" {q}")) {
                    score += 30.0;
                } else if words.first().is_some_and(|w| w.starts_with(tokens[0])) {
                    score += 20.0;
                }
                // fewer extra words = closer match
                score -= words.len().saturating_sub(tokens.len()) as f64;
                let b = best.entry(g).or_insert(f64::MIN);
                if score > *b {
                    *b = score;
                }
            }
        }
        let mut out: Vec<(u32, f64)> = best.into_iter().collect();
        out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        out.truncate(limit);
        out
    }
}

/// Words starting a platform suffix of a stop name (`Gl. 11-12`).
const PLATFORM_WORDS: &[&str] = &[
    "gl", "gleis", "bstg", "bussteig", "steig", "spoor", "perron", "track", "platform", "quai",
    "voie",
];

/// MOTIS modes as returned by `motis::mode`, bit `i` in `group_modes`.
pub const MODES: [&str; 16] = [
    "TRAM",
    "SUBWAY",
    "METRO",
    "REGIONAL_RAIL",
    "HIGHSPEED_RAIL",
    "LONG_DISTANCE",
    "NIGHT_RAIL",
    "SUBURBAN",
    "BUS",
    "COACH",
    "FERRY",
    "AIRPLANE",
    "AERIAL_LIFT",
    "FUNICULAR",
    "RAIL",
    "OTHER",
];

fn mode_bit(m: &str) -> u32 {
    MODES
        .iter()
        .position(|&x| x == m)
        .map_or(1 << 15, |i| 1 << i)
}

/// Bit set of the modes `ok` accepts.
pub fn mode_mask(ok: impl Fn(&str) -> bool) -> u32 {
    MODES
        .iter()
        .enumerate()
        .filter(|(_, m)| ok(m))
        .fold(0, |a, (i, _)| a | 1 << i)
}

/// Query token `t` matches name word `w`: a prefix of it, or `bf` for
/// `hbf` (`Stuttgart Bf`).
fn word_matches(w: &str, t: &str) -> bool {
    w.starts_with(t) || (t == "bf" && w == "hbf")
}

/// Upper bound of index entries looked at per query (short prefixes).
const MAX_SCAN: usize = 200_000;

pub fn hash(b: &[u8]) -> u64 {
    // FNV-1a
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &x in b {
        h ^= u64::from(x);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn fold(c: char, out: &mut String) {
    let r = match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ą' | 'ā' => "a",
        'æ' => "ae",
        'ç' | 'ć' | 'č' => "c",
        'ď' => "d",
        'è' | 'é' | 'ê' | 'ë' | 'ę' | 'ě' | 'ē' => "e",
        'ì' | 'í' | 'î' | 'ï' => "i",
        'ł' => "l",
        'ñ' | 'ń' | 'ň' => "n",
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ő' => "o",
        'œ' => "oe",
        'ř' => "r",
        'ś' | 'š' | 'ş' => "s",
        'ß' => "ss",
        'ť' => "t",
        'ù' | 'ú' | 'û' | 'ü' | 'ů' | 'ű' => "u",
        'ý' | 'ÿ' => "y",
        'ź' | 'ż' | 'ž' => "z",
        c if c.is_alphanumeric() => {
            out.push(c);
            return;
        }
        _ => " ",
    };
    out.push_str(r);
}

/// Search form of a stop name: lower case, diacritics folded, punctuation as
/// spaces, common abbreviations unified (`Hauptbahnhof` → `hbf`,
/// `Bahnhof`/`Bhf` → `bf`, `Straße` → `str`, `Sankt` → `st`).
pub fn norm(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    for c in name.chars().flat_map(char::to_lowercase) {
        fold(c, &mut s);
    }
    let mut out = String::with_capacity(s.len());
    for w in s.split_whitespace() {
        let w = match w {
            "hauptbahnhof" | "hbhf" => "hbf",
            "bahnhof" | "bhf" => "bf",
            "strasse" => "str",
            "sankt" => "st",
            w => {
                if let Some(p) = w.strip_suffix("strasse") {
                    push_word(&mut out, &format!("{p}str"));
                    continue;
                } else {
                    w
                }
            }
        };
        push_word(&mut out, w);
    }
    out
}

fn push_word(out: &mut String, w: &str) {
    if w.is_empty() {
        return;
    }
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(w);
}

/// The normalised query, plus a variant with `ae`/`oe`/`ue` read as umlauts
/// (`Muenchen` → `munchen`).
fn query_variants(q: &str) -> Vec<String> {
    let a = norm(q);
    let b = a.replace("ae", "a").replace("oe", "o").replace("ue", "u");
    if b != a { vec![a, b] } else { vec![a] }
}

/// Name key of a trip for the train-number search: alphanumerics only,
/// lower case (`ICE 930` → `ice930`).
pub fn name_key(s: &str) -> String {
    let mut out = String::new();
    let mut prev_digit = false;
    for c in s
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
    {
        let digit = c.is_ascii_digit();
        // leading zeros of a number are dropped (`RE 014506` = `RE 14506`)
        if c == '0' && !prev_digit {
            continue;
        }
        prev_digit = digit;
        out.push(c);
    }
    out
}

fn haversine(a: [f64; 2], b: [f64; 2]) -> f64 {
    let (la1, la2) = (a[0].to_radians(), b[0].to_radians());
    let dla = la2 - la1;
    let dlo = (b[1] - a[1]).to_radians();
    let h = (dla / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlo / 2.0).sin().powi(2);
    2.0 * 6_371_008.8 * h.sqrt().asin()
}

/// German independent cities by district code (Destatis Kreisschlüssel):
/// the town for stop ids `de:<code>:…` whose names lack it.
const TOWNS: &[(&str, &str)] = &[
    ("01001", "Flensburg"),
    ("01002", "Kiel"),
    ("01003", "Lübeck"),
    ("02000", "Hamburg"),
    ("03101", "Braunschweig"),
    ("03102", "Salzgitter"),
    ("03103", "Wolfsburg"),
    ("03401", "Delmenhorst"),
    ("03402", "Emden"),
    ("03404", "Osnabrück"),
    ("03405", "Wilhelmshaven"),
    ("04011", "Bremen"),
    ("04012", "Bremerhaven"),
    ("05111", "Düsseldorf"),
    ("05112", "Duisburg"),
    ("05113", "Essen"),
    ("05114", "Krefeld"),
    ("05116", "Mönchengladbach"),
    ("05117", "Mülheim an der Ruhr"),
    ("05119", "Oberhausen"),
    ("05122", "Solingen"),
    ("05124", "Wuppertal"),
    ("05314", "Bonn"),
    ("05315", "Köln"),
    ("05316", "Leverkusen"),
    ("05512", "Bottrop"),
    ("05513", "Gelsenkirchen"),
    ("05515", "Münster"),
    ("05711", "Bielefeld"),
    ("05911", "Bochum"),
    ("05913", "Dortmund"),
    ("05914", "Hagen"),
    ("05915", "Hamm"),
    ("05916", "Herne"),
    ("06411", "Darmstadt"),
    ("06412", "Frankfurt am Main"),
    ("06413", "Oldenburg (Oldenburg)"),
    ("06414", "Wiesbaden"),
    ("06438", "Offenbach am Main"),
    ("06611", "Kassel"),
    ("07111", "Koblenz"),
    ("07211", "Trier"),
    ("07311", "Frankenthal (Pfalz)"),
    ("07312", "Kaiserslautern"),
    ("07313", "Landau in der Pfalz"),
    ("07314", "Ludwigshafen am Rhein"),
    ("07315", "Mainz"),
    ("07317", "Pirmasens"),
    ("07318", "Speyer"),
    ("07319", "Worms"),
    ("07320", "Zweibrücken"),
    ("08111", "Stuttgart"),
    ("08119", "Remscheid"),
    ("08121", "Heilbronn"),
    ("08211", "Baden-Baden"),
    ("08212", "Karlsruhe"),
    ("08221", "Heidelberg"),
    ("08222", "Mannheim"),
    ("08231", "Pforzheim"),
    ("08311", "Freiburg im Breisgau"),
    ("08421", "Ulm"),
    ("09161", "Ingolstadt"),
    ("09162", "München"),
    ("09163", "Rosenheim"),
    ("09261", "Landshut"),
    ("09262", "Passau"),
    ("09278", "Straubing"),
    ("09361", "Amberg"),
    ("09362", "Regensburg"),
    ("09363", "Weiden i. d. Opf"),
    ("09461", "Bamberg"),
    ("09462", "Bayreuth"),
    ("09463", "Coburg"),
    ("09464", "Hof"),
    ("09561", "Ansbach"),
    ("09562", "Erlangen"),
    ("09563", "Fürth"),
    ("09564", "Nürnberg"),
    ("09565", "Schwabach"),
    ("09661", "Aschaffenburg"),
    ("09662", "Schweinfurt"),
    ("09663", "Würzburg"),
    ("09761", "Augsburg"),
    ("09762", "Kaufbeuren"),
    ("09763", "Kempten (Allgäu)"),
    ("09764", "Memmingen"),
    ("09775", "Neustadt an der Weinstraße"),
    ("10043", "Neumünster"),
    ("11000", "Berlin"),
    ("12051", "Brandenburg an der Havel"),
    ("12052", "Cottbus"),
    ("12053", "Frankfurt (Oder)"),
    ("12069", "Potsdam"),
    ("13003", "Rostock"),
    ("13004", "Schwerin"),
    ("14511", "Chemnitz"),
    ("14612", "Dresden"),
    ("14713", "Leipzig"),
    ("15001", "Dessau-Roßlau"),
    ("15002", "Halle (Saale)"),
    ("15003", "Magdeburg"),
    ("16051", "Erfurt"),
    ("16052", "Gera"),
    ("16053", "Jena"),
    ("16054", "Suhl"),
    ("16055", "Weimar"),
];

fn town(code: &[u8]) -> Option<&'static str> {
    let k = std::str::from_utf8(code.strip_prefix(b"de:")?).ok()?;
    TOWNS
        .binary_search_by(|(c, _)| c.cmp(&k))
        .ok()
        .map(|i| TOWNS[i].1)
}

/// Municipality part of a DHID-style stop id (`de-DELFI:de:08111:6115` →
/// `de:08111`).
fn area_code(id: &[u8]) -> Option<&[u8]> {
    let local = match id.iter().position(|&b| b == b':') {
        Some(i) => &id[i + 1..],
        None => id,
    };
    if local.len() < 4 || local[2] != b':' || !local[..2].iter().all(u8::is_ascii_lowercase) {
        return None;
    }
    let end = local[3..]
        .iter()
        .position(|&b| b == b':')
        .map_or(local.len(), |p| p + 3);
    (end > 3 && local[3..end].iter().all(u8::is_ascii_digit)).then(|| &local[..end])
}

struct Uf(Vec<u32>);
impl Uf {
    fn find(&mut self, mut x: u32) -> u32 {
        while self.0[x as usize] != x {
            let p = self.0[self.0[x as usize] as usize];
            self.0[x as usize] = p;
            x = p;
        }
        x
    }
    fn union(&mut self, a: u32, b: u32) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            let (lo, hi) = if a < b { (a, b) } else { (b, a) };
            self.0[hi as usize] = lo;
        }
    }
}

/// Writes all index arrays of the snapshot in `dir` into `m`.
pub fn write(dir: &Path, m: &mut Manifest, n_stops: usize) -> Result<()> {
    let t = std::time::Instant::now();
    write_stations(dir, m, n_stops)?;
    eprintln!("index: stations {:.1}s", t.elapsed().as_secs_f64());
    let t = std::time::Instant::now();
    write_trips(dir, m)?;
    eprintln!("index: trips {:.1}s", t.elapsed().as_secs_f64());
    Ok(())
}

fn write_stations(dir: &Path, m: &mut Manifest, n: usize) -> Result<()> {
    let cat = Catalog::open(&dir.join("catalog.bin"), n)?;
    let stop_days: Array<u32> = Array::open(dir, "stop_days", m)?;
    let stop_route_off: Array<u32> = Array::open(dir, "stop_route_off", m)?;
    let stop_route: Array<u32> = Array::open(dir, "stop_route", m)?;
    let route_trip_off: Array<u32> = Array::open(dir, "route_trip_off", m)?;
    let route_info: Array<u32> = Array::open(dir, "route_info", m)?;
    let route_info_type: Array<u16> = Array::open(dir, "route_info_type", m)?;
    let served = |s: usize| stop_days[s] != NONE;
    // departures-ish weight: trips of all routes through the stop
    let weight = |s: usize| -> u64 {
        (stop_route_off[s] as usize..stop_route_off[s + 1] as usize)
            .map(|k| {
                let r = stop_route[k] as usize;
                let trips = u64::from(route_trip_off[r + 1] - route_trip_off[r]);
                // rail stations first: a train counts like ten buses
                let rail = matches!(
                    route_info_type[route_info[r] as usize],
                    2 | 100..=199 | 400..=404
                );
                if rail { 10 * trips } else { trips }
            })
            .sum()
    };
    let modes = |s: usize| -> u32 {
        (stop_route_off[s] as usize..stop_route_off[s + 1] as usize)
            .map(|k| {
                let t = route_info_type[route_info[stop_route[k] as usize] as usize];
                mode_bit(crate::motis::mode(t))
            })
            .fold(0, |a, b| a | b)
    };
    let parent = |s: usize| -> Option<usize> { cat.parent(s) };
    let mut uf = Uf((0..n as u32).collect());
    let mut include = vec![false; n];
    let mut keys: Vec<String> = vec![String::new(); n];
    for s in 0..n {
        if !served(s) {
            continue;
        }
        include[s] = true;
        keys[s] = norm(&cat.name(s));
        // the whole parent chain (bounded against cycles)
        let mut c = s;
        for _ in 0..4 {
            match parent(c) {
                Some(p) if p != c => {
                    include[p] = true;
                    uf.union(s as u32, p as u32);
                    c = p;
                }
                _ => break,
            }
        }
    }
    // Towns from DHID municipality codes (`de:08111:…`): the first word most
    // stop names of the code start with (`Stuttgart`). Networks that name
    // stops without the town (`Hauptbahnhof (tief)`) become findable as
    // `Stuttgart Hbf`.
    let mut area_words: HashMap<Vec<u8>, HashMap<String, (u32, String)>> = HashMap::new();
    let mut area_total: HashMap<Vec<u8>, u32> = HashMap::new();
    for (s, key) in keys.iter().enumerate() {
        if key.is_empty() {
            continue;
        }
        let Some(code) = area_code(cat.id_bytes(s)) else {
            continue;
        };
        *area_total.entry(code.to_vec()).or_default() += 1;
        let name = cat.name(s);
        let first = name
            .split(|c: char| c.is_whitespace() || c == ',' || c == '(' || c == '/')
            .next()
            .unwrap_or("");
        let k = norm(first);
        if k.is_empty() || k.contains(' ') || k.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let e = area_words
            .entry(code.to_vec())
            .or_default()
            .entry(k)
            .or_insert((0, first.to_owned()));
        e.0 += 1;
    }
    let mut area_of_code: HashMap<Vec<u8>, u32> = HashMap::new();
    let mut area_names: Vec<String> = Vec::new();
    for (code, words) in area_words {
        let total = area_total[&code];
        if let Some(t) = town(&code) {
            area_of_code.insert(code, area_names.len() as u32);
            area_names.push(t.to_owned());
            continue;
        }
        if let Some((_, (c, word))) = words
            .into_iter()
            .max_by(|a, b| a.1.0.cmp(&b.1.0).then_with(|| b.0.cmp(&a.0)))
            && c >= 3
            && c * 5 >= total
        {
            area_of_code.insert(code, area_names.len() as u32);
            area_names.push(word);
        }
    }
    drop(area_total);
    // Same station: stops with the same name within the radius, and stops
    // within `MERGE_RADIUS_M` whose names match after dropping leading words
    // (`S+U Berlin Hauptbahnhof` ~ `Berlin Hbf`) or adding the town
    // (`Hauptbahnhof` in Stuttgart ~ `Stuttgart Hbf` of another feed); at
    // least one of the two must carry that name itself.
    let stop_town = |s: usize| -> Option<String> {
        area_code(cat.id_bytes(s))
            .and_then(|c| area_of_code.get(c))
            .map(|&a| norm(&area_names[a as usize]))
    };
    let mut named: Vec<(String, bool, u32, f64, f64)> = Vec::new();
    // names cut before a platform suffix (`Köln Messe/De Gl.11-12`): may be
    // truncated, matched as a prefix below
    let mut cut: Vec<(String, u32, f64, f64)> = Vec::new();
    for s in (0..n).filter(|&s| served(s) && !keys[s].is_empty()) {
        let Some([lat, lon]) = cat.coord(s) else {
            continue;
        };
        let k = &keys[s];
        named.push((k.clone(), false, s as u32, lat, lon));
        let words: Vec<&str> = k.split(' ').collect();
        for i in 1..words.len().saturating_sub(1) {
            named.push((words[i..].join(" "), true, s as u32, lat, lon));
        }
        // `Köln Messe/Deutz Bf` ~ `Köln Messe/Deutz` (foreign feed copies)
        if words.len() >= 2 && matches!(*words.last().unwrap(), "bf" | "hbf") {
            named.push((words[..words.len() - 1].join(" "), true, s as u32, lat, lon));
        }
        if let Some(i) = (2..words.len().saturating_sub(1)).find(|&i| {
            PLATFORM_WORDS.contains(&words[i])
                && words[i + 1].starts_with(|c: char| c.is_ascii_digit())
        }) {
            let base = words[..i].join(" ");
            named.push((base.clone(), true, s as u32, lat, lon));
            if base.len() >= 8 {
                cut.push((base, s as u32, lat, lon));
            }
        }
        if let Some(t) = stop_town(s)
            && !k.starts_with(&format!("{t} "))
            && *k != t
        {
            named.push((format!("{t} {k}"), true, s as u32, lat, lon));
        }
    }
    named.sort_by(|a, b| a.0.cmp(&b.0).then(a.3.total_cmp(&b.3)));
    let dlat = STATION_RADIUS_M / 111_000.0;
    let mut i = 0;
    while i < named.len() {
        let mut j = i;
        while j < named.len() && named[j].0 == named[i].0 {
            j += 1;
        }
        if named[i..j].iter().any(|x| !x.1) {
            for a in i..j {
                for b in a + 1..j {
                    let (x, y) = (&named[a], &named[b]);
                    if y.3 - x.3 > dlat {
                        break;
                    }
                    if x.1 && y.1 {
                        continue;
                    }
                    let max = if x.1 || y.1 {
                        MERGE_RADIUS_M
                    } else {
                        STATION_RADIUS_M
                    };
                    if haversine([x.3, x.4], [y.3, y.4]) <= max {
                        uf.union(x.2, y.2);
                    }
                }
            }
        }
        i = j;
    }
    for (base, s, lat, lon) in cut {
        let lo = named.partition_point(|x| x.0.as_str() < base.as_str());
        for x in named[lo..]
            .iter()
            .take_while(|x| x.0.starts_with(base.as_str()))
            .take(64)
        {
            if !x.1 && haversine([lat, lon], [x.3, x.4]) <= MERGE_RADIUS_M {
                uf.union(s, x.2);
            }
        }
    }
    drop(named);
    // groups
    let mut group_of_root: HashMap<u32, u32> = HashMap::new();
    let mut stop_group = vec![NONE; n];
    let mut members: Vec<Vec<u32>> = Vec::new();
    let mut parents: Vec<Vec<u32>> = Vec::new();
    for s in 0..n {
        if !include[s] {
            continue;
        }
        let r = uf.find(s as u32);
        let g = *group_of_root.entry(r).or_insert_with(|| {
            members.push(Vec::new());
            parents.push(Vec::new());
            (members.len() - 1) as u32
        });
        stop_group[s] = g;
        if served(s) {
            members[g as usize].push(s as u32);
        } else {
            parents[g as usize].push(s as u32);
        }
    }
    drop(group_of_root);
    drop(uf);
    let ng = members.len();
    let mut group_area = Vec::with_capacity(ng);
    let mut group_off = Vec::with_capacity(ng + 1);
    let mut group_members = Vec::new();
    let mut group_rep = Vec::with_capacity(ng);
    let mut group_weight = Vec::with_capacity(ng);
    let mut group_modes = Vec::with_capacity(ng);
    let mut key_strs: Vec<(String, u32)> = Vec::new();
    group_off.push(0u32);
    for g in 0..ng {
        let mem = &members[g];
        let w: Vec<u64> = mem.iter().map(|&s| weight(s as usize)).collect();
        let total: u64 = w.iter().sum();
        // representative: a stop with a DHID (`de:08421:1008`, the national
        // feed rather than a foreign copy), then a parent station, then the
        // most service (a parent's is that of its stops); ties by id (stable
        // across rebuilds)
        let dhid = |s: u32| area_code(cat.id_bytes(s as usize)).is_some();
        let pw = |p: u32| -> u64 {
            mem.iter()
                .zip(&w)
                .filter(|(s, _)| parent(**s as usize) == Some(p as usize))
                .map(|(_, w)| *w)
                .sum()
        };
        let rep = parents[g]
            .iter()
            .map(|&p| (p, true, pw(p)))
            .chain(mem.iter().zip(&w).map(|(&s, &w)| (s, false, w)))
            .max_by(|a, b| {
                dhid(a.0)
                    .cmp(&dhid(b.0))
                    .then(a.1.cmp(&b.1))
                    .then(a.2.cmp(&b.2))
                    .then_with(|| cat.id_bytes(b.0 as usize).cmp(cat.id_bytes(a.0 as usize)))
            })
            .map(|x| x.0)
            .unwrap_or(mem[0]);
        group_rep.push(rep);
        group_weight.push(total.min(u64::from(u32::MAX)) as u32);
        group_modes.push(mem.iter().fold(0, |a, &s| a | modes(s as usize)));
        group_members.extend_from_slice(mem);
        group_off.push(group_members.len() as u32);
        let mut ks: Vec<String> = mem
            .iter()
            .map(|&s| std::mem::take(&mut keys[s as usize]))
            .chain(std::iter::once(norm(&cat.name(rep as usize))))
            .filter(|k| !k.is_empty())
            .collect();
        let area = std::iter::once(rep)
            .chain(mem.iter().copied())
            .find_map(|s| area_code(cat.id_bytes(s as usize)))
            .and_then(|c| area_of_code.get(c).copied())
            .unwrap_or(NONE);
        group_area.push(area);
        if area != NONE {
            let a = norm(&area_names[area as usize]);
            let prefix = format!("{a} ");
            let extra: Vec<String> = ks
                .iter()
                .filter(|k| **k != a && !k.starts_with(&prefix) && !k.contains(&format!(" {a}")))
                .map(|k| format!("{a} {k}"))
                .collect();
            ks.extend(extra);
        }
        ks.sort();
        ks.dedup();
        key_strs.extend(ks.into_iter().map(|k| (k, g as u32)));
    }
    drop(members);
    drop(parents);
    drop(keys);
    write_all(dir, "stop_group", &stop_group, m)?;
    write_all(dir, "group_off", &group_off, m)?;
    write_all(dir, "group_members", &group_members, m)?;
    write_all(dir, "group_rep", &group_rep, m)?;
    write_all(dir, "group_weight", &group_weight, m)?;
    write_all(dir, "group_modes", &group_modes, m)?;
    write_all(dir, "group_area", &group_area, m)?;
    let mut off = ArrayWriter::<u64>::create(dir, "area_name.off")?;
    let mut bytes = ArrayWriter::<u8>::create(dir, "area_name.bytes")?;
    off.push(0)?;
    for a in &area_names {
        bytes.extend(a.bytes())?;
        off.push(bytes.len())?;
    }
    off.finish(m)?;
    bytes.finish(m)?;
    drop((
        stop_group,
        group_off,
        group_members,
        group_rep,
        group_weight,
        group_modes,
    ));
    // word-start entries, sorted by the text from the word on
    let mut entries: Vec<u64> = Vec::new();
    for (k, (key, _)) in key_strs.iter().enumerate() {
        let b = key.as_bytes();
        for (o, _) in b
            .iter()
            .enumerate()
            .filter(|&(o, _)| o == 0 || b[o - 1] == b' ')
        {
            if o <= 0xffff {
                entries.push((k as u64) << 16 | o as u64);
            }
        }
    }
    let text = |e: u64| &key_strs[(e >> 16) as usize].0.as_bytes()[(e & 0xffff) as usize..];
    entries.sort_unstable_by(|&a, &b| match text(a).cmp(text(b)) {
        Ordering::Equal => a.cmp(&b),
        o => o,
    });
    let mut off = ArrayWriter::<u64>::create(dir, "geo_key.off")?;
    let mut bytes = ArrayWriter::<u8>::create(dir, "geo_key.bytes")?;
    off.push(0)?;
    for (k, _) in &key_strs {
        bytes.extend(k.bytes())?;
        off.push(bytes.len())?;
    }
    off.finish(m)?;
    bytes.finish(m)?;
    let kg: Vec<u32> = key_strs.iter().map(|x| x.1).collect();
    write_all(dir, "geo_key_group", &kg, m)?;
    write_all(dir, "geo_entry", &entries, m)?;
    Ok(())
}

fn write_trips(dir: &Path, m: &mut Manifest) -> Result<()> {
    let ids = Strs::open(dir, "gtfs_trip_id", m)?;
    let short = Strs::open(dir, "gtfs_trip_short", m)?;
    let display = Strs::open(dir, "gtfs_trip_display", m)?;
    let line = Strs::open(dir, "route_info_line", m)?;
    let trip_gtfs: Array<u32> = Array::open(dir, "trip_gtfs", m)?;
    let route_trip_off: Array<u32> = Array::open(dir, "route_trip_off", m)?;
    let route_info: Array<u32> = Array::open(dir, "route_info", m)?;
    let n_gtfs = ids.len();
    // GTFS trip → snapshot trips (CSR)
    let mut off = vec![0u32; n_gtfs + 1];
    for &g in trip_gtfs.iter() {
        off[g as usize + 1] += 1;
    }
    for i in 0..n_gtfs {
        off[i + 1] += off[i];
    }
    let mut fill = off.clone();
    let mut trips = vec![0u32; trip_gtfs.len()];
    // the route (→ line) of one snapshot trip per GTFS trip
    let mut info = vec![NONE; n_gtfs];
    let mut r = 0usize;
    for (t, &g) in trip_gtfs.iter().enumerate() {
        while route_trip_off[r + 1] as usize <= t {
            r += 1;
        }
        let f = &mut fill[g as usize];
        trips[*f as usize] = t as u32;
        *f += 1;
        info[g as usize] = route_info[r];
    }
    drop(fill);
    write_all(dir, "gtfs_trips_off", &off, m)?;
    write_all(dir, "gtfs_trips", &trips, m)?;
    drop((off, trips));
    let mut by_id: Vec<(u64, u32)> = (0..n_gtfs as u32)
        .map(|g| (hash(ids.get(g as usize).as_bytes()), g))
        .collect();
    by_id.sort_unstable();
    write_all(
        dir,
        "trip_id_hash",
        &by_id.iter().map(|x| x.0).collect::<Vec<_>>(),
        m,
    )?;
    write_all(
        dir,
        "trip_id_gtfs",
        &by_id.iter().map(|x| x.1).collect::<Vec<_>>(),
        m,
    )?;
    drop(by_id);
    let mut by_name: Vec<(u64, u32)> = Vec::new();
    for (g, &inf) in info.iter().enumerate() {
        if inf == NONE {
            continue; // not in the snapshot period
        }
        for k in trip_keys(&display, &short, &line, g, inf as usize) {
            by_name.push((hash(k.as_bytes()), g as u32));
        }
    }
    by_name.sort_unstable();
    by_name.dedup();
    write_all(
        dir,
        "trip_name_hash",
        &by_name.iter().map(|x| x.0).collect::<Vec<_>>(),
        m,
    )?;
    write_all(
        dir,
        "trip_name_gtfs",
        &by_name.iter().map(|x| x.1).collect::<Vec<_>>(),
        m,
    )?;
    Ok(())
}

/// Name keys a GTFS trip is found by: its display name (or line + trip
/// number) and the bare trip number.
pub fn trip_keys(display: &Strs, short: &Strs, line: &Strs, g: usize, info: usize) -> Vec<String> {
    let (d, s) = (display.get(g), short.get(g));
    let mut out = Vec::new();
    if !d.is_empty() {
        out.push(name_key(d));
    }
    if !s.is_empty() {
        out.push(name_key(&format!("{} {s}", line.get(info))));
        out.push(name_key(s));
    }
    out.retain(|k| !k.is_empty());
    out.sort();
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names() {
        assert_eq!(norm("Ulm Hauptbahnhof"), "ulm hbf");
        assert_eq!(norm("Ulm Hbf"), "ulm hbf");
        assert_eq!(norm("Freital-Hainsberg"), "freital hainsberg");
        assert_eq!(norm("München Hbf (tief)"), "munchen hbf tief");
        assert_eq!(norm("Oldenburg(Oldb) Hbf"), "oldenburg oldb hbf");
        assert_eq!(norm("Bahnhofstraße"), "bahnhofstr");
        assert_eq!(norm("Berlin Hbf."), "berlin hbf");
        assert_eq!(norm("Frankfurt(M) Hauptbahnhof"), "frankfurt m hbf");
        assert_eq!(query_variants("Muenchen"), vec!["muenchen", "munchen"]);
        assert_eq!(name_key("ICE 930"), "ice930");
        assert_eq!(name_key("RE 014506"), "re14506");
        assert_eq!(name_key("S 10"), "s10");
        assert_eq!(area_code(b"de-DELFI:de:08111:6115"), Some(&b"de:08111"[..]));
        assert_eq!(area_code(b"de-DELFI:de:08111"), Some(&b"de:08111"[..]));
        assert_eq!(area_code(b"de-VBB:900100001"), None);
        assert_eq!(area_code(b"X"), None);
        assert_eq!(town(b"de:08111"), Some("Stuttgart"));
        assert!(TOWNS.windows(2).all(|w| w[0].0 < w[1].0));
    }
}
