#!/bin/sh
# Build dist/tether-host-macos-<version>.pkg: tetherd in /usr/local/bin plus a
# per-user LaunchAgent installed and started by the postinstall script.
#
# The result is a product archive (productbuild + distribution.xml) wrapping
# the pkgbuild component, so Installer refuses a Mac older than macOS 26 or of
# the wrong architecture before touching anything.
#
# tetherd is always re-signed with the identifier com.tether.daemon (the
# LaunchAgent label, also the CFBundleIdentifier embedded by crates/tetherd/
# build.rs) instead of the linker's "tetherd-<hash>". Without a certificate
# that is an ad-hoc signature: macOS still pins privacy grants to the exact
# build, so each new build must be granted again. With a Developer ID the
# grants survive upgrades.
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
bundle_id=com.tether.daemon

cargo build --release -p tetherd

stage=$(mktemp -d)
trap 'rm -rf "$stage"' EXIT
mkdir -p "$stage/root/usr/local/bin" "$stage/root/usr/local/share/tether" \
  "$stage/scripts" "$stage/pkgs"
bin="$stage/root/usr/local/bin/tetherd"
cp target/release/tetherd "$bin"
# Template: postinstall fills in __HOME__ (log path) for each user's copy.
cp packaging/macos/com.tether.daemon.plist "$stage/root/usr/local/share/tether/"
cp packaging/macos/uninstall.sh "$stage/root/usr/local/share/tether/"
cp packaging/macos/postinstall "$stage/scripts/postinstall"
chmod 755 "$stage/scripts/postinstall" "$stage/root/usr/local/share/tether/uninstall.sh"

if [ -n "${TETHER_APP_IDENTITY:-}" ]; then
  # Hardened runtime is a notarization requirement.
  codesign --force --options runtime --timestamp --identifier "$bundle_id" \
    --sign "$TETHER_APP_IDENTITY" "$bin"
else
  codesign --force --identifier "$bundle_id" --sign - "$bin"
fi
codesign --verify --strict "$bin"
codesign -dv "$bin" 2>&1 | grep -E '^(Identifier|Signature|Info.plist)' || true

pkgbuild --root "$stage/root" --scripts "$stage/scripts" \
  --identifier com.tether.host --version "$version" \
  --install-location / "$stage/pkgs/tether-host-component.pkg"

archs=$(lipo -archs "$bin" | tr ' ' ',')
sed -e "s/__VERSION__/$version/g" -e "s/__ARCHS__/$archs/g" \
  packaging/macos/distribution.xml >"$stage/distribution.xml"
unsigned="$stage/tether-unsigned.pkg"
productbuild --distribution "$stage/distribution.xml" \
  --package-path "$stage/pkgs" "$unsigned"

mkdir -p dist
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
echo "built $out ($archs, macOS 26+)"
