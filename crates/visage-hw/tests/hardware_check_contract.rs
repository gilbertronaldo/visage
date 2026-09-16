//! Contract: `scripts/check-hardware.sh` reads the same quirk database the
//! binary embeds.
//!
//! The pre-install check exists to answer "will this work on my laptop?" before
//! the user has a Rust toolchain, so it cannot link against `visage-hw` — it
//! parses `contrib/hw/*.toml` with `sed`. That makes it a *second*
//! implementation of quirk lookup, and this repository has already shipped that
//! exact class of defect twice: the hardware compatibility table drifted from
//! the shipped quirks (#94), and the README's copy of the same table was four
//! entries behind that.
//!
//! A drifted check is worse than no check. It would tell someone their camera is
//! unsupported when a quirk for it ships — and they would believe it, because it
//! is the first thing the project ever told them.
//!
//! So: every quirk the binary embeds must be findable by the script, under the
//! script's own parser, by name. And a device that is absent must come back
//! empty, or a passing test here would only prove the script says yes to
//! everything.

use std::path::{Path, PathBuf};
use std::process::Command;

fn repo_root() -> PathBuf {
    // crates/visage-hw/ -> crates/ -> repo root
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above the crate manifest")
        .to_path_buf()
}

fn script() -> PathBuf {
    repo_root().join("scripts/check-hardware.sh")
}

fn lookup(vid: u16, pid: u16) -> String {
    let out = Command::new("sh")
        .arg(script())
        .arg("--quirk-lookup")
        .arg(format!("{vid:04x}"))
        .arg(format!("{pid:04x}"))
        .output()
        .expect("failed to run scripts/check-hardware.sh");
    assert!(
        out.status.success(),
        "--quirk-lookup {vid:04x} {pid:04x} exited {:?}: {}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn script_exists_and_is_executable() {
    let s = script();
    assert!(s.is_file(), "{} is missing", s.display());

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = s.metadata().expect("stat the script").permissions().mode();
        assert!(
            mode & 0o111 != 0,
            "{} is not executable (mode {mode:o}) — git records the bit, and a \
             fresh clone of a 644 file leaves the documented command failing",
            s.display()
        );
    }
}

/// The positive direction: every embedded quirk is findable by the script.
#[test]
fn every_embedded_quirk_is_found_by_the_script() {
    let quirks = visage_hw::quirks::list_quirks();
    assert!(
        !quirks.is_empty(),
        "the quirk database is empty — this test would otherwise pass vacuously"
    );

    let mut missing = Vec::new();
    for q in quirks {
        let (vid, pid) = (q.device.vendor_id, q.device.product_id);
        let got = lookup(vid, pid);
        if got != q.device.name {
            missing.push(format!(
                "{vid:04x}:{pid:04x} — binary says {:?}, script says {:?}",
                q.device.name, got
            ));
        }
    }
    assert!(
        missing.is_empty(),
        "scripts/check-hardware.sh disagrees with the embedded quirk database:\n  {}",
        missing.join("\n  ")
    );
}

/// The negative direction, without which the test above proves nothing: a device
/// with no quirk must come back empty rather than matching something.
///
/// `3277:0055` is a real, measured case — the Shinetech IR module in an ASUS
/// Zenbook 14 UM3406HA, which ships no quirk entry and works anyway because its
/// emitter strobes by firmware default.
#[test]
fn a_device_without_a_quirk_returns_nothing() {
    assert_eq!(
        lookup(0x3277, 0x0055),
        "",
        "the script matched a quirk for a device that has none — its parser is \
         too loose, and every 'supported' verdict it gives is suspect"
    );
}

/// Lookup must not care about hex case: sysfs reports lowercase, the TOML files
/// declare uppercase.
#[test]
fn lookup_is_case_insensitive() {
    let q = visage_hw::quirks::list_quirks()
        .first()
        .expect("at least one quirk");
    let (vid, pid) = (q.device.vendor_id, q.device.product_id);

    let lower = Command::new("sh")
        .arg(script())
        .arg("--quirk-lookup")
        .arg(format!("{vid:04x}"))
        .arg(format!("{pid:04x}"))
        .output()
        .expect("run script");
    let upper = Command::new("sh")
        .arg(script())
        .arg("--quirk-lookup")
        .arg(format!("{vid:04X}"))
        .arg(format!("{pid:04X}"))
        .output()
        .expect("run script");

    assert_eq!(
        String::from_utf8_lossy(&lower.stdout).trim(),
        String::from_utf8_lossy(&upper.stdout).trim(),
        "uppercase and lowercase VID:PID gave different answers"
    );
    assert_eq!(String::from_utf8_lossy(&lower.stdout).trim(), q.device.name);
}
