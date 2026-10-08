"""Local web server: JSON API plus the browser UI. Binds to localhost only."""
from __future__ import annotations

import json
import os
import platform
import shutil
import threading
import urllib.error
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

from . import __version__
from .backups import list_backups
from .devices import DeviceScanner
from .firmware import Catalog, Library, check_download_url, download
from .jobs import Job, JobManager, parse_percent_line, parse_restore_line
from .tools import TOOL_NAMES, Tools, ToolMissing

WEB_DIR = os.path.join(os.path.dirname(__file__), "web")
STATIC_TYPES = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
                ".css": "text/css; charset=utf-8", ".svg": "image/svg+xml"}
CSRF_HEADER = "X-iDevice-Fleet"


class ApiError(Exception):
    def __init__(self, message: str, status: int = 400):
        super().__init__(message)
        self.status = status


class App:
    def __init__(self, data_dir: str, tools_dir: str | None = None, max_restores: int = 4):
        self.data_dir = os.path.abspath(os.path.expanduser(data_dir))
        self.backup_dir = os.path.join(self.data_dir, "backups")
        self.cache_dir = os.path.join(self.data_dir, "cache")
        self.log_dir = os.path.join(self.data_dir, "logs")
        for d in (self.backup_dir, self.cache_dir, self.log_dir):
            os.makedirs(d, exist_ok=True)
        self.tools = Tools(tools_dir)
        self.jobs = JobManager()
        self.scanner = DeviceScanner(self.tools, self.jobs.active_device_keys)
        self.library = Library(os.path.join(self.data_dir, "firmware"))
        self.catalog = Catalog()
        self.max_restores = max_restores
        self.restore_slots = threading.Semaphore(max_restores)

    def start(self) -> None:
        self.library.scan()
        self.scanner.start()

    # ---- state -------------------------------------------------------------
    def state(self) -> dict:
        signed = self.catalog.signed_builds()
        library = [{**e, "signed": (e.get("build") in signed) if signed else None} for e in self.library.entries]
        return {
            "version": __version__,
            "platform": platform.system(),
            "data_dir": self.data_dir,
            "max_restores": self.max_restores,
            "tools": {n: self.tools.paths.get(n) for n in TOOL_NAMES},
            "scanner_error": self.scanner.last_error,
            "devices": self.scanner.snapshot(),
            "jobs": [j.to_dict() for j in self.jobs.list()],
            "library": library,
            "library_dir": self.library.dir,
            "backup_dir": self.backup_dir,
        }

    # ---- helpers -----------------------------------------------------------
    def _device(self, key: str, need: str | None = None) -> dict:
        dev = self.scanner.get(key or "")
        if not dev:
            raise ApiError("That device is no longer connected. Refresh and try again.", 404)
        if need == "udid" and (not dev.get("udid") or dev.get("mode") != "Normal"):
            raise ApiError("This action needs the device in normal mode (booted and unlocked).")
        if need == "ecid" and not dev.get("ecid"):
            raise ApiError("The device's ECID isn't known yet. Connect it in normal mode once, or wait for it to be detected.")
        busy = self.jobs.device_busy(dev.get("key"), dev.get("udid"), dev.get("ecid"))
        if busy:
            raise ApiError(f"Busy with job #{busy.id} ({busy.title}). Wait for it or cancel it first.", 409)
        return dev

    def _label(self, dev: dict) -> str:
        return dev.get("name") or dev.get("display_name") or dev.get("product_type") or dev["key"]

    def _tool_job(self, kind: str, title: str, dev: dict, args: list[str], tool: str, **kw) -> Job:
        cmd = [self.tools.path(tool), *args]
        job = Job(kind, title, dev["key"], cmd=cmd, **kw)
        self.jobs.submit(job)
        self.scanner.refresh_now(dev.get("udid"))
        return job

    # ---- actions -----------------------------------------------------------
    def pair(self, body: dict) -> Job:
        dev = self._device(body.get("key"), "udid")
        return self._tool_job("pair", f"Pair {self._label(dev)}", dev, ["-u", dev["udid"], "pair"], "idevicepair",
                              on_finish=lambda j: self.scanner.refresh_now(dev["udid"]))

    def backup(self, body: dict) -> Job:
        dev = self._device(body.get("key"), "udid")
        if not dev.get("paired"):
            raise ApiError("Pair the device first: unlock it, tap Trust, then use Pair.")
        return self._tool_job("backup", f"Back up {self._label(dev)}", dev,
                              ["-u", dev["udid"], "backup", "--full", self.backup_dir], "idevicebackup2",
                              on_line=_percent_progress)

    def encryption(self, body: dict) -> Job:
        dev = self._device(body.get("key"), "udid")
        password = body.get("password") or ""
        if not password:
            raise ApiError("Enter the backup password.")
        state = "on" if body.get("enable") else "off"
        return self._tool_job("encryption", f"Backup encryption {state}: {self._label(dev)}", dev,
                              ["-u", dev["udid"], "encryption", state], "idevicebackup2",
                              env={"BACKUP_PASSWORD": password})

    def restore_backup(self, body: dict) -> Job:
        dev = self._device(body.get("key"), "udid")
        folder = os.path.basename(body.get("backup") or "")
        if not folder or not os.path.isfile(os.path.join(self.backup_dir, folder, "Info.plist")):
            raise ApiError("Choose a backup from the list.")
        env = {"BACKUP_PASSWORD": body["password"]} if body.get("password") else None
        args = ["-u", dev["udid"], "-s", folder, "restore", "--system", "--settings", self.backup_dir]
        return self._tool_job("backup-restore", f"Restore backup to {self._label(dev)}", dev, args,
                              "idevicebackup2", env=env, on_line=_percent_progress)

    def enter_recovery(self, body: dict) -> Job:
        dev = self._device(body.get("key"), "udid")
        return self._tool_job("recovery", f"Enter recovery: {self._label(dev)}", dev,
                              [dev["udid"]], "ideviceenterrecovery")

    def exit_recovery(self, body: dict) -> Job:
        dev = self._device(body.get("key"), "ecid")
        if dev.get("mode") not in ("Recovery", "DFU"):
            raise ApiError("The device isn't in recovery mode.")
        return self._tool_job("recovery", f"Exit recovery: {self._label(dev)}", dev,
                              ["-i", "0x" + dev["ecid"], "-n"], "irecovery")

    def restore(self, body: dict) -> list[Job]:
        erase = bool(body.get("erase", True))
        expected = "ERASE" if erase else "UPDATE"
        if (body.get("confirm") or "").strip().upper() != expected:
            raise ApiError(f"Type {expected} to confirm.")
        targets = body.get("targets") or []
        if not targets:
            raise ApiError("Select at least one device.")
        planned = []
        for t in targets:
            dev = self._device(t.get("key"), "ecid")
            path = self.library.resolve(t.get("ipsw"))
            meta = next((e for e in self.library.entries if e["file"] == os.path.basename(path)), {})
            if dev.get("product_type") and dev["product_type"] not in meta.get("product_types", []):
                raise ApiError(f"{os.path.basename(path)} doesn't support {dev['product_type']} ({self._label(dev)}).")
            planned.append((dev, path))
        tool = self.tools.path("idevicerestore")
        jobs = []
        for dev, path in planned:
            job = Job("restore", f"{'Erase-restore' if erase else 'Update'} {self._label(dev)} → {os.path.basename(path)}",
                      dev["key"], limit=self.restore_slots, on_line=_restore_progress)
            cache = os.path.join(self.cache_dir, f"job-{job.id}")
            os.makedirs(cache, exist_ok=True)
            logfile = os.path.join(self.log_dir, "restore-%d-%s.log" % (job.id, dev["ecid"]))
            job.cmd = [tool, "-i", "0x" + dev["ecid"], "-y", "-P", "-C", cache, "--logfile=" + logfile]
            if erase:
                job.cmd.append("-e")
            job.cmd.append(path)
            job.on_finish = lambda j, c=cache: shutil.rmtree(c, ignore_errors=True)
            jobs.append(self.jobs.submit(job))
        return jobs

    def catalog_lookup(self, identifier: str) -> dict:
        try:
            data = self.catalog.get(identifier.strip())
        except urllib.error.HTTPError as e:
            raise ApiError("ipsw.me doesn't know that model identifier." if e.code == 404 else f"ipsw.me answered {e.code}", 502)
        except (urllib.error.URLError, TimeoutError) as e:
            raise ApiError(f"Couldn't reach ipsw.me: {e}", 502)
        have = {(e.get("build"), tuple(e.get("product_types", []))) for e in self.library.entries}
        builds_here = {b for b, _ in have}
        return {**data, "firmwares": [{**fw, "in_library": fw.get("buildid") in builds_here and any(
            data["identifier"] in pts for b, pts in have if b == fw.get("buildid"))} for fw in data["firmwares"]]}

    def download_firmware(self, body: dict) -> Job:
        url = body.get("url") or ""
        name = check_download_url(url)
        for j in self.jobs.list():
            if j.kind == "download" and j.active and j.title.endswith(name):
                raise ApiError("That firmware is already downloading.", 409)

        def runner(job, cancel):
            download(url, self.library.dir, name, body.get("sha256"), body.get("sha1"), job, cancel)
            self.library.scan()

        return self.jobs.submit(Job("download", f"Download {name}", None, runner=runner))


def _percent_progress(job: Job, line: str) -> None:
    pct = parse_percent_line(line)
    if pct is not None:
        job.progress = pct
    elif not line.startswith("$ "):
        job.stage = line[:120]


def _restore_progress(job: Job, line: str) -> None:
    parsed = parse_restore_line(line)
    if parsed:
        job.stage, job.progress = parsed


class Handler(BaseHTTPRequestHandler):
    app: App
    server_version = "idevice-fleet"

    def log_message(self, fmt, *args):  # quiet: the UI shows what matters
        pass

    # Refuse requests whose Host isn't localhost (blocks DNS-rebinding attacks).
    def _host_ok(self) -> bool:
        host = (self.headers.get("Host") or "").rsplit(":", 1)[0].strip("[]")
        return host in ("127.0.0.1", "localhost", "::1")

    def _send(self, status: int, body: bytes, ctype: str) -> None:
        self.send_response(status)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Content-Security-Policy", "default-src 'self'; style-src 'self'; script-src 'self'")
        self.end_headers()
        self.wfile.write(body)

    def _json(self, data, status: int = 200) -> None:
        self._send(status, json.dumps(data).encode(), "application/json")

    def do_GET(self):
        if not self._host_ok():
            return self._send(403, b"Forbidden", "text/plain")
        url = urlparse(self.path)
        q = parse_qs(url.query)
        try:
            if url.path == "/api/state":
                return self._json(self.app.state())
            if url.path.startswith("/api/jobs/"):
                job = self.app.jobs.get(int(url.path.rsplit("/", 1)[1]))
                if not job:
                    raise ApiError("No such job", 404)
                return self._json(job.to_dict(log_since=int(q.get("since", ["0"])[0])))
            if url.path == "/api/backups":
                return self._json(list_backups(self.app.backup_dir))
            if url.path == "/api/catalog":
                return self._json(self.app.catalog_lookup(q.get("identifier", [""])[0]))
            return self._static(url.path)
        except ApiError as e:
            self._json({"error": str(e)}, e.status)
        except (ValueError, ToolMissing) as e:
            self._json({"error": str(e)}, 400)

    def _static(self, path: str):
        name = "index.html" if path in ("/", "") else path.lstrip("/")
        full = os.path.realpath(os.path.join(WEB_DIR, name))
        if os.path.dirname(full) != os.path.realpath(WEB_DIR) or not os.path.isfile(full):
            return self._send(404, b"Not found", "text/plain")
        with open(full, "rb") as f:
            self._send(200, f.read(), STATIC_TYPES.get(os.path.splitext(full)[1], "application/octet-stream"))

    def do_POST(self):
        if not self._host_ok() or self.headers.get(CSRF_HEADER) != "1":
            return self._send(403, b"Forbidden", "text/plain")
        try:
            length = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(length) or b"{}") if length else {}
            path = urlparse(self.path).path
            a = self.app
            routes = {
                "/api/pair": a.pair, "/api/backup": a.backup, "/api/encryption": a.encryption,
                "/api/backup-restore": a.restore_backup, "/api/recovery/enter": a.enter_recovery,
                "/api/recovery/exit": a.exit_recovery, "/api/firmware/download": a.download_firmware,
            }
            if path in routes:
                return self._json(routes[path](body).to_dict(), 202)
            if path == "/api/restore":
                return self._json([j.to_dict() for j in a.restore(body)], 202)
            if path == "/api/refresh":
                a.tools.refresh()
                a.library.scan()
                a.scanner.refresh_now()
                return self._json({"ok": True})
            if path.startswith("/api/jobs/") and path.endswith("/cancel"):
                ok = a.jobs.cancel(int(path.split("/")[3]))
                return self._json({"ok": ok}, 200 if ok else 409)
            if path == "/api/jobs/clear":
                a.jobs.clear_finished()
                return self._json({"ok": True})
            raise ApiError("Unknown endpoint", 404)
        except ApiError as e:
            self._json({"error": str(e)}, e.status)
        except (ValueError, ToolMissing, OSError) as e:
            self._json({"error": str(e)}, 400)


def serve(app: App, host: str, port: int) -> ThreadingHTTPServer:
    handler = type("BoundHandler", (Handler,), {"app": app})
    httpd = ThreadingHTTPServer((host, port), handler)
    httpd.daemon_threads = True
    return httpd
