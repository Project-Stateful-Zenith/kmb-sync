use kmb::KmbTime;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

#[must_use]
pub fn unix_ms(t: OffsetDateTime) -> i64 {
    i64::try_from(t.unix_timestamp_nanos().div_euclid(1_000_000)).unwrap_or(i64::MAX)
}

#[must_use]
pub fn rfc3339(ms: i64) -> String {
    let nanos = i128::from(ms).saturating_mul(1_000_000);
    OffsetDateTime::from_unix_timestamp_nanos(nanos)
        .ok()
        .and_then(|t| t.format(&Rfc3339).ok())
        .unwrap_or_else(|| format!("@{ms}ms"))
}

#[must_use]
pub fn kmb_rfc3339(t: KmbTime) -> String {
    t.to_unix_ms().map_or_else(|| "invalid".into(), rfc3339)
}

#[must_use]
pub const fn to_kmb(ms: i64) -> KmbTime {
    KmbTime::from_unix_ms(ms)
}

#[must_use]
pub fn ms(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

#[must_use]
pub fn until(deadline: i64, now: i64) -> Duration {
    Duration::from_millis(u64::try_from(deadline.saturating_sub(now)).unwrap_or(0))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        let ms = 1_789_646_400_000;
        assert_eq!(rfc3339(ms), "2026-09-17T12:00:00Z");
        let t = crate::config::parse_time("2026-09-17T12:00:00+00:00").unwrap();
        assert_eq!(unix_ms(t), ms);
        assert_eq!(to_kmb(946_684_800_000), KmbTime(0));
        assert_eq!(until(10, 20), Duration::ZERO);
        assert_eq!(until(20, 10), Duration::from_millis(10));
    }
}
