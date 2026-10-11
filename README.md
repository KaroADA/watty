# watty

TUI for FTMS smart trainers with `.zwo` workouts, ERG control, BLE heart rate and TCX recording.

```sh
cargo build --release
./target/release/watty scan                                # find devices
./target/release/watty monitor --trainer <ADDR> --hr <ADDR> # read-only data
./target/release/watty erg --trainer <ADDR> --watts 100     # manual ERG test
./target/release/watty ride workouts/sweet_spot_3x10.zwo --ftp 250
./target/release/watty ride workouts/test_short.zwo --sim --speed 10
./target/release/watty export ~/.local/share/watty/rides/<ride>.csv
```

- Config: `~/.config/watty/config.toml` (`ftp`, `weight`, `trainer`, `hr`, `cadence_tolerance`, `auto_pause`, `max_hr`, `hr_zones`, `segment_warning`), written by `ride`.
- Cadence targets come from `Cadence` / `CadenceResting` in `.zwo`; acceptable range is target ± `cadence_tolerance` rpm (default 5).
- Rides: `~/.local/share/watty/rides/` (CSV journal + TCX). Log: `~/.local/share/watty/watty.log`.
- Heart-rate zones: set `max_hr` (zones at 60/70/80/90%) or `hr_zones = [z1, z2, z3, z4]` with each zone's upper bound in bpm; the HR panel is then colored by zone.
- The last `segment_warning` seconds of a segment (default 10, `0` = off) show a popup with a countdown and the next segment's targets.
- Auto-pause (`auto_pause`, default on): pauses after 3 s without pedaling (power under 15 W and no cadence, so a cadence sensor is optional) and resumes after 2 s of pedaling. A manual pause only resumes with `space`.
- Ride keys: `space` start/pause, `+`/`-` intensity ±5%, `n` next segment, `q` quit (confirm); with `--sim`, `s` stops/starts simulated pedaling.
