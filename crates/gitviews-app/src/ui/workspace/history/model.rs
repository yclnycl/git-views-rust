use std::ops::Range;

pub(super) fn short_id(id: &str) -> &str {
    id.get(..8).unwrap_or(id)
}

pub(super) fn display_text(text: &str, max_chars: usize) -> String {
    let mut chars = text.chars();
    let mut shown: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        shown.push('…');
    }
    shown
}

/// Every search term can match a different metadata field. No file contents or
/// commit messages are scanned, and the caller has already bounded the list.
#[cfg(test)]
pub(super) fn matches_query(query: &str, id: &str, author: &str, subject: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let fields = format!("{id}\n{author}\n{subject}").to_lowercase();
    query.split_whitespace().all(|term| fields.contains(term))
}

pub(super) fn message_ranges(message: &str) -> Vec<Range<usize>> {
    let mut start = 0;
    message
        .split_inclusive('\n')
        .map(|line| {
            let range = start..start + line.len();
            start = range.end;
            range
        })
        .collect()
}

pub(super) fn status_label(status: char) -> &'static str {
    match status {
        'A' => "已新增",
        'D' => "已删除",
        'R' => "已重命名",
        'C' => "已复制",
        'T' => "类型变化",
        _ => "已修改",
    }
}

/// Display the stored instant unambiguously without depending on the machine's
/// timezone or a new runtime library. Extremely unusual Git dates stay numeric.
pub(super) fn format_utc(timestamp: i64) -> String {
    let seconds = timestamp.rem_euclid(86_400);
    let days = i128::from(timestamp.div_euclid(86_400)) + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    if !(0..=9_999).contains(&year) {
        return format!("Unix {timestamp} 秒");
    }
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02} UTC",
        seconds / 3_600,
        seconds % 3_600 / 60,
        seconds % 60,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_search_matches_terms_across_hash_author_and_unicode_title() {
        let id = "aB12cd34ee567890";
        let author = "Alice 王";
        let subject = "Fix 中文文件 preview";
        for query in ["ab12", "alice 中文", "王 preview", "fix cd34"] {
            assert!(matches_query(query, id, author, subject), "{query}");
        }
        assert!(!matches_query("alice absent", id, author, subject));
        assert!(!matches_query("message body", id, author, subject));
    }

    #[test]
    fn utc_dates_handle_epoch_negative_time_leap_day_and_extreme_git_dates() {
        assert_eq!(format_utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(format_utc(-1), "1969-12-31 23:59:59 UTC");
        assert_eq!(format_utc(951_782_400), "2000-02-29 00:00:00 UTC");
        assert_eq!(format_utc(1_700_000_000), "2023-11-14 22:13:20 UTC");
        assert!(format_utc(i64::MAX).starts_with("Unix "));
        assert!(format_utc(i64::MIN).starts_with("Unix "));
    }

    #[test]
    fn message_offsets_preserve_utf8_crlf_and_unterminated_last_line() {
        let text = "标题\r\n\n最后一行";
        let lines: Vec<_> = message_ranges(text)
            .into_iter()
            .map(|range| &text[range])
            .collect();
        assert_eq!(lines, ["标题\r\n", "\n", "最后一行"]);
        assert!(message_ranges("").is_empty());
    }
}
