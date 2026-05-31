//! `wt` library crate — houses the testable application logic.
//!
//! The binary entry point in `src/main.rs` stays intentionally thin and calls
//! into this crate, so the logic here is what coverage measures.

/// Builds the application's greeting message.
pub fn greeting(name: &str) -> String {
    format!("Hello, {name}! Welcome to wt.")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn greeting_includes_name() {
        assert_eq!(greeting("world"), "Hello, world! Welcome to wt.");
    }

    #[test]
    fn greeting_handles_empty_name() {
        assert_eq!(greeting(""), "Hello, ! Welcome to wt.");
    }
}
