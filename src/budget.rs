//! Cost guard: `budget_session` / `budget_day` (USD, 0 = off). Day spend is kept in
//! state_dir/spend-day.json as {"date":"YYYY-MM-DD","usd":x}, UTC days.
use serde_json::{json, Value};
use std::path::Path;

/// UTC civil date for unix seconds (Hinnant's days-from-civil, inverted).
pub fn date(secs: u64) -> String {
    let z = (secs / 86400) as i64 + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

fn file() -> std::path::PathBuf { crate::config::state_dir().join("spend-day.json") }

pub fn day_spent_in(p: &Path, today: &str) -> f64 {
    let v: Value = std::fs::read_to_string(p).ok().and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
    if v["date"] == today { v["usd"].as_f64().unwrap_or(0.0) } else { 0.0 }
}

pub fn add_day_in(p: &Path, today: &str, usd: f64) -> f64 {
    let total = day_spent_in(p, today) + usd;
    if let Some(d) = p.parent() { let _ = std::fs::create_dir_all(d); }
    let _ = std::fs::write(p, json!({"date": today, "usd": total}).to_string());
    total
}

pub fn day_spent() -> f64 { day_spent_in(&file(), &date(crate::activity::now())) }
pub fn add_day(usd: f64) -> f64 {
    if usd <= 0.0 { return day_spent() }
    add_day_in(&file(), &date(crate::activity::now()), usd)
}

/// The first budget passed: (spent, limit, "this session" | "today"). Limits ≤ 0 are off.
pub fn over(session: f64, session_limit: f64, day: f64, day_limit: f64) -> Option<(f64, f64, &'static str)> {
    if day_limit > 0.0 && day >= day_limit { return Some((day, day_limit, "today")) }
    if session_limit > 0.0 && session >= session_limit { return Some((session, session_limit, "this session")) }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(date(0), "1970-01-01");
        assert_eq!(date(1_791_331_200), "2026-10-07");
        assert_eq!(date(951_782_400), "2000-02-29");
    }

    #[test]
    fn day_file_resets_on_a_new_date() {
        let p = std::env::temp_dir().join(format!("qh-spend-{}.json", std::process::id()));
        let _ = std::fs::remove_file(&p);
        assert_eq!(add_day_in(&p, "2026-10-19", 1.5), 1.5);
        assert!((add_day_in(&p, "2026-10-19", 0.25) - 1.75).abs() < 1e-9);
        assert_eq!(day_spent_in(&p, "2026-10-20"), 0.0);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn over_checks_both_and_ignores_off() {
        assert_eq!(over(5.0, 0.0, 5.0, 0.0), None);
        assert_eq!(over(5.0, 4.0, 1.0, 0.0), Some((5.0, 4.0, "this session")));
        assert_eq!(over(1.0, 4.0, 9.0, 8.0), Some((9.0, 8.0, "today")));
        assert_eq!(over(1.0, 4.0, 1.0, 8.0), None);
    }
}
