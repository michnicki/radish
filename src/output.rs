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

use std::fmt::{self, Arguments, Write as _};
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
        // Ignore a failure here: stderr may be the same broken destination.
        let _ = writeln!(io::stderr(), "error: failed to write output: {error}");
    }
    std::process::exit(code);
}

/// Displays text with terminal control and bidirectional-override characters
/// escaped.
///
/// Tags and provider responses are untrusted, so printing them raw lets a
/// crafted value inject escape sequences or reorder the visible text. Control
/// characters and the bidi embedding, override and isolate ranges are written
/// as `\u{..}` escapes; every other character is unchanged.
pub struct Clean<'a>(pub &'a str);

impl fmt::Display for Clean<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut escaped = String::with_capacity(self.0.len());
        for c in self.0.chars() {
            if c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                write!(escaped, "{}", c.escape_unicode())?;
            } else {
                escaped.push(c);
            }
        }
        // `pad` applies the caller's width, alignment and precision.
        f.pad(&escaped)
    }
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

    #[test]
    fn clean_honors_width_and_alignment() {
        assert_eq!(format!("{:<5}|", Clean("ab")), "ab   |");
        assert_eq!(format!("{:>5}|", Clean("ab")), "   ab|");
    }

    #[test]
    fn clean_escapes_control_and_bidi_characters_only() {
        let shown = Clean("a\x1b]0;x\x07b\u{202e}c").to_string();
        assert!(
            !shown.chars().any(|c| c.is_control() || c == '\u{202e}'),
            "{shown:?}"
        );
        assert_eq!(shown, "a\\u{1b}]0;x\\u{7}b\\u{202e}c");
        assert_eq!(Clean("Sigur Rós ( )").to_string(), "Sigur Rós ( )");
        assert_eq!(Clean("÷").to_string(), "÷");
        assert_eq!(Clean("\u{2066}\u{2069}").to_string(), "\\u{2066}\\u{2069}");
    }
}
