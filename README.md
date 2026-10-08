# iDevice Fleet

A desktop app for restoring, backing up and re-provisioning many iPhones and iPads at once. No Mac needed.
Written in Rust on top of the pure-Rust [`idevice`](https://github.com/jkcoxson/idevice) crate, with an
[egui](https://github.com/emilk/egui) interface.

> **Status: work in progress (Linux first).** The Python web version is still available as tag
> [`v0.1-python`](https://github.com/Maxjr2/idevice-fleet/tree/v0.1-python).

| | Status |
|---|---|
| Device list across normal / recovery / DFU mode, tracked by ECID | ✅ |
| Pair (trust), enter recovery, exit recovery | ✅ |
| Job engine: retries, watchdog, crash recovery, logs | ✅ |
| Firmware library: signed-version lookup, resumable verified downloads | planned |
| Backups (mobilebackup2) | planned |
| Firmware restore (native, with `idevicerestore` as per-job fallback) | planned |
| Windows | later |

## Built to keep going

- **Every device operation is a job** that runs in its own task. If one device's job hits a bug, that job
  fails and everything else keeps running.
- **Retries that make sense.** Failures are sorted into *temporary* (USB hiccup, device rebooting, usbmuxd
  restarting, Apple's servers unreachable), *needs you* (unlock the device, tap Trust), *permanent* and
  *cancelled*. Only temporary failures are retried, with growing delays between attempts.
- **Watchdog.** An attempt that reports no progress for too long is cancelled and retried, so a hung
  restore doesn't sit there forever.
- **Survives crashes and restarts.** Jobs are saved in SQLite (WAL mode). After a crash or reboot, a job that
  was running shows up as *Interrupted* with a **Run again** button. Every job writes its own log file as it goes.
- **One job per device** and a limit on parallel restores. Only one copy of the app can use a data folder.
- **Devices don't vanish mid-job.** A device rebooting between restore stages stays listed as *Reconnecting*,
  keeping its name and serial.
- **Self-healing device watchers.** The usbmuxd connection reconnects when usbmuxd restarts. Recovery and DFU
  devices are found by reading USB descriptors only, which never interferes with a running restore.

## Build and run (Linux)

```bash
sudo apt install usbmuxd build-essential
cargo run --release -p fleet-app
```

To use recovery and DFU mode without root, install the udev rule:

```bash
sudo cp packaging/linux/70-idevice-fleet.rules /etc/udev/rules.d/ && sudo udevadm control --reload
```

Data lives in `~/.local/share/idevice-fleet` (database, logs, firmware, backups). Use `--data DIR` or
`IDEVICE_FLEET_DATA` to put it elsewhere, for example on an external drive.

## Layout

- `crates/fleet-core`: everything except the UI: job engine, persistence, device registry, the `idevice` backend.
  Testable without a device: `cargo test -p fleet-core`.
- `crates/fleet-app`: the egui desktop app.

## License

MIT. Recovery-mode USB transport adapted from the idevice project's examples (MIT).
