"""Background jobs: tool processes and downloads, with progress and logs."""
from __future__ import annotations

import collections
import itertools
import os
import re
import subprocess
import threading
import time
from typing import Callable

from .tools import CREATION_FLAGS

RESTORE_STEPS = (
    "Detecting device",
    "Preparing",
    "Uploading filesystem",
    "Verifying filesystem",
    "Flashing firmware",
    "Flashing baseband",
    "Updating firmware",
    "Uploading images",
)

_RESTORE_PROGRESS = re.compile(r"^progress:\s+(\d+)\s+([0-9.]+)")
_PERCENT = re.compile(r"(\d{1,3}(?:\.\d+)?)\s?%")
_LINE_SPLIT = re.compile(r"[\r\n]")


def parse_restore_line(line: str) -> tuple[str, float] | None:
    """Parse an `idevicerestore -P` progress line into (stage, percent overall)."""
    m = _RESTORE_PROGRESS.match(line)
    if not m:
        return None
    step, frac = int(m.group(1)), min(max(float(m.group(2)), 0.0), 1.0)
    stage = RESTORE_STEPS[step] if step < len(RESTORE_STEPS) else f"Step {step}"
    return stage, round(frac * 100, 1)


def parse_percent_line(line: str) -> float | None:
    """Last percentage on a line, as printed by idevicebackup2's progress bar."""
    found = _PERCENT.findall(line)
    if not found:
        return None
    return min(float(found[-1]), 100.0)


class Job:
    _ids = itertools.count(1)

    def __init__(self, kind: str, title: str, device_key: str | None = None, *,
                 cmd: list[str] | None = None, env: dict | None = None,
                 runner: Callable[["Job", threading.Event], None] | None = None,
                 on_line: Callable[["Job", str], None] | None = None,
                 on_finish: Callable[["Job"], None] | None = None,
                 limit: threading.Semaphore | None = None):
        self.id = next(self._ids)
        self.kind, self.title, self.device_key = kind, title, device_key
        self.cmd, self.env, self.runner = cmd, env, runner
        self.on_line, self.on_finish, self.limit = on_line, on_finish, limit
        self.status = "queued"
        self.stage: str | None = None
        self.progress: float | None = None
        self.error: str | None = None
        self.created = time.time()
        self.started: float | None = None
        self.ended: float | None = None
        self.returncode: int | None = None
        self.log: collections.deque[str] = collections.deque(maxlen=4000)
        self.log_total = 0
        self.cancel_event = threading.Event()
        self._proc: subprocess.Popen | None = None

    @property
    def active(self) -> bool:
        return self.status in ("queued", "running")

    def add_line(self, line: str) -> None:
        self.log.append(line)
        self.log_total += 1

    def to_dict(self, log_since: int | None = None) -> dict:
        d = {
            "id": self.id, "kind": self.kind, "title": self.title, "device_key": self.device_key,
            "status": self.status, "stage": self.stage, "progress": self.progress, "error": self.error,
            "created": self.created, "started": self.started, "ended": self.ended,
            "returncode": self.returncode, "log_total": self.log_total,
            "last_line": self.log[-1] if self.log else None,
            # Show the command, never the environment (it can hold backup passwords).
            "command": " ".join(self.cmd) if self.cmd else None,
        }
        if log_since is not None:
            lines = list(self.log)
            first = self.log_total - len(lines)
            d["log_first"] = max(first, log_since)
            d["log"] = lines[max(0, log_since - first):]
        return d


class JobManager:
    def __init__(self) -> None:
        self.jobs: dict[int, Job] = {}
        self._lock = threading.Lock()

    def submit(self, job: Job) -> Job:
        with self._lock:
            self.jobs[job.id] = job
        threading.Thread(target=self._run, args=(job,), name=f"job-{job.id}", daemon=True).start()
        return job

    def list(self) -> list[Job]:
        with self._lock:
            return sorted(self.jobs.values(), key=lambda j: -j.id)

    def get(self, job_id: int) -> Job | None:
        return self.jobs.get(job_id)

    def active_device_keys(self) -> set[str]:
        keys: set[str] = set()
        for j in self.list():
            if j.active and j.device_key:
                keys.add(j.device_key)
        return keys

    def device_busy(self, *keys: str | None) -> Job | None:
        for j in self.list():
            if j.active and j.device_key and j.device_key in keys:
                return j
        return None

    def cancel(self, job_id: int) -> bool:
        job = self.jobs.get(job_id)
        if not job or not job.active:
            return False
        job.cancel_event.set()
        if job._proc and job._proc.poll() is None:
            job._proc.terminate()
        return True

    def clear_finished(self) -> None:
        with self._lock:
            self.jobs = {i: j for i, j in self.jobs.items() if j.active}

    def _run(self, job: Job) -> None:
        if job.limit:
            job.stage = "Waiting for a free slot"
            while not job.limit.acquire(timeout=0.5):
                if job.cancel_event.is_set():
                    self._finish(job, "cancelled")
                    return
        try:
            if job.cancel_event.is_set():
                self._finish(job, "cancelled")
                return
            job.status, job.started, job.stage = "running", time.time(), None
            if job.runner:
                job.runner(job, job.cancel_event)
                self._finish(job, "cancelled" if job.cancel_event.is_set() else "done")
            else:
                self._run_process(job)
        except Exception as e:
            job.error = str(e)
            job.add_line(f"ERROR: {e}")
            self._finish(job, "cancelled" if job.cancel_event.is_set() else "failed")
        finally:
            if job.limit:
                job.limit.release()

    def _run_process(self, job: Job) -> None:
        assert job.cmd
        job.add_line("$ " + " ".join(job.cmd))
        env = {**os.environ, **(job.env or {})}
        proc = subprocess.Popen(job.cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                                stdin=subprocess.DEVNULL, env=env, creationflags=CREATION_FLAGS)
        job._proc = proc
        buf = ""
        assert proc.stdout
        while True:
            chunk = proc.stdout.read1(4096) if hasattr(proc.stdout, "read1") else proc.stdout.read(1)
            if not chunk:
                break
            buf += chunk.decode("utf-8", errors="replace")
            parts = _LINE_SPLIT.split(buf)
            buf = parts.pop()
            for line in parts:
                if line.strip():
                    self._line(job, line.rstrip())
        if buf.strip():
            self._line(job, buf.rstrip())
        job.returncode = proc.wait()
        if job.cancel_event.is_set():
            self._finish(job, "cancelled")
        elif job.returncode == 0:
            self._finish(job, "done")
        else:
            job.error = job.error or self._guess_error(job)
            self._finish(job, "failed")

    def _line(self, job: Job, line: str) -> None:
        job.add_line(line)
        if job.on_line:
            try:
                job.on_line(job, line)
            except Exception:
                pass

    @staticmethod
    def _guess_error(job: Job) -> str:
        for line in reversed(job.log):
            if "ERROR" in line.upper() or "FAIL" in line.upper():
                return line[:300]
        return f"Exited with code {job.returncode}"

    def _finish(self, job: Job, status: str) -> None:
        job.status, job.ended = status, time.time()
        if status == "done" and job.progress is not None:
            job.progress = 100.0
        if job.on_finish:
            try:
                job.on_finish(job)
            except Exception:
                pass
