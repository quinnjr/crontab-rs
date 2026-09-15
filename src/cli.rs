//! Command-line helpers shared by the bundled `crond` and `crontab`.

use std::io::Write;
use std::process::ExitCode;

use clap::error::ErrorKind;

/// Parse arguments with cronie's exit statuses: the version goes to stdout
/// with status 0; help and usage errors go to stderr with status 1. Write
/// errors (a closed pipe, a full disk) are ignored rather than panicking.
pub fn parse_args<P: clap::Parser>() -> Result<P, ExitCode> {
    P::try_parse().map_err(|e| {
        if e.kind() == ErrorKind::DisplayVersion {
            let _ = e.print();
            ExitCode::SUCCESS
        } else {
            let _ = write!(std::io::stderr(), "{e}");
            ExitCode::FAILURE
        }
    })
}

/// cronie refuses to read a new crontab from a terminal when no file (or
/// `-`) was given, so a stray Ctrl-D cannot install an empty crontab.
pub fn stdin_file_required(file: Option<&str>, stdin_is_terminal: bool) -> bool {
    file.is_none() && stdin_is_terminal
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminal_stdin_needs_a_file_argument() {
        assert!(stdin_file_required(None, true));
        assert!(!stdin_file_required(None, false));
        assert!(!stdin_file_required(Some("-"), true));
        assert!(!stdin_file_required(Some("tab"), true));
    }
}
