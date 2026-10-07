//! `--inbox` (issue #36) parsing/conflict rules plus its headless JSON
//! path's failure mode when `gh` is missing. Clap validates flag
//! combinations before any feature-gated code runs, so these run the same
//! in every feature combination. See `tests/cli_inbox_feature_gate.rs` for
//! the picker's "no `tui` feature" behavior.

use std::process::Command;

use clap::Parser;
use tempfile::TempDir;
use vdiff::cli::Cli;

#[test]
fn inbox_flags_parse_and_default_off() {
    let cli = Cli::try_parse_from(["vdiff"]).unwrap();
    assert!(!cli.inbox && !cli.json && !cli.all_repos);

    let cli = Cli::try_parse_from(["vdiff", "--inbox", "--json", "--all-repos"]).unwrap();
    assert!(cli.inbox && cli.json && cli.all_repos);
}

#[test]
fn json_and_all_repos_require_inbox() {
    assert!(Cli::try_parse_from(["vdiff", "--json"]).is_err());
    assert!(Cli::try_parse_from(["vdiff", "--all-repos"]).is_err());
}

#[test]
fn inbox_combines_with_tui_and_no_nvim_for_the_opened_pr() {
    let cli = Cli::try_parse_from(["vdiff", "--inbox", "--tui", "--no-nvim"]).unwrap();
    assert!(cli.inbox && cli.tui && !cli.nvim);
}

#[test]
fn inbox_conflicts_with_other_modes() {
    for other in [
        &["--pr", "1"][..],
        &["--dump", "json"],
        &["--export-comments"],
        &["--publish-comments", "1"],
        &["--findings", "f.json"],
    ] {
        let mut args = vec!["vdiff", "--inbox"];
        args.extend_from_slice(other);
        assert!(
            Cli::try_parse_from(&args).is_err(),
            "--inbox should conflict with {other:?}"
        );
    }
}

#[test]
fn inbox_json_without_gh_fails_with_a_friendly_message() {
    let empty_path = TempDir::new().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_vdiff"))
        .args(["--inbox", "--json"])
        .env("PATH", empty_path.path())
        .output()
        .expect("run vdiff");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("`gh` (GitHub CLI) not found"), "{stderr}");
    assert!(output.stdout.is_empty());
}
