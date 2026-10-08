"""Locate and run the libimobiledevice command-line tools."""
from __future__ import annotations

import os
import shutil
import subprocess

IS_WINDOWS = os.name == "nt"

TOOL_NAMES = (
    "idevice_id",
    "ideviceinfo",
    "idevicepair",
    "idevicebackup2",
    "ideviceenterrecovery",
    "idevicerestore",
    "irecovery",
)

# Common install prefixes that are often missing from PATH
# (/opt/local is what `./autogen.sh --prefix=/opt/local` produces).
EXTRA_DIRS = ("/opt/local/bin", "/usr/local/bin", "/opt/homebrew/bin")

# Keep Windows from flashing a console window for every tool call.
CREATION_FLAGS = getattr(subprocess, "CREATE_NO_WINDOW", 0) if IS_WINDOWS else 0


class ToolMissing(RuntimeError):
    pass


class Tools:
    def __init__(self, tools_dir: str | None = None):
        self.tools_dir = tools_dir
        self.paths: dict[str, str | None] = {}
        self.refresh()

    def refresh(self) -> None:
        self.paths = {name: self._find(name) for name in TOOL_NAMES}

    def _find(self, name: str) -> str | None:
        for d in (self.tools_dir, os.environ.get("IDEVICE_FLEET_TOOLS")):
            if d:
                found = shutil.which(name, path=d)
                if found:
                    return found
        found = shutil.which(name)
        if found:
            return found
        if not IS_WINDOWS:
            return shutil.which(name, path=os.pathsep.join(EXTRA_DIRS))
        return None

    def has(self, name: str) -> bool:
        return bool(self.paths.get(name))

    def path(self, name: str) -> str:
        p = self.paths.get(name)
        if not p:
            raise ToolMissing(f"{name} was not found. Install it or pass --tools-dir.")
        return p

    def run(self, name: str, *args: str, timeout: float = 15) -> subprocess.CompletedProcess:
        return subprocess.run(
            [self.path(name), *args],
            capture_output=True,
            timeout=timeout,
            creationflags=CREATION_FLAGS,
        )
