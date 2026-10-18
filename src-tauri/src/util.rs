//! Small shared helpers: poison-tolerant locks, text trimming, name checks, clocks.

use std::sync::{Mutex, MutexGuard};

/// `lock()` that ignores poisoning (a panicked holder must not take the kernel down with it).
pub trait MutexExt<T: ?Sized> {
    fn locked(&self) -> MutexGuard<'_, T>;
}

impl<T: ?Sized> MutexExt<T> for Mutex<T> {
    fn locked(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// The last `n` chars of `s`.
pub fn tail_chars(s: &str, n: usize) -> String {
    let count = s.chars().count();
    if count <= n { s.to_string() } else { s.chars().skip(count - n).collect() }
}

/// At most `n` chars of `s`, with "…" when cut.
pub fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() > n { format!("{}…", s.chars().take(n).collect::<String>()) } else { s.to_string() }
}

/// One line (newlines → spaces), at most `n` chars.
pub fn short(s: &str, n: usize) -> String {
    truncate(&s.replace('\n', " "), n)
}

/// `[a-z0-9-]+`, at most `max` chars (module ids, service names).
pub fn valid_slug(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `[A-Za-z0-9_-]+`, at most `max` chars (layout ids, window ids, job names, agent names).
pub fn valid_ident(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Seconds since the Unix epoch.
pub fn now_secs() -> i64 {
    chrono::Utc::now().timestamp()
}

/// Milliseconds since the Unix epoch.
pub fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_helpers() {
        assert_eq!(tail_chars("héllo", 3), "llo");
        assert_eq!(truncate("abcdef", 3), "abc…");
        assert_eq!(short("a\nb", 10), "a b");
        assert!(valid_slug("my-mod-2", 40) && !valid_slug("My", 40) && !valid_slug("", 40));
        assert!(valid_ident("a_B-1", 64) && !valid_ident("a.b", 64));
    }

    #[test]
    fn poisoned_lock_still_works() {
        let m = std::sync::Arc::new(Mutex::new(1));
        let m2 = m.clone();
        let _ = std::thread::spawn(move || {
            let _g = m2.lock().unwrap();
            panic!("poison");
        })
        .join();
        *m.locked() += 1;
        assert_eq!(*m.locked(), 2);
    }
}
