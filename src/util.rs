//! Small shared helpers.

/// Format a unix timestamp (seconds) as a compact relative time, e.g. "3h ago".
pub fn relative_time(ts: Option<i64>) -> String {
    let ts = match ts {
        Some(t) => t,
        None => return "never".to_string(),
    };
    let now = chrono::Utc::now().timestamp();
    let diff = now - ts;
    if diff < 0 {
        return "just now".to_string();
    }
    const MIN: i64 = 60;
    const HOUR: i64 = 60 * MIN;
    const DAY: i64 = 24 * HOUR;
    const WEEK: i64 = 7 * DAY;

    if diff < MIN {
        "just now".to_string()
    } else if diff < HOUR {
        format!("{}m ago", diff / MIN)
    } else if diff < DAY {
        format!("{}h ago", diff / HOUR)
    } else if diff < WEEK {
        format!("{}d ago", diff / DAY)
    } else {
        format!("{}w ago", diff / WEEK)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_when_absent() {
        assert_eq!(relative_time(None), "never");
    }

    #[test]
    fn formats_relative() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(relative_time(Some(now)), "just now");
        assert_eq!(relative_time(Some(now - 120)), "2m ago");
        assert_eq!(relative_time(Some(now - 3 * 3600)), "3h ago");
        assert_eq!(relative_time(Some(now - 2 * 86400)), "2d ago");
        assert_eq!(relative_time(Some(now - 14 * 86400)), "2w ago");
    }
}
