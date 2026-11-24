//! Deutschlandticket validity (Tarifbestimmungen Deutschlandticket, TB-DTX
//! V16 of 2026-06-15, deutschlandtarifverbund.de; DB "Nahverkehrsfreigabe"
//! page, 2026-10).
//!
//! A ride is valid if
//! * the route belongs to a German feed (`de-…`), is a local mode (regional
//!   rail, S-Bahn, U-Bahn, tram, local bus, rail replacement) and is not run
//!   by a long-distance operator (DB Fernverkehr, Flix, Westbahn – these are
//!   excluded whatever their train category, even with RE numbers), or
//! * it is a DB Fernverkehr train inside one of the released sections
//!   (Anhang 3.1), boarding and alighting within the section,
//!
//! and every stop from boarding to alighting is in Germany or one of the
//! released cross-border stations (Anhang 3.2). Foreign stops are recognised
//! by their DHID country prefix (`at:`, `ch:` …). Not modelled: ICE 1520/1521
//! Rostock–Hamburg (released only until 2026-12-12), tourist/historic
//! services and mountain railways (excluded by mode where the feed says so),
//! ferries other than the urban ÖPNV ferries listed below, the sections
//! Erfurt–Gera and Dortmund–Dillenburg (see `SECTIONS`).
use crate::tt::Timetable;
use std::collections::HashMap;

/// Routes that may be boarded and, for partly released routes, the stop
/// positions that may be used (contiguously).
pub struct RouteFilter {
    /// Bitset over routes: route usable at all.
    pub mask: Vec<u64>,
    /// Partly usable routes: one flag per stop position.
    pub positions: HashMap<u32, Vec<bool>>,
}

impl RouteFilter {
    pub fn allows_route(&self, r: u32) -> bool {
        self.mask[r as usize / 64] >> (r % 64) & 1 == 1
    }

    /// Whether riding route `r` from `board` to `alight` is covered.
    pub fn allows_ride(&self, r: u32, board: usize, alight: usize) -> bool {
        self.allows_route(r)
            && self
                .positions
                .get(&r)
                .is_none_or(|p| (board..=alight).all(|i| p.get(i).copied().unwrap_or(false)))
    }
}

/// Released DB Fernverkehr sections (Anhang 3.1) as the stations along each
/// line (normalised names, see `norm`). A DB Fernverkehr ride is valid if it
/// boards, passes and alights only at stations of one section; this also
/// covers trains that currently end early (e.g. IC ending in Norden).
const SECTIONS: &[&[&str]] = &[
    &[
        "rostock hbf",
        "ribnitz-damgarten west",
        "velgast",
        "stralsund hbf",
    ],
    // Gäubahn; IC partly end in Singen or start in Stuttgart-Vaihingen
    &[
        "stuttgart hbf",
        "stuttgart-vaihingen",
        "böblingen",
        "herrenberg",
        "eutingen im gäu",
        "horb",
        "sulz",
        "oberndorf",
        "rottweil",
        "spaichingen",
        "tuttlingen",
        "engen",
        "singen",
        "radolfzell",
        "allensbach",
        "konstanz",
    ],
    &[
        "bremen hbf",
        "delmenhorst",
        "hude",
        "oldenburg hbf",
        "bad zwischenahn",
        "westerstede-ocholt",
        "augustfehn",
        "leer",
        "emden hbf",
        "emden außenhafen",
        "marienhafe",
        "marienhafe bahnhof",
        "norden",
        "norddeich",
        "norddeich mole",
    ],
    &[
        "dresden hbf",
        "tharandt",
        "freiberg",
        "flöha",
        "chemnitz hbf",
    ],
    // Erfurt–Gera (IC 51) and Dortmund–Dillenburg (IC 34 stopping
    // everywhere) are still in the tariff, but no such IC runs in 2026; they
    // are left out because other, non-released ICE/IC serve Erfurt–Weimar–
    // Jena and Dortmund–Hagen and would be matched by station alone.
];

/// Operators excluded from the Deutschlandticket regardless of category.
const LONG_DISTANCE: &[&str] = &[
    "db fernverkehr",
    "flixtrain",
    "flixbus",
    "flix ",
    "westbahn",
];

/// Ferries that are part of urban public transport (only these are valid).
const FERRY_OPERATORS: &[&str] = &[
    "hadag",
    "bvg",
    "berliner verkehrsbetriebe",
    "schlepp- und fährgesellschaft kiel",
    "sfk",
    "stadtverkehr lübeck",
    "reederei hiddensee",
];

/// Released foreign stations (Anhang 3.2), normalised names; an entry ending
/// in `*` matches as a prefix.
const BORDER: &[&str] = &[
    "tønder*",
    "tonder*",
    "wissembourg*",
    "lauterbourg*",
    "venlo*",
    "hengelo*",
    "enschede*",
    "arnhem*",
    "salzburg hbf",
    "salzburg hauptbahnhof",
    "kufstein*",
    // Außerfernbahn (only in traffic to/from Germany)
    "vils*",
    "pinswang*",
    "musau*",
    "reutte*",
    "pflach*",
    "heiterwang*",
    "bichlbach*",
    "lähn*",
    "lermoos*",
    "ehrwald*",
    "świnoujście*",
    "swinoujscie*",
    "zgorzelec*",
    "krzewina*",
    "riehen*",
    "basel bad*",
    "basel sbb*",
    "schaffhausen*",
    "thayngen*",
    "neuhausen*",
    "beringen*",
    "neunkirch*",
    "wilchingen*",
    "trasadingen*",
    "hrádek nad nisou*",
    "vejprty*",
    "varnsdorf*",
];

/// Lower case, `Hauptbahnhof` → `hbf`, parenthesised parts dropped
/// (`Oldenburg(Oldb) Hbf` → `oldenburg hbf`, `Singen (Htw)` → `singen`).
fn norm(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut depth = 0;
    for c in name.chars() {
        match c {
            '(' => {
                depth += 1;
                out.push(' ');
            }
            ')' => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.to_lowercase()
        .replace("hauptbahnhof", "hbf")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn border_ok(name: &str) -> bool {
    let n = norm(name);
    BORDER.iter().any(|b| match b.strip_suffix('*') {
        Some(p) => n.starts_with(p),
        None => n == *b,
    })
}

/// Country of a DHID-style stop id (`de-DELFI:at:42:…` → `at`).
fn country(id: &[u8]) -> Option<&[u8]> {
    let local = match id.iter().position(|&b| b == b':') {
        Some(i) => &id[i + 1..],
        None => id,
    };
    (local.len() > 3
        && local[2] == b':'
        && local[..2].iter().all(u8::is_ascii_lowercase)
        && local[3].is_ascii_digit())
    .then(|| &local[..2])
}

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    No,
    Local,
    LongDistance,
}

fn route_kind(feed: &str, agency: &str, route_type: u16) -> Kind {
    if !feed.starts_with("de-") {
        return Kind::No;
    }
    let a = agency.to_lowercase();
    if LONG_DISTANCE.iter().any(|x| a.starts_with(x)) {
        return if a.contains("db fernverkehr") {
            Kind::LongDistance
        } else {
            Kind::No
        };
    }
    match crate::motis::mode(route_type) {
        "REGIONAL_RAIL" | "REGIONAL_FAST_RAIL" | "SUBURBAN" | "SUBWAY" | "METRO" | "TRAM"
        | "RAIL" => Kind::Local,
        // local buses incl. rail replacement and demand-responsive buses
        "BUS" => Kind::Local,
        "FERRY" if FERRY_OPERATORS.iter().any(|x| a.contains(x)) => Kind::Local,
        _ => Kind::No,
    }
}

pub fn build(tt: &Timetable) -> RouteFilter {
    let n = tt.route_info.len();
    let mut mask = vec![0u64; n.div_ceil(64)];
    let mut positions = HashMap::new();
    let mut kinds: HashMap<u32, Kind> = HashMap::new();
    // per stop: 0 unknown, 1 usable, 2 foreign and not released
    let mut stop_ok = vec![0u8; tt.meta.n_stops];
    let mut usable = |s: u32| -> bool {
        let v = &mut stop_ok[s as usize];
        if *v == 0 {
            let id = tt.catalog.id_bytes(s as usize);
            *v = match country(id) {
                Some(c) if c != b"de" && !border_ok(&tt.catalog.name(s as usize)) => 2,
                _ => 1,
            };
        }
        *v == 1
    };
    for r in 0..n {
        let info = tt.route_info[r];
        let kind = *kinds.entry(info).or_insert_with(|| {
            let id = tt.route_info_id.get(info as usize);
            let feed = id.split(':').next().unwrap_or("");
            route_kind(
                feed,
                tt.route_info_agency.get(info as usize),
                tt.route_info_type[info as usize],
            )
        });
        let len = tt.n_route_stops(r);
        let flags: Vec<bool> = match kind {
            Kind::No => continue,
            Kind::Local => (0..len).map(|p| usable(tt.route_stop(r, p))).collect(),
            Kind::LongDistance => {
                let names: Vec<String> = (0..len)
                    .map(|p| norm(&tt.catalog.name(tt.route_stop(r, p) as usize)))
                    .collect();
                // the section covering most stops of this route
                SECTIONS
                    .iter()
                    .map(|sec| {
                        names
                            .iter()
                            .map(|x| sec.contains(&x.as_str()))
                            .collect::<Vec<_>>()
                    })
                    .max_by_key(|f| f.iter().filter(|&&x| x).count())
                    .unwrap_or_default()
            }
        };
        if flags.iter().filter(|&&x| x).count() < 2 {
            continue;
        }
        mask[r / 64] |= 1 << (r % 64);
        if flags.iter().any(|&x| !x) {
            positions.insert(r as u32, flags);
        }
    }
    RouteFilter { mask, positions }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules() {
        assert!(route_kind("de-DELFI", "DB Regio AG Bayern", 106) == Kind::Local);
        assert!(route_kind("de-DELFI", "DB Fernverkehr AG", 106) == Kind::LongDistance);
        assert!(route_kind("de-DELFI", "FlixTrain", 2) == Kind::No);
        assert!(route_kind("de-DELFI", "Arverio Baden-Württemberg", 106) == Kind::Local);
        assert!(route_kind("de-DELFI", "Busreisen X", 200) == Kind::No);
        assert!(
            route_kind("de-DELFI", "HADAG Seetouristik und Fährdienst AG", 1200) == Kind::Local
        );
        assert!(route_kind("de-DELFI", "Weiße Flotte", 1200) == Kind::No);
        assert!(route_kind("at-OEBB", "ÖBB", 106) == Kind::No);
        assert_eq!(country(b"de-DELFI:at:42:123:1"), Some(&b"at"[..]));
        assert_eq!(country(b"de-DELFI:de:08421:1008:31:1"), Some(&b"de"[..]));
        assert_eq!(country(b"de-VBB:900100001"), None);
        assert!(border_ok("Salzburg Hauptbahnhof"));
        assert!(border_ok("Basel Bad Bf"));
        assert!(!border_ok("Salzburg Taxham Europark"));
        assert!(!border_ok("Szczecin Główny"));
        assert!(route_kind("de-DELFI", "NordWestBahn", 109) == Kind::Local);
        assert!(route_kind("de-DELFI", "WESTbahn", 102) == Kind::No);
        assert_eq!(norm("Oldenburg(Oldb) Hbf"), "oldenburg hbf");
        assert_eq!(norm("Norddeich(Norden) Mole"), "norddeich mole");
        assert_eq!(norm("Singen (Htw)"), "singen");
        assert_eq!(norm("Dresden Hauptbahnhof"), "dresden hbf");
    }
}
