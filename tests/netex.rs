use rustis::build::{BuildOptions, FeedSpec, parse_feed};
use std::io::Write;
#[test]
fn imports_netex_calendar_and_journey() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("feed.zip");
    let mut z = zip::ZipWriter::new(std::fs::File::create(&path).unwrap());
    z.start_file("nested/feed.xml", zip::write::SimpleFileOptions::default())
        .unwrap();
    z.write_all(include_bytes!("fixtures/netex/fr.xml"))
        .unwrap();
    z.finish().unwrap();
    let p = parse_feed(
        &FeedSpec {
            name: "fr".into(),
            path,
        },
        &BuildOptions::for_date("2026-11-14"),
    )
    .unwrap();
    assert_eq!(p.report().trips, 1);
    assert_eq!(p.report().stops, 2);
    assert_eq!(p.report().timezone, "Europe/Paris");
}

#[test]
fn routes_overnight_only_on_published_days() {
    use rustis::{Endpoint, PlanRequest, Snapshot, build_snapshot};
    let dir = tempfile::tempdir().unwrap();
    let feeds = dir.path().join("feed");
    std::fs::create_dir(&feeds).unwrap();
    std::fs::write(
        feeds.join("fr.xml"),
        include_bytes!("fixtures/netex/fr.xml"),
    )
    .unwrap();
    let out = dir.path().join("snap");
    let meta = build_snapshot(
        &[FeedSpec {
            name: "fr".into(),
            path: feeds,
        }],
        &out,
        BuildOptions::for_date("2026-11-14"),
    )
    .unwrap();
    assert!(meta.feeds[0].imported, "{:?}", meta.feeds[0].error);
    let snap = Snapshot::open(&out).unwrap();
    for (day, count) in [("2026-11-14", 1), ("2026-11-15", 0)] {
        let q = PlanRequest {
            date: day.into(),
            departure: 23 * 3600,
            from: vec![Endpoint {
                stop_id: "fr:A".into(),
                walk_seconds: 0,
            }],
            to: vec![Endpoint {
                stop_id: "fr:B".into(),
                walk_seconds: 0,
            }],
            max_transfers: 0,
            max_results: 1,
            max_travel_seconds: 7200,
            alternatives_within_seconds: 0,
        };
        let r = snap.plan(&q).unwrap();
        assert_eq!(r.itineraries.len(), count);
        if count == 1 {
            assert_eq!(r.itineraries[0].arrival, 88200);
        }
    }
}

#[test]
fn italian_profile_and_unresolved_references() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("feed.xml");
    let xml = include_str!("fixtures/netex/it.xml");
    std::fs::write(&path, xml).unwrap();
    let spec = FeedSpec {
        name: "it-trenitalia".into(),
        path: dir.path().into(),
    };
    let opts = BuildOptions::for_date("2026-11-14");
    let p = parse_feed(&spec, &opts).unwrap();
    assert_eq!(p.report().trips, 1);
    assert_eq!(p.report().timezone, "Europe/Rome");
    std::fs::write(
        &path,
        xml.replace("ref=\"day\"/>", "ref=\"unknown\"/>").replacen(
            "ref=\"unknown\"/>",
            "ref=\"day\"/>",
            1,
        ),
    )
    .unwrap();
    assert!(
        parse_feed(&spec, &opts)
            .err()
            .unwrap()
            .to_string()
            .contains("unknown day type")
    );
}

#[test]
fn netex_wall_times_across_spring_dst() {
    use rustis::{Endpoint, PlanRequest, Snapshot, build_snapshot};
    let dir = tempfile::tempdir().unwrap();
    let feed = dir.path().join("feed");
    std::fs::create_dir(&feed).unwrap();
    let xml = include_str!("fixtures/netex/fr.xml")
        .replace("2026-11-14", "2027-03-28")
        .replace("2026-11-16", "2027-03-30")
        .replace("23:30:00", "00:30:00")
        .replace(
            "<ArrivalTime>00:30:00</ArrivalTime><ArrivalDayOffset>1</ArrivalDayOffset>",
            "<ArrivalTime>03:30:00</ArrivalTime>",
        );
    std::fs::write(feed.join("fr.xml"), xml).unwrap();
    let out = dir.path().join("snap");
    build_snapshot(
        &[FeedSpec {
            name: "fr".into(),
            path: feed,
        }],
        &out,
        BuildOptions::for_date("2027-03-28"),
    )
    .unwrap();
    let snap = Snapshot::open(&out).unwrap();
    let q = PlanRequest {
        date: "2027-03-28".into(),
        departure: 0,
        from: vec![Endpoint {
            stop_id: "fr:A".into(),
            walk_seconds: 0,
        }],
        to: vec![Endpoint {
            stop_id: "fr:B".into(),
            walk_seconds: 0,
        }],
        max_transfers: 0,
        max_results: 1,
        max_travel_seconds: 14400,
        alternatives_within_seconds: 0,
    };
    let r = snap.plan(&q).unwrap();
    assert_eq!(r.itineraries[0].departure, 1800);
    assert_eq!(r.itineraries[0].arrival, 9000);
}

#[test]
fn boarding_prohibition_is_preserved() {
    use rustis::{Endpoint, PlanRequest, Snapshot, build_snapshot};
    let dir = tempfile::tempdir().unwrap();
    let feed = dir.path().join("feed");
    std::fs::create_dir(&feed).unwrap();
    let xml=include_str!("fixtures/netex/it.xml").replace("<ScheduledStopPointRef ref=\"sa\"/></StopPointInJourneyPattern>","<ScheduledStopPointRef ref=\"sa\"/><ForBoarding>false</ForBoarding></StopPointInJourneyPattern>");
    assert!(xml.contains("<ForBoarding>false"));
    std::fs::write(feed.join("it.xml"), xml).unwrap();
    let out = dir.path().join("snap");
    let meta = build_snapshot(
        &[FeedSpec {
            name: "it".into(),
            path: feed,
        }],
        &out,
        BuildOptions::for_date("2026-11-14"),
    )
    .unwrap();
    assert!(meta.feeds[0].imported);
    let snap = Snapshot::open(&out).unwrap();
    let q = PlanRequest {
        date: "2026-11-14".into(),
        departure: 23 * 3600,
        from: vec![Endpoint {
            stop_id: "it:A".into(),
            walk_seconds: 0,
        }],
        to: vec![Endpoint {
            stop_id: "it:B".into(),
            walk_seconds: 0,
        }],
        max_transfers: 0,
        max_results: 1,
        max_travel_seconds: 7200,
        alternatives_within_seconds: 0,
    };
    assert!(snap.plan(&q).unwrap().itineraries.is_empty());
}

#[test]
fn missing_passing_time_uses_previous_time_like_nigiri() {
    let dir = tempfile::tempdir().unwrap();
    let xml=include_str!("fixtures/netex/fr.xml").replace("<StopPointInJourneyPattern id=\"p2\"", "<StopPointInJourneyPattern id=\"middle\"><ScheduledStopPointRef ref=\"sa\"/></StopPointInJourneyPattern><StopPointInJourneyPattern id=\"p2\"");
    std::fs::write(dir.path().join("fr.xml"), xml).unwrap();
    let p = parse_feed(
        &FeedSpec {
            name: "fr".into(),
            path: dir.path().into(),
        },
        &BuildOptions::for_date("2026-11-14"),
    )
    .unwrap();
    assert_eq!(p.report().trips, 1);
    assert_eq!(p.report().stop_times, 3);
    assert_eq!(
        p.report().skipped["netex_untimed_stops_filled_like_nigiri"],
        1
    );
}

#[test]
fn autumn_padding_does_not_reject_following_service() {
    let dir = tempfile::tempdir().unwrap();
    let xml = include_str!("fixtures/netex/fr.xml")
        .replace("2026-11-14", "2026-10-25")
        .replace("2026-11-16", "2026-10-27")
        .replace("101", "111")
        .replace("23:30:00", "00:30:00")
        .replace(
            "<ArrivalTime>00:30:00</ArrivalTime><ArrivalDayOffset>1</ArrivalDayOffset>",
            "<ArrivalTime>03:30:00</ArrivalTime>",
        );
    std::fs::write(dir.path().join("fr.xml"), xml).unwrap();
    let p = parse_feed(
        &FeedSpec {
            name: "fr".into(),
            path: dir.path().into(),
        },
        &BuildOptions::for_date("2026-10-26"),
    )
    .unwrap();
    assert!(p.report().trips > 0);
    assert_eq!(p.report().skipped["netex_before_padding_service_days"], 1);
}

#[test]
fn repeated_point_ids_on_loop_journeys_keep_occurrence_order() {
    let dir = tempfile::tempdir().unwrap();
    let xml=include_str!("fixtures/netex/fr.xml")
        .replace("</pointsInSequence>","<StopPointInJourneyPattern id=\"p1\"><ScheduledStopPointRef ref=\"sa\"/></StopPointInJourneyPattern></pointsInSequence>")
        .replace("</passingTimes>","<TimetabledPassingTime><PointInJourneyPatternRef ref=\"p1\"/><ArrivalTime>01:30:00</ArrivalTime><ArrivalDayOffset>1</ArrivalDayOffset></TimetabledPassingTime></passingTimes>");
    std::fs::write(dir.path().join("fr.xml"), xml).unwrap();
    let p = parse_feed(
        &FeedSpec {
            name: "fr".into(),
            path: dir.path().into(),
        },
        &BuildOptions::for_date("2026-11-14"),
    )
    .unwrap();
    assert_eq!(p.report().trips, 1);
    assert_eq!(p.report().stop_times, 3);
}

#[test]
fn journey_transport_mode_overrides_general_line_mode() {
    let dir = tempfile::tempdir().unwrap();
    let feed = dir.path().join("feed");
    std::fs::create_dir(&feed).unwrap();
    let xml = include_str!("fixtures/netex/fr.xml").replace(
        "<ServiceJourney id=\"J\">",
        "<ServiceJourney id=\"J\"><TransportMode>coach</TransportMode>",
    );
    std::fs::write(feed.join("fr.xml"), xml).unwrap();
    let out = dir.path().join("snap");
    let m = rustis::build_snapshot(
        &[FeedSpec {
            name: "fr".into(),
            path: feed,
        }],
        &out,
        BuildOptions::for_date("2026-11-14"),
    )
    .unwrap();
    assert!(m.feeds[0].imported);
    assert_eq!(
        std::fs::read(out.join("route_info_type")).unwrap(),
        200u16.to_le_bytes()
    );
}

#[test]
fn repeated_points_after_an_untimed_stop_do_not_overwrite_previous_occurrence() {
    let dir = tempfile::tempdir().unwrap();
    let xml=include_str!("fixtures/netex/fr.xml")
        .replace("<StopPointInJourneyPattern id=\"p2\"", "<StopPointInJourneyPattern id=\"middle\"><ScheduledStopPointRef ref=\"sa\"/></StopPointInJourneyPattern><StopPointInJourneyPattern id=\"p2\"")
        .replace("</pointsInSequence>","<StopPointInJourneyPattern id=\"p1\"><ScheduledStopPointRef ref=\"sa\"/></StopPointInJourneyPattern></pointsInSequence>")
        .replace("</passingTimes>","<TimetabledPassingTime><PointInJourneyPatternRef ref=\"p1\"/><ArrivalTime>01:30:00</ArrivalTime><ArrivalDayOffset>1</ArrivalDayOffset></TimetabledPassingTime></passingTimes>");
    std::fs::write(dir.path().join("fr.xml"), xml).unwrap();
    let p = parse_feed(
        &FeedSpec {
            name: "fr".into(),
            path: dir.path().into(),
        },
        &BuildOptions::for_date("2026-11-14"),
    )
    .unwrap();
    assert_eq!(p.report().trips, 1);
    assert_eq!(p.report().stop_times, 4);
}
