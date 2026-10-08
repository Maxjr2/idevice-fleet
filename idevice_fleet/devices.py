"""Device discovery across normal, recovery and DFU mode."""
from __future__ import annotations

import os
import plistlib
import re
import subprocess
import threading
import time
from typing import Callable, Iterable

from .tools import Tools

APPLE_VID = 0x05AC
RECOVERY_PIDS = {0x1280, 0x1281, 0x1282, 0x1283}
DFU_PIDS = {0x1227}

_SERIAL_TOKEN = re.compile(r"(\w+):(\[[^\]]*\]|\S+)")


def ecid_key(ecid: int) -> str:
    return f"{ecid:x}"


def parse_apple_usb_serial(serial: str) -> dict[str, str]:
    """Parse the iSerialNumber an iOS device exposes in recovery/DFU mode.

    e.g. "CPID:8101 CPRV:10 CPFM:03 SCEP:01 BDID:14 ECID:001A2B3C4D5E6F78 IBFL:3D SRTG:[iBoot-8419.0.42]"
    """
    return {k.upper(): v.strip("[]") for k, v in _SERIAL_TOKEN.findall(serial or "")}


def parse_irecovery_query(text: str) -> dict[str, str]:
    """Parse `irecovery -q` output ("KEY: value" per line)."""
    out: dict[str, str] = {}
    for line in (text or "").splitlines():
        if ":" in line:
            k, v = line.split(":", 1)
            out[k.strip().upper()] = v.strip()
    return out


def parse_lockdown_plist(data: bytes) -> dict:
    """Parse `ideviceinfo -x` output into the fields the UI uses."""
    info = plistlib.loads(data)
    ecid = info.get("UniqueChipID")
    return {
        "udid": info.get("UniqueDeviceID"),
        "ecid": ecid_key(int(ecid)) if ecid is not None else None,
        "name": info.get("DeviceName"),
        "product_type": info.get("ProductType"),
        "os_version": info.get("ProductVersion"),
        "build": info.get("BuildVersion"),
        "serial": info.get("SerialNumber"),
        "activation_state": info.get("ActivationState"),
        "device_class": info.get("DeviceClass"),
    }


def scan_sysfs(root: str = "/sys/bus/usb/devices") -> list[dict]:
    """Find Apple devices in recovery or DFU mode via Linux sysfs (passive, no USB traffic)."""
    found: list[dict] = []
    try:
        entries = os.listdir(root)
    except OSError:
        return found

    def read(d: str, name: str) -> str | None:
        try:
            with open(os.path.join(root, d, name), encoding="utf-8", errors="replace") as f:
                return f.read().strip()
        except OSError:
            return None

    for d in entries:
        vid, pid = read(d, "idVendor"), read(d, "idProduct")
        if not vid or not pid:
            continue
        try:
            vid_i, pid_i = int(vid, 16), int(pid, 16)
        except ValueError:
            continue
        if vid_i != APPLE_VID or (pid_i not in RECOVERY_PIDS and pid_i not in DFU_PIDS):
            continue
        fields = parse_apple_usb_serial(read(d, "serial") or "")
        if "ECID" not in fields:
            continue
        found.append({
            "mode": "DFU" if pid_i in DFU_PIDS else "Recovery",
            "ecid": ecid_key(int(fields["ECID"], 16)),
            "cpid": fields.get("CPID"),
            "bdid": fields.get("BDID"),
        })
    return found


class DeviceScanner:
    """Polls for devices in the background and keeps a merged, cached view.

    Devices are keyed by ECID, which stays the same across normal, recovery and
    DFU mode, so a device keeps its name and serial while it is being restored.
    """

    NORMAL_REFRESH = 30.0

    def __init__(self, tools: Tools, busy_keys: Callable[[], set[str]], interval: float = 3.0):
        self.tools = tools
        self.busy_keys = busy_keys
        self.interval = interval
        self.devices: dict[str, dict] = {}
        self.known: dict[str, dict] = {}  # ecid -> identity seen earlier
        self._normal_cache: dict[str, tuple[float, dict]] = {}
        self._lock = threading.Lock()
        self._wake = threading.Event()
        self.last_error: str | None = None

    def start(self) -> None:
        threading.Thread(target=self._loop, name="device-scanner", daemon=True).start()

    def refresh_now(self, udid: str | None = None) -> None:
        if udid:
            self._normal_cache.pop(udid, None)
        else:
            self._normal_cache.clear()
        self._wake.set()

    def snapshot(self) -> list[dict]:
        with self._lock:
            return sorted(self.devices.values(), key=lambda d: (d.get("name") or "", d["key"]))

    def get(self, key: str) -> dict | None:
        with self._lock:
            d = self.devices.get(key)
            return dict(d) if d else None

    def _loop(self) -> None:
        while True:
            try:
                self.scan()
                self.last_error = None
            except Exception as e:  # keep scanning; surface the problem in the UI
                self.last_error = str(e)
            self._wake.wait(self.interval)
            self._wake.clear()

    def scan(self) -> None:
        busy = self.busy_keys()
        found: dict[str, dict] = {}
        now = time.time()

        for dev in self._scan_normal(busy, now):
            found[dev["key"]] = dev

        recovery: Iterable[dict] = scan_sysfs()
        if not recovery and os.name == "nt":
            recovery = self._scan_irecovery_windows(busy)
        for r in recovery:
            key = r["ecid"]
            if key in found:
                continue
            dev = {"key": key, "udid": None, "paired": None, **r}
            dev.update({k: v for k, v in self.known.get(key, {}).items() if k not in dev or dev[k] is None})
            if not dev.get("product_type") and key not in busy:
                dev.update(self._irecovery_identity(key))
            found[key] = dev

        # Keep a device that is mid-restore in the list even while it re-enumerates.
        with self._lock:
            for key in busy:
                if key in self.devices and key not in found:
                    found[key] = {**self.devices[key], "mode": "Restoring"}
            for key, dev in found.items():
                dev["last_seen"] = now
            self.devices = found

    def _scan_normal(self, busy: set[str], now: float) -> list[dict]:
        if not self.tools.has("idevice_id"):
            return []
        r = self.tools.run("idevice_id", "-l", timeout=10)
        udids = list(dict.fromkeys(line.strip() for line in r.stdout.decode(errors="replace").splitlines() if line.strip()))
        out = []
        for udid in udids:
            cached = self._normal_cache.get(udid)
            if cached and (udid in busy or cached[1].get("ecid") in busy or now - cached[0] < self.NORMAL_REFRESH):
                out.append(cached[1])
                continue
            dev = self._normal_info(udid)
            self._normal_cache[udid] = (now, dev)
            out.append(dev)
        for gone in set(self._normal_cache) - set(udids):
            del self._normal_cache[gone]
        return out

    def _normal_info(self, udid: str) -> dict:
        base = {"key": udid, "udid": udid, "mode": "Normal", "paired": False}
        if not self.tools.has("ideviceinfo"):
            return base
        try:
            r = self.tools.run("ideviceinfo", "-u", udid, "-x", timeout=15)
            paired = r.returncode == 0 and r.stdout.strip().startswith(b"<?xml")
            if not paired:
                # Unpaired: lockdown still answers a few basic keys without trust.
                r = self.tools.run("ideviceinfo", "-u", udid, "-s", "-x", timeout=15)
            if r.returncode != 0 or not r.stdout.strip():
                base["error"] = (r.stderr or r.stdout).decode(errors="replace").strip()[:300]
                return base
            info = parse_lockdown_plist(r.stdout)
        except (subprocess.TimeoutExpired, plistlib.InvalidFileException, ValueError) as e:
            base["error"] = str(e)
            return base
        dev = {**base, **{k: v for k, v in info.items() if v is not None}, "paired": paired}
        dev["udid"] = udid
        if dev.get("ecid"):
            dev["key"] = dev["ecid"]
            self.known[dev["ecid"]] = {k: dev.get(k) for k in ("name", "product_type", "serial", "udid", "os_version")}
        return dev

    def _irecovery_identity(self, ecid: str) -> dict:
        if not self.tools.has("irecovery"):
            return {}
        try:
            r = self.tools.run("irecovery", "-i", "0x" + ecid, "-q", timeout=10)
        except subprocess.TimeoutExpired:
            return {}
        q = parse_irecovery_query(r.stdout.decode(errors="replace"))
        ident = {"product_type": q.get("PRODUCT"), "model": q.get("MODEL"), "display_name": q.get("NAME")}
        ident = {k: v for k, v in ident.items() if v and v != "N/A"}
        if ident:
            self.known.setdefault(ecid, {}).update(ident)
        return ident

    def _scan_irecovery_windows(self, busy: set[str]) -> list[dict]:
        # Without sysfs we can only ask irecovery for "the" recovery device.
        # Never do this while a restore is running: it would grab the USB device.
        if busy or not self.tools.has("irecovery"):
            return []
        try:
            r = self.tools.run("irecovery", "-q", timeout=10)
        except subprocess.TimeoutExpired:
            return []
        if r.returncode != 0:
            return []
        q = parse_irecovery_query(r.stdout.decode(errors="replace"))
        if "ECID" not in q:
            return []
        mode = q.get("MODE", "Recovery")
        return [{
            "mode": "DFU" if "DFU" in mode.upper() else "Recovery",
            "ecid": ecid_key(int(q["ECID"], 16)),
            "cpid": q.get("CPID"),
            "product_type": q.get("PRODUCT"),
            "model": q.get("MODEL"),
            "display_name": q.get("NAME"),
        }]
