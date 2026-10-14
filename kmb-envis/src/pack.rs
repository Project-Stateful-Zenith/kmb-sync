use jiff::civil::DateTime;
use jiff::tz::{Disambiguation, TimeZone};
use jiff::{Timestamp, ToSpan};

pub const PACK_LIMIT: u64 = 209_715_200;
pub const EPOCH_2000_MS: i64 = 946_684_800_000;
pub const INDEX_ENTRY_LEN: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    Year,
    Quarter,
    Month,
    Week,
    Day,
    Hour6,
    Hour,
    Minute6,
    Minute,
}

const DAY_MS: u64 = 86_400_000;

impl Unit {
    const fn nominal_ms(self) -> u64 {
        match self {
            Self::Year => 365 * DAY_MS,
            Self::Quarter => 90 * DAY_MS,
            Self::Month => 31 * DAY_MS,
            Self::Week => 7 * DAY_MS,
            Self::Day => DAY_MS,
            Self::Hour6 => 6 * 3_600_000,
            Self::Hour => 3_600_000,
            Self::Minute6 => 360_000,
            Self::Minute => 60_000,
        }
    }

    #[must_use]
    pub fn for_archive(period_ms: u64, row_len: usize) -> Self {
        if period_ms == 0 {
            return Self::Year;
        }
        let len = u64::try_from(row_len).unwrap_or(u64::MAX);
        [
            Self::Year,
            Self::Quarter,
            Self::Month,
            Self::Week,
            Self::Day,
            Self::Hour6,
            Self::Hour,
            Self::Minute6,
        ]
        .into_iter()
        .find(|u| {
            u.nominal_ms()
                .checked_div(period_ms)
                .unwrap_or(0)
                .saturating_mul(len)
                < PACK_LIMIT
        })
        .unwrap_or(Self::Minute)
    }
}

#[derive(Clone, Debug)]
pub struct Calendar {
    pub zone: TimeZone,
    pub week_start: jiff::civil::Weekday,
}

impl Calendar {
    #[must_use]
    pub fn system() -> Self {
        Self {
            zone: TimeZone::system(),
            week_start: jiff::civil::Weekday::Monday,
        }
    }

    pub fn new(zone: &str) -> Result<Self, jiff::Error> {
        Ok(Self {
            zone: TimeZone::get(zone)?,
            week_start: jiff::civil::Weekday::Monday,
        })
    }

    pub fn interval(&self, unit: Unit, t: u64) -> Result<(u64, u64), jiff::Error> {
        let at = to_timestamp(t.saturating_sub(1))?;
        let utc = matches!(unit, Unit::Hour | Unit::Minute6 | Unit::Minute);
        let zone = if utc {
            TimeZone::UTC
        } else {
            self.zone.clone()
        };
        let local = zone.to_datetime(at);
        let start = self.start_of(unit, local)?;
        let end = match unit {
            Unit::Year => start.checked_add(1.year())?,
            Unit::Quarter => start.checked_add(3.months())?,
            Unit::Month => start.checked_add(1.month())?,
            Unit::Week => start.checked_add(7.days())?,
            Unit::Day => start.checked_add(1.day())?,
            Unit::Hour6 => start.checked_add(6.hours())?,
            Unit::Hour => start.checked_add(1.hour())?,
            Unit::Minute6 => start.checked_add(6.minutes())?,
            Unit::Minute => start.checked_add(1.minute())?,
        };
        Ok((from_civil(&zone, start)?, from_civil(&zone, end)?))
    }

    fn start_of(&self, unit: Unit, t: DateTime) -> Result<DateTime, jiff::Error> {
        let date = t.date();
        let (h, m) = (t.hour(), t.minute());
        let down = |v: i8, step: i8| v.saturating_sub(v.checked_rem(step).unwrap_or(0));
        Ok(match unit {
            Unit::Year => DateTime::new(t.year(), 1, 1, 0, 0, 0, 0)?,
            Unit::Quarter => {
                let month = down(t.month().saturating_sub(1), 3).saturating_add(1);
                DateTime::new(t.year(), month, 1, 0, 0, 0, 0)?
            }
            Unit::Month => DateTime::new(t.year(), t.month(), 1, 0, 0, 0, 0)?,
            Unit::Week => {
                let back = date.weekday().since(self.week_start);
                date.checked_sub(i64::from(back).days())?
                    .to_datetime(jiff::civil::Time::midnight())
            }
            Unit::Day => date.to_datetime(jiff::civil::Time::midnight()),
            Unit::Hour6 => date.at(down(h, 6), 0, 0, 0),
            Unit::Hour => date.at(h, 0, 0, 0),
            Unit::Minute6 => date.at(h, down(m, 6), 0, 0),
            Unit::Minute => date.at(h, m, 0, 0),
        })
    }
}

fn from_civil(zone: &TimeZone, t: DateTime) -> Result<u64, jiff::Error> {
    let ts = zone
        .to_ambiguous_zoned(t)
        .disambiguate(Disambiguation::Later)?
        .timestamp();
    Ok(from_timestamp(ts))
}

pub fn to_timestamp(t: u64) -> Result<Timestamp, jiff::Error> {
    let ms = i64::try_from(t)
        .unwrap_or(i64::MAX)
        .saturating_add(EPOCH_2000_MS);
    Timestamp::from_millisecond(ms)
}

#[must_use]
pub fn from_timestamp(t: Timestamp) -> u64 {
    u64::try_from(t.as_millisecond().saturating_sub(EPOCH_2000_MS)).unwrap_or(0)
}

#[must_use]
pub fn row_time(row: &[u8]) -> Option<u64> {
    let b: [u8; 8] = row.get(..8)?.try_into().ok()?;
    Some(u64::from_be_bytes(b))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexEntry {
    pub row: u32,
    pub time: u64,
}

#[must_use]
pub fn sparse_step(row_len: usize) -> u32 {
    let n = 10_000_usize
        .checked_div(row_len)
        .unwrap_or(0)
        .saturating_add(1);
    u32::try_from(n).unwrap_or(u32::MAX)
}

#[must_use]
pub const fn is_break(prev: u64, next: u64, period_ms: u64) -> bool {
    next.saturating_sub(prev).saturating_mul(2) > period_ms.saturating_mul(3)
}

#[must_use]
pub fn time_index(times: &[u64], period_ms: u64, row_len: usize) -> Vec<IndexEntry> {
    let entry = |i: usize, time: u64| IndexEntry {
        row: u32::try_from(i).unwrap_or(u32::MAX),
        time,
    };
    let mut out = Vec::new();
    if period_ms == 0 {
        let step = usize::try_from(sparse_step(row_len)).unwrap_or(usize::MAX);
        out.extend(
            times
                .iter()
                .enumerate()
                .step_by(step.max(1))
                .map(|(i, &t)| entry(i, t)),
        );
        return out;
    }
    let Some((&first, rest)) = times.split_first() else {
        return out;
    };
    out.push(entry(0, first));
    let mut prev = first;
    for (i, &t) in rest.iter().enumerate() {
        let i = i.saturating_add(1);
        if is_break(prev, t, period_ms) {
            out.push(entry(i.saturating_sub(1), prev));
            out.push(entry(i, t));
        }
        prev = t;
    }
    out.push(entry(times.len().saturating_sub(1), prev));
    out
}

#[must_use]
pub fn encode_index(index: &[IndexEntry]) -> Option<Vec<u8>> {
    if index.is_empty() {
        return None;
    }
    let mut out = Vec::with_capacity(index.len().saturating_mul(INDEX_ENTRY_LEN));
    for e in index {
        out.extend_from_slice(&e.row.to_be_bytes());
        out.extend_from_slice(&e.time.to_be_bytes());
    }
    Some(out)
}

#[must_use]
pub fn decode_index(bytes: &[u8]) -> Vec<IndexEntry> {
    bytes
        .as_chunks::<INDEX_ENTRY_LEN>()
        .0
        .iter()
        .filter_map(|c| {
            let (row, time) = c.split_at(4);
            Some(IndexEntry {
                row: u32::from_be_bytes(row.try_into().ok()?),
                time: u64::from_be_bytes(time.try_into().ok()?),
            })
        })
        .collect()
}

#[must_use]
pub fn covered(index: &[IndexEntry], end: u64) -> Vec<(u64, u64)> {
    let mut spans: Vec<(u64, u64)> = index
        .chunks(2)
        .map(|pair| match pair {
            [a, b] => (a.time, b.time),
            [a] => (a.time, end),
            _ => (0, 0),
        })
        .collect();
    if let Some(last) = spans.last_mut() {
        last.1 = last.1.max(end);
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: u64 = 60_000;

    #[test]
    fn units() {
        assert_eq!(Unit::for_archive(60_000, 1361), Unit::Quarter);
        assert_eq!(Unit::for_archive(900_000, 5985), Unit::Year);
        assert_eq!(Unit::for_archive(900_000, 5986), Unit::Quarter);
        assert_eq!(Unit::for_archive(600_000, 3990), Unit::Year);
        assert_eq!(Unit::for_archive(600_000, 3991), Unit::Quarter);
        assert_eq!(Unit::for_archive(0, 110), Unit::Year);
        assert_eq!(Unit::for_archive(1, 1_000_000), Unit::Minute);
    }

    fn t(s: &str) -> u64 {
        from_timestamp(s.parse().unwrap())
    }

    #[test]
    fn quarters_in_local_time() {
        let cal = Calendar::new("UTC").unwrap();
        let (s, e) = cal
            .interval(Unit::Quarter, t("2026-08-15T14:00:00Z"))
            .unwrap();
        assert_eq!(
            (s, e),
            (t("2026-07-01T00:00:00Z"), t("2026-10-01T00:00:00Z"))
        );
        let (_, e) = cal
            .interval(Unit::Quarter, t("2026-07-01T00:00:00Z"))
            .unwrap();
        assert_eq!(e, t("2026-07-01T00:00:00Z"));
        let (s, e) = cal
            .interval(Unit::Quarter, t("2026-11-01T01:00:00Z"))
            .unwrap();
        assert_eq!(
            (s, e),
            (t("2026-10-01T00:00:00Z"), t("2027-01-01T00:00:00Z"))
        );
        let (s, e) = cal.interval(Unit::Year, t("2026-03-01T01:00:00Z")).unwrap();
        assert_eq!(
            (s, e),
            (t("2026-01-01T00:00:00Z"), t("2027-01-01T00:00:00Z"))
        );
        let (s, _) = cal.interval(Unit::Week, t("2026-10-14T14:00:00Z")).unwrap();
        assert_eq!(s, t("2026-10-12T00:00:00Z"));
        let (s, e) = cal.interval(Unit::Hour, t("2026-10-14T12:30:00Z")).unwrap();
        assert_eq!(
            (s, e),
            (t("2026-10-14T12:00:00Z"), t("2026-10-14T13:00:00Z"))
        );
    }

    #[test]
    fn continuous_index() {
        let times = [0, MIN, 2 * MIN, 5 * MIN, 6 * MIN, 7 * MIN + MIN / 2];
        let idx = time_index(&times, MIN, 100);
        let rows: Vec<(u32, u64)> = idx.iter().map(|e| (e.row, e.time)).collect();
        assert_eq!(
            rows,
            [(0, 0), (2, 2 * MIN), (3, 5 * MIN), (5, 7 * MIN + MIN / 2)]
        );
        assert_eq!(time_index(&[7], MIN, 100).len(), 2);
        assert_eq!(time_index(&[], MIN, 100), []);
        let bytes = encode_index(&idx).unwrap();
        assert_eq!(bytes.len(), 48);
        assert_eq!(decode_index(&bytes), idx);
        assert_eq!(encode_index(&[]), None);
        assert_eq!(covered(&idx, 9 * MIN), [(0, 2 * MIN), (5 * MIN, 9 * MIN)]);
    }

    #[test]
    fn sparse_index() {
        assert_eq!(sparse_step(110), 91);
        assert_eq!(sparse_step(504), 20);
        assert_eq!(sparse_step(100), 101);
        let times: Vec<u64> = (0..45).collect();
        let idx = time_index(&times, 0, 504);
        let rows: Vec<u32> = idx.iter().map(|e| e.row).collect();
        assert_eq!(rows, [0, 20, 40]);
        assert_eq!(time_index(&times[..8], 0, 64).len(), 1);
    }

    #[test]
    fn row_times() {
        let mut row = 800_000_000_000_u64.to_be_bytes().to_vec();
        row.extend([1, 2, 3]);
        assert_eq!(row_time(&row), Some(800_000_000_000));
        assert_eq!(row_time(&[1, 2]), None);
    }
}
