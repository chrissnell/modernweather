//! Instants, in and out, without a calendar dependency.
//!
//! The harness needs two conversions: an operator's `--since 2026-11-13T18:00:00Z` into an
//! epoch, and an epoch back into the same spelling for the report. Both sides of the diff
//! already speak epochs -- `/span` and `/latest` serialise `ts` as milliseconds since the
//! epoch (Q-3), and every SQL query here selects `extract(epoch from time)` rather than a
//! formatted timestamp, because the data migration's second defect was a server's LOCAL year
//! leaking into a key and reporting all 1,182 digest rows as mismatches
//! (docs/data-migration.md §2b). Nothing in this harness formats a timestamp for comparison.

/// Days from 1970-01-01 to the given civil date. Howard Hinnant's `days_from_civil`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse, for the report.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// `YYYY-MM-DDTHH:MM:SSZ` (and the `Z`-less and fractional spellings) into epoch milliseconds.
///
/// UTC only, and the `Z` is accepted rather than required because an operator typing a window
/// by hand drops it. An offset is refused rather than interpreted: a window silently read in
/// the wrong zone is the defect this module's own documentation is about.
pub fn parse_utc_ms(s: &str) -> Result<i64, String> {
    let t = s.trim().trim_end_matches('Z');
    if t.contains('+') || t.matches('-').count() > 2 {
        return Err(format!(
            "{s}: give the window in UTC. An offset is not accepted here"
        ));
    }
    let (date, time) = match t.split_once(['T', ' ']) {
        Some((d, r)) => (d, r),
        None => (t, "00:00:00"),
    };
    let dp: Vec<&str> = date.split('-').collect();
    if dp.len() != 3 {
        return Err(format!("{s}: expected YYYY-MM-DD[THH:MM:SS]"));
    }
    let num = |v: &str| -> Result<i64, String> {
        v.parse::<i64>()
            .map_err(|_| format!("{s}: `{v}` is not a number"))
    };
    let (y, mo, d) = (num(dp[0])?, num(dp[1])?, num(dp[2])?);
    let tp: Vec<&str> = time.split(':').collect();
    let h = num(tp.first().copied().unwrap_or("0"))?;
    let mi = num(tp.get(1).copied().unwrap_or("0"))?;
    let (sec, frac) = match tp.get(2) {
        Some(sv) => match sv.split_once('.') {
            Some((w, f)) => (num(w)?, format!("0.{f}").parse::<f64>().unwrap_or(0.0)),
            None => (num(sv)?, 0.0),
        },
        None => (0, 0.0),
    };
    let days = days_from_civil(y, mo, d);
    Ok((days * 86_400 + h * 3_600 + mi * 60 + sec) * 1_000 + (frac * 1000.0) as i64)
}

/// Epoch milliseconds as `YYYY-MM-DDTHH:MM:SSZ`, which is the only spelling this harness
/// prints. Sub-second precision is dropped except where a caller asks for it explicitly, so
/// that two instants three milliseconds apart do not read as the same one.
pub fn format_utc_ms(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (y, mo, d) = civil_from_days(days);
    let (h, mi, s) = (rem / 3_600_000, (rem / 60_000) % 60, (rem / 1_000) % 60);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
}

/// The same, with milliseconds, for a pairing offset that is the point of the line.
pub fn format_utc_ms_exact(ms: i64) -> String {
    format!(
        "{}.{:03}",
        format_utc_ms(ms).trim_end_matches('Z'),
        ms.rem_euclid(1_000)
    )
}

/// A `--last` duration: `90s`, `30m`, `24h`, `7d`. The spelling `/span` takes is a Go
/// duration and this is not it -- an operator's window and a URL's span are different inputs
/// and conflating them is how `/span/30d` becomes a 400 nobody expected.
pub fn parse_duration_secs(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num
        .parse()
        .map_err(|_| format!("{s}: expected <number><s|m|h|d>"))?;
    let mul = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3_600,
        "d" => 86_400,
        _ => return Err(format!("{s}: expected <number><s|m|h|d>")),
    };
    Ok(n * mul)
}

/// Now, in epoch milliseconds.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_spellings_the_report_prints() {
        let ms = parse_utc_ms("2026-11-13T18:43:13Z").unwrap();
        assert_eq!(ms, 1_794_595_393_000);
        assert_eq!(format_utc_ms(ms), "2026-11-13T18:43:13Z");
    }

    #[test]
    fn reads_the_ts_the_contract_serialises() {
        // Q-3: `ts` is milliseconds since the epoch. This is the first element of the
        // /span/3h response captured from the Go server on 2026-11-13.
        assert_eq!(format_utc_ms(1_794_584_700_000), "2026-11-13T15:45:00Z");
    }

    #[test]
    fn refuses_an_offset_rather_than_guessing() {
        assert!(parse_utc_ms("2026-11-13T18:43:13-07:00").is_err());
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration_secs("24h").unwrap(), 86_400);
        assert_eq!(parse_duration_secs("7d").unwrap(), 604_800);
        assert!(parse_duration_secs("24").is_err());
    }
}
