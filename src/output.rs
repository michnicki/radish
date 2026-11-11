//! Stdout helpers that treat a closed pipe as a clean end of output.
//!
//! Rust ignores `SIGPIPE`, so writing to a pipe whose reader has exited (for
//! example `radish list | head`) fails with `EPIPE`. The standard `println!`
//! macro panics on that error. CLI output goes through these helpers instead,
//! so a consumer that stops reading simply ends the command.

use std::fmt::Arguments;
use std::io::{self, ErrorKind, Write};

/// Treats a broken pipe as success and leaves every other write error visible.
pub fn ignore_broken_pipe(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if error.kind() == ErrorKind::BrokenPipe => Ok(()),
        other => other,
    }
}

/// Writes formatted text to stdout without a trailing newline.
pub fn write(args: Arguments<'_>) {
    let mut stdout = io::stdout().lock();
    let _ = ignore_broken_pipe(stdout.write_fmt(args));
}

/// Writes one formatted line to stdout.
pub fn write_line(args: Arguments<'_>) {
    let mut stdout = io::stdout().lock();
    let _ = ignore_broken_pipe(stdout.write_fmt(args));
    let _ = ignore_broken_pipe(stdout.write_all(b"\n"));
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Closed;

    impl Write for Closed {
        fn write(&mut self, _: &[u8]) -> io::Result<usize> {
            Err(io::Error::new(ErrorKind::BrokenPipe, "reader closed"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_closed_pipe_ends_output_cleanly() {
        assert!(ignore_broken_pipe(Closed.write_all(b"line")).is_ok());
    }

    #[test]
    fn other_write_failures_stay_visible() {
        assert!(ignore_broken_pipe(Err(io::Error::other("disk full"))).is_err());
    }
}
