//! Every knob the daemon reads must be settable from the NixOS module.
//!
//! The daemon's configuration surface is a set of `VISAGE_*` environment
//! variables read in `crates/visaged/src/config.rs`. The NixOS module is
//! supposed to expose each of them as an option. Nothing checked that, and the
//! two drifted: `VISAGE_FRAMING_MS` shipped in the daemon and was unreachable
//! from NixOS config entirely, so the framing phase could not be tuned or
//! disabled on the one platform Visage is the default authentication layer for.
//!
//! The failure is quiet in the worst way. A missing option is not a build
//! error, not a warning, and not visible in `visage status` — the daemon simply
//! uses its compiled default forever while the module's documentation implies
//! the setting is configurable.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is <repo>/crates/visaged
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("manifest dir should be <repo>/crates/visaged")
        .to_path_buf()
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Every `VISAGE_*` name appearing in a source or Nix file.
///
/// ⚠️ Skips the bare prefix. Both files write `VISAGE_*` in prose, which
/// tokenises to `VISAGE_` with nothing after it — a first version of this
/// scraper collected that as if it were a variable, so the daemon side carried
/// a name the module side never could and the test failed for a reason that had
/// nothing to do with the contract.
fn visage_env_names(body: &str) -> BTreeSet<String> {
    const PREFIX: &str = "VISAGE_";
    let mut names = BTreeSet::new();
    let mut rest = body;
    while let Some(i) = rest.find(PREFIX) {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(tail.len());
        let name = &tail[..end];
        if name.len() > PREFIX.len() && !name.ends_with('_') {
            names.insert(name.to_string());
        }
        rest = &tail[end..];
    }
    names
}

/// Knobs the daemon reads that the module deliberately does not expose.
///
/// `VISAGE_SESSION_BUS` puts the daemon on the session bus instead of the
/// system bus. That is a development and integration-test affordance — it also
/// skips `require_root_caller` — and a NixOS option for it would be an option
/// for weakening the service's own access control. It stays environment-only.
const DELIBERATELY_NOT_IN_MODULE: &[&str] = &["VISAGE_SESSION_BUS"];

#[test]
fn every_daemon_env_knob_is_exposed_as_a_nixos_option() {
    let daemon = visage_env_names(&read("crates/visaged/src/config.rs"));
    let module = visage_env_names(&read("packaging/nix/module.nix"));

    // Positive controls, both directions: an empty scrape on either side would
    // make the comparison below vacuously pass.
    assert!(
        daemon.contains("VISAGE_CAMERA_DEVICE"),
        "extractor found no VISAGE_CAMERA_DEVICE in config.rs, so the daemon \
         list is not trustworthy and a pass here would mean nothing. found: {daemon:?}"
    );
    assert!(
        module.contains("VISAGE_CAMERA_DEVICE"),
        "extractor found no VISAGE_CAMERA_DEVICE in module.nix, so the module \
         list is not trustworthy. found: {module:?}"
    );

    let missing: Vec<&String> = daemon
        .iter()
        .filter(|n| !module.contains(*n))
        .filter(|n| !DELIBERATELY_NOT_IN_MODULE.contains(&n.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "the daemon reads these knobs but the NixOS module cannot set them:\n  {}\n\
         Add an option in packaging/nix/module.nix and wire it into the service's \
         `environment` block, or add it to DELIBERATELY_NOT_IN_MODULE with a reason.",
        missing
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}

#[test]
fn the_module_does_not_set_knobs_the_daemon_never_reads() {
    let daemon = visage_env_names(&read("crates/visaged/src/config.rs"));
    let module = visage_env_names(&read("packaging/nix/module.nix"));

    assert!(
        !daemon.is_empty() && !module.is_empty(),
        "positive control failed — one side scraped empty"
    );

    // The reverse direction catches a different bug: a renamed daemon knob
    // leaves the module setting a variable nothing reads, which looks exactly
    // like a working setting and silently does nothing.
    let orphaned: Vec<&String> = module.iter().filter(|n| !daemon.contains(*n)).collect();

    assert!(
        orphaned.is_empty(),
        "the NixOS module sets these, but the daemon's config.rs never reads them \
         — a rename would leave the option silently inert:\n  {}",
        orphaned
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n  ")
    );
}
