use rustis::{BuildOptions, Endpoint, PlanRequest, Snapshot, import_gtfs_with};
fn fixture(zone: bool) -> (tempfile::TempDir, Snapshot, PlanRequest) {
    let t = tempfile::tempdir().unwrap();
    let f = t.path().join("feed");
    std::fs::create_dir(&f).unwrap();
    for (name, data) in [
        (
            "agency.txt",
            "agency_id,agency_name,agency_timezone\na,Flex,Europe/Vienna\n",
        ),
        (
            "stops.txt",
            "stop_id,stop_name,stop_lat,stop_lon\nA,A,46,14\nB,B,46.1,14\n",
        ),
        (
            "routes.txt",
            "route_id,agency_id,route_short_name,route_type\nr,a,Rufbus,3\n",
        ),
        ("trips.txt", "route_id,service_id,trip_id\nr,s,t\n"),
        (
            "calendar_dates.txt",
            "service_id,date,exception_type\ns,20261116,1\n",
        ),
        ("location_groups.txt", "location_group_id\ng1\ng2\n"),
        (
            "location_group_stops.txt",
            "location_group_id,stop_id\ng1,A\ng2,B\n",
        ),
        (
            "booking_rules.txt",
            "booking_rule_id,booking_type,prior_notice_duration_min,phone_number\nb,1,30,12345\n",
        ),
        (
            "stop_times.txt",
            "trip_id,stop_sequence,location_group_id,start_pickup_drop_off_window,end_pickup_drop_off_window,pickup_type,drop_off_type,pickup_booking_rule_id\nt,1,g1,25:00:00,26:00:00,2,1,b\nt,2,g2,25:00:00,27:00:00,1,2,b\n",
        ),
    ] {
        std::fs::write(f.join(name), data).unwrap();
    }
    if zone {
        std::fs::write(f.join("locations.geojson"), r#"{"type":"FeatureCollection","features":[{"type":"Feature","id":"g1","geometry":{"type":"Polygon","coordinates":[[[13.9,45.9],[14.1,45.9],[14.1,46.01],[13.9,46.01],[13.9,45.9]]]}},{"type":"Feature","id":"g2","geometry":{"type":"MultiPolygon","coordinates":[[[[13.9,46.09],[14.1,46.09],[14.1,46.2],[13.9,46.2],[13.9,46.09]]]]}}]}"#).unwrap();
        let times = std::fs::read_to_string(f.join("stop_times.txt"))
            .unwrap()
            .replace("location_group_id", " location_id");
        std::fs::write(f.join("stop_times.txt"), times).unwrap();
    }
    let out = t.path().join("snap");
    import_gtfs_with(&f, &out, &BuildOptions::for_date("2026-11-16")).unwrap();
    let s = Snapshot::open(&out).unwrap();
    s.verify().unwrap();
    let req = PlanRequest {
        date: "2026-11-16".into(),
        departure: 90000,
        from: vec![Endpoint {
            stop_id: "A".into(),
            walk_seconds: 0,
        }],
        to: vec![Endpoint {
            stop_id: "B".into(),
            walk_seconds: 0,
        }],
        max_transfers: 0,
        max_results: 1,
        max_travel_seconds: 7200,
        alternatives_within_seconds: 0,
    };
    let meta: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("snapshot.json")).unwrap()).unwrap();
    assert_eq!(meta["feeds"][0]["flex_services"], 1);
    (t, s, req)
}
#[test]
fn group_and_zone_windows_are_routed() {
    for zone in [false, true] {
        let (_t, s, req) = fixture(zone);
        let r = s.plan(&req).unwrap();
        assert_eq!(r.itineraries.len(), 1, "{r:?}");
        assert_eq!(r.itineraries[0].legs[0].mode.as_deref(), Some("flex"));
        assert_eq!(r.itineraries[0].arrival, 92302);
        assert!(r.itineraries[0].booking_required);
        assert!(r.warnings.iter().any(|w| w.contains("12345")));
        for (date, departure) in [("2026-11-16", 96000), ("2026-11-18", 90000)] {
            let r = s
                .plan(&PlanRequest {
                    date: date.into(),
                    departure,
                    ..req.clone()
                })
                .unwrap();
            assert!(r.itineraries.is_empty());
        }
    }
}

#[tokio::test]
async fn motis_flex_is_explicit_opt_in() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (_t, s, _) = fixture(false);
    let app = rustis::http::router(std::sync::Arc::new(s), 1);
    for modes in [
        "",
        "&transitModes=BUS",
        "&transitModes=FLEX",
        "&transitModes=ODM",
    ] {
        let uri = format!(
            "/api/v6/plan?fromPlace=A&toPlace=B&time=2026-11-17T00:00:00Z&numItineraries=1{modes}"
        );
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert!(response.status().is_success());
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let its = json["itineraries"].as_array().unwrap();
        if modes.ends_with("FLEX") || modes.ends_with("ODM") {
            assert_eq!(its.len(), 1, "{json}");
            assert_eq!(its[0]["legs"][0]["mode"], "FLEX");
            assert_eq!(its[0]["legs"][0]["bookingRequired"], true);
            assert!(
                its[0]["legs"][0]["bookingInfo"]
                    .as_str()
                    .unwrap()
                    .contains("12345")
            );
        } else {
            assert!(its.is_empty(), "{json}");
        }
    }
}
#[test]
fn feasible_departures_are_not_pruned() {
    let (_t, s, req) = fixture(false);
    for offset in (0..=3600).step_by(60) {
        let r = s
            .plan(&PlanRequest {
                departure: 90000 + offset,
                ..req.clone()
            })
            .unwrap();
        assert_eq!(r.itineraries.len(), 1, "offset={offset}: {r:?}");
        assert_eq!(r.itineraries[0].arrival, 92302 + offset);
    }
}

#[test]
fn timed_to_flex_counts_a_transfer_and_observes_change_time() {
    let (t, _, req) = fixture(false);
    let feed = t.path().join("feed");
    std::fs::write(
        feed.join("stops.txt"),
        "stop_id,stop_name,stop_lat,stop_lon\nA,A,46,14\nB,B,46.1,14\nC,C,45,14\n",
    )
    .unwrap();
    std::fs::write(
        feed.join("trips.txt"),
        "route_id,service_id,trip_id\nr,s,t\nr,s,feeder\n",
    )
    .unwrap();
    std::fs::write(feed.join("stop_times.txt"),"trip_id,stop_sequence,stop_id,arrival_time,departure_time,location_group_id,start_pickup_drop_off_window,end_pickup_drop_off_window,pickup_type,drop_off_type\nt,1,,,,g1,25:00:00,26:00:00,2,1\nt,2,,,,g2,25:00:00,27:00:00,1,2\nfeeder,1,C,24:50:00,24:50:00,,,,0,1\nfeeder,2,A,25:00:00,25:00:00,,,,1,0\n").unwrap();
    let out = t.path().join("transfer");
    let mut opts = BuildOptions::for_date("2026-11-16");
    opts.default_change_seconds = 120;
    import_gtfs_with(&feed, &out, &opts).unwrap();
    let s = Snapshot::open(&out).unwrap();
    let req = PlanRequest {
        from: vec![Endpoint {
            stop_id: "C".into(),
            walk_seconds: 0,
        }],
        departure: 89400,
        ..req
    };
    assert!(s.plan(&req).unwrap().itineraries.is_empty());
    let r = s
        .plan(&PlanRequest {
            max_transfers: 1,
            ..req.clone()
        })
        .unwrap();
    assert_eq!(r.omitted_unreconstructable, 0);
    assert_eq!(r.itineraries[0].arrival, 92422);
    assert_eq!(r.itineraries[0].transfers, 1);
    assert_eq!(r.itineraries[0].legs[1].departure, 90120);
    for (name, ty, expected) in [("minimum", 2, Some(92902)), ("forbidden", 3, None)] {
        std::fs::write(feed.join("transfers.txt"),format!("from_stop_id,to_stop_id,from_trip_id,to_trip_id,transfer_type,min_transfer_time\nA,A,feeder,t,{ty},600\n")).unwrap();
        let dest = t.path().join(name);
        import_gtfs_with(&feed, &dest, &opts).unwrap();
        let response = Snapshot::open(&dest)
            .unwrap()
            .plan(&PlanRequest {
                max_transfers: 1,
                ..req.clone()
            })
            .unwrap();
        assert_eq!(response.itineraries.first().map(|i| i.arrival), expected);
    }
}

#[test]
fn same_area_uses_diameter_and_waits_for_dropoff_window() {
    let (t, _, req) = fixture(false);
    let feed = t.path().join("feed");
    std::fs::write(
        feed.join("location_group_stops.txt"),
        "location_group_id,stop_id\ng1,A\ng1,B\n",
    )
    .unwrap();
    std::fs::write(feed.join("stop_times.txt"),"trip_id,stop_sequence,location_group_id,start_pickup_drop_off_window,end_pickup_drop_off_window,pickup_type,drop_off_type\nt,1,g1,25:00:00,26:00:00,2,1\nt,2,g1,26:00:00,27:00:00,1,2\n").unwrap();
    let out = t.path().join("same-area");
    import_gtfs_with(&feed, &out, &BuildOptions::for_date("2026-11-16")).unwrap();
    let s = Snapshot::open(&out).unwrap();
    s.verify().unwrap();
    let r = s.plan(&req).unwrap();
    assert_eq!(r.itineraries[0].arrival, 93600);
    let r = s
        .plan(&PlanRequest {
            departure: 93600,
            ..req
        })
        .unwrap();
    assert_eq!(r.itineraries[0].arrival, 95902);
}

#[tokio::test]
async fn default_motis_keeps_bus_when_faster_flex_exists() {
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use tower::ServiceExt;
    let (t, _, _) = fixture(false);
    let feed = t.path().join("feed");
    std::fs::write(
        feed.join("trips.txt"),
        "route_id,service_id,trip_id\nr,s,t\nr,s,bus\n",
    )
    .unwrap();
    std::fs::write(feed.join("stop_times.txt"),"trip_id,stop_sequence,stop_id,arrival_time,departure_time,location_group_id,start_pickup_drop_off_window,end_pickup_drop_off_window,pickup_type,drop_off_type\nt,1,,,,g1,25:00:00,26:00:00,2,1\nt,2,,,,g2,25:00:00,27:00:00,1,2\nbus,1,A,25:30:00,25:30:00,,,,0,1\nbus,2,B,25:35:00,25:35:00,,,,1,0\n").unwrap();
    let out = t.path().join("mixed");
    import_gtfs_with(&feed, &out, &BuildOptions::for_date("2026-11-16")).unwrap();
    let app = rustis::http::router(std::sync::Arc::new(Snapshot::open(&out).unwrap()), 1);
    for modes in ["", "&transitModes=BUS", "&transitModes=FLEX"] {
        let uri = format!(
            "/api/v6/plan?fromPlace=A&toPlace=B&time=2026-11-17T00:00:00Z&numItineraries=1{modes}"
        );
        let response = app
            .clone()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        if modes.ends_with("FLEX") {
            assert_eq!(
                json["itineraries"][0]["endTime"], "2026-11-17T00:38:22Z",
                "{json}"
            );
        } else {
            assert_eq!(json["itineraries"][0]["legs"][0]["mode"], "BUS", "{json}");
        }
    }
}

#[test]
fn flex_to_timed_preserves_trip_specific_prohibition() {
    let (t, _, req) = fixture(false);
    let feed = t.path().join("feed");
    std::fs::write(
        feed.join("stops.txt"),
        "stop_id,stop_name,stop_lat,stop_lon\nA,A,46,14\nB,B,46.1,14\nC,C,45,14\n",
    )
    .unwrap();
    std::fs::write(
        feed.join("trips.txt"),
        "route_id,service_id,trip_id\nr,s,t\nr,s,exit\n",
    )
    .unwrap();
    std::fs::write(feed.join("stop_times.txt"),"trip_id,stop_sequence,stop_id,arrival_time,departure_time,location_group_id,start_pickup_drop_off_window,end_pickup_drop_off_window,pickup_type,drop_off_type\nt,1,,,,g1,25:00:00,26:00:00,2,1\nt,2,,,,g2,25:00:00,27:00:00,1,2\nexit,1,B,25:50:00,25:50:00,,,,0,1\nexit,2,C,26:00:00,26:00:00,,,,1,0\n").unwrap();
    let req = PlanRequest {
        to: vec![Endpoint {
            stop_id: "C".into(),
            walk_seconds: 0,
        }],
        max_transfers: 1,
        ..req
    };
    for restricted in [false, true] {
        if restricted {
            std::fs::write(
                feed.join("transfers.txt"),
                "from_stop_id,to_stop_id,from_trip_id,to_trip_id,transfer_type\nB,B,t,exit,3\n",
            )
            .unwrap();
        }
        let dest = t.path().join(format!("exit-{restricted}"));
        import_gtfs_with(&feed, &dest, &BuildOptions::for_date("2026-11-16")).unwrap();
        let response = Snapshot::open(&dest).unwrap().plan(&req).unwrap();
        assert_eq!(response.itineraries.is_empty(), restricted);
        if !restricted {
            assert_eq!(response.itineraries[0].arrival, 93600);
        }
    }
}

#[test]
fn identical_services_are_deduplicated() {
    let (t, _, _) = fixture(false);
    let feed = t.path().join("feed");
    let trips = std::fs::read_to_string(feed.join("trips.txt")).unwrap();
    std::fs::write(feed.join("trips.txt"), trips + "r,s,twin\n").unwrap();
    let times = std::fs::read_to_string(feed.join("stop_times.txt")).unwrap();
    let twin = times
        .lines()
        .skip(1)
        .map(|l| l.replacen("t,", "twin,", 1))
        .collect::<Vec<_>>()
        .join("\n");
    std::fs::write(feed.join("stop_times.txt"), times + &twin + "\n").unwrap();
    let m = import_gtfs_with(
        &feed,
        &t.path().join("dedup"),
        &BuildOptions::for_date("2026-11-16"),
    )
    .unwrap();
    assert_eq!(m.feeds[0].flex_services, 1);
}
