//! JSON output helpers: the newline-delimited framing used by `list` and
//! `status --all` (spec §7 — one object per line, never a wrapping array).

use serde::Serialize;

use crate::error::Result;

/// Serializes `value` to a single-line JSON string with no trailing newline.
/// Callers write each line through [`crate::cx::Stream::line`], producing the
/// newline-delimited stream.
pub fn to_line<T: Serialize>(value: &T) -> Result<String> {
    Ok(serde_json::to_string(value)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_line_is_single_line() {
        let line = to_line(&serde_json::json!({"a": 1, "b": [1, 2]})).unwrap();
        assert!(!line.contains('\n'));
    }

    #[test]
    fn lines_join_into_newline_delimited_stream() {
        let a = to_line(&serde_json::json!({"n": 1})).unwrap();
        let b = to_line(&serde_json::json!({"n": 2})).unwrap();
        let stream = format!("{a}\n{b}\n");
        assert_eq!(stream.lines().count(), 2);
        // Each line is independently parseable (not a single array document).
        for l in stream.lines() {
            let _: serde_json::Value = serde_json::from_str(l).unwrap();
        }
    }
}
