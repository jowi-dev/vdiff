//! Embedded-nvim is the default file viewer (see `src/cli/mod.rs`): bare
//! `vdiff` must behave as if `--nvim` had always been passed, since
//! launchers like tskmstr's `tm` spawn bare `vdiff` and were silently
//! getting the legacy built-in viewer. `--no-nvim` is the opt-out back to
//! that legacy viewer; the old opt-in `--nvim` flag is gone entirely (not
//! kept as a hidden no-op -- nothing in this repo's docs or scripts invokes
//! it as an external caller, only README prose that this change updates).

use clap::Parser;
use vdiff::cli::Cli;

#[test]
fn no_args_default_to_nvim_mode_on() {
    let cli = Cli::try_parse_from(["vdiff"]).expect("bare `vdiff` should parse");
    assert!(cli.nvim, "nvim mode should default to true");
}

#[test]
fn no_nvim_flag_opts_out() {
    let cli = Cli::try_parse_from(["vdiff", "--no-nvim"]).expect("--no-nvim should parse");
    assert!(!cli.nvim, "--no-nvim should turn nvim mode off");
}

#[test]
fn bare_nvim_flag_is_no_longer_accepted() {
    let result = Cli::try_parse_from(["vdiff", "--nvim"]);
    assert!(
        result.is_err(),
        "--nvim should no longer parse now that nvim mode is the default"
    );
}
