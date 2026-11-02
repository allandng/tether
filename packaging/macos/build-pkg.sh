#!/bin/sh
# Build dist/tether-host-macos-<version>.pkg: tetherd in /usr/local/bin plus a
# per-user LaunchAgent installed and started by the postinstall script.
#
# Signing + notarization (needed for Gatekeeper — an un-notarized
# remote-control daemon is blocked, and "right-click > Open" is not a setup
# step to hand anyone). Optional; set all three to enable:
#   TETHER_APP_IDENTITY        "Developer ID Application: Name (TEAMID)"
#   TETHER_INSTALLER_IDENTITY  "Developer ID Installer: Name (TEAMID)"
#   TETHER_NOTARY_PROFILE      a `xcrun notarytool store-credentials` profile
#
# Prerequisites: macOS 26 + Xcode 26, Rust, `brew install cmake nasm`.
set -eu

root=$(cd "$(dirname "$0")/../.." && pwd)
cd "$root"
version=$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)

cargo build --release -p tetherd

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/root/usr/local/bin" "$stage/root/usr/local/share/tether" "$stage/scripts"
cp target/release/tetherd "$stage/root/usr/local/bin/tetherd"
# Template: postinstall fills in __HOME__ (log path) for each user's copy.
cp packaging/macos/com.tether.daemon.plist "$stage/root/usr/local/share/tether/"
cp packaging/macos/uninstall.sh "$stage/root/usr/local/share/tether/"
cp packaging/macos/postinstall "$stage/scripts/postinstall"
chmod 755 "$stage/scripts/postinstall" "$stage/root/usr/local/share/tether/uninstall.sh"

if [ -n "${TETHER_APP_IDENTITY:-}" ]; then
  # Hardened runtime is a notarization requirement.
  codesign --force --options runtime --timestamp \
    --sign "$TETHER_APP_IDENTITY" "$stage/root/usr/local/bin/tetherd"
fi

mkdir -p dist
unsigned="$stage/tether-unsigned.pkg"
pkgbuild --root "$stage/root" --scripts "$stage/scripts" \
  --identifier com.tether.host --version "$version" \
  --install-location / "$unsigned"

out="dist/tether-host-macos-$version.pkg"
if [ -n "${TETHER_INSTALLER_IDENTITY:-}" ]; then
  productsign --sign "$TETHER_INSTALLER_IDENTITY" "$unsigned" "$out"
else
  cp "$unsigned" "$out"
  echo "warning: unsigned package — Gatekeeper will block it on other Macs" >&2
fi

if [ -n "${TETHER_NOTARY_PROFILE:-}" ]; then
  xcrun notarytool submit "$out" --keychain-profile "$TETHER_NOTARY_PROFILE" --wait
  xcrun stapler staple "$out"
fi
echo "built $out"
