//! Human-facing progress for slow foreground operations.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::cx::Stream;
use crate::error::{Error, Result};

const FRAMES: &[&str] = &["|", "/", "-", "\\"];
const TICK: Duration = Duration::from_millis(80);

/// Runs `operation` while displaying an animated TTY spinner on stderr.
/// Non-TTY streams receive stable start/finish lines with no control codes.
pub(crate) fn run<T, F>(stream: &mut Stream, label: &str, operation: F) -> Result<T>
where
    T: Send,
    F: FnOnce() -> Result<T> + Send,
{
    let started = Instant::now();
    if !stream.is_tty() {
        stream.line(&format!("… {label}"))?;
        let result = operation();
        finish_line(stream, label, started, result.is_ok())?;
        return result;
    }

    std::thread::scope(|scope| {
        let (tx, rx) = mpsc::sync_channel(1);
        scope.spawn(move || {
            let _ = tx.send(operation());
        });
        let mut frame = 0;
        let result = loop {
            stream.text(&format!(
                "\r\x1b[2K{} {label}",
                FRAMES[frame % FRAMES.len()]
            ))?;
            stream.flush()?;
            match rx.recv_timeout(TICK) {
                Ok(result) => break result,
                Err(mpsc::RecvTimeoutError::Timeout) => frame = frame.wrapping_add(1),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(Error::operation(format!("{label} task panicked")));
                }
            }
        };
        stream.text("\r\x1b[2K")?;
        finish_line(stream, label, started, result.is_ok())?;
        result
    })
}

fn finish_line(stream: &mut Stream, label: &str, started: Instant, success: bool) -> Result<()> {
    let marker = if success { "✓" } else { "✗" };
    stream.line(&format!(
        "{marker} {label} ({:.1}s)",
        started.elapsed().as_secs_f64()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::SharedBuf;

    #[test]
    fn non_tty_progress_is_stable_and_reports_success() {
        let buffer = SharedBuf::new();
        let mut stream = Stream::new(Box::new(buffer.clone()), false);
        assert_eq!(run(&mut stream, "Generating", || Ok(42)).unwrap(), 42);
        let output = buffer.contents();
        assert!(output.starts_with("… Generating\n"));
        assert!(output.contains("✓ Generating ("));
        assert!(!output.contains("\x1b"));
    }

    #[test]
    fn non_tty_progress_reports_failure_and_preserves_error() {
        let buffer = SharedBuf::new();
        let mut stream = Stream::new(Box::new(buffer.clone()), false);
        let error =
            run::<(), _>(&mut stream, "Generating", || Err(Error::operation("boom"))).unwrap_err();
        assert_eq!(error.to_string(), "boom");
        assert!(buffer.contents().contains("✗ Generating ("));
    }

    #[test]
    fn tty_progress_uses_spinner_and_clears_the_line() {
        let buffer = SharedBuf::new();
        let mut stream = Stream::new(Box::new(buffer.clone()), true);
        run(&mut stream, "Fetching", || Ok(())).unwrap();
        let output = buffer.contents();
        assert!(output.contains("\r\x1b[2K| Fetching"));
        assert!(output.contains("✓ Fetching ("));
    }
}
