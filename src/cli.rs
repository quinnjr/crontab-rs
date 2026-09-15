//! Command-line helpers shared by the bundled `crond` and `crontab`.

use std::process::ExitCode;

use clap::error::ErrorKind;

/// Parse arguments with cronie's exit statuses: the version goes to stdout
/// with status 0; help and usage errors go to stderr with status 1.
pub fn parse_args<P: clap::Parser>() -> Result<P, ExitCode> {
    P::try_parse().map_err(|e| {
        if e.kind() == ErrorKind::DisplayVersion {
            print!("{e}");
            ExitCode::SUCCESS
        } else {
            eprint!("{e}");
            ExitCode::FAILURE
        }
    })
}
