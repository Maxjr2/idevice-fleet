# iDevice Fleet

A desktop app for restoring, backing up and re-provisioning many iPhones and iPads at once. No Mac needed.
Written in Rust on top of the pure-Rust [`idevice`](https://github.com/jkcoxson/idevice) crate, with an
[egui](https://github.com/emilk/egui) interface.

It was written for MDM migrations: wiping a few dozen company iPads with firmware you downloaded beforehand,
several at a time, and keeping track of each one while it moves between normal, recovery and DFU mode.

![Devices view](docs/devices.png)

> **Status: work in progress, Linux first.** Device tracking, pairing, recovery mode, the firmware library and
> backups work. **Firmware restore is not built yet** and is the next step. Until then, the
> [`python-web`](https://github.com/Maxjr2/idevice-fleet/tree/python-web) branch is a complete web version that
> restores through `idevicerestore`.

## What it does

- **Device list across modes.** Devices are tracked by ECID, so an iPad keeps its name and serial number while it
  reboots into recovery or DFU mode.
- **Pairing and recovery mode.** Trust the computer from the app, put a device into recovery mode, or kick it
  out of a recovery loop.
- **Firmware library with preloading.** Look up a model on [ipsw.me](https://ipsw.me), see which versions Apple
  still signs, and download them in advance. Downloads resume when Apple's servers drop the connection (they
  often do) and are checked against their SHA-256 before they enter the library. You can also drop `.ipsw` files
  into the library folder.
- **Backups.** Full backups with `mobilebackup2`, with a check that your disk has room first, live progress, and a
  list of every backup showing whether it is encrypted and finished.
- **Jobs with live logs.** Everything the app does is a job with its own progress bar, cancel button and log file.

| | Status |
|---|---|
| Device list across normal / recovery / DFU mode | ✅ |
| Pair (trust), enter recovery, exit recovery | ✅ |
| Firmware library: signed-version lookup, resumable verified downloads | ✅ |
| Backups: full backup, disk-space check, backup list | ✅ |
| Job engine: retries, watchdog, crash recovery, logs | ✅ |
| Firmware restore, several devices in parallel | next |
| Backup encryption on/off, restoring a backup to a device | planned |
| Windows | planned |

Nothing leaves your machine except the ipsw.me lookup and firmware downloads from Apple's servers. A restore
will also send Apple the usual signing request.

![Firmware library](docs/firmware.png)

## Built to keep going

Restoring a few dozen devices takes hours, so the app is built to survive things going wrong:

- **Every device operation is a job** that runs in its own task. If one device's job hits a bug, that job
  fails and everything else keeps running. Even a bug in the window code shows a recovery screen instead of
  closing the app.
- **Retries that make sense.** Failures are sorted into *temporary* (USB hiccup, device rebooting, usbmuxd
  restarting, Apple's servers unreachable), *needs you* (unlock the device, tap Trust), *permanent* and
  *cancelled*. Only temporary failures are retried, with growing delays between attempts. A job that keeps making
  progress between failures, like a big download, isn't cut off after a fixed number of tries.
- **Watchdog.** An attempt that reports no progress for too long is cancelled and retried, so a hung transfer
  doesn't sit there forever.
- **Survives crashes and restarts.** Jobs are saved in SQLite (WAL mode). After a crash or reboot, a job that
  was running shows up as *Interrupted* with a **Run again** button. Every job writes its own log file as it goes.
- **One job per device** and a limit on parallel restores. Only one copy of the app can use a data folder.
- **Devices don't vanish mid-job.** A device rebooting between stages stays listed as *Reconnecting*, keeping its
  name and serial.
- **Self-healing device watchers.** The usbmuxd connection reconnects when usbmuxd restarts. Recovery and DFU
  devices are found by reading USB descriptors only, which never interferes with a running restore.

![Backups](docs/backups.png)

## Requirements

- **Linux** (Debian/Ubuntu tested) with `usbmuxd`. It starts when a device is plugged in.
- **Rust 1.88 or newer** to build it. There are no other tools to install: the app talks to devices itself.
- Internet access for firmware lookup and downloads, and later for restores: Apple has to sign every restore, so
  firmware Apple no longer signs can't be installed.
- Disk space: one iPad firmware file is 7–10 GB, and a backup needs about as much room as the device uses.

### Linux

```bash
sudo apt install usbmuxd build-essential
```

To use recovery and DFU mode without root, install the udev rule:

```bash
sudo cp packaging/linux/70-idevice-fleet.rules /etc/udev/rules.d/ && sudo udevadm control --reload
```

### Windows

Not built yet. It will need Apple's **Apple Devices** app (or iTunes) from the Microsoft Store, which provides the
USB drivers and the Apple Mobile Device Service. Run it natively on Windows, not in WSL (see below).

### Why not WSL?

WSL2 has no direct USB access, so iOS devices have to be forwarded with
[usbipd-win](https://github.com/dorssel/usbipd-win). Backups and device info can work that way, but restores
don't work reliably: the device disconnects and reconnects with a new USB ID every time it changes mode, and
large firmware transfers fail
([usbipd-win#959](https://github.com/dorssel/usbipd-win/issues/959)).

## Run it

```bash
git clone https://github.com/Maxjr2/idevice-fleet
cd idevice-fleet
cargo run --release -p fleet-app
```

Options go after `--`, for example `cargo run --release -p fleet-app -- --data /media/ssd/fleet`:

| Option | Default | |
|---|---|---|
| `--data DIR` | `~/.local/share/idevice-fleet` | Database, logs, firmware library and backups. Also settable with `IDEVICE_FLEET_DATA`. |
| `--demo` | | Show invented devices and jobs and don't touch USB. Handy for trying the interface. |
| `--theme light\|dark` | system | Override the system theme |
| `--screenshot FILE` | | Save a screenshot and quit (this is how the images here were made) |

Put `--data` on an external drive if your disk is small, since firmware and backups are large.

## A typical session

1. **Firmware tab:** look up the model identifier (for example `iPad13,18`; the models you connect are offered
   as buttons) and download the signed version.
2. **Devices tab:** connect the devices. A device that isn't trusted yet shows **Trust**: unlock it and tap Trust
   when asked. Then **Back up** takes a full backup.
3. Watch progress in **Jobs**. Click a job to read its log. If something fails, the message says what to do, and
   **Run again** starts it over.

Activation Lock is still enforced after an erase. Turn off Find My first, or have your MDM's bypass code ready.

## Development

```bash
cargo test -p fleet-core        # no device needed
cargo test -p fleet-core -- --ignored   # also hits the real ipsw.me
cargo run -p fleet-app -- --demo         # try the interface with invented data
```

- `crates/fleet-core`: everything except the window: job engine, persistence, device registry, firmware and
  backup handling, and the `idevice` backend.
- `crates/fleet-app`: the egui desktop app.

The tests cover retries, the watchdog, crash recovery, concurrency limits, resumable downloads against a server
that drops every connection, and the parsers.

## License

MIT. The recovery-mode USB transport is adapted from the [idevice](https://github.com/jkcoxson/idevice) project's
examples (also MIT).
