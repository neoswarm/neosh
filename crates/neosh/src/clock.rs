//! What time it was, said the way a transcript can afford to say it.
//!
//! A conversation happens in time and the transcript never said so. An answer that took four
//! minutes and one that took four seconds read identically once they were on the screen; a
//! question asked yesterday read exactly like the one asked a minute ago; and a card rebuilt from
//! a stored conversation carried no duration at all, because the messages recorded what happened
//! and never when. [`neosh_proto::Message::at`] is the missing fact and this is how it is written.
//!
//! **Everything here is fixed.** A clock time, a duration and the interval between two stamps are
//! all true forever; *how long ago* is not, and a row is written once. So the relative reading is
//! offered separately ([`ago`]) for the one row that is redrawn — the newest turn's — and
//! everything that settles into the transcript is absolute.

/// Seconds this machine is ahead of UTC, asked of the OS at most every few minutes.
///
/// [`crate::services::utc_offset`] spawns `date +%z`, which is the right answer for a chart drawn
/// once and the wrong one for a transcript that stamps every turn: rebuilding a long conversation
/// would fork a process per row. Cached, and re-asked on a timer rather than once for all time —
/// a workspace left running across a daylight-saving change would otherwise draw every turn after
/// it an hour out, which is exactly the sort of quietly wrong number this exists to remove.
pub fn local_offset() -> i64 {
    use std::sync::Mutex;
    use std::time::{Duration, Instant};
    static CACHE: Mutex<Option<(Instant, i64)>> = Mutex::new(None);
    const FRESH: Duration = Duration::from_secs(300);
    let Ok(mut cache) = CACHE.lock() else { return crate::services::utc_offset(None) };
    if let Some((when, offset)) = *cache {
        if when.elapsed() < FRESH {
            return offset;
        }
    }
    let offset = crate::services::utc_offset(None);
    *cache = Some((Instant::now(), offset));
    offset
}

/// Which local day an instant falls on, counted from the epoch.
///
/// The offset is applied before the division and not after, which is what makes a day start at
/// midnight where the person is rather than at midnight in London.
pub fn day(at: i64, offset: i64) -> i64 {
    (at + offset).div_euclid(86_400)
}

/// The wall clock, `14:32` or `2:32pm`.
pub fn clock(at: i64, offset: i64, twelve: bool) -> String {
    let secs = (at + offset).rem_euclid(86_400);
    let (h, m) = (secs / 3_600, (secs % 3_600) / 60);
    if !twelve {
        return format!("{h:02}:{m:02}");
    }
    let suffix = if h < 12 { "am" } else { "pm" };
    let h = match h % 12 {
        0 => 12,
        n => n,
    };
    format!("{h}:{m:02}{suffix}")
}

/// The date, `8 Sep`, with the year when it is asked for.
///
/// No weekday and no full month name: this goes in the right margin of a transcript, beside the
/// clock, and every column it takes is a column the conversation's own text does not have.
pub fn date(at: i64, offset: i64, with_year: bool) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d) = civil(day(at, offset));
    let name = MONTHS.get((m as usize).saturating_sub(1)).copied().unwrap_or("?");
    if with_year {
        format!("{d} {name} {y}")
    } else {
        format!("{d} {name}")
    }
}

/// Which year a local day falls in, for deciding whether [`date`] has to say.
pub fn year(at: i64, offset: i64) -> i64 {
    civil(day(at, offset)).0
}

/// Days since the epoch back into a date, by Howard Hinnant's `civil_from_days`.
///
/// The inverse of the `days_from_civil` in [`crate::usage`], and the same algorithm read the other
/// way: an era of 400 years is exactly 146,097 days, which is what lets the whole thing be
/// integer arithmetic with no table and no leap-year special cases.
fn civil(days: i64) -> (i64, i64, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// How long a turn took, at the least precision that is still an answer.
///
/// Coarser than [`crate::cards::took`], which is about a tool call: a call that came back in
/// 40 ms is a fact about the call, and a *turn* that took 40 ms did not happen. Whole seconds up
/// to a minute, minutes and seconds up to an hour, hours and minutes past that — because by then
/// the seconds are noise beside the hours.
pub fn lasted(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 60 {
        return format!("{secs}s");
    }
    if secs < 3_600 {
        return format!("{}m {:02}s", secs / 60, secs % 60);
    }
    format!("{}h {:02}m", secs / 3_600, (secs % 3_600) / 60)
}

/// How long ago something was, for the one row that is redrawn often enough to keep it true.
///
/// Deliberately vaguer the further back it goes: the difference between 3 and 4 minutes is
/// something you might act on, and the difference between 3 and 4 days is not.
pub fn ago(secs: i64) -> String {
    let secs = secs.max(0);
    if secs < 45 {
        return "just now".to_string();
    }
    if secs < 3_600 {
        return format!("{}m ago", (secs + 30) / 60);
    }
    if secs < 86_400 {
        return format!("{}h ago", secs / 3_600);
    }
    format!("{}d ago", secs / 86_400)
}

/// What a turn's margin says, before it is a string.
///
/// The two instants it is between, and the two decisions about how much of the date to spell out
/// — both of which are comparisons against the turn *above*, never against today, because these
/// rows are written once and today is a fact about when they were drawn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    /// When the question was asked, and when the last thing the turn produced landed. `None`
    /// while the turn is still running: a duration that stopped where the drawing happened to be
    /// would be a measurement of the redraw.
    pub asked: i64,
    pub ended: Option<i64>,
    /// Whether the label leads with the date, and whether that date carries a year.
    pub dated: bool,
    pub year: bool,
}

impl Stamp {
    /// `[<date> ]<clock>[  ·  <how long it took>][  ·  <how long ago>]`.
    ///
    /// `now` is `Some` only for the newest turn in a transcript, which is the one row kept up to
    /// date; everywhere else it is `None` and what comes out is true forever.
    pub fn label(&self, offset: i64, twelve: bool, now: Option<i64>) -> String {
        let mut out = String::new();
        if self.dated {
            out.push_str(&date(self.asked, offset, self.year));
            out.push(' ');
        }
        out.push_str(&clock(self.asked, offset, twelve));
        if let Some(ended) = self.ended {
            out.push_str("  \u{b7}  ");
            out.push_str(&lasted(ended - self.asked));
            if let Some(now) = now {
                out.push_str("  \u{b7}  ");
                out.push_str(&ago(now - ended));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_is_read_where_the_person_is() {
        // 2026-09-08T12:00:00Z, in a zone two hours ahead.
        let at = 1_788_868_800;
        assert_eq!(clock(at, 0, false), "12:00");
        assert_eq!(clock(at, 2 * 3_600, false), "14:00");
        assert_eq!(clock(at, -5 * 3_600, false), "07:00");
    }

    #[test]
    fn a_twelve_hour_clock_says_which_half_of_the_day_it_is() {
        let midnight = 1_788_868_800 - 12 * 3_600;
        assert_eq!(clock(midnight, 0, true), "12:00am");
        assert_eq!(clock(midnight + 13 * 3_600 + 5 * 60, 0, true), "1:05pm");
        assert_eq!(clock(midnight + 12 * 3_600, 0, true), "12:00pm");
    }

    #[test]
    fn a_day_starts_at_midnight_where_the_person_is_and_not_in_london() {
        // 23:00 UTC is already tomorrow two hours east, and still today five hours west.
        let late = 1_788_868_800 + 11 * 3_600;
        assert_eq!(day(late, 2 * 3_600), day(late, 0) + 1);
        assert_eq!(day(late, -5 * 3_600), day(late, 0));
    }

    #[test]
    fn a_date_reads_back_as_the_date_it_was() {
        assert_eq!(date(1_788_868_800, 0, false), "8 Sep");
        assert_eq!(date(1_788_868_800, 0, true), "8 Sep 2026");
        // The epoch itself, and a leap day, which is where a hand-rolled calendar goes wrong.
        assert_eq!(date(0, 0, true), "1 Jan 1970");
        assert_eq!(date(1_709_208_000, 0, true), "29 Feb 2024");
    }

    #[test]
    fn a_turn_is_measured_in_what_you_would_say_out_loud() {
        assert_eq!(lasted(0), "0s");
        assert_eq!(lasted(41), "41s");
        assert_eq!(lasted(61), "1m 01s");
        assert_eq!(lasted(3_599), "59m 59s");
        assert_eq!(lasted(3_600), "1h 00m");
        assert_eq!(lasted(9_000), "2h 30m");
        // A clock that went backwards is not a negative duration.
        assert_eq!(lasted(-5), "0s");
    }

    fn stamp(asked: i64, ended: Option<i64>) -> Stamp {
        Stamp { asked, ended, dated: false, year: false }
    }

    #[test]
    fn a_turn_still_running_says_when_it_was_asked_and_nothing_it_cannot_know() {
        // A duration stopping wherever the redraw happened would be a measurement of the redraw.
        assert_eq!(stamp(1_788_868_800, None).label(0, false, Some(1_788_900_000)), "12:00");
    }

    #[test]
    fn a_settled_turn_says_when_and_how_long_and_never_how_long_ago() {
        // The whole reason the relative reading is separate: this row is written once and nothing
        // ever comes back to it, so everything on it has to still be true tomorrow.
        let s = stamp(1_788_868_800, Some(1_788_868_800 + 252));
        assert_eq!(s.label(0, false, None), "12:00  \u{b7}  4m 12s");
        assert_eq!(s.label(0, false, Some(1_788_868_800 + 900)), "12:00  \u{b7}  4m 12s  \u{b7}  11m ago");
    }

    #[test]
    fn a_turn_on_a_new_day_leads_with_the_date_and_one_on_a_new_year_with_the_year() {
        let at = 1_788_868_800;
        assert_eq!(Stamp { asked: at, ended: None, dated: true, year: false }.label(0, false, None), "8 Sep 12:00");
        assert_eq!(Stamp { asked: at, ended: None, dated: true, year: true }.label(0, false, None), "8 Sep 2026 12:00");
    }

    #[test]
    fn how_long_ago_gets_vaguer_the_further_back_it_is() {
        assert_eq!(ago(3), "just now");
        assert_eq!(ago(44), "just now");
        assert_eq!(ago(90), "2m ago");
        assert_eq!(ago(3_599), "60m ago");
        assert_eq!(ago(7_200), "2h ago");
        assert_eq!(ago(200_000), "2d ago");
    }
}
