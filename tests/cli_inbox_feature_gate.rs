//! Issue #36: `--inbox`'s picker is part of the terminal UI, so a build
//! without the `tui` feature must exit nonzero naming the missing feature
//! -- before touching `gh` at all -- while `--inbox --json` stays available
//! (see `tests/cli_inbox_flag.rs`). Compiled only into a `not(feature =
//! "tui")` test binary, mirroring `tests/cli_tui_feature_gate.rs`.

#![cfg(not(feature = "tui"))]

use std::process::Command;

use tempfile::TempDir;

#[test]
fn inbox_picker_without_tui_feature_names_the_feature() {
    // An empty PATH proves the gate runs before any `gh` call: with `gh`
    // reachable first, this would fail on "gh not found" instead.
    let empty_path = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .arg("--inbox")
        .env("PATH", empty_path.path())
        .output()
        .expect("run vdiff");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`tui` feature"), "{stderr}");
    assert!(stderr.contains("--inbox --json"), "{stderr}");
}
