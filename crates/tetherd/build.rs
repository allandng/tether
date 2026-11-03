//! macOS only: embed an Info.plist in the `tetherd` binary (`__TEXT,__info_plist`).
//!
//! A bare executable has no bundle, so without this macOS knows it only by
//! its file name: codesign derives the identifier from it, and the privacy
//! panes and prompts have no bundle id, name or usage strings to show. With
//! the section in place the binary carries `com.tether.daemon` (the
//! LaunchAgent label) as its identity, which `packaging/macos/build-pkg.sh`
//! also signs it with.

use std::fmt::Write as _;
use std::path::PathBuf;

const BUNDLE_ID: &str = "com.tether.daemon";

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").expect("set by cargo");
    let entries: &[(&str, &str)] = &[
        ("CFBundleIdentifier", BUNDLE_ID),
        ("CFBundleName", "Tether"),
        ("CFBundleDisplayName", "Tether Host"),
        ("CFBundleExecutable", "tetherd"),
        ("CFBundleInfoDictionaryVersion", "6.0"),
        ("CFBundleShortVersionString", &version),
        ("CFBundleVersion", &version),
        ("LSMinimumSystemVersion", "26.0"),
        (
            "NSScreenCaptureUsageDescription",
            "Tether streams this screen to your paired devices.",
        ),
        (
            "NSDownloadsFolderUsageDescription",
            "Tether saves files you send from a paired device to your Downloads folder.",
        ),
    ];
    let mut plist = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \
         \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
         <plist version=\"1.0\">\n<dict>\n",
    );
    for (key, value) in entries {
        let _ = writeln!(
            plist,
            "  <key>{key}</key>\n  <string>{}</string>",
            xml_escape(value)
        );
    }
    plist.push_str("</dict>\n</plist>\n");

    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("set by cargo")).join("Info.plist");
    std::fs::write(&out, plist).expect("writing Info.plist");
    let path = out.to_str().expect("OUT_DIR is UTF-8");
    // -Wl, splits on commas, so a comma in the path would break the flag;
    // spaces are fine (rustc passes it as one argument, no shell involved).
    assert!(
        !path.contains(','),
        "OUT_DIR contains a comma: {path} — can't pass it to -sectcreate"
    );
    println!("cargo:rustc-link-arg-bin=tetherd=-Wl,-sectcreate,__TEXT,__info_plist,{path}");
}
