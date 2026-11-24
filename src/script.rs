//! MOTIS/nigiri-compatible feed scripts (`script:` of a MOTIS dataset).
//!
//! A dataset may name a Lua script that adjusts the feed while it is
//! imported, e.g. Transitous' `de-DELFI.lua` turns the DB train number
//! `000119` of an ICE into the display name `ICE 119`. The script defines
//! any of the global functions `process_agency(agency)`,
//! `process_route(route)` and `process_trip(trip)`; returning `false` from
//! `process_route`/`process_trip` drops the route/trip. Supported methods
//! mirror nigiri's script API:
//!
//! * agency: `get_id get_name set_name get_url set_url`
//! * route: `get_id get_short_name set_short_name get_long_name
//!   set_long_name get_route_type set_route_type get_color set_color
//!   get_text_color set_text_color get_agency`
//! * trip: `get_id get_short_name set_short_name get_display_name
//!   set_display_name get_headsign set_headsign get_route set_route_type
//!   get_vehicle_type_short_name`; `set_compulsory_reservation` and
//!   `set_clasz` are accepted and ignored.
//!
//! A runtime error inside a call is counted and leaves that object as it
//! was. `require 'scripts.x'` resolves relative to the script's directory
//! and its parent (the MOTIS config directory).
use anyhow::{Context, Result};
use mlua::{Function, Lua, UserData, UserDataMethods, Value};
use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

#[derive(Clone, Debug, Default)]
pub struct Agency {
    pub id: String,
    pub name: String,
    pub url: String,
}

#[derive(Clone, Debug, Default)]
pub struct Route {
    pub id: String,
    pub short_name: String,
    pub long_name: String,
    pub route_type: u16,
    /// Hex without `#`, empty if none.
    pub color: String,
    pub text_color: String,
    /// Index into the agencies (`usize::MAX`: none).
    pub agency: usize,
}

#[derive(Clone, Debug, Default)]
pub struct Trip {
    pub id: String,
    pub short_name: String,
    /// Set by the script only.
    pub display_name: Option<String>,
    pub headsign: String,
    /// Index into the routes.
    pub route: usize,
    /// Trip-level route type set by the script.
    pub route_type: Option<u16>,
}

#[derive(Default)]
struct Shared {
    agencies: Vec<Agency>,
    routes: Vec<Route>,
    trip: Trip,
}

type S = Rc<RefCell<Shared>>;

struct AgencyH(S, usize);
struct RouteH(S, usize);
struct TripH(S);

fn hex(c: &str) -> i64 {
    i64::from_str_radix(c.trim_start_matches('#'), 16).unwrap_or(0)
}

fn to_hex(v: i64) -> String {
    format!("{:06X}", v & 0xFF_FFFF)
}

impl UserData for AgencyH {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("get_id", |_, h, ()| {
            Ok(h.0.borrow().agencies[h.1].id.clone())
        });
        m.add_method("get_name", |_, h, ()| {
            Ok(h.0.borrow().agencies[h.1].name.clone())
        });
        m.add_method("set_name", |_, h, v: String| {
            h.0.borrow_mut().agencies[h.1].name = v;
            Ok(())
        });
        m.add_method("get_url", |_, h, ()| {
            Ok(h.0.borrow().agencies[h.1].url.clone())
        });
        m.add_method("set_url", |_, h, v: String| {
            h.0.borrow_mut().agencies[h.1].url = v;
            Ok(())
        });
    }
}

impl UserData for RouteH {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("get_id", |_, h, ()| Ok(h.0.borrow().routes[h.1].id.clone()));
        m.add_method("get_short_name", |_, h, ()| {
            Ok(h.0.borrow().routes[h.1].short_name.clone())
        });
        m.add_method("set_short_name", |_, h, v: String| {
            h.0.borrow_mut().routes[h.1].short_name = v;
            Ok(())
        });
        m.add_method("get_long_name", |_, h, ()| {
            Ok(h.0.borrow().routes[h.1].long_name.clone())
        });
        m.add_method("set_long_name", |_, h, v: String| {
            h.0.borrow_mut().routes[h.1].long_name = v;
            Ok(())
        });
        m.add_method("get_route_type", |_, h, ()| {
            Ok(h.0.borrow().routes[h.1].route_type)
        });
        m.add_method("set_route_type", |_, h, v: u16| {
            h.0.borrow_mut().routes[h.1].route_type = v;
            Ok(())
        });
        m.add_method("get_color", |_, h, ()| {
            Ok(hex(&h.0.borrow().routes[h.1].color))
        });
        m.add_method("set_color", |_, h, v: i64| {
            h.0.borrow_mut().routes[h.1].color = to_hex(v);
            Ok(())
        });
        m.add_method("get_text_color", |_, h, ()| {
            Ok(hex(&h.0.borrow().routes[h.1].text_color))
        });
        m.add_method("set_text_color", |_, h, v: i64| {
            h.0.borrow_mut().routes[h.1].text_color = to_hex(v);
            Ok(())
        });
        m.add_method("get_agency", |_, h, ()| {
            let a = h.0.borrow().routes[h.1].agency;
            if a == usize::MAX {
                // nigiri always has an agency; an empty one keeps
                // `get_agency():get_name()` chains working
                let mut s = h.0.borrow_mut();
                s.agencies.push(Agency::default());
                let i = s.agencies.len() - 1;
                s.routes[h.1].agency = i;
                return Ok(AgencyH(h.0.clone(), i));
            }
            Ok(AgencyH(h.0.clone(), a))
        });
    }
}

impl UserData for TripH {
    fn add_methods<M: UserDataMethods<Self>>(m: &mut M) {
        m.add_method("get_id", |_, h, ()| Ok(h.0.borrow().trip.id.clone()));
        m.add_method("get_short_name", |_, h, ()| {
            Ok(h.0.borrow().trip.short_name.clone())
        });
        m.add_method("set_short_name", |_, h, v: String| {
            h.0.borrow_mut().trip.short_name = v;
            Ok(())
        });
        m.add_method("get_display_name", |_, h, ()| {
            let s = h.0.borrow();
            Ok(s.trip.display_name.clone().unwrap_or_else(|| {
                let r = &s.routes[s.trip.route];
                default_display(&r.short_name, &s.trip.short_name, &r.long_name).to_owned()
            }))
        });
        m.add_method("set_display_name", |_, h, v: String| {
            h.0.borrow_mut().trip.display_name = Some(v);
            Ok(())
        });
        m.add_method("get_headsign", |_, h, ()| {
            Ok(h.0.borrow().trip.headsign.clone())
        });
        m.add_method("set_headsign", |_, h, v: String| {
            h.0.borrow_mut().trip.headsign = v;
            Ok(())
        });
        m.add_method("get_route", |_, h, ()| {
            let r = h.0.borrow().trip.route;
            Ok(RouteH(h.0.clone(), r))
        });
        m.add_method("set_route_type", |_, h, v: u16| {
            h.0.borrow_mut().trip.route_type = Some(v);
            Ok(())
        });
        m.add_method("get_vehicle_type_short_name", |_, _, ()| Ok(String::new()));
        m.add_method("set_compulsory_reservation", |_, _, _: Value| Ok(()));
        m.add_method("set_clasz", |_, _, _: Value| Ok(()));
    }
}

/// nigiri's display name without a script: route short name, else trip
/// short name, else route long name.
pub fn default_display<'a>(
    route_short: &'a str,
    trip_short: &'a str,
    route_long: &'a str,
) -> &'a str {
    [route_short, trip_short, route_long]
        .into_iter()
        .find(|s| !s.is_empty())
        .unwrap_or("")
}

/// A loaded feed script.
pub struct Script {
    _lua: Lua,
    s: S,
    agency_fn: Option<Function>,
    route_fn: Option<Function>,
    trip_fn: Option<Function>,
    /// Calls that raised a Lua error (object left unchanged).
    pub errors: u64,
    pub first_error: Option<String>,
}

impl Script {
    pub fn load(path: &Path) -> Result<Self> {
        let src =
            std::fs::read_to_string(path).with_context(|| format!("script {}", path.display()))?;
        let lua = Lua::new();
        let dir = path.parent().unwrap_or(Path::new("."));
        let base = dir.parent().unwrap_or(dir);
        let lua_err = |e: mlua::Error| anyhow::anyhow!("script {}: {e}", path.display());
        let package: mlua::Table = lua.globals().get("package").map_err(lua_err)?;
        package
            .set(
                "path",
                format!("{}/?.lua;{}/?.lua", dir.display(), base.display()),
            )
            .map_err(lua_err)?;
        lua.load(&src)
            .set_name(path.display().to_string())
            .exec()
            .map_err(|e| anyhow::anyhow!("script {}: {e}", path.display()))?;
        let get = |n: &str| -> Option<Function> { lua.globals().get::<Function>(n).ok() };
        let (agency_fn, route_fn, trip_fn) = (
            get("process_agency"),
            get("process_route"),
            get("process_trip"),
        );
        Ok(Self {
            s: Rc::new(RefCell::new(Shared::default())),
            _lua: lua,
            agency_fn,
            route_fn,
            trip_fn,
            errors: 0,
            first_error: None,
        })
    }

    pub fn has_trip_fn(&self) -> bool {
        self.trip_fn.is_some()
    }

    /// Runs `process_agency` on every agency and keeps them for the routes.
    pub fn agencies(&mut self, agencies: Vec<Agency>) -> Vec<Agency> {
        self.s.borrow_mut().agencies = agencies;
        if let Some(f) = self.agency_fn.clone() {
            let n = self.s.borrow().agencies.len();
            for i in 0..n {
                let before = self.s.borrow().agencies[i].clone();
                if let Err(e) = f.call::<Value>(AgencyH(self.s.clone(), i)) {
                    self.error(e);
                    self.s.borrow_mut().agencies[i] = before;
                }
            }
        }
        self.s.borrow().agencies.clone()
    }

    /// Runs `process_route` on every route; `None` = dropped by the script.
    /// The (possibly changed) routes stay available to `process_trip`.
    pub fn routes(&mut self, routes: Vec<Route>) -> Vec<Option<Route>> {
        self.s.borrow_mut().routes = routes;
        let n = self.s.borrow().routes.len();
        let mut keep = vec![true; n];
        if let Some(f) = self.route_fn.clone() {
            for (i, k) in keep.iter_mut().enumerate() {
                let before = self.s.borrow().routes[i].clone();
                match f.call::<Value>(RouteH(self.s.clone(), i)) {
                    Ok(Value::Boolean(false)) => *k = false,
                    Ok(_) => {}
                    Err(e) => {
                        self.error(e);
                        self.s.borrow_mut().routes[i] = before;
                    }
                }
            }
        }
        let s = self.s.borrow();
        s.routes
            .iter()
            .zip(keep)
            .map(|(r, k)| k.then(|| r.clone()))
            .collect()
    }

    /// Agency of a processed route (empty if none).
    pub fn agency_name(&self, route: usize) -> String {
        let s = self.s.borrow();
        s.agencies
            .get(s.routes[route].agency)
            .map(|a| a.name.clone())
            .unwrap_or_default()
    }

    /// Runs `process_trip`; `None` = dropped by the script.
    pub fn trip(&mut self, trip: Trip) -> Option<Trip> {
        let Some(f) = self.trip_fn.clone() else {
            return Some(trip);
        };
        self.s.borrow_mut().trip = trip.clone();
        match f.call::<Value>(TripH(self.s.clone())) {
            Ok(Value::Boolean(false)) => None,
            Ok(_) => Some(std::mem::take(&mut self.s.borrow_mut().trip)),
            Err(e) => {
                self.error(e);
                Some(trip)
            }
        }
    }

    fn error(&mut self, e: mlua::Error) {
        self.errors += 1;
        self.first_error.get_or_insert_with(|| e.to_string());
    }
}
