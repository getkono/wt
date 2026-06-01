//! Paging of long human listings through `$PAGER` (spec §13).
//!
//! `wt list`/`wt status` page only when stdout is a TTY, paging is not disabled
//! (`--no-pager`/`--json`/non-TTY), and the output exceeds one screen. The
//! default pager is `less -FRX`.

use std::io::Write as _;
use std::process::{Command, Stdio};

use crate::cx::Cx;
use crate::error::Result;

/// The default pager command.
const DEFAULT_PAGER: &str = "less -FRX";
/// Fallback terminal height when the size cannot be determined.
const DEFAULT_ROWS: usize = 24;

/// Writes `content` to stdout, paging it through `$PAGER` when appropriate.
pub fn page(cx: &mut Cx, content: &str) -> Result<()> {
    if should_page(cx.out.is_tty(), cx.no_pager, content, terminal_rows())
        && run_pager(cx, content).is_ok()
    {
        return Ok(());
    }
    // Fall back to direct output if the pager could not run.
    cx.out.text(content)
}

/// Whether `content` should be paged.
pub fn should_page(is_tty: bool, no_pager: bool, content: &str, rows: usize) -> bool {
    is_tty && !no_pager && content.lines().count() > rows
}

/// The terminal height, or a default.
fn terminal_rows() -> usize {
    crossterm::terminal::size()
        .map(|(_, h)| usize::from(h))
        .unwrap_or(DEFAULT_ROWS)
}

/// Spawns the pager and feeds it `content`.
fn run_pager(cx: &Cx, content: &str) -> Result<()> {
    let pager = cx
        .env
        .get("PAGER")
        .filter(|p| !p.is_empty())
        .unwrap_or(DEFAULT_PAGER);
    let argv = shell_words::split(pager).unwrap_or_else(|_| vec![pager.to_string()]);
    let Some((program, rest)) = argv.split_first() else {
        return cx_text_fallback();
    };
    let mut child = Command::new(program)
        .args(rest)
        .stdin(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(content.as_bytes())?;
    }
    child.wait()?;
    Ok(())
}

/// A sentinel error so `run_pager` can signal "use direct output".
fn cx_text_fallback() -> Result<()> {
    Err(crate::error::Error::operation("empty pager command"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::test_cx;

    #[test]
    fn should_page_only_when_tty_and_overflowing() {
        let long = "x\n".repeat(100);
        // Not a TTY -> never page.
        assert!(!should_page(false, false, &long, 24));
        // TTY but --no-pager -> no page.
        assert!(!should_page(true, true, &long, 24));
        // TTY and overflowing -> page.
        assert!(should_page(true, false, &long, 24));
        // TTY but fits on screen -> no page.
        assert!(!should_page(true, false, "one\ntwo\n", 24));
    }

    #[test]
    fn page_writes_directly_when_not_a_tty() {
        // test_cx streams are non-TTY, so page() writes straight to stdout.
        let mut t = test_cx(&[], "/tmp");
        let content = "line1\nline2\n".repeat(50);
        page(&mut t.cx, &content).unwrap();
        assert_eq!(t.out.contents(), content);
    }
}
