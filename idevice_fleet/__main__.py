from __future__ import annotations

import argparse
import os
import sys
import webbrowser

from . import __version__
from .server import App, serve


def main(argv: list[str] | None = None) -> int:
    p = argparse.ArgumentParser(prog="idevice-fleet", description="Local web UI for libimobiledevice restores and backups.")
    p.add_argument("--data", default=os.environ.get("IDEVICE_FLEET_DATA", os.path.join("~", "idevice-fleet")),
                   help="folder for firmware, backups, logs and cache (default: ~/idevice-fleet)")
    p.add_argument("--port", type=int, default=8765)
    p.add_argument("--tools-dir", help="folder containing idevicerestore, irecovery, idevicebackup2, ...")
    p.add_argument("--max-restores", type=int, default=4, help="restores to run at the same time (default: 4)")
    p.add_argument("--no-browser", action="store_true", help="don't open the browser")
    p.add_argument("--version", action="version", version=f"%(prog)s {__version__}")
    args = p.parse_args(argv)

    app = App(args.data, args.tools_dir, max(1, args.max_restores))
    app.start()
    httpd = serve(app, "127.0.0.1", args.port)
    url = f"http://127.0.0.1:{args.port}/"
    missing = [n for n, path in app.tools.paths.items() if not path]
    print(f"iDevice Fleet {__version__} running at {url}")
    print(f"Data folder: {app.data_dir}")
    if missing:
        print("Missing tools: " + ", ".join(missing))
    if not args.no_browser:
        webbrowser.open(url)
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        print("\nStopping.")
    return 0


if __name__ == "__main__":
    sys.exit(main())
