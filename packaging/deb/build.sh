#!/usr/bin/env bash
# Build idevice-fleet_<version>_amd64.deb (and SHA256SUMS) into ./dist
#   packaging/deb/build.sh
# Needs: cargo, dpkg-deb, python3 (for the changelog)
set -euo pipefail
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
cd "$ROOT"
VERSION=$(cargo metadata --no-deps --format-version 1 | python3 -c 'import json,sys; print([p["version"] for p in json.load(sys.stdin)["packages"] if p["name"]=="fleet-app"][0])')
ARCH=amd64
PKG=idevice-fleet
STAGE=$ROOT/target/deb/$PKG
OUT=$ROOT/dist

echo "==> Building $PKG $VERSION (release)"
cargo build --release -p fleet-app --locked
BIN=$ROOT/target/release/idevice-fleet

rm -rf "$STAGE"
install -Dm755 "$BIN" "$STAGE/usr/bin/idevice-fleet"
install -Dm644 packaging/deb/idevice-fleet.desktop "$STAGE/usr/share/applications/idevice-fleet.desktop"
install -Dm644 packaging/icons/idevice-fleet.svg "$STAGE/usr/share/icons/hicolor/scalable/apps/idevice-fleet.svg"
install -Dm644 packaging/linux/70-idevice-fleet.rules "$STAGE/usr/lib/udev/rules.d/70-idevice-fleet.rules"
install -Dm644 LICENSE "$STAGE/usr/share/doc/$PKG/copyright"
gzip -9n -c README.md > "$STAGE/usr/share/doc/$PKG/README.md.gz"
python3 - "$VERSION" > "$STAGE/changelog" <<'PY'
import sys
v = sys.argv[1]
print(f"idevice-fleet ({v}) stable; urgency=medium\n\n  * See https://github.com/Maxjr2/idevice-fleet/releases/tag/v{v}\n\n -- Maximilian Erkens <max@erkens.net>  Thu, 08 Oct 2026 12:00:00 +0000")
PY
gzip -9n -c "$STAGE/changelog" > "$STAGE/usr/share/doc/$PKG/changelog.gz" && rm "$STAGE/changelog"

# The oldest glibc the binary needs.
GLIBC=$(objdump -T "$BIN" | grep -o 'GLIBC_[0-9.]*' | sort -uV | tail -1 | sed 's/GLIBC_//')
SIZE=$(du -sk "$STAGE" | cut -f1)

mkdir -p "$STAGE/DEBIAN"
cat > "$STAGE/DEBIAN/control" <<CTL
Package: $PKG
Version: $VERSION
Architecture: $ARCH
Maintainer: Maximilian Erkens <max@erkens.net>
Installed-Size: $SIZE
Depends: libc6 (>= $GLIBC), usbmuxd, libxkbcommon0, libwayland-client0, libx11-6, libvulkan1 | libgl1
Recommends: idevicerestore, pkexec
Suggests: libimobiledevice-utils
Section: utils
Priority: optional
Homepage: https://github.com/Maxjr2/idevice-fleet
Description: Reset, back up and re-provision many iPhones and iPads
 iDevice Fleet is a desktop app for wiping and reinstalling, backing up and
 tracking many iPhones and iPads at once, without a Mac. It follows devices
 through normal, recovery and DFU mode, keeps a library of firmware that
 Apple still signs, and takes the person through each step.
 .
 Resets use a built-in installer, with idevicerestore as a fallback.
CTL
install -m755 packaging/deb/postinst "$STAGE/DEBIAN/postinst"
install -m755 packaging/deb/postrm "$STAGE/DEBIAN/postrm"

mkdir -p "$OUT"
DEB=$OUT/${PKG}_${VERSION}_${ARCH}.deb
dpkg-deb --root-owner-group -Zxz --build "$STAGE" "$DEB" >/dev/null
( cd "$OUT" && sha256sum "$(basename "$DEB")" > SHA256SUMS )
echo "==> $DEB ($(du -h "$DEB" | cut -f1))"
dpkg-deb -I "$DEB" | sed -n '1,25p'
