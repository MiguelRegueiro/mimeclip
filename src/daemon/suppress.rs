use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SUPPRESS_TTL: Duration = Duration::from_secs(5);

pub type SharedSuppressState = Arc<Mutex<SuppressState>>;

#[derive(Debug, Default)]
pub struct SuppressState {
    pending: Option<PendingHash>,
}

#[derive(Debug)]
struct PendingHash {
    hash: String,
    expires_at: Instant,
}

impl SuppressState {
    pub fn arm(&mut self, hash: String) {
        self.arm_at(hash, Instant::now());
    }

    pub fn should_suppress(&mut self, hash: &str) -> bool {
        self.should_suppress_at(hash, Instant::now())
    }

    fn arm_at(&mut self, hash: String, now: Instant) {
        self.pending = Some(PendingHash {
            hash,
            expires_at: now + SUPPRESS_TTL,
        });
    }

    fn should_suppress_at(&mut self, hash: &str, now: Instant) -> bool {
        match self.pending.as_ref() {
            Some(pending) if now > pending.expires_at => {
                self.pending = None;
                false
            }
            Some(pending) if pending.hash == hash => {
                self.pending = None;
                true
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SuppressState, SUPPRESS_TTL};
    use std::time::{Duration, Instant};

    #[test]
    fn matching_hash_is_suppressed_once() {
        let now = Instant::now();
        let mut suppress = SuppressState::default();

        suppress.arm_at("abc".into(), now);

        assert!(suppress.should_suppress_at("abc", now + Duration::from_millis(1)));
        assert!(!suppress.should_suppress_at("abc", now + Duration::from_millis(2)));
    }

    #[test]
    fn mismatched_hash_keeps_pending_suppression() {
        let now = Instant::now();
        let mut suppress = SuppressState::default();

        suppress.arm_at("abc".into(), now);

        assert!(!suppress.should_suppress_at("def", now + Duration::from_millis(1)));
        assert!(suppress.should_suppress_at("abc", now + Duration::from_millis(2)));
    }

    #[test]
    fn expired_hash_is_cleared_without_suppressing() {
        let now = Instant::now();
        let mut suppress = SuppressState::default();

        suppress.arm_at("abc".into(), now);

        assert!(!suppress.should_suppress_at("abc", now + SUPPRESS_TTL + Duration::from_millis(1)));
        assert!(!suppress.should_suppress_at("abc", now + SUPPRESS_TTL + Duration::from_millis(2)));
    }
}
