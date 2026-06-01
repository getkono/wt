//! Test-only helpers shared across the crate's unit tests.
//!
//! Provides an in-memory [`SharedBuf`] writer whose contents can be inspected
//! after a command runs, and [`test_cx`] which wires a [`Cx`] to two such
//! buffers plus a fixed environment.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::cx::{Cx, Env, Stream};

/// A cloneable in-memory writer whose contents can be inspected after writes.
///
/// Clones share the same underlying buffer, so a clone handed to a [`Stream`]
/// can be read back through the original handle.
#[derive(Clone, Default)]
pub(crate) struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    /// Creates an empty buffer.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Returns the bytes written so far, decoded as UTF-8 (lossy).
    pub(crate) fn contents(&self) -> String {
        let guard = self.0.lock().expect("buffer lock poisoned");
        String::from_utf8_lossy(&guard).into_owned()
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("buffer lock poisoned")
            .extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A [`Cx`] wired to in-memory buffers, with handles to inspect what was
/// written to stdout (`out`) and stderr (`err`).
pub(crate) struct TestCx {
    /// The context under test.
    pub cx: Cx,
    /// Captures everything written to stdout.
    pub out: SharedBuf,
    /// Captures everything written to stderr.
    pub err: SharedBuf,
}

/// Builds a [`TestCx`] over in-memory buffers, the given environment pairs, and
/// working directory. Both streams report themselves as non-TTYs.
pub(crate) fn test_cx(env: &[(&str, &str)], cwd: &str) -> TestCx {
    let out = SharedBuf::new();
    let err = SharedBuf::new();
    let env_map: HashMap<String, String> = env
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    let cx = Cx::new(
        Stream::new(Box::new(out.clone()), false),
        Stream::new(Box::new(err.clone()), false),
        Env::from_map(env_map),
        PathBuf::from(cwd),
    );
    TestCx { cx, out, err }
}
