//! `--tui` (issue #16) parsing/conflict rules, independent of whether this
//! build actually has the `tui` feature -- clap validates flag
//! combinations before any feature-gated launch code ever runs, so these
//! run the same in every feature combination. See
//! `tests/cli_tui_feature_gate.rs` for the process-level "no `tui` feature"
//! behavior these conflicts feed into.

use clap::Parser;
use vdiff::cli::Cli;

#[test]
fn tui_flag_parses_and_defaults_off() {
    let cli = Cli::try_parse_from(["vdiff"]).expect("bare `vdiff` should parse");
    assert!(!cli.tui, "--tui should default to off");

    let cli = Cli::try_parse_from(["vdiff", "--tui"]).expect("--tui should parse");
    assert!(cli.tui);
}

#[test]
fn tui_conflicts_with_dump() {
    let result = Cli::try_parse_from(["vdiff", "--tui", "--dump", "text"]);
    assert!(result.is_err(), "--tui and --dump should conflict");
}

#[test]
fn tui_conflicts_with_export_comments() {
    let result = Cli::try_parse_from(["vdiff", "--tui", "--export-comments"]);
    assert!(
        result.is_err(),
        "--tui and --export-comments should conflict"
    );
}

#[test]
fn tui_conflicts_with_publish_comments() {
    let result = Cli::try_parse_from(["vdiff", "--tui", "--publish-comments", "42"]);
    assert!(
        result.is_err(),
        "--tui and --publish-comments should conflict"
    );
}

#[test]
fn tui_combines_with_pr_and_findings() {
    let cli = Cli::try_parse_from([
        "vdiff",
        "--tui",
        "--pr",
        "42",
        "--findings",
        "findings.json",
    ])
    .expect("--tui should combine with --pr and --findings, same as the GUI path");
    assert!(cli.tui);
    assert_eq!(cli.pr, Some(42));
    assert_eq!(cli.findings, Some("findings.json".into()));
}
