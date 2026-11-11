//! Stdout helpers that end the process when output cannot be delivered.
//!
//! Rust ignores `SIGPIPE`, so writing to a pipe whose reader has exited (for
//! example `radish list | head`) fails with `EPIPE`. The standard `println!`
//! macro panics on that error. CLI output goes through these helpers instead:
//! a closed pipe exits with status 0 (the consumer chose to stop reading, and
//! this also stops long-running commands such as `watch`), while any other
//! write error is reported on stderr and exits with status 1.
//!
//! Exiting mid-command is safe because SQLite rolls back uncommitted
//! transactions when the process ends.

use std::fmt::Arguments;
use std::io::{self, ErrorKind, Write};

/// Maps an output error to the process exit status: 0 for a closed pipe, 1 otherwise.
pub fn exit_code_for(error: &io::Error) -> i32 {
    if error.kind() == ErrorKind::BrokenPipe {
        0
    } else {
        1
    }
}

/// Ends the process if a write failed; returns normally on success.
fn exit_on_error(result: io::Result<()>) {
    let Err(error) = result else { return };
    let code = exit_code_for(&error);
    if code != 0 {
        eprintln!("error: failed to write output: {error}");
    }
    std::process::exit(code);
}

/// Writes formatted text to stdout without a trailing newline.
///
/// Exits the process with status 0 on a broken pipe, and with status 1 after
/// reporting any other write error on stderr.
pub fn write(args: Arguments<'_>) {
    let result = io::stdout().lock().write_fmt(args);
    exit_on_error(result);
}

/// Writes one formatted line to stdout.
///
/// Exits the process like [`write`] when the line cannot be written.
pub fn write_line(args: Arguments<'_>) {
    let result = {
        let mut stdout = io::stdout().lock();
        stdout
            .write_fmt(args)
            .and_then(|()| stdout.write_all(b"\n"))
    };
    exit_on_error(result);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_closed_pipe_exits_cleanly_and_other_errors_fail() {
        let closed = io::Error::new(ErrorKind::BrokenPipe, "reader closed");
        assert_eq!(exit_code_for(&closed), 0);
        assert_eq!(exit_code_for(&io::Error::other("disk full")), 1);
        assert_eq!(exit_code_for(&io::Error::from(ErrorKind::StorageFull)), 1);
    }
}
