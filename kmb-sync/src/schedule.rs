#[derive(Clone, Debug, Default)]
pub enum Anchor {
    #[default]
    None,
    At(i64),
    Local {
        start: jiff::civil::Time,
        zone: jiff::tz::TimeZone,
    },
}

pub const MAX_BACKOFF_MS: i64 = 3_600_000;
pub const FIRST_BACKOFF_MS: i64 = 30_000;

#[must_use]
pub fn first_run(now: i64, every_ms: i64, anchor: &Anchor) -> i64 {
    match anchor {
        Anchor::None => now,
        Anchor::At(a) => next_boundary(now, every_ms, *a),
        Anchor::Local { start, zone } => next_local(now, every_ms, *start, zone),
    }
}

#[must_use]
pub fn after_success(now: i64, started: i64, every_ms: i64, anchor: &Anchor) -> i64 {
    match anchor {
        Anchor::None => started.saturating_add(every_ms).max(now),
        Anchor::At(a) => next_boundary(now.saturating_add(1), every_ms, *a),
        Anchor::Local { start, zone } => next_local(now.saturating_add(1), every_ms, *start, zone),
    }
}

#[must_use]
pub fn next_local(
    now: i64,
    every_ms: i64,
    start: jiff::civil::Time,
    zone: &jiff::tz::TimeZone,
) -> i64 {
    let Ok(at) = jiff::Timestamp::from_millisecond(now) else {
        return now;
    };
    let local = zone.to_datetime(at);
    let Ok(day) = local.date().yesterday() else {
        return now;
    };
    let first = day.to_datetime(start);
    let every = every_ms.max(1);
    let behind = local
        .duration_since(first)
        .as_millis()
        .clamp(0, i128::from(i64::MAX));
    let behind = i64::try_from(behind).unwrap_or(i64::MAX);
    let steps = behind
        .checked_add(every.saturating_sub(1))
        .and_then(|b| b.checked_div(every))
        .unwrap_or(0);
    let back = HOUR_MS.checked_div(every).unwrap_or(0).saturating_add(1);
    let mut best: Option<i64> = None;
    for k in steps.saturating_sub(back).max(0)..=steps {
        let span = jiff::SignedDuration::from_millis(k.saturating_mul(every));
        let Ok(wall) = first.checked_add(span) else {
            continue;
        };
        let ambiguous = zone.to_ambiguous_zoned(wall);
        for how in [
            jiff::tz::Disambiguation::Earlier,
            jiff::tz::Disambiguation::Later,
        ] {
            if let Ok(z) = ambiguous.clone().disambiguate(how) {
                let t = z.timestamp().as_millisecond();
                if t >= now && best.is_none_or(|b| t < b) {
                    best = Some(t);
                }
            }
        }
    }
    best.unwrap_or(now)
}

const HOUR_MS: i64 = 3_600_000;

#[must_use]
pub fn after_failure(now: i64, failures: u32, every_ms: i64) -> i64 {
    let shift = failures.saturating_sub(1).min(20);
    let factor = 1_i64.checked_shl(shift).unwrap_or(i64::MAX);
    let backoff = FIRST_BACKOFF_MS
        .saturating_mul(factor)
        .min(MAX_BACKOFF_MS)
        .min(every_ms.max(1));
    now.saturating_add(backoff)
}

#[must_use]
pub fn next_boundary(now: i64, every_ms: i64, anchor: i64) -> i64 {
    let every = every_ms.max(1);
    let k = now
        .saturating_sub(anchor)
        .checked_div_euclid(every)
        .unwrap_or(0);
    let t = anchor.saturating_add(k.saturating_mul(every));
    if t >= now { t } else { t.saturating_add(every) }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: i64 = 60_000;

    #[test]
    fn boundaries() {
        assert_eq!(next_boundary(0, 15 * MIN, 0), 0);
        assert_eq!(next_boundary(1, 15 * MIN, 0), 15 * MIN);
        assert_eq!(next_boundary(15 * MIN, 15 * MIN, 0), 15 * MIN);
        assert_eq!(next_boundary(MIN, 15 * MIN, 100 * MIN), 10 * MIN);
        assert_eq!(next_boundary(20 * MIN, 15 * MIN, 5 * MIN), 20 * MIN);
        assert_eq!(next_boundary(21 * MIN, 15 * MIN, 5 * MIN), 35 * MIN);
    }

    #[test]
    fn first_runs() {
        assert_eq!(first_run(7, MIN, &Anchor::None), 7);
        assert_eq!(first_run(7, MIN, &Anchor::At(0)), MIN);
    }

    #[test]
    fn after_success_skips_missed_boundaries() {
        assert_eq!(after_success(3 * MIN + 5, 0, MIN, &Anchor::At(0)), 4 * MIN);
        assert_eq!(after_success(MIN, 0, MIN, &Anchor::At(0)), 2 * MIN);
        assert_eq!(after_success(10, 0, MIN, &Anchor::None), MIN);
        assert_eq!(after_success(5 * MIN, 0, MIN, &Anchor::None), 5 * MIN);
    }

    #[test]
    fn local_start_times() {
        let zone = jiff::tz::TimeZone::get("UTC").unwrap();
        let ten = jiff::civil::time(10, 0, 0, 0);
        let ms = |s: &str| s.parse::<jiff::Timestamp>().unwrap().as_millisecond();
        let h = 3_600_000;
        assert_eq!(
            next_local(ms("2026-10-14T09:00:00Z"), 24 * h, ten, &zone),
            ms("2026-10-14T10:00:00Z")
        );
        assert_eq!(
            next_local(ms("2026-10-14T10:00:00.001Z"), 24 * h, ten, &zone),
            ms("2026-10-15T10:00:00Z")
        );
        assert_eq!(
            next_local(ms("2026-10-26T01:00:00Z"), 24 * h, ten, &zone),
            ms("2026-10-26T10:00:00Z")
        );
        assert_eq!(
            next_local(ms("2026-10-14T05:20:00Z"), h, ten, &zone),
            ms("2026-10-14T06:00:00Z")
        );
        assert_eq!(
            next_local(ms("2026-10-14T08:00:00Z"), 7 * h, ten, &zone),
            ms("2026-10-14T14:00:00Z")
        );
        let anchor = Anchor::Local { start: ten, zone };
        assert_eq!(
            after_success(ms("2026-10-14T10:00:00Z"), 0, 24 * h, &anchor),
            ms("2026-10-15T10:00:00Z")
        );
    }

    #[test]
    fn local_start_times_keep_their_period_over_dst() {
        let zone = jiff::tz::TimeZone::get("Europe/Prague").unwrap();
        let ten = jiff::civil::time(10, 0, 0, 0);
        let ms = |s: &str| s.parse::<jiff::Timestamp>().unwrap().as_millisecond();
        for (q, from, to) in [
            (15 * 60_000, "2026-10-24T20:00:00Z", "2026-10-25T06:00:00Z"),
            (15 * 60_000, "2026-03-28T20:00:00Z", "2026-03-29T06:00:00Z"),
            (5_000, "2026-10-24T23:00:00Z", "2026-10-25T02:00:00Z"),
        ] {
            let mut t = next_local(ms(from), q, ten, &zone);
            while t < ms(to) {
                let next = next_local(t + 1, q, ten, &zone);
                assert_eq!(next - t, q, "after {t}");
                t = next;
            }
        }
    }

    #[test]
    fn backoff() {
        let h = 3_600_000;
        assert_eq!(after_failure(0, 1, h), 30_000);
        assert_eq!(after_failure(0, 2, h), 60_000);
        assert_eq!(after_failure(0, 3, h), 120_000);
        assert_eq!(after_failure(0, 30, 10 * h), MAX_BACKOFF_MS);
        assert_eq!(after_failure(0, 5, MIN), MIN);
    }
}
