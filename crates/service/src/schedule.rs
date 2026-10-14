//! Schedule arithmetic: when a definition's schedule fires.
//!
//! Weekdays use 0 = Sunday … 6 = Saturday. A monthly day beyond the end of a month fires on
//! the month's last day. Times that do not exist (DST spring-forward) fire an hour later;
//! ambiguous times fire at their first occurrence.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveTime, TimeZone};
use ipc::proto::{Schedule, ScheduleKind};

/// How far to look for an occurrence. Every valid schedule fires at least once a month.
const SEARCH_DAYS: i64 = 62;

fn time_of(s: &Schedule) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(&s.time, "%H:%M").ok()
}

fn last_day_of_month(date: NaiveDate) -> u32 {
    let (y, m) = (date.year(), date.month());
    let first_next = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y, m + 1, 1) };
    first_next.and_then(|d| d.pred_opt()).map_or(28, |d| d.day())
}

fn fires_on(s: &Schedule, date: NaiveDate) -> bool {
    match s.kind {
        ScheduleKind::Daily => true,
        ScheduleKind::Weekly => s.weekdays.contains(&(date.weekday().num_days_from_sunday() as u8)),
        ScheduleKind::Monthly => match s.day_of_month {
            Some(d) => date.day() == u32::from(d).min(last_day_of_month(date)),
            None => false,
        },
    }
}

fn at<Tz: TimeZone>(tz: &Tz, date: NaiveDate, time: NaiveTime) -> Option<DateTime<Tz>> {
    let local = date.and_time(time);
    tz.from_local_datetime(&local).earliest().or_else(|| tz.from_local_datetime(&(local + Duration::hours(1))).earliest())
}

/// The first occurrence strictly after `after`.
pub fn next_run<Tz: TimeZone>(s: &Schedule, after: &DateTime<Tz>) -> Option<DateTime<Tz>> {
    let time = time_of(s)?;
    let tz = after.timezone();
    let start = after.naive_local().date();
    (0..=SEARCH_DAYS)
        .filter_map(|i| start.checked_add_signed(Duration::days(i)))
        .filter(|d| fires_on(s, *d))
        .filter_map(|d| at(&tz, d, time))
        .find(|t| t > after)
}

/// The latest occurrence at or before `now`.
pub fn previous_run<Tz: TimeZone>(s: &Schedule, now: &DateTime<Tz>) -> Option<DateTime<Tz>> {
    let time = time_of(s)?;
    let tz = now.timezone();
    let start = now.naive_local().date();
    (0..=SEARCH_DAYS)
        .filter_map(|i| start.checked_sub_signed(Duration::days(i)))
        .filter(|d| fires_on(s, *d))
        .filter_map(|d| at(&tz, d, time))
        .find(|t| t <= now)
}

/// Whether an occurrence happened after `reference` and at or before `now`.
pub fn is_due<Tz: TimeZone>(s: &Schedule, reference: &DateTime<Tz>, now: &DateTime<Tz>) -> bool {
    previous_run(s, now).is_some_and(|p| p > *reference)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, Utc};

    fn sched(kind: ScheduleKind, time: &str, weekdays: Vec<u8>, day: Option<u8>) -> Schedule {
        Schedule { kind, time: time.into(), weekdays, day_of_month: day }
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn daily() {
        let s = sched(ScheduleKind::Daily, "02:30", vec![], None);
        assert_eq!(next_run(&s, &utc("2026-10-14T01:00:00Z")).unwrap(), utc("2026-10-14T02:30:00Z"));
        assert_eq!(next_run(&s, &utc("2026-10-14T02:30:00Z")).unwrap(), utc("2026-10-15T02:30:00Z"));
        assert_eq!(previous_run(&s, &utc("2026-10-14T02:30:00Z")).unwrap(), utc("2026-10-14T02:30:00Z"));
        assert_eq!(previous_run(&s, &utc("2026-10-14T02:29:00Z")).unwrap(), utc("2026-10-13T02:30:00Z"));
    }

    #[test]
    fn weekly_uses_sunday_zero() {
        // 2026-10-14 is a Wednesday (3).
        let s = sched(ScheduleKind::Weekly, "09:00", vec![0, 5], None);
        assert_eq!(next_run(&s, &utc("2026-10-14T12:00:00Z")).unwrap(), utc("2026-10-16T09:00:00Z"));
        assert_eq!(next_run(&s, &utc("2026-10-16T09:00:00Z")).unwrap(), utc("2026-10-18T09:00:00Z"));
        assert_eq!(previous_run(&s, &utc("2026-10-14T12:00:00Z")).unwrap(), utc("2026-10-11T09:00:00Z"));
    }

    #[test]
    fn monthly_clamps_to_month_end() {
        let s = sched(ScheduleKind::Monthly, "23:00", vec![], Some(31));
        assert_eq!(next_run(&s, &utc("2026-11-01T00:00:00Z")).unwrap(), utc("2026-11-30T23:00:00Z"));
        assert_eq!(next_run(&s, &utc("2027-02-01T00:00:00Z")).unwrap(), utc("2027-02-28T23:00:00Z"));
        assert_eq!(next_run(&s, &utc("2028-02-01T00:00:00Z")).unwrap(), utc("2028-02-29T23:00:00Z"));
        assert_eq!(previous_run(&s, &utc("2026-11-15T00:00:00Z")).unwrap(), utc("2026-10-31T23:00:00Z"));
    }

    #[test]
    fn local_offset_is_respected() {
        let tz = FixedOffset::east_opt(3 * 3600).unwrap();
        let s = sched(ScheduleKind::Daily, "02:00", vec![], None);
        let now = tz.with_ymd_and_hms(2026, 10, 14, 1, 0, 0).unwrap();
        assert_eq!(next_run(&s, &now).unwrap().with_timezone(&Utc), utc("2026-10-13T23:00:00Z"));
    }

    #[test]
    fn due_detection() {
        let s = sched(ScheduleKind::Daily, "02:00", vec![], None);
        let now = utc("2026-10-14T08:00:00Z");
        assert!(is_due(&s, &utc("2026-10-13T03:00:00Z"), &now));
        assert!(!is_due(&s, &utc("2026-10-14T02:00:00Z"), &now));
        assert!(!is_due(&s, &utc("2026-10-14T05:00:00Z"), &now));
    }

    #[test]
    fn invalid_schedule_never_fires() {
        let s = sched(ScheduleKind::Weekly, "02:00", vec![], None);
        assert!(next_run(&s, &utc("2026-10-14T08:00:00Z")).is_none());
        let s = sched(ScheduleKind::Daily, "25:00", vec![], None);
        assert!(previous_run(&s, &utc("2026-10-14T08:00:00Z")).is_none());
    }
}
