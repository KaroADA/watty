# watty

TUI for FTMS smart trainers with `.zwo` workouts, ERG control, BLE heart rate and TCX recording.

```sh
cargo build --release
./target/release/watty                                     # home screen: pick a workout and ride
./target/release/watty scan                                # find devices
./target/release/watty monitor --trainer <ADDR> --hr <ADDR> # read-only data
./target/release/watty erg --trainer <ADDR> --watts 100     # manual ERG test
./target/release/watty ride workouts/sweet_spot_3x10.zwo --ftp 250
./target/release/watty ride workouts/test_short.zwo --sim --speed 10
./target/release/watty export ~/.local/share/watty/rides/<ride>.csv
```

- Config: `~/.config/watty/config.toml` (`ftp`, `weight`, `trainer`, `hr`, `cadence_tolerance`, `auto_pause`, `max_hr`, `hr_zones`, `segment_warning`, `keep_awake`, `soft_start`, `workouts_dir`), written by `ride`.
- Workouts: the home screen lists `.zwo` files from `~/.local/share/watty/workouts` (or `workouts_dir`); a new folder gets the example workouts. `Enter` rides the selected one, `s` does a test ride on the simulator, and after the ride you're back on the list.
- Cadence targets come from `Cadence` / `CadenceResting` in `.zwo`; acceptable range is target ± `cadence_tolerance` rpm (default 5).
- Activities tab: a month calendar of rides (●) and planned workouts (○) with each ride's time, distance, average power, NP, HR and kJ. `a` plans a workout on the selected day, `d` removes it, `Enter` rides it, `r` resumes an interrupted ride; `[`/`]` change month, `t` jumps to today. Today's planned workouts are marked on the Workouts tab.
- Plan: `~/.local/share/watty/plan.toml`, also editable by hand:
  ```toml
  [[workout]]
  date = 2026-10-16
  file = "sweet_spot_3x10.zwo"   # relative to the workouts folder, or absolute
  ```
- Rides: `~/.local/share/watty/rides/` (CSV journal + TCX); simulator rides go to `rides/test/`. Quitting asks whether to save or discard the ride. Each ride has a `.json` next to its CSV with the workout and progress: if the latest ride was interrupted (crash or quit before the end) less than 3 h ago, the home screen offers to resume it, paused where it stopped. Log: `~/.local/share/watty/watty.log`.
- Heart-rate zones: set `max_hr` (zones at 60/70/80/90%) or `hr_zones = [z1, z2, z3, z4]` with each zone's upper bound in bpm; the HR panel is then colored by zone.
- The last `segment_warning` seconds of a segment (default 10, `0` = off) show a popup with a countdown and the next segment's targets.
- `keep_awake` (default on) stops the screen from dimming or locking during a ride: via `org.freedesktop.ScreenSaver` (GNOME, KDE, Xfce), `systemd-inhibit` (e.g. hypridle), and on Omarchy by tagging the terminal window `noidle`.
- `soft_start` (default on): while paused ERG drops to 50% of the target, and after starting or resuming it climbs back to 100% over 5 s.
- Auto-pause (`auto_pause`, default on): pauses after 3 s without pedaling (power under 15 W and no cadence, so a cadence sensor is optional) and resumes after 2 s of pedaling. A manual pause only resumes with `space`.
- With HR zones set, if heart rate stays above the zone matching an endurance or tempo segment (power zone 2–3) for 60 s, after the first 2 min of the segment, the status line suggests lowering intensity: `a` lowers it 5%, `x` dismisses; it won't ask again for 2 min.
- Ride keys: `space` start/pause, `+`/`-` intensity ±5%, `n` next segment, `q` quit (confirm); with `--sim`, `s` stops/starts simulated pedaling.
