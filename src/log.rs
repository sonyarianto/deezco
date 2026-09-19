//! Timestamped logging for long-lived processes.
//!
//! `println!`/`eprintln!` carry no time, which makes 24/7 radio logs
//! undebuggable ("when exactly did the reconnect happen?"). These macros
//! prepend a local `[YYYY-MM-DD HH:MM:SS]` stamp and keep the existing
//! `deezco:` message convention, so old greps keep working.
//!
//! Scope is deliberately the streaming path (`icecast.rs`): short-lived
//! download commands print human-scanned progress where stamps add noise.

/// Current local time as `YYYY-MM-DD HH:MM:SS` for log prefixes.
pub fn stamp() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Timestamped stdout log. Drop-in replacement for `println!`.
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {
        println!("[{}] {}", $crate::log::stamp(), format!($($arg)*))
    };
}

/// Timestamped stderr log. Drop-in replacement for `eprintln!`.
#[macro_export]
macro_rules! warn {
    ($($arg:tt)*) => {
        eprintln!("[{}] {}", $crate::log::stamp(), format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamp_renders_local_datetime_shape() {
        let s = stamp();
        assert_eq!(s.len(), 19, "YYYY-MM-DD HH:MM:SS");
        assert_eq!(&s[4..5], "-");
        assert_eq!(&s[7..8], "-");
        assert_eq!(&s[10..11], " ");
        assert_eq!(&s[13..14], ":");
        assert_eq!(&s[16..17], ":");
        assert!(s[..10].replace('-', "").chars().all(|c| c.is_ascii_digit()));
    }
}
