"""Read the backups idevicebackup2 has written to the backup folder."""
from __future__ import annotations

import datetime
import os
import plistlib


def list_backups(directory: str) -> list[dict]:
    out = []
    try:
        names = sorted(os.listdir(directory))
    except OSError:
        return out
    for name in names:
        folder = os.path.join(directory, name)
        info_path = os.path.join(folder, "Info.plist")
        if not os.path.isfile(info_path):
            continue
        try:
            with open(info_path, "rb") as f:
                info = plistlib.load(f)
        except (OSError, plistlib.InvalidFileException, ValueError):
            continue
        encrypted = None
        try:
            with open(os.path.join(folder, "Manifest.plist"), "rb") as f:
                encrypted = bool(plistlib.load(f).get("IsEncrypted"))
        except (OSError, plistlib.InvalidFileException, ValueError):
            pass
        date = info.get("Last Backup Date")
        if isinstance(date, datetime.datetime):
            date = date.replace(tzinfo=date.tzinfo or datetime.timezone.utc).isoformat()
        out.append({
            "folder": name,
            "device_name": info.get("Device Name") or info.get("Display Name"),
            "product_type": info.get("Product Type"),
            "os_version": info.get("Product Version"),
            "serial": info.get("Serial Number"),
            "udid": info.get("Target Identifier") or name,
            "date": date,
            "encrypted": encrypted,
            "complete": os.path.isfile(os.path.join(folder, "Status.plist")),
        })
    return out
