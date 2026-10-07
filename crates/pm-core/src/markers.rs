//! Structured markers (projects/pm/README.md §Data model "Structured
//! markers"; AGT-1342): the pure rules behind `pm hold`, `pm waive` and the
//! `not_before=` / `parked=` assignments of `pm set`.
//!
//! Dates are strict ISO-8601 calendar dates, `YYYY-MM-DD`, validated here
//! so every surface (CLI, import, hub) rejects the same inputs. Because the
//! format is fixed-width and zero-padded, two valid dates compare correctly
//! as strings, which is how [`NotBefore`] and [`Parked`] are compared
//! against "today".

use std::fmt;

use crate::domain::{NotBefore, Parked, Waiver};

/// `parked=forever`: parked with no end date.
pub const PARKED_FOREVER: &str = "forever";

/// Why a marker value was rejected. The message names the input.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MarkerError(pub String);

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for MarkerError {}

/// Validates a strict `YYYY-MM-DD` calendar date (four-digit year, month
/// 01–12, a day that exists in that month, leap years included) and
/// returns it unchanged.
pub fn parse_date(s: &str) -> Result<String, MarkerError> {
    let bad = || MarkerError(format!("'{s}' is not a YYYY-MM-DD date"));
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' {
        return Err(bad());
    }
    let digits = |r: std::ops::Range<usize>| -> Result<u32, MarkerError> {
        let part = &s[r];
        if part.bytes().all(|c| c.is_ascii_digit()) {
            part.parse().map_err(|_| bad())
        } else {
            Err(bad())
        }
    };
    let (year, month, day) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    if !(1..=12).contains(&month) || day == 0 || day > days_in_month(year, month) {
        return Err(MarkerError(format!(
            "'{s}' is not a YYYY-MM-DD date: {year:04}-{month:02} has no day {day:02}"
        )));
    }
    Ok(s.to_string())
}

/// `not_before=YYYY-MM-DD`.
pub fn parse_not_before(s: &str) -> Result<NotBefore, MarkerError> {
    Ok(NotBefore {
        date: parse_date(s.trim())?,
    })
}

/// `parked=YYYY-MM-DD|forever`.
pub fn parse_parked(s: &str) -> Result<Parked, MarkerError> {
    let s = s.trim();
    if s == PARKED_FOREVER {
        return Ok(Parked {
            until: PARKED_FOREVER.to_string(),
        });
    }
    parse_date(s)
        .map(|until| Parked { until })
        .map_err(|e| MarkerError(format!("{e} or '{PARKED_FOREVER}'")))
}

impl NotBefore {
    /// Still gating on `today` (a `YYYY-MM-DD`): the date has not arrived.
    pub fn is_active(&self, today: &str) -> bool {
        self.date.as_str() > today
    }
}

impl Parked {
    /// Still parked on `today`: forever, or the end date is today or later.
    pub fn is_active(&self, today: &str) -> bool {
        self.until == PARKED_FOREVER || self.until.as_str() >= today
    }
}

/// Hygiene rule R1 ("every ticket declares a project, or says why not") is
/// waived by a waiver naming `R1`, or the vault's `waived: standalone`
/// spelling, which import records as rule `standalone`.
pub fn waives_r1(waivers: &[Waiver]) -> bool {
    waives(waivers, "R1") || waives(waivers, "standalone")
}

/// Whether `waivers` holds one for `rule` (compared ignoring ASCII case).
/// `pm check` honours waivers for R1 (via [`waives_r1`]) and `parked`
/// (AGT-1575); other findings are not waivable.
pub fn waives(waivers: &[Waiver], rule: &str) -> bool {
    waivers.iter().any(|w| w.rule.eq_ignore_ascii_case(rule))
}

/// Normalizes a rule name: trimmed, and `r1` → `R1` so the hygiene rules
/// have one spelling. Empty is rejected.
pub fn normalize_rule(rule: &str) -> Result<String, MarkerError> {
    let rule = rule.trim();
    if rule.is_empty() {
        return Err(MarkerError("waiver rule must not be empty".into()));
    }
    let hygiene = rule.len() > 1
        && rule.as_bytes()[0].eq_ignore_ascii_case(&b'r')
        && rule[1..].bytes().all(|b| b.is_ascii_digit());
    Ok(if hygiene {
        rule.to_ascii_uppercase()
    } else {
        rule.to_string()
    })
}

/// The waiver list after `pm waive <rule> <reason>`: a waiver for the same
/// rule is replaced in place (its reason updated), otherwise the new one is
/// appended. `FieldSet::Waivers` is an LWW register on the whole list, so
/// the caller reads the current list, extends it here, and sets the result.
pub fn with_waiver(current: &[Waiver], waiver: Waiver) -> Vec<Waiver> {
    let mut out = current.to_vec();
    match out.iter_mut().find(|w| w.rule == waiver.rule) {
        Some(existing) => existing.reason = waiver.reason,
        None => out.push(waiver),
    }
    out
}

fn is_leap(year: u32) -> bool {
    (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400)
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(year) => 29,
        2 => 28,
        _ => 0,
    }
}

/// The UTC calendar date (`YYYY-MM-DD`) of a Unix-epoch millisecond
/// timestamp. Pure: the caller passes the clock reading in. (Howard
/// Hinnant's `civil_from_days`.)
pub fn date_from_ms(ms: u64) -> String {
    let days = (ms / 86_400_000) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_are_strict() {
        for good in ["2026-10-01", "2024-02-29", "2000-02-29", "1999-12-31"] {
            assert_eq!(parse_date(good).as_deref(), Ok(good));
        }
        for bad in [
            "2026-1-01",
            "2026-13-01",
            "2026-00-10",
            "2026-02-29",
            "1900-02-29",
            "2026-04-31",
            "2026-10-00",
            "26-10-01",
            "2026/10/01",
            "2026-10-01T00:00",
            " 2026-10-01",
            "２０２６-10-01",
            "",
            "tomorrow",
        ] {
            assert!(parse_date(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn parked_takes_a_date_or_forever() {
        assert_eq!(parse_parked("forever").unwrap().until, "forever");
        assert_eq!(parse_parked(" 2026-11-01 ").unwrap().until, "2026-11-01");
        let err = parse_parked("never").unwrap_err();
        assert!(err.0.contains("forever"), "{err}");
        assert!(parse_parked("Forever").is_err());
    }

    #[test]
    fn activity_compares_against_today() {
        let nb = NotBefore {
            date: "2026-10-01".into(),
        };
        assert!(nb.is_active("2026-09-30"));
        assert!(!nb.is_active("2026-10-01"));
        let p = Parked {
            until: "2026-10-01".into(),
        };
        assert!(p.is_active("2026-10-01"));
        assert!(!p.is_active("2026-10-02"));
        assert!(
            Parked {
                until: PARKED_FOREVER.into()
            }
            .is_active("9999-12-31")
        );
    }

    #[test]
    fn waivers_replace_by_rule_and_r1_is_recognized() {
        let w = |rule: &str, reason: &str| Waiver {
            rule: rule.into(),
            reason: reason.into(),
        };
        let list = with_waiver(&[w("R3", "old")], w("R1", "standalone"));
        assert_eq!(list, [w("R3", "old"), w("R1", "standalone")]);
        let list = with_waiver(&list, w("R3", "new"));
        assert_eq!(list, [w("R3", "new"), w("R1", "standalone")]);
        assert!(waives_r1(&list));
        assert!(waives_r1(&[w("standalone", "one-off")]));
        assert!(!waives_r1(&[w("R3", "x")]));
        assert_eq!(normalize_rule(" r1 ").unwrap(), "R1");
        assert_eq!(normalize_rule("standalone").unwrap(), "standalone");
        assert_eq!(normalize_rule("rust").unwrap(), "rust");
        assert!(normalize_rule("  ").is_err());
    }

    #[test]
    fn epoch_ms_to_calendar_date() {
        assert_eq!(date_from_ms(0), "1970-01-01");
        assert_eq!(date_from_ms(951_782_400_000), "2000-02-29");
        // 2026-09-28T23:59:59.999Z
        assert_eq!(date_from_ms(1_790_639_999_999), "2026-09-28");
        assert_eq!(date_from_ms(1_790_640_000_000), "2026-09-29");
    }
}
