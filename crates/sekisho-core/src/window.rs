//! Budget windows: the `[start, end)` span whose spend counts against a budget.

use time::{Date, Duration, OffsetDateTime, Time, UtcOffset};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum WindowKind {
    Day,
    Week,
    Month,
}

/// A budget's window. Calendar windows follow UTC days, ISO weeks (from
/// Monday) and months. Rolling windows are fixed lengths (1, 7 or 30 days)
/// counted from the budget's creation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BudgetWindow {
    pub kind: WindowKind,
    pub calendar: bool,
}

impl BudgetWindow {
    /// The window containing `now`, as `[start, end)` in UTC.
    pub fn current(
        &self,
        now: OffsetDateTime,
        created_at: OffsetDateTime,
    ) -> (OffsetDateTime, OffsetDateTime) {
        if self.calendar {
            calendar_window(self.kind, now.to_offset(UtcOffset::UTC).date())
        } else {
            rolling_window(self.kind, now, created_at.to_offset(UtcOffset::UTC))
        }
    }
}

fn midnight(d: Date) -> OffsetDateTime {
    d.with_time(Time::MIDNIGHT).assume_utc()
}

fn calendar_window(kind: WindowKind, today: Date) -> (OffsetDateTime, OffsetDateTime) {
    let (start, end) = match kind {
        WindowKind::Day => (today, today + Duration::DAY),
        WindowKind::Week => {
            let monday = today - Duration::days(today.weekday().number_days_from_monday().into());
            (monday, monday + Duration::WEEK)
        }
        WindowKind::Month => {
            let first = today.replace_day(1).expect("day 1 exists in every month");
            let next = match first.month() {
                time::Month::December => {
                    Date::from_calendar_date(first.year() + 1, time::Month::January, 1)
                }
                m => Date::from_calendar_date(first.year(), m.next(), 1),
            }
            .expect("the first of the next month is a valid date");
            (first, next)
        }
    };
    (midnight(start), midnight(end))
}

fn rolling_window(
    kind: WindowKind,
    now: OffsetDateTime,
    created_at: OffsetDateTime,
) -> (OffsetDateTime, OffsetDateTime) {
    let len = match kind {
        WindowKind::Day => Duration::DAY,
        WindowKind::Week => Duration::WEEK,
        WindowKind::Month => Duration::days(30),
    };
    // Floor division, so a `now` slightly before `created_at` (clock skew)
    // lands in the window just before creation instead of panicking.
    let elapsed = (now - created_at).whole_nanoseconds();
    let n = elapsed.div_euclid(len.whole_nanoseconds());
    let n = i32::try_from(n).expect("budget windows are within i32::MAX lengths of creation");
    let start = created_at + len * n;
    (start, start + len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::macros::datetime;

    #[test]
    fn calendar_month_rolls_over_at_utc_midnight() {
        let w = BudgetWindow {
            kind: WindowKind::Month,
            calendar: true,
        };
        let (s, e) = w.current(
            datetime!(2027-01-31 23:59:59 UTC),
            datetime!(2026-06-15 0:00 UTC),
        );
        assert_eq!(
            (s, e),
            (
                datetime!(2027-01-01 0:00 UTC),
                datetime!(2027-02-01 0:00 UTC)
            )
        );
        // The end is exclusive: midnight starts the next month.
        let (s, e) = w.current(
            datetime!(2027-02-01 0:00 UTC),
            datetime!(2026-06-15 0:00 UTC),
        );
        assert_eq!(
            (s, e),
            (
                datetime!(2027-02-01 0:00 UTC),
                datetime!(2027-03-01 0:00 UTC)
            )
        );
        // December rolls into the next year.
        let (s, e) = w.current(
            datetime!(2026-12-15 8:00 UTC),
            datetime!(2026-06-15 0:00 UTC),
        );
        assert_eq!(
            (s, e),
            (
                datetime!(2026-12-01 0:00 UTC),
                datetime!(2027-01-01 0:00 UTC)
            )
        );
        // UTC decides, not the offset `now` carries: 00:30 +01:00 is still January in UTC.
        let (s, _) = w.current(
            datetime!(2027-02-01 0:30 +1),
            datetime!(2026-06-15 0:00 UTC),
        );
        assert_eq!(s, datetime!(2027-01-01 0:00 UTC));
    }

    #[test]
    fn calendar_week_starts_monday() {
        let w = BudgetWindow {
            kind: WindowKind::Week,
            calendar: true,
        };
        assert_eq!(
            w.current(
                datetime!(2026-11-08 12:00 UTC),
                datetime!(2026-01-01 0:00 UTC)
            )
            .0,
            datetime!(2026-11-02 0:00 UTC)
        );
        // A Monday at midnight starts its own week.
        let (s, e) = w.current(
            datetime!(2026-11-09 0:00 UTC),
            datetime!(2026-01-01 0:00 UTC),
        );
        assert_eq!(
            (s, e),
            (
                datetime!(2026-11-09 0:00 UTC),
                datetime!(2026-11-16 0:00 UTC)
            )
        );
    }

    #[test]
    fn calendar_day_is_the_utc_date() {
        let w = BudgetWindow {
            kind: WindowKind::Day,
            calendar: true,
        };
        let (s, e) = w.current(
            datetime!(2026-11-08 23:30 -2),
            datetime!(2026-01-01 0:00 UTC),
        );
        assert_eq!(
            (s, e),
            (
                datetime!(2026-11-09 0:00 UTC),
                datetime!(2026-11-10 0:00 UTC)
            )
        );
    }

    #[test]
    fn rolling_window_aligns_to_creation() {
        let w = BudgetWindow {
            kind: WindowKind::Day,
            calendar: false,
        };
        let (s, e) = w.current(
            datetime!(2026-11-08 09:00 UTC),
            datetime!(2026-11-01 10:30 UTC),
        );
        assert_eq!(
            (s, e),
            (
                datetime!(2026-11-07 10:30 UTC),
                datetime!(2026-11-08 10:30 UTC)
            )
        );
        // At exactly one window length after creation, the next window starts.
        let (s, _) = w.current(
            datetime!(2026-11-02 10:30 UTC),
            datetime!(2026-11-01 10:30 UTC),
        );
        assert_eq!(s, datetime!(2026-11-02 10:30 UTC));
    }

    #[test]
    fn rolling_week_is_7_days_and_month_is_30_days() {
        let created = datetime!(2026-01-01 6:00 UTC);
        let week = BudgetWindow {
            kind: WindowKind::Week,
            calendar: false,
        };
        let (s, e) = week.current(datetime!(2026-01-20 0:00 UTC), created);
        assert_eq!(
            (s, e),
            (
                datetime!(2026-01-15 6:00 UTC),
                datetime!(2026-01-22 6:00 UTC)
            )
        );
        let month = BudgetWindow {
            kind: WindowKind::Month,
            calendar: false,
        };
        let (s, e) = month.current(datetime!(2026-03-05 0:00 UTC), created);
        assert_eq!(
            (s, e),
            (
                datetime!(2026-03-02 6:00 UTC),
                datetime!(2026-04-01 6:00 UTC)
            )
        );
    }

    #[test]
    fn rolling_window_before_creation_does_not_panic() {
        // Clock skew between replicas: `now` slightly before `created_at`.
        let w = BudgetWindow {
            kind: WindowKind::Day,
            calendar: false,
        };
        let (s, e) = w.current(
            datetime!(2026-11-01 10:29 UTC),
            datetime!(2026-11-01 10:30 UTC),
        );
        assert_eq!(
            (s, e),
            (
                datetime!(2026-10-31 10:30 UTC),
                datetime!(2026-11-01 10:30 UTC)
            )
        );
    }
}
