# Vulture

Source: https://github.com/urschrei/vulture
Revision: `1b9e87c4d2a6753dffc2a859b99d4f4ddd07f523` (package 0.24.0).
Vendored library sources retain their upstream Apache-2.0 OR BlueOak-1.0.0 licences.

Local change: expose read-only `GtfsTimetable::trips_on_route` so Rustis can
export **every** prepared trip, including equal-time departures, without
reimplementing the importer's pattern/overtaking/calendar logic. The algorithm
is unchanged. Vendoring makes this small patch reproducible without a private fork.
