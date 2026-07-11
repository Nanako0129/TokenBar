//! Session interval derivation and time-based metrics.

use crate::sessions::UnifiedMessage;
use crate::TokenBreakdown;
use std::collections::HashMap;

/// Default idle gap threshold: 3 minutes (ms).
pub const DEFAULT_IDLE_GAP_MS: i64 = 3 * 60 * 1000;

/// A derived session interval representing one continuous usage session.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionInterval {
    pub client: String,
    pub session_id: String,
    /// First message timestamp (Unix ms)
    pub start_ts: i64,
    /// Last message timestamp (Unix ms)
    pub end_ts: i64,
    /// Wall-clock duration: end_ts - start_ts
    pub wall_duration_ms: i64,
    /// Active duration excluding idle gaps beyond the threshold
    pub active_duration_ms: i64,
    pub message_count: i32,
    pub tokens: TokenBreakdown,
    pub cost: f64,
}

/// Time-based usage metrics computed from session intervals.
#[derive(Debug, Clone, serde::Serialize)]
pub struct TimeMetrics {
    /// Total active usage time across all sessions (ms)
    pub total_active_time_ms: i64,
    /// Total wall-clock usage time across all sessions (ms)
    pub total_wall_time_ms: i64,
    /// Longest single session active duration (ms)
    pub longest_continuous_ms: i64,
    /// Peak number of sessions overlapping at the same time
    pub max_concurrent_sessions: u32,
    /// Total number of derived session intervals
    pub session_count: u32,
}

/// Derive session intervals from unified messages.
///
/// Groups messages by `(client, session_id)`, sorts each group by timestamp,
/// and computes per-session start/end/duration/active-time.
///
/// `idle_gap_ms` controls how much silence between messages is still counted
/// as "active". Time gaps exceeding this threshold are excluded from
/// `active_duration_ms`.
pub fn sessionize(messages: &[UnifiedMessage], idle_gap_ms: i64) -> Vec<SessionInterval> {
    if messages.is_empty() {
        return Vec::new();
    }

    // Group by (client, session_id)
    let mut groups: HashMap<(&str, &str), Vec<&UnifiedMessage>> = HashMap::new();
    for msg in messages {
        if msg.timestamp <= 0 {
            continue;
        }
        groups
            .entry((&msg.client, &msg.session_id))
            .or_default()
            .push(msg);
    }

    let mut intervals: Vec<SessionInterval> = Vec::with_capacity(groups.len());

    for ((client, session_id), mut msgs) in groups {
        // Sort by timestamp within session
        msgs.sort_unstable_by_key(|m| m.timestamp);

        let start_ts = msgs.first().unwrap().timestamp;
        let end_ts = msgs.last().unwrap().timestamp;
        let wall_duration_ms = end_ts - start_ts;

        // Calculate active duration: sum of gaps that are <= idle_gap_ms
        let mut active_duration_ms: i64 = 0;
        for window in msgs.windows(2) {
            let gap = window[1].timestamp - window[0].timestamp;
            if gap <= idle_gap_ms {
                active_duration_ms += gap;
            }
        }

        // Aggregate tokens and cost
        let mut tokens = TokenBreakdown::default();
        let mut cost = 0.0;
        let mut message_count: i32 = 0;

        for msg in &msgs {
            // saturating_add so clamped (i64::MAX) buckets from a corrupt source
            // can't overflow the fold.
            tokens.input = tokens.input.saturating_add(msg.tokens.input);
            tokens.output = tokens.output.saturating_add(msg.tokens.output);
            tokens.cache_read = tokens.cache_read.saturating_add(msg.tokens.cache_read);
            tokens.cache_write = tokens.cache_write.saturating_add(msg.tokens.cache_write);
            tokens.reasoning = tokens.reasoning.saturating_add(msg.tokens.reasoning);
            cost += msg.cost;
            message_count += msg.message_count.max(1);
        }

        intervals.push(SessionInterval {
            client: client.to_string(),
            session_id: session_id.to_string(),
            start_ts,
            end_ts,
            wall_duration_ms,
            active_duration_ms,
            message_count,
            tokens,
            cost,
        });
    }

    // Sort by start time for downstream consumers
    intervals.sort_unstable_by_key(|s| s.start_ts);
    intervals
}

/// Compute time-based metrics from session intervals.
///
/// - `total_active_time_ms`: sum of all `active_duration_ms`
/// - `total_wall_time_ms`: sum of all `wall_duration_ms`
/// - `longest_continuous_ms`: longest merged activity window across ALL sessions
///   (using the idle gap threshold to merge overlapping/adjacent activity)
/// - `max_concurrent_sessions`: peak overlap of session wall-clock intervals
pub fn compute_time_metrics(intervals: &[SessionInterval], _idle_gap_ms: i64) -> TimeMetrics {
    if intervals.is_empty() {
        return TimeMetrics {
            total_active_time_ms: 0,
            total_wall_time_ms: 0,
            longest_continuous_ms: 0,
            max_concurrent_sessions: 0,
            session_count: 0,
        };
    }

    let total_active_time_ms: i64 = intervals.iter().map(|s| s.active_duration_ms).sum();
    let total_wall_time_ms: i64 = intervals.iter().map(|s| s.wall_duration_ms).sum();
    let session_count = intervals.len() as u32;

    // --- Longest continuous usage ---
    // Collect all session [start, end] as activity windows, merge overlapping
    // ones (with idle_gap_ms tolerance), find the longest merged span.
    // Use active_duration_ms instead of wall-clock span to exclude idle gaps
    // within sessions from inflating the metric.
    let longest_continuous_ms = {
        let mut windows: Vec<(i64, i64)> = intervals
            .iter()
            .filter(|s| s.start_ts > 0 && s.active_duration_ms > 0)
            .map(|s| (s.start_ts, s.start_ts + s.active_duration_ms))
            .collect();
        windows.sort_unstable_by_key(|w| w.0);

        let mut longest: i64 = 0;
        if let Some(&first) = windows.first() {
            let mut merged_start = first.0;
            let mut merged_end = first.1;

            for &(start, end) in &windows[1..] {
                if start <= merged_end + _idle_gap_ms {
                    // Overlapping or within idle gap tolerance — extend
                    merged_end = merged_end.max(end);
                } else {
                    // Gap too large — finalize previous window
                    longest = longest.max(merged_end - merged_start);
                    merged_start = start;
                    merged_end = end;
                }
            }
            longest = longest.max(merged_end - merged_start);
        }
        longest
    };

    // --- Max concurrent sessions ---
    let max_concurrent_sessions = compute_max_concurrent(intervals);

    TimeMetrics {
        total_active_time_ms,
        total_wall_time_ms,
        longest_continuous_ms,
        max_concurrent_sessions,
        session_count,
    }
}

/// Sweep-line algorithm to find peak concurrent sessions.
fn compute_max_concurrent(intervals: &[SessionInterval]) -> u32 {
    if intervals.is_empty() {
        return 0;
    }

    let mut events: Vec<(i64, i32)> = Vec::with_capacity(intervals.len() * 2);
    for s in intervals {
        if s.start_ts <= 0 {
            continue;
        }
        events.push((s.start_ts, 1));
        // For zero-duration sessions (start == end), push end as start+1
        // so the +1 event is processed before the -1 event at the same logical point
        let end = if s.end_ts <= s.start_ts {
            s.start_ts + 1
        } else {
            s.end_ts
        };
        events.push((end, -1));
    }

    // Sort by time; ties broken by start (+1) before end (-1) so concurrent
    // sessions at the same timestamp are counted together
    events.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));

    let mut max_concurrent: u32 = 0;
    let mut current: i32 = 0;

    for (_, delta) in events {
        current += delta;
        if current > max_concurrent as i32 {
            max_concurrent = current as u32;
        }
    }

    max_concurrent
}

/// Compute per-day active time (ms) from session intervals.
///
/// For each interval, distributes its `active_duration_ms` proportionally
/// across local-time days for the active timezone. Single-day sessions get their full active
/// time assigned to that day.
pub fn compute_daily_active_time(
    intervals: &[SessionInterval],
) -> std::collections::HashMap<String, i64> {
    compute_daily_active_time_with_timezone(intervals, &chrono::Local)
}

fn compute_daily_active_time_with_timezone<Tz>(
    intervals: &[SessionInterval],
    timezone: &Tz,
) -> std::collections::HashMap<String, i64>
where
    Tz: chrono::TimeZone,
{
    use std::collections::HashMap;

    let mut daily: HashMap<String, i64> = HashMap::new();

    for interval in intervals {
        if interval.active_duration_ms <= 0 {
            continue;
        }

        let start_date = match local_date(interval.start_ts, timezone) {
            Some(date) => date,
            None => continue,
        };
        let end_date = match local_date(interval.end_ts, timezone) {
            Some(date) => date,
            None => continue,
        };

        let wall = interval.wall_duration_ms.max(1);
        let mut day = start_date;

        loop {
            let day_key = day.format("%Y-%m-%d").to_string();
            let Some(day_start) = local_day_start(day, timezone) else {
                // DST gap: skip the rest of this interval when local midnight
                // is not representable for the active timezone.
                break;
            };

            let Some(next_day) = day.succ_opt() else {
                break;
            };
            let Some(next_day_start) = local_day_start(next_day, timezone) else {
                // DST gap: skip the rest of this interval when the next local
                // midnight is not representable for the active timezone.
                break;
            };

            let overlap_start = interval.start_ts.max(day_start);
            let overlap_end = interval.end_ts.min(next_day_start);
            let overlap = (overlap_end - overlap_start).max(0);
            let proportion = overlap as f64 / wall as f64;
            let active_for_day = (interval.active_duration_ms as f64 * proportion) as i64;

            if active_for_day > 0 {
                *daily.entry(day_key).or_default() += active_for_day;
            }

            if day == end_date {
                break;
            }

            if let Some(next) = day.succ_opt() {
                day = next;
            } else {
                break;
            }
        }
    }

    daily
}

fn local_date<Tz>(timestamp_ms: i64, timezone: &Tz) -> Option<chrono::NaiveDate>
where
    Tz: chrono::TimeZone,
{
    timezone
        .timestamp_millis_opt(timestamp_ms)
        .single()
        .map(|datetime| datetime.date_naive())
}

fn local_day_start<Tz>(date: chrono::NaiveDate, timezone: &Tz) -> Option<i64>
where
    Tz: chrono::TimeZone,
{
    let midnight = date.and_time(chrono::NaiveTime::MIN);
    match timezone.from_local_datetime(&midnight) {
        chrono::LocalResult::Single(datetime) => Some(datetime.timestamp_millis()),
        chrono::LocalResult::Ambiguous(earliest, _) => Some(earliest.timestamp_millis()),
        chrono::LocalResult::None => None,
    }
}

/// Streaming accumulator that builds [`SessionInterval`]s one message at a time.
///
/// Equivalent to [`sessionize`] but does not require a materialised
/// `&[UnifiedMessage]` slice.  Call [`SessionizeAccumulator::feed`] for each
/// incoming message (timestamp ≤ 0 are silently skipped, matching the behaviour
/// of [`sessionize`]), then call [`SessionizeAccumulator::finalize`] to obtain
/// the sorted `Vec<SessionInterval>`.  Output is bit-for-bit identical to
/// `sessionize(all_messages, idle_gap_ms)` for the same stream.
pub struct SessionizeAccumulator {
    groups: std::collections::HashMap<(String, String), SessionAcc>,
}

struct SessionAcc {
    timestamps: Vec<i64>,
    tokens: TokenBreakdown,
    cost: f64,
    message_count: i32,
}

impl Default for SessionizeAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionizeAccumulator {
    pub fn new() -> Self {
        Self {
            groups: std::collections::HashMap::new(),
        }
    }

    /// Feed one message into the accumulator. Messages with timestamp ≤ 0 are
    /// skipped (identical behaviour to [`sessionize`]).
    pub fn feed(&mut self, msg: &UnifiedMessage) {
        if msg.timestamp <= 0 {
            return;
        }
        let key = (msg.client.clone(), msg.session_id.clone());
        let acc = self.groups.entry(key).or_insert_with(|| SessionAcc {
            timestamps: Vec::new(),
            tokens: TokenBreakdown::default(),
            cost: 0.0,
            message_count: 0,
        });
        acc.timestamps.push(msg.timestamp);
        // saturating_add so clamped (i64::MAX) buckets from a corrupt source
        // can't overflow the fold.
        acc.tokens.input = acc.tokens.input.saturating_add(msg.tokens.input);
        acc.tokens.output = acc.tokens.output.saturating_add(msg.tokens.output);
        acc.tokens.cache_read = acc.tokens.cache_read.saturating_add(msg.tokens.cache_read);
        acc.tokens.cache_write = acc.tokens.cache_write.saturating_add(msg.tokens.cache_write);
        acc.tokens.reasoning = acc.tokens.reasoning.saturating_add(msg.tokens.reasoning);
        acc.cost += msg.cost;
        acc.message_count += msg.message_count.max(1);
    }

    /// Consume the accumulator and produce a sorted `Vec<SessionInterval>`.
    ///
    /// Output is identical to [`sessionize`] called on the same messages.
    pub fn finalize(self, idle_gap_ms: i64) -> Vec<SessionInterval> {
        let mut intervals: Vec<SessionInterval> = Vec::with_capacity(self.groups.len());

        for ((client, session_id), mut acc) in self.groups {
            if acc.timestamps.is_empty() {
                continue;
            }
            acc.timestamps.sort_unstable();

            let start_ts = acc.timestamps[0];
            let end_ts = *acc.timestamps.last().unwrap();
            let wall_duration_ms = end_ts - start_ts;

            let mut active_duration_ms: i64 = 0;
            for w in acc.timestamps.windows(2) {
                let gap = w[1] - w[0];
                if gap <= idle_gap_ms {
                    active_duration_ms += gap;
                }
            }

            intervals.push(SessionInterval {
                client,
                session_id,
                start_ts,
                end_ts,
                wall_duration_ms,
                active_duration_ms,
                message_count: acc.message_count,
                tokens: acc.tokens,
                cost: acc.cost,
            });
        }

        intervals.sort_unstable_by_key(|s| s.start_ts);
        intervals
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};

    fn make_msg(client: &str, session_id: &str, timestamp: i64) -> UnifiedMessage {
        UnifiedMessage {
            client: client.to_string(),
            model_id: "test-model".to_string(),
            provider_id: "test-provider".to_string(),
            session_id: session_id.to_string(),
            workspace_key: None,
            workspace_label: None,
            timestamp,
            date: "2024-01-01".to_string(),
            tokens: TokenBreakdown {
                input: 100,
                output: 50,
                cache_read: 0,
                cache_write: 0,
                reasoning: 0,
            },
            cost: 0.01,
            message_count: 1,
            agent: None,
            dedup_key: None,
            is_turn_start: false,
            duration_ms: None,
        }
    }

    #[test]
    fn test_sessionize_empty() {
        let result = sessionize(&[], DEFAULT_IDLE_GAP_MS);
        assert!(result.is_empty());
    }

    #[test]
    fn test_sessionize_single_message() {
        let msgs = vec![make_msg("opencode", "ses1", 1000000)];
        let result = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].wall_duration_ms, 0);
        assert_eq!(result[0].active_duration_ms, 0);
        assert_eq!(result[0].message_count, 1);
    }

    #[test]
    fn test_sessionize_continuous_session() {
        // 5 messages, each 1 minute apart (within 3-min threshold)
        let msgs: Vec<UnifiedMessage> = (0..5)
            .map(|i| make_msg("opencode", "ses1", 1000000 + i * 60_000))
            .collect();

        let result = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].wall_duration_ms, 4 * 60_000);
        assert_eq!(result[0].active_duration_ms, 4 * 60_000); // all gaps <= 3min
        assert_eq!(result[0].message_count, 5);
    }

    #[test]
    fn test_sessionize_with_idle_gap() {
        // 3 messages: first two 1 min apart, then 5 min gap (exceeds 3-min threshold)
        let msgs = vec![
            make_msg("opencode", "ses1", 1000000),
            make_msg("opencode", "ses1", 1000000 + 60_000),
            make_msg("opencode", "ses1", 1000000 + 60_000 + 5 * 60_000),
        ];

        let result = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        assert_eq!(result.len(), 1);
        // Wall duration = 6 minutes
        assert_eq!(result[0].wall_duration_ms, 6 * 60_000);
        // Active duration = only the first gap (1 min), second gap (5 min) excluded
        assert_eq!(result[0].active_duration_ms, 60_000);
    }

    #[test]
    fn test_sessionize_multiple_sessions() {
        let msgs = vec![
            make_msg("opencode", "ses1", 1000000),
            make_msg("opencode", "ses1", 1060000),
            make_msg("claude", "ses2", 1000000),
            make_msg("claude", "ses2", 1120000),
        ];

        let result = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        assert_eq!(result.len(), 2);
    }

    #[test]
    fn test_sessionize_skips_zero_timestamp() {
        let msgs = vec![
            make_msg("opencode", "ses1", 0),
            make_msg("opencode", "ses1", 1000000),
        ];

        let result = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message_count, 1); // only the non-zero one
    }

    #[test]
    fn test_compute_time_metrics_empty() {
        let metrics = compute_time_metrics(&[], DEFAULT_IDLE_GAP_MS);
        assert_eq!(metrics.total_active_time_ms, 0);
        assert_eq!(metrics.longest_continuous_ms, 0);
        assert_eq!(metrics.max_concurrent_sessions, 0);
        assert_eq!(metrics.session_count, 0);
    }

    #[test]
    fn test_max_concurrent_sessions() {
        // Two overlapping sessions
        let intervals = vec![
            SessionInterval {
                client: "opencode".to_string(),
                session_id: "ses1".to_string(),
                start_ts: 1000,
                end_ts: 5000,
                wall_duration_ms: 4000,
                active_duration_ms: 4000,
                message_count: 3,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
            SessionInterval {
                client: "claude".to_string(),
                session_id: "ses2".to_string(),
                start_ts: 3000,
                end_ts: 7000,
                wall_duration_ms: 4000,
                active_duration_ms: 4000,
                message_count: 3,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
        ];

        let metrics = compute_time_metrics(&intervals, DEFAULT_IDLE_GAP_MS);
        assert_eq!(metrics.max_concurrent_sessions, 2);
    }

    #[test]
    fn test_max_concurrent_non_overlapping() {
        // Two non-overlapping sessions
        let intervals = vec![
            SessionInterval {
                client: "opencode".to_string(),
                session_id: "ses1".to_string(),
                start_ts: 1000,
                end_ts: 3000,
                wall_duration_ms: 2000,
                active_duration_ms: 2000,
                message_count: 2,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
            SessionInterval {
                client: "claude".to_string(),
                session_id: "ses2".to_string(),
                start_ts: 5000,
                end_ts: 7000,
                wall_duration_ms: 2000,
                active_duration_ms: 2000,
                message_count: 2,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
        ];

        let metrics = compute_time_metrics(&intervals, DEFAULT_IDLE_GAP_MS);
        assert_eq!(metrics.max_concurrent_sessions, 1);
    }

    #[test]
    fn test_longest_continuous_is_max_session_active_duration() {
        let intervals = vec![
            SessionInterval {
                client: "opencode".to_string(),
                session_id: "ses1".to_string(),
                start_ts: 1000,
                end_ts: 5000,
                wall_duration_ms: 4000,
                active_duration_ms: 3000,
                message_count: 3,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
            SessionInterval {
                client: "claude".to_string(),
                session_id: "ses2".to_string(),
                start_ts: 3000,
                end_ts: 8000,
                wall_duration_ms: 5000,
                active_duration_ms: 5000,
                message_count: 3,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
        ];

        let metrics = compute_time_metrics(&intervals, DEFAULT_IDLE_GAP_MS);
        assert_eq!(metrics.longest_continuous_ms, 7000);
    }

    #[test]
    fn test_longest_continuous_picks_max_active() {
        let intervals = vec![
            SessionInterval {
                client: "opencode".to_string(),
                session_id: "ses1".to_string(),
                start_ts: 1,
                end_ts: 60_000,
                wall_duration_ms: 60_000,
                active_duration_ms: 60_000,
                message_count: 3,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
            SessionInterval {
                client: "opencode".to_string(),
                session_id: "ses2".to_string(),
                start_ts: 60_000 + 2 * 60_000,
                end_ts: 60_000 + 2 * 60_000 + 120_000,
                wall_duration_ms: 120_000,
                active_duration_ms: 120_000,
                message_count: 3,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
        ];

        let metrics = compute_time_metrics(&intervals, DEFAULT_IDLE_GAP_MS);
        assert_eq!(metrics.longest_continuous_ms, 299_999);
    }

    #[test]
    fn test_longest_continuous_single_session() {
        let intervals = vec![
            SessionInterval {
                client: "opencode".to_string(),
                session_id: "ses1".to_string(),
                start_ts: 1000,
                end_ts: 61_000,
                wall_duration_ms: 60_000,
                active_duration_ms: 60_000,
                message_count: 3,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
            SessionInterval {
                client: "opencode".to_string(),
                session_id: "ses2".to_string(),
                start_ts: 61_000 + 10 * 60_000,
                end_ts: 61_000 + 10 * 60_000 + 120_000,
                wall_duration_ms: 120_000,
                active_duration_ms: 120_000,
                message_count: 5,
                tokens: TokenBreakdown::default(),
                cost: 0.0,
            },
        ];

        let metrics = compute_time_metrics(&intervals, DEFAULT_IDLE_GAP_MS);
        assert_eq!(metrics.longest_continuous_ms, 120_000);
    }

    #[test]
    fn test_compute_daily_active_time_matches_local_day_boundaries_for_fixed_offset() {
        let interval = SessionInterval {
            client: "trae".to_string(),
            session_id: "session-local-boundary".to_string(),
            start_ts: FixedOffset::east_opt(9 * 3600)
                .unwrap()
                .with_ymd_and_hms(2026, 1, 1, 23, 30, 0)
                .single()
                .unwrap()
                .timestamp_millis(),
            end_ts: FixedOffset::east_opt(9 * 3600)
                .unwrap()
                .with_ymd_and_hms(2026, 1, 2, 0, 30, 0)
                .single()
                .unwrap()
                .timestamp_millis(),
            wall_duration_ms: 3_600_000,
            active_duration_ms: 3_600_000,
            message_count: 2,
            tokens: TokenBreakdown::default(),
            cost: 0.0,
        };

        let daily = compute_daily_active_time_with_timezone(
            &[interval],
            &FixedOffset::east_opt(9 * 3600).unwrap(),
        );

        assert_eq!(daily.get("2026-01-01"), Some(&1_800_000));
        assert_eq!(daily.get("2026-01-02"), Some(&1_800_000));
        assert_eq!(daily.len(), 2);
    }
    // =========================================================================
    // SessionizeAccumulator parity tests
    // =========================================================================

    fn make_msg_full(
        client: &str,
        session_id: &str,
        timestamp: i64,
        input: i64,
        output: i64,
        cost: f64,
        message_count: i32,
    ) -> UnifiedMessage {
        UnifiedMessage {
            client: client.to_string(),
            model_id: "model".to_string(),
            provider_id: "provider".to_string(),
            session_id: session_id.to_string(),
            workspace_key: None,
            workspace_label: None,
            timestamp,
            date: "2024-01-01".to_string(),
            tokens: TokenBreakdown { input, output, cache_read: 0, cache_write: 0, reasoning: 0 },
            cost,
            message_count,
            agent: None,
            dedup_key: None,
            is_turn_start: false,
            duration_ms: None,
        }
    }

    #[test]
    fn test_sessionize_accumulator_empty() {
        let acc = SessionizeAccumulator::new();
        let result = acc.finalize(DEFAULT_IDLE_GAP_MS);
        assert!(result.is_empty());
    }

    #[test]
    fn test_sessionize_accumulator_parity_single_session() {
        let msgs = vec![
            make_msg("opencode", "s1", 1_000_000),
            make_msg("opencode", "s1", 1_060_000),
            make_msg("opencode", "s1", 1_120_000),
        ];

        let reference = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);

        let mut acc = SessionizeAccumulator::new();
        for m in &msgs { acc.feed(m); }
        let result = acc.finalize(DEFAULT_IDLE_GAP_MS);

        assert_eq!(reference.len(), result.len(), "session count must match");
        assert_eq!(reference[0].start_ts,           result[0].start_ts);
        assert_eq!(reference[0].end_ts,             result[0].end_ts);
        assert_eq!(reference[0].wall_duration_ms,   result[0].wall_duration_ms);
        assert_eq!(reference[0].active_duration_ms, result[0].active_duration_ms);
        assert_eq!(reference[0].message_count,      result[0].message_count);
    }

    #[test]
    fn test_sessionize_accumulator_parity_idle_gap() {
        // Gap > threshold in the middle: first two close, last one far.
        let msgs = vec![
            make_msg("opencode", "ses1", 1_000_000),
            make_msg("opencode", "ses1", 1_060_000),
            make_msg("opencode", "ses1", 1_060_000 + 5 * 60_000), // 5 min gap
        ];
        let reference = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        let mut acc = SessionizeAccumulator::new();
        for m in &msgs { acc.feed(m); }
        let result = acc.finalize(DEFAULT_IDLE_GAP_MS);

        assert_eq!(reference[0].active_duration_ms, result[0].active_duration_ms,
            "active_duration_ms must match (idle gap excluded)");
        assert_eq!(reference[0].wall_duration_ms, result[0].wall_duration_ms);
    }

    #[test]
    fn test_sessionize_accumulator_parity_multi_session_multi_client() {
        let msgs = vec![
            make_msg("claude",   "s1", 1_000_000),
            make_msg("claude",   "s1", 1_060_000),
            make_msg("opencode", "s2", 1_000_000),
            make_msg("opencode", "s2", 1_120_000),
            make_msg("codex",    "s3", 2_000_000),
        ];
        let mut reference = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        let mut acc = SessionizeAccumulator::new();
        for m in &msgs { acc.feed(m); }
        let mut result = acc.finalize(DEFAULT_IDLE_GAP_MS);

        assert_eq!(reference.len(), result.len(), "session count must match");
        // Sort both by (client, session_id) for deterministic comparison
        reference.sort_by(|a, b| a.client.cmp(&b.client).then(a.session_id.cmp(&b.session_id)));
        result.sort_by(|a, b| a.client.cmp(&b.client).then(a.session_id.cmp(&b.session_id)));
        for (r, a) in reference.iter().zip(result.iter()) {
            assert_eq!(r.session_id, a.session_id, "session_id must match");
            assert_eq!(r.start_ts, a.start_ts, "start_ts mismatch for session {}", r.session_id);
            assert_eq!(r.wall_duration_ms, a.wall_duration_ms,
                "wall_duration_ms mismatch for session {}", r.session_id);
            assert_eq!(r.active_duration_ms, a.active_duration_ms,
                "active_duration_ms mismatch for session {}", r.session_id);
            assert_eq!(r.message_count, a.message_count,
                "message_count mismatch for session {}", r.session_id);
        }
    }

    #[test]
    fn test_sessionize_accumulator_skips_zero_timestamp() {
        let msgs = vec![
            make_msg("opencode", "s1", 0),         // skipped
            make_msg("opencode", "s1", 1_000_000), // kept
        ];
        let mut acc = SessionizeAccumulator::new();
        for m in &msgs { acc.feed(m); }
        let result = acc.finalize(DEFAULT_IDLE_GAP_MS);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].message_count, 1); // only non-zero counted
    }

    #[test]
    fn test_sessionize_accumulator_parity_tokens_and_cost() {
        let msgs = vec![
            make_msg_full("claude", "s1", 1_000_000, 100, 50, 0.01, 1),
            make_msg_full("claude", "s1", 1_060_000, 200, 80, 0.02, 2),
        ];
        let reference = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        let mut acc = SessionizeAccumulator::new();
        for m in &msgs { acc.feed(m); }
        let result = acc.finalize(DEFAULT_IDLE_GAP_MS);

        assert_eq!(result.len(), 1);
        assert_eq!(reference[0].tokens.input,  result[0].tokens.input,  "input tokens");
        assert_eq!(reference[0].tokens.output, result[0].tokens.output, "output tokens");
        assert!((reference[0].cost - result[0].cost).abs() < 1e-9, "cost");
        assert_eq!(reference[0].message_count, result[0].message_count, "message_count");
    }

    #[test]
    fn test_sessionize_saturates_overflowing_token_fold() {
        // A parser can clamp an untrusted token count to i64::MAX (e.g.
        // antigravity_cli). Two such messages in one (client, session_id) block
        // within the idle gap fold into the same session; a plain `+=` fold
        // would overflow (debug panic / release wrap) before it is serialized
        // into SessionInterval.tokens. Covers both fold sites: the free
        // `sessionize()` function and `SessionizeAccumulator::feed`.
        let overlarge = |ts: i64| {
            let mut message = make_msg("antigravity-cli", "ses1", ts);
            message.tokens = TokenBreakdown {
                input: i64::MAX,
                output: 0,
                cache_read: i64::MAX,
                cache_write: 0,
                reasoning: 0,
            };
            message
        };
        let msgs = vec![overlarge(1_000_000), overlarge(1_001_000)];

        let result = sessionize(&msgs, DEFAULT_IDLE_GAP_MS);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].tokens.input, i64::MAX);
        assert_eq!(result[0].tokens.cache_read, i64::MAX);

        let mut acc = SessionizeAccumulator::new();
        for m in &msgs { acc.feed(m); }
        let acc_result = acc.finalize(DEFAULT_IDLE_GAP_MS);
        assert_eq!(acc_result.len(), 1);
        assert_eq!(acc_result[0].tokens.input, i64::MAX);
        assert_eq!(acc_result[0].tokens.cache_read, i64::MAX);
    }
}
