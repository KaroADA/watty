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

- Config: `~/.config/watty/config.toml` (`ftp`, `trainer`, `hr`), written by `ride`.
- Rides: `~/.local/share/watty/rides/` (CSV journal + TCX). Log: `~/.local/share/watty/watty.log`.
- Ride keys: `space` start/pause, `+`/`-` intensity ±5%, `n` next segment, `q` quit (confirm).
