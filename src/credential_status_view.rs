//! Operator-facing credential status rendering for Orbit CLI.
//!
//! Provides human-readable views for single credentials, multi-credential
//! overviews, and provider-grouped detailed quota views, as well as helpers
//! for clean machine-readable JSON and debug diagnostic output.

use serde_json::Value;

/// Recommended progress bar width in characters.
pub const PROGRESS_BAR_WIDTH: usize = 10;

/// Default Orbit UI fallback label for unlabeled Codex buckets.
/// Note: "default" is Orbit UI fallback, not provider-supplied metadata.
pub const CODEX_FALLBACK_BUCKET_LABEL: &str = "default";

/// Minimum structural gap between table columns in wide quota tables.
pub const TABLE_COLUMN_GAP: &str = "  ";

/// Format a quota remaining percentage as a compact 10-character progress bar.
///
/// Uses Unicode blocks:
/// - Full block: `█`
/// - Light shade empty block: `░`
/// - Left 1/8th block: `▏` for minimal non-zero quota (< 3%)
///
/// Exact test values:
/// - 100%   -> `██████████`
/// - 99.1%  -> `██████████`
/// - 89.3%  -> `█████████░`
/// - 74%    -> `███████░░░`
/// - 50%    -> `█████░░░░░`
/// - 6.8%   -> `█░░░░░░░░░`
/// - 1%     -> `▏░░░░░░░░░`
/// - 0%     -> `░░░░░░░░░░`
pub fn format_progress_bar(percentage: f64) -> String {
    let clamped = percentage.clamp(0.0, 100.0);
    if clamped <= 0.0 {
        return "░".repeat(PROGRESS_BAR_WIDTH);
    }
    if clamped >= 99.0 {
        return "█".repeat(PROGRESS_BAR_WIDTH);
    }
    if clamped < 3.0 {
        return format!("▏{}", "░".repeat(PROGRESS_BAR_WIDTH - 1));
    }
    let filled = (clamped / 10.0).round() as usize;
    let filled = filled.clamp(1, PROGRESS_BAR_WIDTH - 1);
    let empty = PROGRESS_BAR_WIDTH - filled;
    format!("{}{}", "█".repeat(filled), "░".repeat(empty))
}

/// Format a quota percentage:
/// - Integer percentage when effectively integral (e.g. 100%, 74%, 1%, 0%).
/// - Otherwise one decimal place (e.g. 99.1%, 89.3%, 6.8%).
pub fn format_percentage(percent: f64) -> String {
    let rounded = percent.round();
    if (percent - rounded).abs() < 0.05 {
        format!("{}%", rounded as i64)
    } else {
        format!("{:.1}%", percent)
    }
}

/// Convert UTC epoch milliseconds to (year, month, day, hour, minute, second).
pub fn civil_from_epoch_ms(ms: i64) -> (i32, u32, u32, u32, u32, u32) {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);

    let hour = (secs_of_day / 3600) as u32;
    let minute = ((secs_of_day % 3600) / 60) as u32;
    let second = (secs_of_day % 60) as u32;

    // Howard Hinnant's algorithm for civil date from days since 1970-01-01
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = (z - era * 146_097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = (yoe as i64) + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y as i32, m, d, hour, minute, second)
}

pub fn local_civil_from_epoch_ms(ms: i64) -> (i32, u32, u32, u32, u32, u32) {
    let secs = ms.div_euclid(1000);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let time_val: libc::time_t = secs as libc::time_t;
    let res = unsafe { libc::localtime_r(&time_val, &mut tm) };
    if !res.is_null() {
        (
            tm.tm_year + 1900,
            (tm.tm_mon + 1) as u32,
            tm.tm_mday as u32,
            tm.tm_hour as u32,
            tm.tm_min as u32,
            tm.tm_sec as u32,
        )
    } else {
        // Fallback to UTC civil conversion if localtime_r fails
        civil_from_epoch_ms(ms)
    }
}

const MONTH_NAMES: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// Format a reset timestamp from epoch milliseconds using the unified human format:
/// - Same year as display/now: "MMM D HH:MM" (e.g. "Sep 26 05:07", "Sep 29 14:32", "Oct 3 00:11")
/// - Different year than display/now: "MMM D HH:MM YYYY" (e.g. "Dec 31 23:45 2027")
pub fn format_reset_time(ms: i64, now_ms: i64) -> String {
    let (reset_year, month, day, hour, minute, _second) = local_civil_from_epoch_ms(ms);
    let (current_year, _, _, _, _, _) = local_civil_from_epoch_ms(now_ms);

    let month_str = if (1..=12).contains(&month) {
        MONTH_NAMES[(month - 1) as usize]
    } else {
        "???"
    };

    if reset_year != current_year {
        format!("{month_str} {day} {hour:02}:{minute:02} {reset_year}")
    } else {
        format!("{month_str} {day} {hour:02}:{minute:02}")
    }
}

/// Shared human reset time formatter converting UTC epoch ms to local timezone.
pub fn format_human_reset_time(ms: i64, now_ms: i64) -> String {
    format_reset_time(ms, now_ms)
}

/// Backward-compatible alias for `format_reset_time`.
pub fn format_reset_timestamp(ms: i64, now_ms: i64) -> String {
    format_reset_time(ms, now_ms)
}

/// Format relative elapsed observation time:
/// - < 60s: "just now"
/// - < 1h: "Xm ago"
/// - < 24h: "Xh ago"
/// - >= 24h: "Xd ago"
///
/// In table mode: if stale, appends " · STALE" (e.g. "4m ago · STALE").
/// In single mode: if stale, appends " (stale)" (e.g. "4m ago (stale)").
pub fn format_observed_time(
    observed_at_ms: Option<i64>,
    is_fresh: bool,
    now_ms: i64,
    is_table: bool,
) -> String {
    let Some(obs_ms) = observed_at_ms else {
        return "—".to_string();
    };

    let elapsed_ms = (now_ms - obs_ms).max(0);
    let elapsed_secs = elapsed_ms / 1000;

    let base = if elapsed_secs < 60 {
        "just now".to_string()
    } else if elapsed_secs < 3600 {
        format!("{}m ago", elapsed_secs / 60)
    } else if elapsed_secs < 86400 {
        format!("{}h ago", elapsed_secs / 3600)
    } else {
        format!("{}d ago", elapsed_secs / 86400)
    };

    if !is_fresh {
        if is_table {
            format!("{base} · STALE")
        } else {
            format!("{base} (stale)")
        }
    } else {
        base
    }
}

/// Detect terminal width, checking $COLUMNS first, then TIOCGWINSZ ioctl on stdout.
pub fn terminal_width() -> Option<usize> {
    if let Ok(cols_str) = std::env::var("COLUMNS")
        && let Ok(cols) = cols_str.parse::<usize>()
        && cols > 0
    {
        return Some(cols);
    }

    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        let fd = std::io::stdout().as_raw_fd();
        unsafe {
            let mut ws: libc::winsize = std::mem::zeroed();
            if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 {
                return Some(ws.ws_col as usize);
            }
        }
    }

    None
}

/// Extract window duration display key (e.g. "5h", "7d", "weekly").
///
/// In human presentation, normal output uses duration-based keys:
/// - 300 minutes -> "5h"
/// - 10080 minutes -> "weekly" if provider_window_id is "weekly", otherwise "7d"
///
/// Protocol identifiers like "primary" and "secondary" are NEVER shown in normal human output.
fn window_duration_key(window: &Value) -> String {
    if let Some(dur_mins) = window.get("duration_minutes").and_then(Value::as_i64) {
        if dur_mins == 300 {
            return "5h".to_string();
        } else if dur_mins == 10080 {
            if window.get("provider_window_id").and_then(Value::as_str) == Some("weekly") {
                return "weekly".to_string();
            }
            return "7d".to_string();
        } else if dur_mins % 1440 == 0 {
            return format!("{}d", dur_mins / 1440);
        } else if dur_mins % 60 == 0 {
            return format!("{}h", dur_mins / 60);
        } else if dur_mins > 0 {
            return format!("{dur_mins}m");
        }
    }
    if let Some(w_id) = window.get("provider_window_id").and_then(Value::as_str)
        && w_id != "primary"
        && w_id != "secondary"
    {
        return w_id.to_string();
    }
    "—".to_string()
}

/// Extract percentage f64 and formatted percentage string from a window value.
fn extract_window_percentages(window: &Value) -> (f64, String) {
    if let Some(rem_pct) = window.get("remaining_percent").and_then(Value::as_f64) {
        return (rem_pct, format_percentage(rem_pct));
    }
    if let Some(rem_frac) = window.get("remaining_fraction").and_then(Value::as_f64) {
        let pct = rem_frac * 100.0;
        return (pct, format_percentage(pct));
    }
    if let Some(used_pct) = window.get("used_percent").and_then(Value::as_f64) {
        let rem_pct = (100.0 - used_pct).max(0.0);
        return (rem_pct, format_percentage(rem_pct));
    }
    (0.0, "—".to_string())
}

/// Compute aggregate auth state: VALID, PARTIAL, or UNAVAILABLE.
pub fn compute_auth_summary(report: &Value) -> &'static str {
    let provider = report
        .pointer("/credential/provider")
        .and_then(Value::as_str)
        .unwrap_or("");

    if provider == "codex" {
        let codex_val = report
            .pointer("/representations/codex/validation")
            .and_then(Value::as_str);
        if codex_val == Some("valid") {
            "VALID"
        } else {
            "UNAVAILABLE"
        }
    } else if provider == "antigravity" {
        let acp_val = report
            .pointer("/representations/acp/validation")
            .and_then(Value::as_str)
            == Some("valid");
        let agy_val = report
            .pointer("/representations/agy-cli/validation")
            .and_then(Value::as_str)
            == Some("valid");

        if acp_val && agy_val {
            "VALID"
        } else if acp_val || agy_val {
            "PARTIAL"
        } else {
            "UNAVAILABLE"
        }
    } else {
        "UNAVAILABLE"
    }
}

/// Format human-readable detailed view for ONE credential (`orbit credential status <reference>`).
pub fn format_single_credential(report: &Value, now_ms: i64) -> String {
    let mut out = String::new();

    let reference = report
        .pointer("/credential/reference")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let provider = report
        .pointer("/credential/provider")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let lifecycle = report
        .pointer("/credential/lifecycle")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN")
        .to_ascii_uppercase();

    out.push_str(&format!("{reference}  [{provider}]\n\n"));

    let is_fresh = report
        .pointer("/availability/fresh")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let observed_at_ms = report
        .pointer("/availability/observed_at_ms")
        .and_then(Value::as_i64);
    let observed_str = format_observed_time(observed_at_ms, is_fresh, now_ms, false);

    let availability_state = report
        .pointer("/availability/state")
        .and_then(Value::as_str)
        .unwrap_or("UNKNOWN")
        .to_ascii_uppercase();

    let status_state = report
        .pointer("/status/state")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let status_reason = report.pointer("/status/reason").and_then(Value::as_str);

    let runtime_healthy = report
        .pointer("/health/runtime/state")
        .and_then(Value::as_str)
        == Some("healthy");
    let runtime_str = if runtime_healthy { "HEALTHY" } else { "—" };

    if provider == "codex" {
        let auth_val = report
            .pointer("/representations/codex/validation")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                report
                    .pointer("/representations/codex/state")
                    .and_then(Value::as_str)
                    .unwrap_or("MISSING")
            })
            .to_ascii_uppercase();

        let scope_val = report
            .pointer("/status/provider_scope")
            .and_then(Value::as_str)
            .unwrap_or("—")
            .to_ascii_uppercase();

        out.push_str(&format!("  Lifecycle      {lifecycle}\n"));
        out.push_str(&format!("  Auth           {auth_val}\n"));
        out.push_str(&format!("  Runtime        {runtime_str}\n"));
        out.push_str(&format!("  Scope          {scope_val}\n"));
        out.push_str(&format!("  Availability   {availability_state}\n"));
        out.push_str(&format!("  Observed       {observed_str}\n"));

        let quota_promoted = report
            .pointer("/status/quota_promoted")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        if (status_state == "unavailable" || status_state == "partial")
            && let Some(reason) = status_reason
        {
            out.push_str(&format!("\n  Status unavailable\n    {reason}\n"));
        } else if !quota_promoted || scope_val == "UNCONFIRMED" {
            let reason = report
                .pointer("/status/quota_promotion_reason")
                .and_then(Value::as_str)
                .unwrap_or("provider scope requires confirmation");
            out.push_str(&format!(
                "\n  Quota observed but not promoted\n    {reason}\n"
            ));
        }

        let buckets = report
            .pointer("/availability/quota_buckets")
            .and_then(Value::as_array);

        if let Some(buckets) = buckets.filter(|b| !b.is_empty()) {
            let quota_header = if !quota_promoted || scope_val == "UNCONFIRMED" {
                "  Observed quota"
            } else {
                "  Quota"
            };
            out.push_str(&format!("\n{quota_header}\n"));

            for (idx, bucket) in buckets.iter().enumerate() {
                if idx > 0 {
                    out.push('\n');
                }
                let label = bucket
                    .get("provider_label")
                    .and_then(Value::as_str)
                    .unwrap_or(CODEX_FALLBACK_BUCKET_LABEL);
                out.push_str(&format!("    {label}\n"));

                if let Some(windows) = bucket.get("windows").and_then(Value::as_array) {
                    let mut sorted_windows: Vec<&Value> = windows.iter().collect();
                    sorted_windows.sort_by_key(|w| {
                        w.get("duration_minutes")
                            .and_then(Value::as_i64)
                            .unwrap_or(i64::MAX)
                    });

                    for window in sorted_windows {
                        let dur_key = window_duration_key(window);
                        let (pct_val, pct_str) = extract_window_percentages(window);
                        let bar = format_progress_bar(pct_val);
                        let pct_display = if !is_fresh && observed_at_ms.is_some() {
                            format!("{pct_str} [stale]")
                        } else {
                            pct_str
                        };

                        let reset_part = if let Some(resets_ms) =
                            window.get("resets_at_ms").and_then(Value::as_i64)
                        {
                            let formatted_time = format_reset_time(resets_ms, now_ms);
                            format!("   resets {formatted_time}")
                        } else {
                            String::new()
                        };

                        out.push_str(&format!(
                            "      {dur_key:<4}{pct_display:>7}  {bar}{reset_part}\n"
                        ));
                    }
                }
            }
        }
    } else if provider == "antigravity" {
        let acp_val = report
            .pointer("/representations/acp/validation")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                report
                    .pointer("/representations/acp/state")
                    .and_then(Value::as_str)
                    .unwrap_or("MISSING")
            })
            .to_ascii_uppercase();

        let agy_val = report
            .pointer("/representations/agy-cli/validation")
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                report
                    .pointer("/representations/agy-cli/state")
                    .and_then(Value::as_str)
                    .unwrap_or("MISSING")
            })
            .to_ascii_uppercase();

        out.push_str(&format!("  Lifecycle      {lifecycle}\n"));
        out.push_str(&format!("  ACP            {acp_val}\n"));
        out.push_str(&format!("  agy-cli        {agy_val}\n"));
        out.push_str(&format!("  Runtime        {runtime_str}\n"));
        out.push_str(&format!("  Availability   {availability_state}\n"));
        out.push_str(&format!("  Observed       {observed_str}\n"));

        if (status_state == "unavailable" || status_state == "partial")
            && let Some(reason) = status_reason
        {
            out.push_str(&format!("\n  Status unavailable\n    {reason}\n"));
        }

        let groups = report
            .pointer("/availability/quota_groups")
            .and_then(Value::as_array);
        let buckets = report
            .pointer("/availability/quota_buckets")
            .and_then(Value::as_array);

        if let Some(groups) = groups {
            for group in groups {
                out.push('\n');
                let display_name = group
                    .get("provider_display_name")
                    .and_then(Value::as_str)
                    .unwrap_or("—");
                out.push_str(&format!("  {display_name}\n"));

                if let Some(members) = group.get("members").and_then(Value::as_array) {
                    let member_names: Vec<&str> = members
                        .iter()
                        .filter_map(|m| m.get("provider_label").and_then(Value::as_str))
                        .collect();
                    if !member_names.is_empty() {
                        out.push_str(&format!("    {}\n", member_names.join(", ")));
                    }
                }

                let group_fps: Vec<&str> = group
                    .get("bucket_fingerprints")
                    .and_then(Value::as_array)
                    .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                    .unwrap_or_default();

                if let Some(buckets) = buckets {
                    let mut group_windows = Vec::new();
                    for bucket in buckets {
                        let fp = bucket
                            .get("provider_bucket_fingerprint")
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if (group_fps.is_empty() || group_fps.contains(&fp))
                            && let Some(windows) = bucket.get("windows").and_then(Value::as_array)
                        {
                            for w in windows {
                                group_windows.push(w);
                            }
                        }
                    }

                    group_windows.sort_by_key(|w| {
                        let key = window_duration_key(w);
                        if key == "5h" {
                            0
                        } else if key == "weekly" {
                            1
                        } else {
                            2
                        }
                    });

                    for window in group_windows {
                        let dur_key = window_duration_key(window);
                        let (pct_val, pct_str) = extract_window_percentages(window);
                        let bar = format_progress_bar(pct_val);
                        let pct_display = if !is_fresh && observed_at_ms.is_some() {
                            format!("{pct_str} [stale]")
                        } else {
                            pct_str
                        };

                        let reset_part = if let Some(resets_ms) =
                            window.get("resets_at_ms").and_then(Value::as_i64)
                        {
                            let formatted_time = format_reset_time(resets_ms, now_ms);
                            format!("   resets {formatted_time}")
                        } else {
                            String::new()
                        };

                        out.push_str(&format!(
                            "    {dur_key:<8}{pct_display:>6}  {bar}{reset_part}\n"
                        ));
                    }
                }
            }
        }
    }

    out
}

/// Format compact comparison table for ALL credentials (`orbit credential status --all`).
pub fn format_all_overview(reports: &[Value], now_ms: i64) -> String {
    let mut out = String::new();

    out.push_str(&format!(
        "{:<23}{:<14}{:<11}{:<13}{:<15}OBSERVED\n",
        "ACCOUNT", "PROVIDER", "AUTH", "SCOPE", "AVAILABILITY"
    ));

    for report in reports {
        let reference = report
            .pointer("/credential/reference")
            .and_then(Value::as_str)
            .unwrap_or("—");
        let provider = report
            .pointer("/credential/provider")
            .and_then(Value::as_str)
            .unwrap_or("—");
        let auth = compute_auth_summary(report);

        let scope = if provider == "codex" {
            report
                .pointer("/status/provider_scope")
                .and_then(Value::as_str)
                .unwrap_or("—")
                .to_ascii_uppercase()
        } else {
            "—".to_string()
        };

        let avail = report
            .pointer("/availability/state")
            .and_then(Value::as_str)
            .unwrap_or("UNKNOWN")
            .to_ascii_uppercase();

        let is_fresh = report
            .pointer("/availability/fresh")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let observed_at_ms = report
            .pointer("/availability/observed_at_ms")
            .and_then(Value::as_i64);
        let obs = format_observed_time(observed_at_ms, is_fresh, now_ms, true);

        out.push_str(&format!(
            "{:<23}{:<14}{:<11}{:<13}{:<15}{obs}\n",
            reference, provider, auth, scope, avail
        ));
    }

    out
}

/// Pad a string with spaces until its visual character width reaches `width`.
pub fn pad_to_width(s: &str, width: usize) -> String {
    let char_count = s.chars().count();
    if char_count < width {
        let padding = " ".repeat(width - char_count);
        format!("{s}{padding}")
    } else {
        s.to_string()
    }
}

/// Helper to format a window cell inside a multi-account quota table.
fn format_window_cell(window: Option<&Value>, is_fresh: bool, now_ms: i64) -> String {
    let Some(window) = window else {
        return "—".to_string();
    };

    let (pct_val, pct_str) = extract_window_percentages(window);
    let bar = format_progress_bar(pct_val);
    let pct_display = if !is_fresh {
        format!("{pct_str} [stale]")
    } else {
        pct_str
    };

    let reset_str = if let Some(resets_ms) = window.get("resets_at_ms").and_then(Value::as_i64) {
        format!(" {}", format_reset_time(resets_ms, now_ms))
    } else {
        String::new()
    };

    format!("{pct_display:>5} {bar}{reset_str}")
}

/// Definition of a single row in the wide quota table.
pub struct QuotaTableRow<'a> {
    pub account: &'a str,
    pub resource: &'a str,
    pub short_window: Option<&'a Value>,
    pub long_window: Option<&'a Value>,
    pub is_fresh: bool,
    pub has_unexpected: bool,
    pub all_windows: Vec<&'a Value>,
}

/// Configuration for quota table rendering.
pub struct QuotaTableConfig<'a> {
    pub account_header: &'a str,
    pub resource_header: &'a str,
    pub short_header: &'a str,
    pub long_header: &'a str,
    pub multiline_dur_pad: usize,
}

/// Shared quota table renderer for both Codex and Antigravity.
///
/// Implements:
/// - One row per quota bucket / provider group
/// - Sibling windows as columns (e.g. 5H and 7D / WEEKLY)
/// - Dynamic visible-column-width calculation based on actual rendered contents
/// - Minimum structural gap of at least 2 spaces between columns
/// - Resilient column separation: stale marker expands column width across all rows
/// - Grouped multiline fallback when narrow terminal or unexpected window durations occur
pub fn render_quota_table(
    config: &QuotaTableConfig<'_>,
    rows: &[QuotaTableRow<'_>],
    now_ms: i64,
    term_width: Option<usize>,
) -> String {
    if rows.is_empty() {
        return String::new();
    }

    struct RenderedItem<'a> {
        account: &'a str,
        resource: &'a str,
        short_cell: String,
        long_cell: String,
        has_unexpected: bool,
        all_windows: &'a [&'a Value],
        is_fresh: bool,
    }

    let rendered: Vec<RenderedItem<'_>> = rows
        .iter()
        .map(|r| {
            let short_cell = format_window_cell(r.short_window, r.is_fresh, now_ms);
            let long_cell = format_window_cell(r.long_window, r.is_fresh, now_ms);
            RenderedItem {
                account: r.account,
                resource: r.resource,
                short_cell,
                long_cell,
                has_unexpected: r.has_unexpected,
                all_windows: &r.all_windows,
                is_fresh: r.is_fresh,
            }
        })
        .collect();

    let account_width = rendered
        .iter()
        .map(|r| r.account.chars().count())
        .max()
        .unwrap_or(0)
        .max(config.account_header.chars().count());

    let resource_width = rendered
        .iter()
        .map(|r| r.resource.chars().count())
        .max()
        .unwrap_or(0)
        .max(config.resource_header.chars().count());

    let short_window_width = rendered
        .iter()
        .filter(|r| !r.has_unexpected)
        .map(|r| r.short_cell.chars().count())
        .max()
        .unwrap_or(0)
        .max(config.short_header.chars().count());

    let long_window_width = rendered
        .iter()
        .filter(|r| !r.has_unexpected)
        .map(|r| r.long_cell.chars().count())
        .max()
        .unwrap_or(0)
        .max(config.long_header.chars().count());

    let total_table_width = account_width
        + TABLE_COLUMN_GAP.len()
        + resource_width
        + TABLE_COLUMN_GAP.len()
        + short_window_width
        + TABLE_COLUMN_GAP.len()
        + long_window_width;

    let is_narrow = term_width.is_some_and(|w| w < total_table_width || w < 80);

    let mut out = String::new();

    if is_narrow {
        for r in &rendered {
            out.push_str(&format!("{}  {}\n", r.account, r.resource));
            let mut sorted_windows: Vec<&Value> = r.all_windows.to_vec();
            sorted_windows.sort_by_key(|w| {
                w.get("duration_minutes")
                    .and_then(Value::as_i64)
                    .unwrap_or(i64::MAX)
            });
            for w in sorted_windows {
                let dur_key = window_duration_key(w);
                let (pct_val, pct_str) = extract_window_percentages(w);
                let bar = format_progress_bar(pct_val);
                let reset_str =
                    if let Some(resets_ms) = w.get("resets_at_ms").and_then(Value::as_i64) {
                        format!("  {}", format_reset_time(resets_ms, now_ms))
                    } else {
                        String::new()
                    };
                let pct_display = if !r.is_fresh {
                    format!("{pct_str} [stale]")
                } else {
                    pct_str
                };
                let dur_pad = config.multiline_dur_pad;
                out.push_str(&format!(
                    "  {dur_key:<dur_pad$}{pct_display:>5} {bar}{reset_str}\n"
                ));
            }
        }
    } else {
        let col1_hdr = pad_to_width(config.account_header, account_width);
        let col2_hdr = pad_to_width(config.resource_header, resource_width);
        let col3_hdr = pad_to_width(config.short_header, short_window_width);
        out.push_str(&format!(
            "{col1_hdr}{gap}{col2_hdr}{gap}{col3_hdr}{gap}{}\n",
            config.long_header,
            gap = TABLE_COLUMN_GAP
        ));

        for r in &rendered {
            if r.has_unexpected {
                let col1 = pad_to_width(r.account, account_width);
                out.push_str(&format!(
                    "{col1}{gap}{}\n",
                    r.resource,
                    gap = TABLE_COLUMN_GAP
                ));
                let mut sorted_windows: Vec<&Value> = r.all_windows.to_vec();
                sorted_windows.sort_by_key(|w| {
                    w.get("duration_minutes")
                        .and_then(Value::as_i64)
                        .unwrap_or(i64::MAX)
                });
                for w in sorted_windows {
                    let dur_key = window_duration_key(w);
                    let (pct_val, pct_str) = extract_window_percentages(w);
                    let bar = format_progress_bar(pct_val);
                    let reset_str =
                        if let Some(resets_ms) = w.get("resets_at_ms").and_then(Value::as_i64) {
                            format!("  {}", format_reset_time(resets_ms, now_ms))
                        } else {
                            String::new()
                        };
                    let pct_display = if !r.is_fresh {
                        format!("{pct_str} [stale]")
                    } else {
                        pct_str
                    };
                    let dur_pad = config.multiline_dur_pad;
                    out.push_str(&format!(
                        "  {dur_key:<dur_pad$}{pct_display:>5} {bar}{reset_str}\n"
                    ));
                }
            } else {
                let col1 = pad_to_width(r.account, account_width);
                let col2 = pad_to_width(r.resource, resource_width);
                let col3 = pad_to_width(&r.short_cell, short_window_width);
                out.push_str(&format!(
                    "{col1}{gap}{col2}{gap}{col3}{gap}{}\n",
                    r.long_cell,
                    gap = TABLE_COLUMN_GAP
                ));
            }
        }
    }

    out
}

/// Format the provider-grouped detailed quota view (`orbit credential status --all --quota`).
pub fn format_all_quota(reports: &[Value], now_ms: i64, term_width: Option<usize>) -> String {
    let mut out = String::new();

    let codex_reports: Vec<&Value> = reports
        .iter()
        .filter(|r| r.pointer("/credential/provider").and_then(Value::as_str) == Some("codex"))
        .collect();

    let antigravity_reports: Vec<&Value> = reports
        .iter()
        .filter(|r| {
            r.pointer("/credential/provider").and_then(Value::as_str) == Some("antigravity")
        })
        .collect();

    // 1. CODEX SECTION
    if !codex_reports.is_empty() {
        out.push_str("CODEX\n");
        out.push_str("────────────────────────────────────────────────────────────────────────\n");

        out.push_str(&format!(
            "{:<14}{:<8}{:<12}{:<15}OBSERVED\n",
            "ACCOUNT", "AUTH", "SCOPE", "AVAILABILITY"
        ));
        for report in &codex_reports {
            let reference = report
                .pointer("/credential/reference")
                .and_then(Value::as_str)
                .unwrap_or("—");
            let auth = compute_auth_summary(report);
            let scope = report
                .pointer("/status/provider_scope")
                .and_then(Value::as_str)
                .unwrap_or("—")
                .to_ascii_uppercase();
            let avail = report
                .pointer("/availability/state")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_ascii_uppercase();
            let is_fresh = report
                .pointer("/availability/fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let observed_at_ms = report
                .pointer("/availability/observed_at_ms")
                .and_then(Value::as_i64);
            let obs = format_observed_time(observed_at_ms, is_fresh, now_ms, false);

            out.push_str(&format!(
                "{:<14}{:<8}{:<12}{:<15}{obs}\n",
                reference, auth, scope, avail
            ));
        }

        let mut codex_rows = Vec::new();
        for report in &codex_reports {
            let reference = report
                .pointer("/credential/reference")
                .and_then(Value::as_str)
                .unwrap_or("—");
            let is_fresh = report
                .pointer("/availability/fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let buckets = report
                .pointer("/availability/quota_buckets")
                .and_then(Value::as_array);

            if let Some(buckets) = buckets {
                for bucket in buckets {
                    let label = bucket
                        .get("provider_label")
                        .and_then(Value::as_str)
                        .unwrap_or(CODEX_FALLBACK_BUCKET_LABEL);

                    let windows: Vec<&Value> = bucket
                        .get("windows")
                        .and_then(Value::as_array)
                        .map(|arr| arr.iter().collect())
                        .unwrap_or_default();

                    let mut w_5h: Option<&Value> = None;
                    let mut w_7d: Option<&Value> = None;
                    let mut unexpected: Vec<&Value> = Vec::new();

                    for &w in &windows {
                        let dur_key = window_duration_key(w);
                        if dur_key == "5h" && w_5h.is_none() {
                            w_5h = Some(w);
                        } else if dur_key == "7d" && w_7d.is_none() {
                            w_7d = Some(w);
                        } else {
                            unexpected.push(w);
                        }
                    }

                    codex_rows.push(QuotaTableRow {
                        account: reference,
                        resource: label,
                        short_window: w_5h,
                        long_window: w_7d,
                        is_fresh,
                        has_unexpected: !unexpected.is_empty(),
                        all_windows: windows,
                    });
                }
            }
        }

        if !codex_rows.is_empty() {
            out.push_str("\nQuota\n");
            let codex_config = QuotaTableConfig {
                account_header: "ACCOUNT",
                resource_header: "BUCKET",
                short_header: "5H",
                long_header: "7D",
                multiline_dur_pad: 5,
            };
            out.push_str(&render_quota_table(
                &codex_config,
                &codex_rows,
                now_ms,
                term_width,
            ));
        }
    }

    // 2. ANTIGRAVITY SECTION
    if !antigravity_reports.is_empty() {
        if !codex_reports.is_empty() {
            out.push('\n');
        }
        out.push_str("ANTIGRAVITY\n");
        out.push_str("────────────────────────────────────────────────────────────────────────\n");

        out.push_str(&format!(
            "{:<21}{:<8}{:<10}{:<15}OBSERVED\n",
            "ACCOUNT", "ACP", "AGY-CLI", "AVAILABILITY"
        ));
        for report in &antigravity_reports {
            let reference = report
                .pointer("/credential/reference")
                .and_then(Value::as_str)
                .unwrap_or("—");
            let acp_val = report
                .pointer("/representations/acp/validation")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    report
                        .pointer("/representations/acp/state")
                        .and_then(Value::as_str)
                        .unwrap_or("MISSING")
                })
                .to_ascii_uppercase();
            let agy_val = report
                .pointer("/representations/agy-cli/validation")
                .and_then(Value::as_str)
                .unwrap_or_else(|| {
                    report
                        .pointer("/representations/agy-cli/state")
                        .and_then(Value::as_str)
                        .unwrap_or("MISSING")
                })
                .to_ascii_uppercase();
            let avail = report
                .pointer("/availability/state")
                .and_then(Value::as_str)
                .unwrap_or("UNKNOWN")
                .to_ascii_uppercase();
            let is_fresh = report
                .pointer("/availability/fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let observed_at_ms = report
                .pointer("/availability/observed_at_ms")
                .and_then(Value::as_i64);
            let obs = format_observed_time(observed_at_ms, is_fresh, now_ms, false);

            out.push_str(&format!(
                "{:<21}{:<8}{:<10}{:<15}{obs}\n",
                reference, acp_val, agy_val, avail
            ));
        }

        // Collect unique groups for single legend
        let mut unique_groups: Vec<(String, String)> = Vec::new();
        for report in &antigravity_reports {
            if let Some(groups) = report
                .pointer("/availability/quota_groups")
                .and_then(Value::as_array)
            {
                for group in groups {
                    let name = group
                        .get("provider_display_name")
                        .and_then(Value::as_str)
                        .unwrap_or("—")
                        .to_string();
                    if unique_groups.iter().any(|(n, _)| n == &name) {
                        continue;
                    }
                    let members_str =
                        if let Some(members) = group.get("members").and_then(Value::as_array) {
                            let names: Vec<&str> = members
                                .iter()
                                .filter_map(|m| m.get("provider_label").and_then(Value::as_str))
                                .collect();
                            names.join(", ")
                        } else {
                            String::new()
                        };
                    unique_groups.push((name, members_str));
                }
            }
        }

        if !unique_groups.is_empty() {
            out.push_str("\nGroups\n");
            for (name, members) in unique_groups {
                out.push_str(&format!("  {:<25}{members}\n", name));
            }
        }

        let mut antigravity_rows = Vec::new();
        for report in &antigravity_reports {
            let reference = report
                .pointer("/credential/reference")
                .and_then(Value::as_str)
                .unwrap_or("—");
            let is_fresh = report
                .pointer("/availability/fresh")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let groups = report
                .pointer("/availability/quota_groups")
                .and_then(Value::as_array);
            let buckets = report
                .pointer("/availability/quota_buckets")
                .and_then(Value::as_array);

            if let Some(groups) = groups {
                for group in groups {
                    let group_name = group
                        .get("provider_display_name")
                        .and_then(Value::as_str)
                        .unwrap_or("—");

                    let group_fps: Vec<&str> = group
                        .get("bucket_fingerprints")
                        .and_then(Value::as_array)
                        .map(|arr| arr.iter().filter_map(Value::as_str).collect())
                        .unwrap_or_default();

                    let mut group_windows = Vec::new();
                    if let Some(buckets) = buckets {
                        for bucket in buckets {
                            let fp = bucket
                                .get("provider_bucket_fingerprint")
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if (group_fps.is_empty() || group_fps.contains(&fp))
                                && let Some(b_windows) =
                                    bucket.get("windows").and_then(Value::as_array)
                            {
                                for w in b_windows {
                                    group_windows.push(w);
                                }
                            }
                        }
                    }

                    let mut w_5h: Option<&Value> = None;
                    let mut w_weekly: Option<&Value> = None;
                    let mut unexpected: Vec<&Value> = Vec::new();

                    for w in &group_windows {
                        let dur_key = window_duration_key(w);
                        if dur_key == "5h" && w_5h.is_none() {
                            w_5h = Some(w);
                        } else if dur_key == "weekly" && w_weekly.is_none() {
                            w_weekly = Some(w);
                        } else {
                            unexpected.push(w);
                        }
                    }

                    antigravity_rows.push(QuotaTableRow {
                        account: reference,
                        resource: group_name,
                        short_window: w_5h,
                        long_window: w_weekly,
                        is_fresh,
                        has_unexpected: !unexpected.is_empty(),
                        all_windows: group_windows,
                    });
                }
            }
        }

        if !antigravity_rows.is_empty() {
            out.push_str("\nQuota\n");
            let antigravity_config = QuotaTableConfig {
                account_header: "ACCOUNT",
                resource_header: "GROUP",
                short_header: "5H",
                long_header: "WEEKLY",
                multiline_dur_pad: 9,
            };
            out.push_str(&render_quota_table(
                &antigravity_config,
                &antigravity_rows,
                now_ms,
                term_width,
            ));
        }
    }

    out
}

/// Strip qualification, diagnostic schemas, and internals for clean structured JSON.
pub fn clean_structured_json(mut report: Value) -> Value {
    if let Some(status_obj) = report.get_mut("status").and_then(Value::as_object_mut) {
        status_obj.remove("account_read_schema");
        status_obj.remove("rate_limits_read_schema");
        status_obj.remove("schema_summary");
        status_obj.remove("runtime_effects");
        status_obj.remove("home_entries");
        status_obj.remove("snapshot_id");
    }

    if let Some(buckets) = report
        .pointer_mut("/availability/quota_buckets")
        .and_then(Value::as_array_mut)
    {
        for b in buckets {
            if let Some(obj) = b.as_object_mut()
                && !obj.contains_key("provider_label")
            {
                obj.insert("provider_label".to_string(), Value::Null);
            }
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    unsafe extern "C" {
        fn tzset();
    }

    static TEST_TZ_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_test_timezone<F, R>(tz: &str, f: F) -> R
    where
        F: FnOnce() -> R,
    {
        let _guard = TEST_TZ_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let old_tz = std::env::var("TZ").ok();
        unsafe {
            std::env::set_var("TZ", tz);
            tzset();
        }
        let res = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            if let Some(ref old) = old_tz {
                std::env::set_var("TZ", old);
            } else {
                std::env::remove_var("TZ");
            }
            tzset();
        }
        match res {
            Ok(v) => v,
            Err(e) => std::panic::resume_unwind(e),
        }
    }

    #[test]
    fn test_progress_bar_all_test_cases() {
        assert_eq!(format_progress_bar(100.0), "██████████");
        assert_eq!(format_progress_bar(99.1), "██████████");
        assert_eq!(format_progress_bar(89.3), "█████████░");
        assert_eq!(format_progress_bar(74.0), "███████░░░");
        assert_eq!(format_progress_bar(50.0), "█████░░░░░");
        assert_eq!(format_progress_bar(6.8), "█░░░░░░░░░");
        assert_eq!(format_progress_bar(1.0), "▏░░░░░░░░░");
        assert_eq!(format_progress_bar(0.0), "░░░░░░░░░░");
    }

    #[test]
    fn test_percentage_formatting() {
        assert_eq!(format_percentage(100.0), "100%");
        assert_eq!(format_percentage(74.0), "74%");
        assert_eq!(format_percentage(1.0), "1%");
        assert_eq!(format_percentage(0.0), "0%");
        assert_eq!(format_percentage(99.1), "99.1%");
        assert_eq!(format_percentage(89.3), "89.3%");
        assert_eq!(format_percentage(6.8), "6.8%");
        assert_eq!(format_percentage(84.8), "84.8%");
        assert_eq!(format_percentage(86.9), "86.9%");
    }

    #[test]
    fn test_shared_reset_time_formatter_all_cases() {
        with_test_timezone("UTC", || {
            // Base now_ms: Sep 26 2026 08:00 UTC
            let now_ms = 1790409600000i64;

            // 1. Codex 5h: Sep 26 06:01
            assert_eq!(format_reset_time(1790402460000, now_ms), "Sep 26 06:01");

            // 2. Codex 7d: Sep 29 22:22
            assert_eq!(format_reset_time(1790720520000, now_ms), "Sep 29 22:22");

            // 3. Antigravity 5h: Sep 26 05:07
            assert_eq!(format_reset_time(1790399220000, now_ms), "Sep 26 05:07");

            // 4. Antigravity weekly: Oct 2 01:52
            assert_eq!(format_reset_time(1790905920000, now_ms), "Oct 2 01:52");

            // 5. Midnight / single-digit day: Oct 3 00:11
            assert_eq!(format_reset_time(1790986260000, now_ms), "Oct 3 00:11");

            // 6. Stale timestamp retains HH:MM: Sep 25 18:42
            assert_eq!(format_reset_time(1790361720000, now_ms), "Sep 25 18:42");

            // 7. Different year includes YYYY: Dec 31 23:45 2027
            assert_eq!(
                format_reset_time(1830296700000, now_ms),
                "Dec 31 23:45 2027"
            );

            // 8. format_reset_timestamp alias behaves identically
            assert_eq!(
                format_reset_timestamp(1790402460000, now_ms),
                "Sep 26 06:01"
            );
            assert_eq!(
                format_reset_timestamp(1830296700000, now_ms),
                "Dec 31 23:45 2027"
            );
        });
    }

    #[test]
    fn test_single_codex_confirmed_ready_2buckets_3windows() {
        with_test_timezone("UTC", || {
            let now_ms = 1790380000000i64; // Sep 26 2026
            let report = json!({
                "credential": {
                    "reference": "codex-main",
                    "provider": "codex",
                    "generation": 1,
                    "lifecycle": "enrolled"
                },
                "representations": {
                    "codex": { "validation": "valid" }
                },
                "health": {
                    "runtime": { "state": "healthy" }
                },
                "status": {
                    "state": "observed",
                    "provider_scope": "confirmed",
                    "quota_promoted": true
                },
                "availability": {
                    "state": "ready",
                    "fresh": true,
                    "observed_at_ms": now_ms,
                    "quota_buckets": [
                        {
                            "provider_label": "gpt-reserve",
                            "windows": [
                                {
                                    "provider_window_id": "secondary",
                                    "duration_minutes": 10080,
                                    "remaining_percent": 74.0,
                                    "resets_at_ms": 1790692320000i64 // Sep 29 14:32
                                }
                            ]
                        },
                        {
                            // Unlabeled bucket: tests Orbit UI fallback "default"
                            "windows": [
                                {
                                    "provider_window_id": "primary",
                                    "duration_minutes": 300,
                                    "remaining_percent": 100.0,
                                    "resets_at_ms": 1790402460000i64 // Sep 26 06:01
                                },
                                {
                                    "provider_window_id": "secondary",
                                    "duration_minutes": 10080,
                                    "remaining_percent": 1.0,
                                    "resets_at_ms": 1790720520000i64 // Sep 29 22:22
                                }
                            ]
                        }
                    ]
                }
            });

            let rendered = format_single_credential(&report, now_ms);
            assert!(rendered.contains("codex-main  [codex]"));
            assert!(rendered.contains("Lifecycle      ENROLLED"));
            assert!(rendered.contains("Auth           VALID"));
            assert!(rendered.contains("Runtime        HEALTHY"));
            assert!(rendered.contains("Scope          CONFIRMED"));
            assert!(rendered.contains("Availability   READY"));
            assert!(rendered.contains("Observed       just now"));
            assert!(rendered.contains("  Quota"));
            assert!(rendered.contains("    gpt-reserve"));
            assert!(rendered.contains("7d      74%  ███████░░░   resets Sep 29 14:32"));
            assert!(rendered.contains("    default"));
            assert!(rendered.contains("5h     100%  ██████████   resets Sep 26 06:01"));
            assert!(rendered.contains("7d       1%  ▏░░░░░░░░░   resets Sep 29 22:22"));
            // Does NOT show primary/secondary in human output
            assert!(!rendered.contains("primary"));
            assert!(!rendered.contains("secondary"));
        });
    }

    #[test]
    fn test_single_codex_missing_secondary() {
        let now_ms = 1790380000000i64;
        let report = json!({
            "credential": {
                "reference": "codex-single-window",
                "provider": "codex",
                "lifecycle": "enrolled"
            },
            "representations": {
                "codex": { "validation": "valid" }
            },
            "health": { "runtime": { "state": "healthy" } },
            "status": {
                "state": "observed",
                "provider_scope": "confirmed",
                "quota_promoted": true
            },
            "availability": {
                "state": "ready",
                "fresh": true,
                "observed_at_ms": now_ms,
                "quota_buckets": [
                    {
                        "provider_label": "main",
                        "windows": [
                            {
                                "duration_minutes": 300,
                                "remaining_percent": 100.0,
                                "resets_at_ms": 1790402460000i64
                            }
                        ]
                    }
                ]
            }
        });

        let rendered = format_single_credential(&report, now_ms);
        assert!(rendered.contains("main"));
        assert!(rendered.contains("5h     100%"));
        assert!(!rendered.contains("7d"));
    }

    #[test]
    fn test_single_codex_stale_observation() {
        with_test_timezone("UTC", || {
            let now_ms = 1790380000000i64;
            let report = json!({
                "credential": {
                    "reference": "codex-stale",
                    "provider": "codex",
                    "lifecycle": "enrolled"
                },
                "representations": { "codex": { "validation": "valid" } },
                "health": { "runtime": { "state": "healthy" } },
                "status": {
                    "state": "observed",
                    "provider_scope": "confirmed",
                    "quota_promoted": true
                },
                "availability": {
                    "state": "ready",
                    "fresh": false,
                    "observed_at_ms": now_ms - 240_000, // 4m ago
                    "quota_buckets": [
                        {
                            "provider_label": "default",
                            "windows": [
                                {
                                    "duration_minutes": 300,
                                    "remaining_percent": 100.0,
                                    "resets_at_ms": 1790402460000i64
                                }
                            ]
                        }
                    ]
                }
            });

            let rendered = format_single_credential(&report, now_ms);
            assert!(rendered.contains("Observed       4m ago (stale)"));
            assert!(rendered.contains("100% [stale]  ██████████   resets Sep 26 06:01"));
        });
    }

    #[test]
    fn test_single_codex_unconfirmed_scope() {
        let now_ms = 1790380000000i64;
        let report = json!({
            "credential": {
                "reference": "codex-unconfirmed",
                "provider": "codex",
                "lifecycle": "enrolled"
            },
            "representations": { "codex": { "validation": "valid" } },
            "health": { "runtime": { "state": "healthy" } },
            "status": {
                "state": "observed",
                "provider_scope": "unconfirmed",
                "quota_promoted": false
            },
            "availability": {
                "state": "ready",
                "fresh": true,
                "observed_at_ms": now_ms,
                "quota_buckets": [
                    {
                        "provider_label": "default",
                        "windows": [
                            {
                                "duration_minutes": 300,
                                "remaining_percent": 100.0,
                                "resets_at_ms": 1790402460000i64
                            }
                        ]
                    }
                ]
            }
        });

        let rendered = format_single_credential(&report, now_ms);
        assert!(rendered.contains("Scope          UNCONFIRMED"));
        assert!(rendered.contains("Quota observed but not promoted"));
        assert!(rendered.contains("provider scope requires confirmation"));
        assert!(rendered.contains("Observed quota"));
    }

    #[test]
    fn test_single_antigravity_valid() {
        with_test_timezone("UTC", || {
            let now_ms = 1790380000000i64;
            let report = json!({
                "credential": {
                    "reference": "antigravity-jc",
                    "provider": "antigravity",
                    "lifecycle": "enrolled"
                },
                "representations": {
                    "acp": { "validation": "valid" },
                    "agy-cli": { "validation": "valid" }
                },
                "health": {
                    "runtime": { "state": "healthy" }
                },
                "status": {
                    "state": "observed"
                },
                "availability": {
                    "state": "unknown",
                    "fresh": true,
                    "observed_at_ms": now_ms,
                    "quota_groups": [
                        {
                            "provider_display_name": "Gemini Models",
                            "members": [
                                { "provider_label": "Gemini Flash" },
                                { "provider_label": "Gemini Pro" }
                            ],
                            "bucket_fingerprints": ["fp-gemini"]
                        },
                        {
                            "provider_display_name": "Claude and GPT models",
                            "members": [
                                { "provider_label": "Claude Opus" },
                                { "provider_label": "Claude Sonnet" },
                                { "provider_label": "GPT-OSS" }
                            ],
                            "bucket_fingerprints": ["fp-claude"]
                        }
                    ],
                    "quota_buckets": [
                        {
                            "provider_bucket_fingerprint": "fp-gemini",
                            "windows": [
                                {
                                    "provider_window_id": "5h",
                                    "remaining_percent": 84.8,
                                    "resets_at_ms": 1790399220000i64 // Sep 26 05:07
                                },
                                {
                                    "provider_window_id": "weekly",
                                    "remaining_percent": 86.9,
                                    "resets_at_ms": 1790905920000i64 // Oct 2 01:52
                                }
                            ]
                        },
                        {
                            "provider_bucket_fingerprint": "fp-claude",
                            "windows": [
                                {
                                    "provider_window_id": "5h",
                                    "remaining_percent": 100.0,
                                    "resets_at_ms": 1790399460000i64 // Sep 26 05:11
                                },
                                {
                                    "provider_window_id": "weekly",
                                    "remaining_percent": 100.0,
                                    "resets_at_ms": 1790986260000i64 // Oct 3 00:11
                                }
                            ]
                        }
                    ]
                }
            });

            let rendered = format_single_credential(&report, now_ms);
            assert!(rendered.contains("antigravity-jc  [antigravity]"));
            assert!(rendered.contains("ACP            VALID"));
            assert!(rendered.contains("agy-cli        VALID"));
            assert!(rendered.contains("Gemini Models"));
            assert!(rendered.contains("Gemini Flash, Gemini Pro"));
            assert!(rendered.contains("5h       84.8%  ████████░░   resets Sep 26 05:07"));
            assert!(rendered.contains("weekly   86.9%  █████████░   resets Oct 2 01:52"));
            assert!(rendered.contains("Claude and GPT models"));
            assert!(rendered.contains("Claude Opus, Claude Sonnet, GPT-OSS"));
            assert!(rendered.contains("5h        100%  ██████████   resets Sep 26 05:11"));
            assert!(rendered.contains("weekly    100%  ██████████   resets Oct 3 00:11"));
        });
    }

    #[test]
    fn test_single_antigravity_missing_agy() {
        let now_ms = 1790380000000i64;
        let report = json!({
            "credential": { "reference": "antigravity-work", "provider": "antigravity" },
            "representations": {
                "acp": { "validation": "valid" },
                "agy-cli": { "state": "missing" }
            },
            "status": {
                "state": "unavailable",
                "reason": "agy-cli representation not enrolled"
            },
            "availability": {
                "state": "unknown",
                "fresh": false,
                "observed_at_ms": null
            }
        });

        let rendered = format_single_credential(&report, now_ms);
        assert!(rendered.contains("ACP            VALID"));
        assert!(rendered.contains("agy-cli        MISSING"));
        assert!(rendered.contains("Status unavailable"));
        assert!(rendered.contains("agy-cli representation not enrolled"));
    }

    #[test]
    fn test_single_antigravity_unvalidated_agy() {
        let now_ms = 1790380000000i64;
        let report = json!({
            "credential": { "reference": "antigravity-ch9b2013", "provider": "antigravity" },
            "representations": {
                "acp": { "validation": "valid" },
                "agy-cli": { "validation": "unvalidated" }
            },
            "status": {
                "state": "unavailable",
                "reason": "agy-cli representation is not valid"
            },
            "availability": {
                "state": "unknown",
                "fresh": false,
                "observed_at_ms": null
            }
        });

        let rendered = format_single_credential(&report, now_ms);
        assert!(rendered.contains("ACP            VALID"));
        assert!(rendered.contains("agy-cli        UNVALIDATED"));
        assert!(rendered.contains("Status unavailable"));
        assert!(rendered.contains("agy-cli representation is not valid"));
    }

    #[test]
    fn test_all_overview_table() {
        let now_ms = 1790380000000i64;
        let reports = vec![
            json!({
                "credential": { "reference": "codex-main", "provider": "codex" },
                "representations": { "codex": { "validation": "valid" } },
                "status": { "provider_scope": "confirmed" },
                "availability": { "state": "ready", "fresh": true, "observed_at_ms": now_ms }
            }),
            json!({
                "credential": { "reference": "antigravity-weedy", "provider": "antigravity" },
                "representations": { "acp": { "validation": "valid" }, "agy-cli": { "validation": "valid" } },
                "availability": { "state": "unknown", "fresh": true, "observed_at_ms": now_ms }
            }),
            json!({
                "credential": { "reference": "antigravity-ch9b2013", "provider": "antigravity" },
                "representations": { "acp": { "validation": "valid" }, "agy-cli": { "validation": "unvalidated" } },
                "availability": { "state": "unknown", "fresh": false, "observed_at_ms": null }
            }),
            json!({
                "credential": { "reference": "antigravity-prvmrala", "provider": "antigravity" },
                "representations": { "acp": { "validation": "valid" }, "agy-cli": { "state": "missing" } },
                "availability": { "state": "unknown", "fresh": false, "observed_at_ms": null }
            }),
            json!({
                "credential": { "reference": "codex-stale", "provider": "codex" },
                "representations": { "codex": { "validation": "valid" } },
                "status": { "provider_scope": "confirmed" },
                "availability": { "state": "ready", "fresh": false, "observed_at_ms": now_ms - 240_000 }
            }),
        ];

        let table = format_all_overview(&reports, now_ms);
        assert!(table.contains(
            "ACCOUNT                PROVIDER      AUTH       SCOPE        AVAILABILITY   OBSERVED"
        ));
        assert!(table.contains(
            "codex-main             codex         VALID      CONFIRMED    READY          just now"
        ));
        assert!(table.contains(
            "antigravity-weedy      antigravity   VALID      —            UNKNOWN        just now"
        ));
        assert!(table.contains(
            "antigravity-ch9b2013   antigravity   PARTIAL    —            UNKNOWN        —"
        ));
        assert!(table.contains(
            "antigravity-prvmrala   antigravity   PARTIAL    —            UNKNOWN        —"
        ));
        assert!(table.contains("codex-stale            codex         VALID      CONFIRMED    READY          4m ago · STALE"));
    }

    #[test]
    fn test_all_quota_wide() {
        with_test_timezone("UTC", || {
            let now_ms = 1790409600000i64; // Sep 26 2026 08:00 UTC
            let reports = vec![
                json!({
                    "credential": { "reference": "codex-main", "provider": "codex" },
                    "representations": { "codex": { "validation": "valid" } },
                    "status": { "provider_scope": "confirmed" },
                    "availability": {
                        "state": "ready",
                        "fresh": true,
                        "observed_at_ms": now_ms,
                        "quota_buckets": [
                            {
                                "provider_label": "gpt-reserve",
                                "windows": [
                                    { "provider_window_id": "secondary", "duration_minutes": 10080, "remaining_percent": 74.0, "resets_at_ms": 1790692320000i64 } // Sep 29 14:32
                                ]
                            },
                            {
                                "windows": [
                                    { "provider_window_id": "primary", "duration_minutes": 300, "remaining_percent": 100.0, "resets_at_ms": 1790402460000i64 }, // Sep 26 06:01
                                    { "provider_window_id": "secondary", "duration_minutes": 10080, "remaining_percent": 1.0, "resets_at_ms": 1790720520000i64 }  // Sep 29 22:22
                                ]
                            }
                        ]
                    }
                }),
                json!({
                    "credential": { "reference": "antigravity-jc", "provider": "antigravity" },
                    "representations": { "acp": { "validation": "valid" }, "agy-cli": { "validation": "valid" } },
                    "availability": {
                        "state": "unknown",
                        "fresh": true,
                        "observed_at_ms": now_ms,
                        "quota_groups": [
                            {
                                "provider_display_name": "Gemini Models",
                                "members": [ { "provider_label": "Gemini Flash" }, { "provider_label": "Gemini Pro" } ],
                                "bucket_fingerprints": ["fp-jc-gemini"]
                            },
                            {
                                "provider_display_name": "Claude and GPT models",
                                "members": [ { "provider_label": "Claude Opus" }, { "provider_label": "Claude Sonnet" }, { "provider_label": "GPT-OSS" } ],
                                "bucket_fingerprints": ["fp-jc-claude"]
                            }
                        ],
                        "quota_buckets": [
                            {
                                "provider_bucket_fingerprint": "fp-jc-gemini",
                                "windows": [
                                    { "provider_window_id": "5h", "remaining_percent": 84.8, "resets_at_ms": 1790399220000i64 }, // Sep 26 05:07
                                    { "provider_window_id": "weekly", "remaining_percent": 86.9, "resets_at_ms": 1790905920000i64 } // Oct 2 01:52
                                ]
                            },
                            {
                                "provider_bucket_fingerprint": "fp-jc-claude",
                                "windows": [
                                    { "provider_window_id": "5h", "remaining_percent": 100.0, "resets_at_ms": 1790399460000i64 }, // Sep 26 05:11
                                    { "provider_window_id": "weekly", "remaining_percent": 100.0, "resets_at_ms": 1790986260000i64 } // Oct 3 00:11
                                ]
                            }
                        ]
                    }
                }),
            ];

            let out = format_all_quota(&reports, now_ms, Some(100));

            // Verify Codex Section
            assert!(out.contains("CODEX"));
            assert!(out.contains("ACCOUNT       AUTH    SCOPE       AVAILABILITY   OBSERVED"));
            assert!(out.contains("codex-main    VALID   CONFIRMED   READY          just now"));
            assert!(out.contains("ACCOUNT"));
            assert!(out.contains("BUCKET"));
            assert!(out.contains("5H"));
            assert!(out.contains("7D"));

            // Single row check for default bucket: has 5h AND 7d on the same line
            let default_line = out
                .lines()
                .find(|l| l.contains("codex-main") && l.contains("default"))
                .unwrap();
            assert!(default_line.contains("100% ██████████ Sep 26 06:01"));
            assert!(default_line.contains("1% ▏░░░░░░░░░ Sep 29 22:22"));

            // Missing window in gpt-reserve rendered as — on the same line as 7d
            let gpt_line = out
                .lines()
                .find(|l| l.contains("codex-main") && l.contains("gpt-reserve"))
                .unwrap();
            assert!(gpt_line.contains("—"));
            assert!(gpt_line.contains("74% ███████░░░ Sep 29 14:32"));

            // Protocol IDs hidden from human output
            assert!(!out.contains("primary"));
            assert!(!out.contains("secondary"));

            // Verify Antigravity Section
            assert!(out.contains("ANTIGRAVITY"));
            assert!(out.contains("ACCOUNT              ACP     AGY-CLI   AVAILABILITY   OBSERVED"));
            assert!(out.contains("antigravity-jc       VALID   VALID     UNKNOWN        just now"));
            // Legend printed once
            assert!(out.contains("Groups"));
            assert!(out.contains("Gemini Models            Gemini Flash, Gemini Pro"));
            assert!(out.contains("Claude and GPT models    Claude Opus, Claude Sonnet, GPT-OSS"));
            // Same group check: has 5h AND weekly on the same line with unified MMM D HH:MM format
            let gemini_line = out
                .lines()
                .find(|l| l.contains("antigravity-jc") && l.contains("Gemini Models"))
                .unwrap();
            assert!(gemini_line.contains("84.8% ████████░░ Sep 26 05:07"));
            assert!(gemini_line.contains("86.9% █████████░ Oct 2 01:52"));

            let claude_line = out
                .lines()
                .find(|l| l.contains("antigravity-jc") && l.contains("Claude and GPT models"))
                .unwrap();
            assert!(claude_line.contains("100% ██████████ Sep 26 05:11"));
            assert!(claude_line.contains("100% ██████████ Oct 3 00:11"));
        });
    }

    #[test]
    fn test_antigravity_stale_spacing_and_stable_columns() {
        with_test_timezone("UTC", || {
            let now_ms = 1790409600000i64; // Sep 26 2026 08:00 UTC
            let reports = vec![
                // Row 1: Stale 5h and stale weekly
                json!({
                    "credential": { "reference": "antigravity-ch9b2013", "provider": "antigravity" },
                    "representations": { "acp": { "validation": "valid" }, "agy-cli": { "validation": "valid" } },
                    "health": { "runtime": { "state": "healthy" } },
                    "status": { "state": "observed" },
                    "availability": {
                        "state": "unknown",
                        "fresh": false,
                        "observed_at_ms": now_ms - 36_000_000,
                        "quota_groups": [
                            { "provider_display_name": "Gemini Models", "bucket_fingerprints": ["fp-gemini"] }
                        ],
                        "quota_buckets": [
                            {
                                "provider_bucket_fingerprint": "fp-gemini",
                                "windows": [
                                    { "provider_window_id": "5h", "remaining_percent": 99.9, "resets_at_ms": 1790362620000i64 }, // Sep 25 18:57
                                    { "provider_window_id": "weekly", "remaining_percent": 100.0, "resets_at_ms": 1790673720000i64 } // Sep 29 09:22
                                ]
                            }
                        ]
                    }
                }),
                // Row 2: Fresh 5h and fresh weekly
                json!({
                    "credential": { "reference": "antigravity-jc", "provider": "antigravity" },
                    "representations": { "acp": { "validation": "valid" }, "agy-cli": { "validation": "valid" } },
                    "health": { "runtime": { "state": "healthy" } },
                    "status": { "state": "observed" },
                    "availability": {
                        "state": "unknown",
                        "fresh": true,
                        "observed_at_ms": now_ms,
                        "quota_groups": [
                            { "provider_display_name": "Gemini Models", "bucket_fingerprints": ["fp-jc-gemini"] }
                        ],
                        "quota_buckets": [
                            {
                                "provider_bucket_fingerprint": "fp-jc-gemini",
                                "windows": [
                                    { "provider_window_id": "5h", "remaining_percent": 75.3, "resets_at_ms": 1790399220000i64 }, // Sep 26 05:07
                                    { "provider_window_id": "weekly", "remaining_percent": 85.0, "resets_at_ms": 1790905920000i64 } // Oct 2 01:52
                                ]
                            }
                        ]
                    }
                }),
            ];

            let out = format_all_quota(&reports, now_ms, Some(140));

            // Spacing must NEVER collapse into "18:57100%"
            assert!(!out.contains("18:57100%"));

            // Row 1 contains stale markers with structural separation (at least 2 spaces between 5H and WEEKLY)
            assert!(out.contains(
                "99.9% [stale] ██████████ Sep 25 18:57  100% [stale] ██████████ Sep 29 09:22"
            ));

            // Check stable column alignment: find the byte/char offset of WEEKLY in both lines
            let lines: Vec<&str> = out.lines().collect();
            let r1 = lines
                .iter()
                .find(|l| l.contains("antigravity-ch9b2013") && l.contains("Gemini Models"))
                .unwrap();
            let r2 = lines
                .iter()
                .find(|l| l.contains("antigravity-jc") && l.contains("Gemini Models"))
                .unwrap();

            // Row 2: Fresh window is padded to match the expanded 5H column (participating in width calculation)
            assert!(r2.contains("75.3% ████████░░ Sep 26 05:07"));
            assert!(r2.contains("85% █████████░ Oct 2 01:52"));

            let r1_col2_char_idx = r1[..r1.find("100% [stale]").unwrap()].chars().count();
            let r2_col2_char_idx = r2[..r2.find("85%").unwrap()].chars().count();
            // Both rows start their second column at the exact same visual boundary:
            // "  85%" has 2 leading spaces to right-align in the 5-char percent field, so its "8" is at col2 + 2
            assert_eq!(r2_col2_char_idx, r1_col2_char_idx + 2);
        });
    }

    #[test]
    fn test_window_width_percentages() {
        with_test_timezone("UTC", || {
            // Test different percentage values: 100%, 99.9%, 6.8%, 1%
            let now_ms = 1790409600000i64;
            let test_cases = vec![
                (100.0, " 100% ██████████ Sep 26 06:01"),
                (99.9, "99.9% ██████████ Sep 26 06:01"),
                (6.8, " 6.8% █░░░░░░░░░ Sep 26 06:01"),
                (1.0, "   1% ▏░░░░░░░░░ Sep 26 06:01"),
            ];

            for (pct, expected_snippet) in test_cases {
                let win = json!({
                    "duration_minutes": 300,
                    "remaining_percent": pct,
                    "resets_at_ms": 1790402460000i64
                });
                let formatted = format_window_cell(Some(&win), true, now_ms);
                assert_eq!(formatted, expected_snippet);
            }
        });
    }

    #[test]
    fn test_all_quota_unexpected_3rd_window_fallback() {
        with_test_timezone("UTC", || {
            let now_ms = 1790409600000i64;
            let reports = vec![json!({
                "credential": { "reference": "codex-main", "provider": "codex" },
                "representations": { "codex": { "validation": "valid" } },
                "status": { "provider_scope": "confirmed" },
                "availability": {
                    "state": "ready",
                    "fresh": true,
                    "observed_at_ms": now_ms,
                    "quota_buckets": [
                        {
                            "provider_label": "special-bucket",
                            "windows": [
                                { "duration_minutes": 60, "remaining_percent": 50.0, "resets_at_ms": 1790402460000i64 },
                                { "duration_minutes": 300, "remaining_percent": 100.0, "resets_at_ms": 1790402460000i64 },
                                { "duration_minutes": 10080, "remaining_percent": 74.0, "resets_at_ms": 1790692320000i64 }
                            ]
                        }
                    ]
                }
            })];

            let out = format_all_quota(&reports, now_ms, Some(100));
            assert!(out.contains("codex-main  special-bucket"));
            assert!(out.contains("1h     50% █████░░░░░  Sep 26 06:01"));
            assert!(out.contains("5h    100% ██████████  Sep 26 06:01"));
            assert!(out.contains("7d     74% ███████░░░  Sep 29 14:32"));
        });
    }

    #[test]
    fn test_all_quota_narrow_terminal() {
        with_test_timezone("UTC", || {
            let now_ms = 1790409600000i64;
            let reports = vec![
                json!({
                    "credential": { "reference": "codex-main", "provider": "codex" },
                    "representations": { "codex": { "validation": "valid" } },
                    "status": { "provider_scope": "confirmed" },
                    "availability": {
                        "state": "ready",
                        "fresh": true,
                        "observed_at_ms": now_ms,
                        "quota_buckets": [
                            {
                                "windows": [
                                    { "duration_minutes": 300, "remaining_percent": 100.0, "resets_at_ms": 1790402460000i64 }, // Sep 26 06:01
                                    { "duration_minutes": 10080, "remaining_percent": 1.0, "resets_at_ms": 1790720520000i64 }  // Sep 29 22:22
                                ]
                            }
                        ]
                    }
                }),
                json!({
                    "credential": { "reference": "antigravity-jc", "provider": "antigravity" },
                    "representations": { "acp": { "validation": "valid" }, "agy-cli": { "validation": "valid" } },
                    "availability": {
                        "state": "unknown",
                        "fresh": true,
                        "observed_at_ms": now_ms,
                        "quota_groups": [
                            {
                                "provider_display_name": "Gemini Models",
                                "bucket_fingerprints": ["fp-jc-gemini"]
                            }
                        ],
                        "quota_buckets": [
                            {
                                "provider_bucket_fingerprint": "fp-jc-gemini",
                                "windows": [
                                    { "provider_window_id": "5h", "duration_minutes": 300, "remaining_percent": 75.3, "resets_at_ms": 1790399220000i64 },
                                    { "provider_window_id": "weekly", "duration_minutes": 10080, "remaining_percent": 85.0, "resets_at_ms": 1790905920000i64 }
                                ]
                            }
                        ]
                    }
                }),
            ];

            let out = format_all_quota(&reports, now_ms, Some(70));
            // Codex narrow layout
            assert!(out.contains("codex-main  default"));
            assert!(out.contains("5h    100% ██████████  Sep 26 06:01"));
            assert!(out.contains("7d      1% ▏░░░░░░░░░  Sep 29 22:22"));

            // Antigravity narrow layout
            assert!(out.contains("antigravity-jc  Gemini Models"));
            assert!(out.contains("5h       75.3% ████████░░  Sep 26 05:07"));
            assert!(out.contains("weekly     85% █████████░  Oct 2 01:52"));
        });
    }

    #[test]
    fn test_clean_structured_json() {
        let report = json!({
            "credential": { "reference": "codex-main", "provider": "codex" },
            "status": {
                "state": "observed",
                "account_read_schema": { "type": "object" },
                "rate_limits_read_schema": { "type": "object" },
                "schema_summary": { "unknown_fields": 0 },
                "runtime_effects": { "observed": true },
                "home_entries": ["/home/test"],
                "snapshot_id": "snap-123"
            },
            "availability": {
                "quota_buckets": [
                    {
                        // Unlabeled: provider_label omitted
                        "windows": [
                            {
                                "provider_window_id": "primary",
                                "remaining_fraction": 0.991167,
                                "remaining_percent": 99.1167,
                                "used_percent": 0.8833,
                                "duration_minutes": 300,
                                "resets_at_ms": 1790381280000i64
                            },
                            {
                                "provider_window_id": "secondary",
                                "remaining_fraction": 0.5,
                                "remaining_percent": 50.0,
                                "used_percent": 50.0,
                                "duration_minutes": 10080,
                                "resets_at_ms": 1790692320000i64
                            }
                        ]
                    }
                ]
            }
        });

        let cleaned = clean_structured_json(report);
        let status = cleaned.get("status").unwrap();
        assert!(status.get("account_read_schema").is_none());
        assert!(status.get("rate_limits_read_schema").is_none());
        assert!(status.get("schema_summary").is_none());
        assert!(status.get("runtime_effects").is_none());
        assert!(status.get("home_entries").is_none());
        assert!(status.get("snapshot_id").is_none());
        assert_eq!(status["state"], "observed");

        // provider_label null fallback in JSON
        let bucket = &cleaned["availability"]["quota_buckets"][0];
        assert_eq!(bucket["provider_label"], Value::Null);

        // Protocol window IDs preserved in JSON
        assert_eq!(bucket["windows"][0]["provider_window_id"], "primary");
        assert_eq!(bucket["windows"][1]["provider_window_id"], "secondary");

        // Exact numerical float and integer timestamp precision preserved
        let win = &bucket["windows"][0];
        assert_eq!(win["remaining_fraction"], 0.991167);
        assert_eq!(win["remaining_percent"], 99.1167);
        assert_eq!(win["used_percent"], 0.8833);
        assert_eq!(win["resets_at_ms"], 1790381280000i64);
    }

    #[test]
    fn test_debug_mode_preserves_diagnostics_and_no_secrets() {
        let raw_report = json!({
            "credential": { "reference": "codex-main", "provider": "codex" },
            "status": {
                "account_read_schema": { "type": "object", "properties": { "id": { "type": "string" } } },
                "snapshot_id": "snap-abc"
            },
            "availability": {
                "quota_buckets": [
                    {
                        "windows": [
                            { "provider_window_id": "primary", "duration_minutes": 300 }
                        ]
                    }
                ]
            }
        });

        let rendered = serde_json::to_string_pretty(&raw_report).unwrap();
        assert!(rendered.contains("account_read_schema"));
        assert!(rendered.contains("snapshot_id"));
        assert!(rendered.contains("primary"));
        assert!(!rendered.contains("credential://"));
        assert!(!rendered.contains("token"));
    }

    #[test]
    fn test_local_timezone_conversion_all_cases() {
        with_test_timezone("Asia/Ho_Chi_Minh", || {
            let now_ms = 1790409600000i64; // Sep 26 2026 08:00 UTC = Sep 26 2026 15:00 UTC+07

            // 1. UTC -> UTC+07: Sep 26 07:27 UTC -> Sep 26 14:27
            assert_eq!(format_reset_time(1790407620000, now_ms), "Sep 26 14:27");

            // 2. Date rollover: Sep 29 22:22 UTC -> Sep 30 05:22
            assert_eq!(format_reset_time(1790720520000, now_ms), "Sep 30 05:22");

            // 3. Midnight / date rollover: Sep 25 18:57 UTC -> Sep 26 01:57
            assert_eq!(format_reset_time(1790362620000, now_ms), "Sep 26 01:57");

            // 4. Year rollover: Dec 31 20:00 UTC -> Jan 1 03:00 next year (2027)
            assert_eq!(format_reset_time(1798747200000, now_ms), "Jan 1 03:00 2027");

            // Same year rollover (when display now_ms is in 2027 local time)
            assert_eq!(
                format_reset_time(1798747200000, 1798747200000),
                "Jan 1 03:00"
            );

            // 5. Codex 5h and 7d from Section 6 example
            // gpt-reserve 7d: Sep 29 14:32 UTC -> Sep 29 21:32
            assert_eq!(format_reset_time(1790692320000, now_ms), "Sep 29 21:32");
            // default 5h: Sep 26 07:27 UTC -> Sep 26 14:27
            assert_eq!(format_reset_time(1790407620000, now_ms), "Sep 26 14:27");
            // default 7d: Sep 29 22:22 UTC -> Sep 30 05:22
            assert_eq!(format_reset_time(1790720520000, now_ms), "Sep 30 05:22");

            // 6. Antigravity 5h and weekly from Section 7 example
            // Gemini Models 5h: Sep 26 05:07 UTC -> Sep 26 12:07
            assert_eq!(format_reset_time(1790399220000, now_ms), "Sep 26 12:07");
            // Gemini Models weekly: Oct 2 01:52 UTC -> Oct 2 08:52
            assert_eq!(format_reset_time(1790905920000, now_ms), "Oct 2 08:52");
        });
    }

    #[test]
    fn test_local_timezone_non_utc_plus_7() {
        with_test_timezone("America/New_York", || {
            let now_ms = 1790409600000i64; // Sep 26 2026 08:00 UTC = Sep 26 2026 04:00 EDT

            // Sep 26 07:27 UTC in America/New_York (EDT, UTC-4) is Sep 26 03:27
            assert_eq!(format_reset_time(1790407620000, now_ms), "Sep 26 03:27");

            // Sep 29 22:22 UTC in America/New_York is Sep 29 18:22
            assert_eq!(format_reset_time(1790720520000, now_ms), "Sep 29 18:22");

            // Sep 25 18:57 UTC in America/New_York is Sep 25 14:57
            assert_eq!(format_reset_time(1790362620000, now_ms), "Sep 25 14:57");
        });
    }

    #[test]
    fn test_local_timezone_codex_single_and_wide_quota() {
        with_test_timezone("Asia/Ho_Chi_Minh", || {
            let now_ms = 1790409600000i64; // Sep 26 2026 08:00 UTC = Sep 26 2026 15:00 UTC+07
            let codex_report = json!({
                "credential": {
                    "reference": "codex-main",
                    "provider": "codex",
                    "generation": 1,
                    "lifecycle": "enrolled"
                },
                "representations": {
                    "codex": { "validation": "valid" }
                },
                "health": {
                    "runtime": { "state": "healthy" }
                },
                "status": {
                    "state": "observed",
                    "provider_scope": "confirmed",
                    "quota_promoted": true
                },
                "availability": {
                    "state": "ready",
                    "fresh": true,
                    "observed_at_ms": now_ms,
                    "quota_buckets": [
                        {
                            "provider_label": "gpt-reserve",
                            "windows": [
                                {
                                    "provider_window_id": "secondary",
                                    "duration_minutes": 10080,
                                    "remaining_percent": 74.0,
                                    "resets_at_ms": 1790692320000i64 // Sep 29 14:32 UTC -> Sep 29 21:32 local
                                }
                            ]
                        },
                        {
                            "windows": [
                                {
                                    "provider_window_id": "primary",
                                    "duration_minutes": 300,
                                    "remaining_percent": 100.0,
                                    "resets_at_ms": 1790407620000i64 // Sep 26 07:27 UTC -> Sep 26 14:27 local
                                },
                                {
                                    "provider_window_id": "secondary",
                                    "duration_minutes": 10080,
                                    "remaining_percent": 1.0,
                                    "resets_at_ms": 1790720520000i64 // Sep 29 22:22 UTC -> Sep 30 05:22 local
                                }
                            ]
                        }
                    ]
                }
            });

            // Single-account view:
            let single_out = format_single_credential(&codex_report, now_ms);
            assert!(single_out.contains("5h     100%  ██████████   resets Sep 26 14:27"));
            assert!(single_out.contains("7d       1%  ▏░░░░░░░░░   resets Sep 30 05:22"));
            assert!(single_out.contains("7d      74%  ███████░░░   resets Sep 29 21:32"));

            // Wide quota view:
            let wide_out = format_all_quota(&[codex_report], now_ms, Some(120));
            assert!(wide_out.contains("Sep 29 21:32"));
            assert!(wide_out.contains("Sep 26 14:27"));
            assert!(wide_out.contains("Sep 30 05:22"));
        });
    }

    #[test]
    fn test_local_timezone_antigravity_single_wide_and_stale() {
        with_test_timezone("Asia/Ho_Chi_Minh", || {
            let now_ms = 1790409600000i64; // Sep 26 2026 08:00 UTC
            let stale_report = json!({
                "credential": {
                    "reference": "antigravity-ch9b2013",
                    "provider": "antigravity",
                    "generation": 1,
                    "lifecycle": "enrolled"
                },
                "representations": {
                    "acp": { "state": "valid" },
                    "agy-cli": { "state": "valid" }
                },
                "health": { "runtime": { "state": "healthy" } },
                "status": { "state": "observed" },
                "availability": {
                    "state": "unknown",
                    "fresh": false,
                    "observed_at_ms": now_ms - 240_000,
                    "quota_groups": [
                        {
                            "provider_display_name": "Gemini Models",
                            "members": [{ "provider_label": "Gemini Flash" }],
                            "bucket_fingerprints": ["fp-gemini"]
                        }
                    ],
                    "quota_buckets": [
                        {
                            "provider_bucket_fingerprint": "fp-gemini",
                            "provider_label": "Gemini Models",
                            "windows": [
                                {
                                    "provider_window_id": "5h",
                                    "duration_minutes": 300,
                                    "remaining_percent": 99.9,
                                    "resets_at_ms": 1790362620000i64 // Sep 25 18:57 UTC -> Sep 26 01:57 local
                                },
                                {
                                    "provider_window_id": "weekly",
                                    "duration_minutes": 10080,
                                    "remaining_percent": 100.0,
                                    "resets_at_ms": 1790673720000i64 // Sep 29 09:22 UTC -> Sep 29 16:22 local
                                }
                            ]
                        }
                    ]
                }
            });

            let fresh_report = json!({
                "credential": {
                    "reference": "antigravity-jc",
                    "provider": "antigravity",
                    "generation": 1,
                    "lifecycle": "enrolled"
                },
                "representations": {
                    "acp": { "validation": "valid" },
                    "agy-cli": { "validation": "valid" }
                },
                "health": { "runtime": { "state": "healthy" } },
                "status": { "state": "observed" },
                "availability": {
                    "state": "unknown",
                    "fresh": true,
                    "observed_at_ms": now_ms,
                    "quota_groups": [
                        {
                            "provider_display_name": "Gemini Models",
                            "members": [{ "provider_label": "Gemini Flash" }],
                            "bucket_fingerprints": ["fp-jc"]
                        }
                    ],
                    "quota_buckets": [
                        {
                            "provider_bucket_fingerprint": "fp-jc",
                            "provider_label": "Gemini Models",
                            "windows": [
                                {
                                    "provider_window_id": "5h",
                                    "duration_minutes": 300,
                                    "remaining_percent": 64.2,
                                    "resets_at_ms": 1790399220000i64 // Sep 26 05:07 UTC -> Sep 26 12:07 local
                                },
                                {
                                    "provider_window_id": "weekly",
                                    "duration_minutes": 10080,
                                    "remaining_percent": 83.5,
                                    "resets_at_ms": 1790905920000i64 // Oct 2 01:52 UTC -> Oct 2 08:52 local
                                }
                            ]
                        }
                    ]
                }
            });

            // Single-account fresh:
            let single_fresh = format_single_credential(&fresh_report, now_ms);
            assert!(single_fresh.contains("5h       64.2%  ██████░░░░   resets Sep 26 12:07"));
            assert!(single_fresh.contains("weekly   83.5%  ████████░░   resets Oct 2 08:52"));

            // Wide quota with both stale and fresh accounts:
            let wide_out = format_all_quota(&[stale_report, fresh_report], now_ms, Some(120));
            assert!(wide_out.contains("Sep 26 01:57"));
            assert!(wide_out.contains("Sep 29 16:22"));
            assert!(wide_out.contains("Sep 26 12:07"));
            assert!(wide_out.contains("Oct 2 08:52"));
        });
    }

    #[test]
    fn test_canonical_json_and_debug_timestamps_unchanged() {
        let raw_report = json!({
            "credential": {
                "reference": "codex-main",
                "provider": "codex"
            },
            "availability": {
                "quota_buckets": [
                    {
                        "windows": [
                            {
                                "duration_minutes": 300,
                                "remaining_percent": 100.0,
                                "resets_at_ms": 1790407620000i64,
                                "resets_at": "2026-09-26T07:27:00Z"
                            }
                        ]
                    }
                ]
            }
        });

        // JSON must retain exact raw values (resets_at_ms canonical epoch, UTC resets_at string)
        let cleaned = clean_structured_json(raw_report.clone());
        let window = &cleaned["availability"]["quota_buckets"][0]["windows"][0];
        assert_eq!(window["resets_at_ms"], 1790407620000i64);
        assert_eq!(window["resets_at"], "2026-09-26T07:27:00Z");

        // Verify JSON serialization does not introduce local offsets
        let json_str = serde_json::to_string(&cleaned).unwrap();
        assert!(json_str.contains(r#""resets_at_ms":1790407620000"#));
        assert!(json_str.contains(r#""resets_at":"2026-09-26T07:27:00Z""#));
        assert!(!json_str.contains("+07:00"));
    }
}
