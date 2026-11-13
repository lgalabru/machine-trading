//! Trading calendars: which session (regular hours, pre-market, overnight,
//! closed) an instant falls in, by local time, weekday and holiday.
//!
//! Times are Unix seconds. The only local clocks are UTC and New York (US
//! Eastern, with daylight saving from the second Sunday of March to the first
//! Sunday of November), which is what US equities need, without a time-zone
//! database.

use super::calibration::{Calendar, CalendarModel};

const DAY: i64 = 86_400;
const WEEKDAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];

/// A local clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Zone {
    Utc,
    NewYork,
}

impl Zone {
    fn parse(name: &str) -> Result<Self, String> {
        match name {
            "UTC" => Ok(Self::Utc),
            "America/New_York" => Ok(Self::NewYork),
            other => Err(format!(
                "calendar.timezone {other} is not supported: UTC or America/New_York"
            )),
        }
    }

    /// Local time minus UTC, in seconds, at a UTC instant.
    pub fn offset(self, unix: i64) -> i64 {
        match self {
            Self::Utc => 0,
            Self::NewYork => {
                if us_daylight_saving(unix) {
                    -4 * 3600
                } else {
                    -5 * 3600
                }
            }
        }
    }

    pub fn to_local(self, unix: i64) -> i64 {
        unix + self.offset(unix)
    }

    /// The UTC instant of a local time. In the hour daylight saving skips
    /// or repeats, the standard-time reading.
    pub fn from_local(self, local: i64) -> i64 {
        match self {
            Self::Utc => local,
            Self::NewYork => {
                let daylight = local + 4 * 3600;
                if us_daylight_saving(daylight) {
                    daylight
                } else {
                    local + 5 * 3600
                }
            }
        }
    }
}

/// US daylight saving at a UTC instant: from 02:00 local standard time on
/// the second Sunday of March (07:00 UTC) to 02:00 local daylight time on
/// the first Sunday of November (06:00 UTC).
fn us_daylight_saving(unix: i64) -> bool {
    let (year, _, _) = civil_from_days(unix.div_euclid(DAY));
    let start = (nth_sunday(year, 3, 2)) * DAY + 7 * 3600;
    let end = (nth_sunday(year, 11, 1)) * DAY + 6 * 3600;
    (start..end).contains(&unix)
}

/// Days since 1970-01-01 of the `n`th Sunday of a month.
fn nth_sunday(year: i64, month: u32, n: i64) -> i64 {
    let first = days_from_civil(year, month, 1);
    let to_sunday = (6 - weekday(first)).rem_euclid(7);
    first + to_sunday + 7 * (n - 1)
}

/// Monday 0 .. Sunday 6. 1970-01-01 was a Thursday.
pub fn weekday(days: i64) -> i64 {
    (days + 3).rem_euclid(7)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Hinnant's
/// algorithm).
pub fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let m = i64::from(month);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(day) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The date of a day number.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// `YYYY-MM-DD` to a day number.
pub fn parse_date(date: &str) -> Result<i64, String> {
    let parts: Vec<&str> = date.split('-').collect();
    let bad = || format!("{date} is not a YYYY-MM-DD date");
    let [y, m, d] = parts[..] else {
        return Err(bad());
    };
    let (y, m, d): (i64, u32, u32) = (
        y.parse().map_err(|_| bad())?,
        m.parse().map_err(|_| bad())?,
        d.parse().map_err(|_| bad())?,
    );
    if !(1..=12).contains(&m)
        || !(1..=31).contains(&d)
        || civil_from_days(days_from_civil(y, m, d)) != (y, m, d)
    {
        return Err(bad());
    }
    Ok(days_from_civil(y, m, d))
}

/// `HH:MM` to seconds after midnight.
fn parse_time(time: &str) -> Result<i64, String> {
    let bad = || format!("{time} is not an HH:MM time");
    let (h, m) = time.split_once(':').ok_or_else(bad)?;
    let (h, m): (i64, i64) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if !(0..24).contains(&h) || !(0..60).contains(&m) {
        return Err(bad());
    }
    Ok(h * 3600 + m * 60)
}

#[derive(Debug, Clone)]
struct Session {
    kind: String,
    /// Bit `i` set: starts on weekday `i` (Monday 0).
    days: u8,
    start: i64,
    end: i64,
    vol_mult: f64,
}

impl Session {
    fn wraps(&self) -> bool {
        self.end <= self.start
    }
}

/// A calendar ready to answer which session an instant is in.
#[derive(Debug, Clone)]
pub struct SessionCalendar {
    zone: Zone,
    sessions: Vec<Session>,
    closed_vol_mult: f64,
    /// Local day numbers.
    holidays: Vec<i64>,
    /// First and last local day a drawn start may fall on.
    draw_days: (i64, i64),
    continuous: bool,
}

/// Where a drawn session starts when a `continuous` calendar has no draw
/// range: the weeks from Monday 2027-01-04.
const CONTINUOUS_DRAW: (i64, i64) = (20_822, 20_822 + 364);

impl SessionCalendar {
    pub fn new(calendar: &Calendar) -> Result<Self, String> {
        let continuous = calendar.model == CalendarModel::Continuous;
        let zone = match &calendar.timezone {
            Some(name) => Zone::parse(name)?,
            None if continuous => Zone::Utc,
            None => return Err("calendar: a sessions calendar needs a timezone".into()),
        };
        let mut sessions = Vec::new();
        for spec in &calendar.sessions {
            let mut days = 0u8;
            for day in &spec.days {
                let i = WEEKDAYS.iter().position(|w| w == day).ok_or_else(|| {
                    format!("calendar.sessions: {day} is not a weekday (mon..sun)")
                })?;
                days |= 1 << i;
            }
            if !(spec.vol_mult.is_finite() && spec.vol_mult > 0.0) {
                return Err(format!(
                    "calendar.sessions.{}: vol_mult must be positive",
                    spec.kind
                ));
            }
            let session = Session {
                kind: spec.kind.clone(),
                days,
                start: parse_time(&spec.start)?,
                end: parse_time(&spec.end)?,
                vol_mult: spec.vol_mult,
            };
            if days == 0 || session.start == session.end {
                return Err(format!(
                    "calendar.sessions.{}: needs days and a start that isn't its end",
                    spec.kind
                ));
            }
            sessions.push(session);
        }
        if !continuous && sessions.is_empty() {
            return Err("calendar: a sessions calendar needs sessions".into());
        }
        let closed_vol_mult = match calendar.closed_vol_mult {
            Some(m) if m.is_finite() && m > 0.0 => m,
            None if continuous => 1.0,
            _ => return Err("calendar.closed_vol_mult must be positive".into()),
        };
        let holidays = calendar
            .holidays
            .iter()
            .map(|d| parse_date(d))
            .collect::<Result<Vec<_>, _>>()?;
        let draw_days = match &calendar.draw_range {
            Some([from, to]) => {
                let (from, to) = (parse_date(from)?, parse_date(to)?);
                if to < from {
                    return Err("calendar.draw_range ends before it starts".into());
                }
                (from, to)
            }
            None if continuous => CONTINUOUS_DRAW,
            None => return Err("calendar: a sessions calendar needs a draw_range".into()),
        };
        let calendar = Self {
            zone,
            sessions,
            closed_vol_mult,
            holidays,
            draw_days,
            continuous,
        };
        // No instant may be in two sessions.
        for (i, a) in calendar.sessions.iter().enumerate() {
            for b in &calendar.sessions[i + 1..] {
                for minute in (0..8 * DAY).step_by(60) {
                    let t = calendar.zone.from_local(CONTINUOUS_DRAW.0 * DAY + minute);
                    if calendar.active(a, t) && calendar.active(b, t) {
                        return Err(format!(
                            "calendar.sessions {} and {} overlap",
                            a.kind, b.kind
                        ));
                    }
                }
            }
        }
        Ok(calendar)
    }

    pub fn zone(&self) -> Zone {
        self.zone
    }

    pub fn is_continuous(&self) -> bool {
        self.continuous
    }

    pub fn draw_days(&self) -> (i64, i64) {
        self.draw_days
    }

    fn opens_on(&self, session: &Session, day: i64) -> bool {
        session.days & (1 << weekday(day)) != 0 && !self.holidays.contains(&day)
    }

    fn active(&self, session: &Session, unix: i64) -> bool {
        let local = self.zone.to_local(unix);
        let (day, secs) = (local.div_euclid(DAY), local.rem_euclid(DAY));
        if session.wraps() {
            (self.opens_on(session, day) && secs >= session.start)
                || (self.opens_on(session, day - 1) && secs < session.end)
        } else {
            self.opens_on(session, day) && (session.start..session.end).contains(&secs)
        }
    }

    /// The session kind at an instant; `None` when closed. A continuous
    /// calendar is always in `"continuous"`.
    pub fn kind_at(&self, unix: i64) -> Option<&str> {
        if self.continuous {
            return Some("continuous");
        }
        self.sessions
            .iter()
            .find(|s| self.active(s, unix))
            .map(|s| s.kind.as_str())
    }

    /// The calendar's volatility multiplier at an instant.
    pub fn vol_mult_at(&self, unix: i64) -> f64 {
        if self.continuous {
            return 1.0;
        }
        self.sessions
            .iter()
            .find(|s| self.active(s, unix))
            .map_or(self.closed_vol_mult, |s| s.vol_mult)
    }

    /// The next session of `kind` that opens at or after `from`: its open
    /// and close, in Unix seconds. Searches a year ahead.
    pub fn next_session(&self, kind: &str, from: i64) -> Option<(i64, i64)> {
        let session = self.sessions.iter().find(|s| s.kind == kind)?;
        let first = self.zone.to_local(from).div_euclid(DAY) - 1;
        (first..first + 366).find_map(|day| {
            if !self.opens_on(session, day) {
                return None;
            }
            let open = self.zone.from_local(day * DAY + session.start);
            let close_day = if session.wraps() { day + 1 } else { day };
            let close = self.zone.from_local(close_day * DAY + session.end);
            (open >= from).then_some((open, close))
        })
    }

    pub fn has_kind(&self, kind: &str) -> bool {
        self.sessions.iter().any(|s| s.kind == kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::market::calibration::SessionSpec;

    fn utc(y: i64, m: u32, d: u32, h: i64, min: i64) -> i64 {
        days_from_civil(y, m, d) * DAY + h * 3600 + min * 60
    }

    fn us_equity() -> SessionCalendar {
        let weekdays = ["mon", "tue", "wed", "thu", "fri"];
        let spec = |kind: &str, days: &[&str], start: &str, end: &str, vol_mult: f64| SessionSpec {
            kind: kind.into(),
            days: days.iter().map(|d| d.to_string()).collect(),
            start: start.into(),
            end: end.into(),
            vol_mult,
        };
        SessionCalendar::new(&Calendar {
            model: CalendarModel::Sessions,
            note: None,
            timezone: Some("America/New_York".into()),
            sessions: vec![
                spec("pre", &weekdays, "04:00", "09:30", 0.6),
                spec("regular", &weekdays, "09:30", "16:00", 1.0),
                spec("post", &weekdays, "16:00", "20:00", 0.6),
                spec(
                    "overnight",
                    &["sun", "mon", "tue", "wed", "thu"],
                    "20:00",
                    "04:00",
                    0.5,
                ),
            ],
            closed_vol_mult: Some(0.4),
            closed_source: None,
            holidays: vec!["2027-11-25".into()],
            draw_range: Some(["2027-01-04".into(), "2027-12-31".into()]),
            gaps: vec![],
            closed_hours_move: None,
        })
        .unwrap()
    }

    #[test]
    fn civil_dates_round_trip() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2027, 1, 4), CONTINUOUS_DRAW.0);
        assert_eq!(weekday(CONTINUOUS_DRAW.0), 0, "2027-01-04 is a Monday");
        for d in [-1000, 0, 59, 10_000, 20_822, 30_000] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
        assert!(parse_date("2027-02-30").is_err());
        assert!(parse_date("2027-13-01").is_err());
    }

    #[test]
    fn new_york_daylight_saving() {
        // 2027: DST from Sunday 14 March to Sunday 7 November.
        assert_eq!(Zone::NewYork.offset(utc(2027, 3, 14, 6, 59)), -5 * 3600);
        assert_eq!(Zone::NewYork.offset(utc(2027, 3, 14, 7, 0)), -4 * 3600);
        assert_eq!(Zone::NewYork.offset(utc(2027, 11, 7, 5, 59)), -4 * 3600);
        assert_eq!(Zone::NewYork.offset(utc(2027, 11, 7, 6, 0)), -5 * 3600);
        let local = utc(2027, 7, 1, 9, 30);
        assert_eq!(Zone::NewYork.from_local(local), utc(2027, 7, 1, 13, 30));
        let local = utc(2027, 1, 5, 9, 30);
        assert_eq!(Zone::NewYork.from_local(local), utc(2027, 1, 5, 14, 30));
    }

    #[test]
    fn sessions_by_clock_weekday_and_holiday() {
        let cal = us_equity();
        // Tuesday 2027-01-05, New York is UTC-5.
        assert_eq!(cal.kind_at(utc(2027, 1, 5, 14, 29)), Some("pre"));
        assert_eq!(cal.kind_at(utc(2027, 1, 5, 14, 30)), Some("regular"));
        assert_eq!(cal.kind_at(utc(2027, 1, 5, 21, 0)), Some("post"));
        assert_eq!(cal.kind_at(utc(2027, 1, 6, 3, 0)), Some("overnight"));
        // Saturday: closed; Sunday 20:00 the overnight opens.
        assert_eq!(cal.kind_at(utc(2027, 1, 9, 17, 0)), None);
        assert_eq!(cal.vol_mult_at(utc(2027, 1, 9, 17, 0)), 0.4);
        assert_eq!(cal.kind_at(utc(2027, 1, 11, 1, 30)), Some("overnight"));
        // Friday night after 20:00: closed until Sunday.
        assert_eq!(cal.kind_at(utc(2027, 1, 9, 2, 0)), None);
        // Thanksgiving: no regular session.
        assert_eq!(cal.kind_at(utc(2027, 11, 25, 16, 0)), None);
        // Summer: the open is 13:30 UTC.
        assert_eq!(cal.kind_at(utc(2027, 7, 1, 13, 30)), Some("regular"));
    }

    #[test]
    fn next_session_skips_weekends_and_holidays() {
        let cal = us_equity();
        // From Friday 2027-01-08 after the open: Monday's.
        let (open, close) = cal.next_session("regular", utc(2027, 1, 8, 15, 0)).unwrap();
        assert_eq!(open, utc(2027, 1, 11, 14, 30));
        assert_eq!(close, utc(2027, 1, 11, 21, 0));
        // From Wednesday before Thanksgiving's open: Friday's.
        let (open, _) = cal
            .next_session("regular", utc(2027, 11, 24, 15, 0))
            .unwrap();
        assert_eq!(open, utc(2027, 11, 26, 14, 30));
        // The overnight runs past midnight: from Monday 19:00 New York, that
        // evening's.
        let (open, close) = cal
            .next_session("overnight", utc(2027, 1, 5, 0, 0))
            .unwrap();
        assert_eq!(
            (open, close),
            (utc(2027, 1, 5, 1, 0), utc(2027, 1, 5, 9, 0))
        );
    }

    #[test]
    fn overlapping_sessions_fail() {
        let mut spec = Calendar {
            model: CalendarModel::Sessions,
            note: None,
            timezone: Some("UTC".into()),
            sessions: vec![
                SessionSpec {
                    kind: "a".into(),
                    days: vec!["mon".into()],
                    start: "09:00".into(),
                    end: "12:00".into(),
                    vol_mult: 1.0,
                },
                SessionSpec {
                    kind: "b".into(),
                    days: vec!["mon".into()],
                    start: "11:00".into(),
                    end: "13:00".into(),
                    vol_mult: 1.0,
                },
            ],
            closed_vol_mult: Some(1.0),
            closed_source: None,
            holidays: vec![],
            draw_range: Some(["2027-01-04".into(), "2027-02-01".into()]),
            gaps: vec![],
            closed_hours_move: None,
        };
        assert!(SessionCalendar::new(&spec).unwrap_err().contains("overlap"));
        spec.sessions[1].start = "12:00".into();
        assert!(SessionCalendar::new(&spec).is_ok());
    }
}
