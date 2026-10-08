"""Local IPSW library and firmware downloads (catalog from ipsw.me)."""
from __future__ import annotations

import hashlib
import json
import os
import plistlib
import re
import threading
import time
import urllib.parse
import urllib.request
import zipfile

CATALOG_URL = "https://api.ipsw.me/v4/device/{identifier}?type=ipsw"
USER_AGENT = "idevice-fleet (+https://github.com/Maxjr2/idevice-fleet)"
ALLOWED_HOSTS = ("updates.cdn-apple.com", "appldnld.apple.com", "secure-appldnld.apple.com",
                 "updates-http.cdn-apple.com", "appldnld.apple.com.edgesuite.net")
_IDENTIFIER = re.compile(r"^[A-Za-z]+\d+,\d+$")


def version_key(v: str | None) -> tuple:
    parts = []
    for p in re.split(r"[.\s]", v or ""):
        parts.append(int(p) if p.isdigit() else 0)
    return tuple(parts)


def read_ipsw_meta(path: str) -> dict:
    """Read version, build and supported models from an IPSW's BuildManifest.plist."""
    with zipfile.ZipFile(path) as z:
        manifest = plistlib.loads(z.read("BuildManifest.plist"))
    return {
        "version": manifest.get("ProductVersion"),
        "build": manifest.get("ProductBuildVersion"),
        "product_types": list(manifest.get("SupportedProductTypes") or []),
    }


class Library:
    """The folder of .ipsw files the restores are taken from."""

    def __init__(self, directory: str):
        self.dir = directory
        os.makedirs(directory, exist_ok=True)
        self._cache_file = os.path.join(directory, ".idevice-fleet-index.json")
        self._lock = threading.Lock()
        self.entries: list[dict] = []
        try:
            with open(self._cache_file, encoding="utf-8") as f:
                self._meta_cache = json.load(f)
        except (OSError, ValueError):
            self._meta_cache = {}

    def scan(self) -> list[dict]:
        entries = []
        changed = False
        for name in sorted(os.listdir(self.dir)):
            if not name.lower().endswith(".ipsw"):
                continue
            path = os.path.join(self.dir, name)
            try:
                st = os.stat(path)
            except OSError:
                continue
            sig = f"{st.st_size}:{int(st.st_mtime)}"
            meta = self._meta_cache.get(name)
            if not meta or meta.get("sig") != sig:
                try:
                    meta = {"sig": sig, **read_ipsw_meta(path)}
                except (zipfile.BadZipFile, KeyError, OSError, plistlib.InvalidFileException) as e:
                    meta = {"sig": sig, "error": f"Not a readable IPSW: {e}"}
                self._meta_cache[name] = meta
                changed = True
            entries.append({"file": name, "size": st.st_size, **{k: v for k, v in meta.items() if k != "sig"}})
        if changed:
            try:
                with open(self._cache_file, "w", encoding="utf-8") as f:
                    json.dump(self._meta_cache, f)
            except OSError:
                pass
        with self._lock:
            self.entries = entries
        return entries

    def for_product(self, product_type: str | None) -> list[dict]:
        """IPSWs that support a model, newest first."""
        with self._lock:
            matches = [e for e in self.entries if product_type and product_type in e.get("product_types", [])]
        return sorted(matches, key=lambda e: version_key(e.get("version")), reverse=True)

    def resolve(self, filename: str) -> str:
        """Absolute path of a library file; refuses anything outside the library folder."""
        name = os.path.basename(filename or "")
        path = os.path.realpath(os.path.join(self.dir, name))
        if not name.lower().endswith(".ipsw") or os.path.dirname(path) != os.path.realpath(self.dir) or not os.path.isfile(path):
            raise ValueError(f"{filename!r} is not in the firmware library")
        return path


class Catalog:
    """Firmware listings from api.ipsw.me, cached for 30 minutes."""

    TTL = 1800

    def __init__(self) -> None:
        self._cache: dict[str, tuple[float, dict]] = {}

    def get(self, identifier: str) -> dict:
        if not _IDENTIFIER.match(identifier or ""):
            raise ValueError("Use a model identifier such as iPad13,18 or iPhone15,2")
        hit = self._cache.get(identifier)
        if hit and time.time() - hit[0] < self.TTL:
            return hit[1]
        req = urllib.request.Request(CATALOG_URL.format(identifier=urllib.parse.quote(identifier)),
                                     headers={"User-Agent": USER_AGENT, "Accept": "application/json"})
        with urllib.request.urlopen(req, timeout=20) as r:
            data = json.load(r)
        result = {
            "identifier": data.get("identifier", identifier),
            "name": data.get("name"),
            "firmwares": [
                {k: fw.get(k) for k in ("version", "buildid", "url", "filesize", "sha256sum", "sha1sum", "signed", "releasedate")}
                for fw in data.get("firmwares", [])
            ],
        }
        self._cache[identifier] = (time.time(), result)
        return result

    def signed_builds(self) -> set[str]:
        out = set()
        for _, data in self._cache.values():
            out.update(fw["buildid"] for fw in data["firmwares"] if fw.get("signed"))
        return out


def check_download_url(url: str) -> str:
    u = urllib.parse.urlparse(url or "")
    if u.scheme != "https" or u.hostname not in ALLOWED_HOSTS or not u.path.lower().endswith(".ipsw"):
        raise ValueError("Only Apple firmware URLs (https, .ipsw) can be downloaded")
    name = os.path.basename(urllib.parse.unquote(u.path))
    if not re.match(r"^[\w.,+-]+\.ipsw$", name, re.IGNORECASE):
        raise ValueError("Unexpected firmware file name")
    return name


def download(url: str, dest_dir: str, filename: str, sha256: str | None, sha1: str | None,
             job, cancel) -> None:
    """Download an IPSW with resume support and verify its checksum before it enters the library."""
    final = os.path.join(dest_dir, filename)
    part = final + ".part"
    if os.path.exists(final):
        job.stage = "Already in library"
        job.progress = 100.0
        return
    hasher = hashlib.sha256() if sha256 else hashlib.sha1() if sha1 else None
    expected = (sha256 or sha1 or "").lower() or None

    have = os.path.getsize(part) if os.path.exists(part) else 0
    if have and hasher:
        job.stage = "Checking partial download"
        with open(part, "rb") as f:
            for block in iter(lambda: f.read(1 << 20), b""):
                hasher.update(block)
                if cancel.is_set():
                    return

    headers = {"User-Agent": USER_AGENT}
    if have:
        headers["Range"] = f"bytes={have}-"
    req = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(req, timeout=60) as r:
        if have and r.status != 206:
            # Server ignored the range: start over.
            have = 0
            hasher = hashlib.sha256() if sha256 else hashlib.sha1() if sha1 else None
        total = have + int(r.headers.get("Content-Length") or 0)
        job.add_line(f"Downloading {filename} ({total / 1e9:.2f} GB){' — resuming' if have else ''}")
        job.stage = "Downloading"
        done = have
        last = 0.0
        with open(part, "ab" if have else "wb") as f:
            while True:
                if cancel.is_set():
                    job.add_line("Paused. Start the download again to resume.")
                    return
                block = r.read(1 << 20)
                if not block:
                    break
                f.write(block)
                if hasher:
                    hasher.update(block)
                done += len(block)
                if total and time.time() - last > 0.5:
                    job.progress = round(done / total * 100, 1)
                    last = time.time()
    if total and done < total:
        raise IOError(f"Download ended early ({done} of {total} bytes). Start it again to resume.")
    if hasher and expected:
        job.stage = "Verifying checksum"
        if hasher.hexdigest().lower() != expected:
            os.remove(part)
            raise IOError("Checksum mismatch: the file was deleted, download it again")
        job.add_line("Checksum OK")
    os.replace(part, final)
    job.stage = "In library"
