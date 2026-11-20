use std::time::{Duration, Instant};

/// Elapsed-time accounting avoids overflowing Instant for a large Retry-After.
#[derive(Clone, Copy)]
pub(crate) struct Cooldown {
    started: Instant,
    delay: Duration,
}
impl Cooldown {
    pub(crate) fn new(now: Instant, seconds: u64) -> Self {
        Self {
            started: now,
            delay: Duration::from_secs(seconds),
        }
    }
    fn remaining(self, now: Instant) -> Duration {
        self.delay
            .saturating_sub(now.saturating_duration_since(self.started))
    }
    pub(crate) fn active(self, now: Instant) -> bool {
        !self.remaining(now).is_zero()
    }
    pub(crate) fn seconds(self, now: Instant) -> u64 {
        let left = self.remaining(now);
        left.as_secs()
            .saturating_add(u64::from(left.subsec_nanos() != 0))
    }
    pub(crate) fn extend(slot: &mut Option<Self>, now: Instant, seconds: u64) {
        let incoming = Duration::from_secs(seconds);
        if slot.is_none_or(|old| old.remaining(now) < incoming) {
            *slot = Some(Self::new(now, seconds));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_after_uses_elapsed_time_ceiling_and_never_shortens_a_limit() {
        let now = Instant::now();
        let mut limit = None;
        Cooldown::extend(&mut limit, now, 10);
        assert_eq!(limit.unwrap().seconds(now + Duration::from_millis(1500)), 9);
        Cooldown::extend(&mut limit, now + Duration::from_secs(2), 3);
        assert_eq!(limit.unwrap().seconds(now + Duration::from_secs(2)), 8);
        Cooldown::extend(&mut limit, now + Duration::from_secs(3), 20);
        assert_eq!(limit.unwrap().seconds(now + Duration::from_secs(3)), 20);
        assert!(!limit.unwrap().active(now + Duration::from_secs(23)));
    }
    #[test]
    fn even_maximum_retry_after_has_no_instant_overflow_or_hour_cap() {
        let now = Instant::now();
        let limit = Cooldown::new(now, u64::MAX);
        assert!(limit.active(now));
        assert_eq!(limit.seconds(now), u64::MAX);
        assert_eq!(limit.seconds(now + Duration::from_secs(1)), u64::MAX - 1);
        assert_eq!(Cooldown::new(now, 7200).seconds(now), 7200);
    }
}
