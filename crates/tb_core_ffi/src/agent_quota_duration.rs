//! Provider-neutral quota duration evidence and observed rollover lifecycle.
//!
//! Stage 2 deliberately keeps this module independent from provider adapters and
//! wire models. Adapters can supply provider/contract evidence later; observed
//! state is persisted by `agent_quota_history` in the same v3 transaction.

#![allow(dead_code)]

use chrono::{Datelike, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};

pub(crate) const MAX_DURATION_SECONDS: i64 = 400 * 86_400;
pub(crate) const ROLLOVER_GRACE_SECONDS: i64 = 15 * 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) enum DurationSource {
    Provider,
    Contract,
    Observed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurationUnavailableReason {
    MissingReset,
    InvalidEvidence,
}

impl std::fmt::Display for DurationUnavailableReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::MissingReset => "missing reset",
            Self::InvalidEvidence => "invalid duration evidence",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DurationEvidence {
    pub(crate) reset_at: Option<i64>,
    pub(crate) duration_seconds: i64,
}

impl DurationEvidence {
    pub(crate) fn provider(reset_at: i64, duration_seconds: i64) -> Self {
        Self {
            reset_at: Some(reset_at),
            duration_seconds,
        }
    }

    pub(crate) fn contract(duration_seconds: i64) -> Self {
        Self {
            reset_at: None,
            duration_seconds,
        }
    }

    pub(crate) fn observed(reset_at: i64, duration_seconds: i64) -> Self {
        Self {
            reset_at: Some(reset_at),
            duration_seconds,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DurationResolution {
    Ready {
        duration_seconds: i64,
        source: DurationSource,
    },
    LearningDuration,
    Unavailable(DurationUnavailableReason),
}

/// Resolve the first valid duration in the frozen provider -> contract ->
/// observed order. A missing observed candidate means the caller is still
/// learning, while malformed supplied evidence is typed as unavailable.
pub(crate) fn resolve_duration(
    now: i64,
    reset_at: Option<i64>,
    provider: Option<DurationEvidence>,
    contract: Option<DurationEvidence>,
    observed: Option<DurationEvidence>,
) -> DurationResolution {
    let Some(reset_at) = reset_at else {
        return DurationResolution::Unavailable(DurationUnavailableReason::MissingReset);
    };
    if !valid_reset(reset_at, now) {
        return DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence);
    }

    if let Some(evidence) = provider {
        return if valid_evidence(evidence, reset_at, now, true) {
            DurationResolution::Ready {
                duration_seconds: evidence.duration_seconds,
                source: DurationSource::Provider,
            }
        } else {
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        };
    }
    if let Some(evidence) = contract {
        return if valid_evidence(evidence, reset_at, now, false) {
            DurationResolution::Ready {
                duration_seconds: evidence.duration_seconds,
                source: DurationSource::Contract,
            }
        } else {
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        };
    }
    if let Some(evidence) = observed {
        return if valid_evidence(evidence, reset_at, now, true) {
            DurationResolution::Ready {
                duration_seconds: evidence.duration_seconds,
                source: DurationSource::Observed,
            }
        } else {
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        };
    }
    DurationResolution::LearningDuration
}

pub(crate) fn valid_reset(reset_at: i64, now: i64) -> bool {
    reset_at > now
}

pub(crate) fn valid_duration(duration_seconds: i64) -> bool {
    (1..=MAX_DURATION_SECONDS).contains(&duration_seconds)
}

pub(crate) fn valid_evidence(
    evidence: DurationEvidence,
    current_reset_at: i64,
    now: i64,
    require_reset_match: bool,
) -> bool {
    let Some(cycle_started_at) = current_reset_at.checked_sub(evidence.duration_seconds) else {
        return false;
    };
    valid_duration(evidence.duration_seconds)
        && cycle_started_at <= now
        && now < current_reset_at
        && (!require_reset_match || evidence.reset_at == Some(current_reset_at))
        && evidence
            .reset_at
            .is_none_or(|evidence_reset| evidence_reset == current_reset_at)
}

/// Copilot's immediate calendar contract. It intentionally accepts only an
/// exact UTC first-of-month midnight reset; all other resets must use observed
/// rollover and cannot silently become a 30-day duration.
pub(crate) fn copilot_calendar_duration(reset_at: i64) -> Option<i64> {
    let reset = Utc.timestamp_opt(reset_at, 0).single()?;
    if reset.day() != 1
        || reset.hour() != 0
        || reset.minute() != 0
        || reset.second() != 0
        || reset.timestamp_subsec_nanos() != 0
    {
        return None;
    }

    let (year, month) = if reset.month() == 1 {
        (reset.year() - 1, 12)
    } else {
        (reset.year(), reset.month() - 1)
    };
    let previous = Utc.with_ymd_and_hms(year, month, 1, 0, 0, 0).single()?;
    let duration = reset.timestamp().checked_sub(previous.timestamp())?;
    valid_duration(duration).then_some(duration)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase", deny_unknown_fields)]
pub(crate) enum ObservedState {
    Watching {
        reset_at: i64,
        first_seen_at: i64,
        last_seen_at: i64,
        consecutive_count: u8,
    },
    Candidate {
        old_reset_at: i64,
        old_seen_at: i64,
        new_reset_at: i64,
        first_new_seen_at: i64,
    },
    Ready {
        cycle_started_at: i64,
        reset_at: i64,
        duration_seconds: i64,
        confirmed_at: i64,
        last_seen_at: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ObservedTransition {
    pub(crate) state: ObservedState,
    pub(crate) duration_seconds: Option<i64>,
    pub(crate) became_ready: bool,
    pub(crate) duplicate: bool,
}

impl ObservedTransition {
    fn learning(state: ObservedState, duplicate: bool) -> Self {
        Self {
            state,
            duration_seconds: None,
            became_ready: false,
            duplicate,
        }
    }

    fn ready(state: ObservedState, became_ready: bool, duplicate: bool) -> Self {
        let duration_seconds = match &state {
            ObservedState::Ready {
                duration_seconds, ..
            } => Some(*duration_seconds),
            _ => None,
        };
        Self {
            state,
            duration_seconds,
            became_ready,
            duplicate,
        }
    }
}

pub(crate) fn ready_state(reset_at: i64, now: i64, duration_seconds: i64) -> Option<ObservedState> {
    if !valid_duration(duration_seconds) {
        return None;
    }
    let cycle_started_at = reset_at.checked_sub(duration_seconds)?;
    let confirmation_deadline = cycle_started_at.checked_add(ROLLOVER_GRACE_SECONDS)?;
    (cycle_started_at <= now && now <= confirmation_deadline && now < reset_at).then_some(
        ObservedState::Ready {
            cycle_started_at,
            reset_at,
            duration_seconds,
            confirmed_at: now,
            last_seen_at: now,
        },
    )
}

pub(crate) fn observed_duration(state: &ObservedState) -> Option<i64> {
    match state {
        ObservedState::Ready {
            duration_seconds, ..
        } => Some(*duration_seconds),
        _ => None,
    }
}

pub(crate) fn validate_observed_state(state: &ObservedState) -> bool {
    match state {
        ObservedState::Watching {
            reset_at,
            first_seen_at,
            last_seen_at,
            consecutive_count,
        } => {
            *consecutive_count > 0
                && *consecutive_count <= 2
                && first_seen_at <= last_seen_at
                && *last_seen_at < *reset_at
        }
        ObservedState::Candidate {
            old_reset_at,
            old_seen_at,
            new_reset_at,
            first_new_seen_at,
        } => {
            let Some(old_boundary_start) = old_reset_at.checked_sub(ROLLOVER_GRACE_SECONDS) else {
                return false;
            };
            let Some(old_boundary_end) = old_reset_at.checked_add(ROLLOVER_GRACE_SECONDS) else {
                return false;
            };
            old_reset_at < new_reset_at
                && new_reset_at
                    .checked_sub(*old_reset_at)
                    .is_some_and(valid_duration)
                && *old_seen_at >= old_boundary_start
                && *old_seen_at < *old_reset_at
                && *first_new_seen_at >= *old_reset_at
                && *first_new_seen_at <= old_boundary_end
                && *old_seen_at <= *first_new_seen_at
                && *first_new_seen_at < *new_reset_at
        }
        ObservedState::Ready {
            cycle_started_at,
            reset_at,
            duration_seconds,
            confirmed_at,
            last_seen_at,
        } => {
            let Some(confirmation_deadline) = cycle_started_at.checked_add(ROLLOVER_GRACE_SECONDS)
            else {
                return false;
            };
            valid_duration(*duration_seconds)
                && cycle_started_at.checked_add(*duration_seconds) == Some(*reset_at)
                && *cycle_started_at <= *confirmed_at
                && *confirmed_at <= confirmation_deadline
                && *confirmed_at <= *last_seen_at
                && *last_seen_at < *reset_at
        }
    }
}

fn watching(reset_at: i64, now: i64) -> ObservedState {
    ObservedState::Watching {
        reset_at,
        first_seen_at: now,
        last_seen_at: now,
        consecutive_count: 1,
    }
}

fn update_seen(previous: i64, now: i64) -> i64 {
    previous.max(now)
}

fn within_after(reset_at: i64, now: i64) -> bool {
    now >= reset_at
        && reset_at
            .checked_add(ROLLOVER_GRACE_SECONDS)
            .is_some_and(|deadline| now <= deadline)
}

fn seen_near_boundary(seen_at: i64, reset_at: i64) -> bool {
    reset_at
        .checked_sub(ROLLOVER_GRACE_SECONDS)
        .is_some_and(|start| seen_at >= start && seen_at <= reset_at)
}

fn candidate(
    old_reset_at: i64,
    old_seen_at: i64,
    new_reset_at: i64,
    now: i64,
) -> Option<ObservedState> {
    let duration_seconds = new_reset_at.checked_sub(old_reset_at)?;
    valid_duration(duration_seconds).then_some(ObservedState::Candidate {
        old_reset_at,
        old_seen_at,
        new_reset_at,
        first_new_seen_at: now,
    })
}

/// Advance the durable observed rollover state machine by one provider reading.
/// The previous state is never mutated in place, so a failed history save can
/// discard the returned transition and leave the last committed transaction
/// intact.
pub(crate) fn observe_reset(
    previous: Option<&ObservedState>,
    reset_at: i64,
    now: i64,
) -> Result<ObservedTransition, DurationUnavailableReason> {
    if !valid_reset(reset_at, now) {
        return Err(DurationUnavailableReason::InvalidEvidence);
    }

    let Some(previous) = previous else {
        return Ok(ObservedTransition::learning(watching(reset_at, now), false));
    };
    if !validate_observed_state(previous) {
        return Err(DurationUnavailableReason::InvalidEvidence);
    }

    match previous {
        ObservedState::Watching {
            reset_at: old_reset_at,
            first_seen_at,
            last_seen_at,
            consecutive_count,
        } => {
            if reset_at == *old_reset_at {
                let state = ObservedState::Watching {
                    reset_at: *old_reset_at,
                    first_seen_at: *first_seen_at,
                    last_seen_at: update_seen(*last_seen_at, now),
                    consecutive_count: consecutive_count.saturating_add(1).min(2),
                };
                return Ok(ObservedTransition::learning(state, true));
            }

            if reset_at > *old_reset_at
                && *consecutive_count >= 2
                && within_after(*old_reset_at, now)
                && seen_near_boundary(*last_seen_at, *old_reset_at)
            {
                if let Some(state) = candidate(*old_reset_at, *last_seen_at, reset_at, now) {
                    return Ok(ObservedTransition::learning(state, false));
                }
            }

            // A forward slide before expiry, a backward reset, an implausible
            // gap, or an unstable old baseline all restart learning at the
            // newest reset without manufacturing a cycle count.
            Ok(ObservedTransition::learning(watching(reset_at, now), false))
        }
        ObservedState::Candidate {
            old_reset_at,
            new_reset_at,
            first_new_seen_at,
            ..
        } => {
            let within_confirmation_window = old_reset_at
                .checked_add(ROLLOVER_GRACE_SECONDS)
                .is_some_and(|deadline| now <= deadline);
            if reset_at == *new_reset_at && now >= *first_new_seen_at && within_confirmation_window
            {
                let duration_seconds = new_reset_at
                    .checked_sub(*old_reset_at)
                    .filter(|duration| valid_duration(*duration))
                    .ok_or(DurationUnavailableReason::InvalidEvidence)?;
                let state = ObservedState::Ready {
                    cycle_started_at: *old_reset_at,
                    reset_at: *new_reset_at,
                    duration_seconds,
                    confirmed_at: now,
                    last_seen_at: now,
                };
                return Ok(ObservedTransition::ready(state, true, false));
            }

            // Any changed, reversed, early, or late candidate is not a
            // confirmed boundary; restart from the newest reset.
            Ok(ObservedTransition::learning(watching(reset_at, now), false))
        }
        ObservedState::Ready {
            cycle_started_at,
            reset_at: old_reset_at,
            duration_seconds,
            confirmed_at,
            last_seen_at,
        } => {
            if reset_at == *old_reset_at {
                let state = ObservedState::Ready {
                    cycle_started_at: *cycle_started_at,
                    reset_at: *old_reset_at,
                    duration_seconds: *duration_seconds,
                    confirmed_at: *confirmed_at,
                    last_seen_at: update_seen(*last_seen_at, now),
                };
                return Ok(ObservedTransition::ready(state, false, true));
            }

            if reset_at > *old_reset_at
                && within_after(*old_reset_at, now)
                && seen_near_boundary(*last_seen_at, *old_reset_at)
            {
                if let Some(state) = candidate(*old_reset_at, *last_seen_at, reset_at, now) {
                    return Ok(ObservedTransition::learning(state, false));
                }
            }

            // Sliding, backward, or missed boundaries restart from the current
            // reset. In particular, never divide a large reset delta by a
            // guessed cycle count.
            Ok(ObservedTransition::learning(watching(reset_at, now), false))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HOUR: i64 = 3_600;
    const DAY: i64 = 86_400;

    #[test]
    fn precedence_prefers_provider_then_contract_then_observed() {
        let now = 10_000;
        let provider_reset = now + 5 * HOUR;
        let provider = DurationEvidence::provider(provider_reset, 5 * HOUR);
        let contract_reset = now + 7 * DAY;
        let contract = DurationEvidence::contract(7 * DAY);
        let observed = DurationEvidence::observed(contract_reset, 7 * DAY);

        assert_eq!(
            resolve_duration(
                now,
                Some(provider_reset),
                Some(provider),
                Some(contract),
                Some(observed)
            ),
            DurationResolution::Ready {
                duration_seconds: 5 * HOUR,
                source: DurationSource::Provider,
            }
        );
        assert_eq!(
            resolve_duration(
                now,
                Some(contract_reset),
                None,
                Some(contract),
                Some(observed)
            ),
            DurationResolution::Ready {
                duration_seconds: 7 * DAY,
                source: DurationSource::Contract,
            }
        );
        assert_eq!(
            resolve_duration(now, Some(contract_reset), None, None, Some(observed)),
            DurationResolution::Ready {
                duration_seconds: 7 * DAY,
                source: DurationSource::Observed,
            }
        );
    }

    /// The boundary #296 turned on. A provider answering a window with no
    /// usage yet reports `reset_at = <its now> + duration`, so the cycle's
    /// start IS the instant the response was built. Evidence evaluated against
    /// that instant is Ready; evaluated against a clock reading from one
    /// second earlier — which is what holding `Utc::now()` across the request
    /// produced — the same evidence is rejected and the window loses its
    /// duration, its `window_minutes` and its pace with it.
    #[test]
    fn a_window_starting_exactly_now_is_ready_and_one_second_early_is_not() {
        let response_at = 1_788_848_567;
        let reset = response_at + 5 * HOUR;
        let evidence = DurationEvidence::provider(reset, 5 * HOUR);

        assert_eq!(
            resolve_duration(response_at, Some(reset), Some(evidence), None, None),
            DurationResolution::Ready {
                duration_seconds: 5 * HOUR,
                source: DurationSource::Provider,
            }
        );
        assert_eq!(
            resolve_duration(response_at - 1, Some(reset), Some(evidence), None, None),
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
    }

    #[test]
    fn malformed_present_evidence_is_invalid_without_fallback() {
        let now = 10_000;
        let reset = now + 7 * DAY;
        assert_eq!(
            resolve_duration(
                now,
                Some(reset),
                Some(DurationEvidence::provider(reset + 1, 5 * HOUR)),
                Some(DurationEvidence::contract(7 * DAY)),
                None,
            ),
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
        assert_eq!(
            resolve_duration(
                now,
                Some(reset),
                None,
                Some(DurationEvidence::contract(5 * HOUR)),
                Some(DurationEvidence::observed(reset, 7 * DAY)),
            ),
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
    }

    #[test]
    fn missing_and_invalid_evidence_are_typed() {
        assert_eq!(
            resolve_duration(10, None, None, None, None),
            DurationResolution::Unavailable(DurationUnavailableReason::MissingReset)
        );
        assert_eq!(
            resolve_duration(
                10,
                None,
                Some(DurationEvidence::provider(10 + DAY, DAY)),
                Some(DurationEvidence::contract(DAY)),
                Some(DurationEvidence::observed(10 + DAY, DAY)),
            ),
            DurationResolution::Unavailable(DurationUnavailableReason::MissingReset)
        );
        assert_eq!(
            resolve_duration(
                10,
                Some(10),
                Some(DurationEvidence::provider(10, 0)),
                None,
                Some(DurationEvidence::observed(10, 0)),
            ),
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
    }

    #[test]
    fn five_hour_and_seven_day_durations_are_exact() {
        let now = 10_000;
        for duration in [5 * HOUR, 7 * DAY] {
            let reset = now + duration;
            assert_eq!(
                resolve_duration(
                    now,
                    Some(reset),
                    None,
                    Some(DurationEvidence::contract(duration)),
                    None,
                ),
                DurationResolution::Ready {
                    duration_seconds: duration,
                    source: DurationSource::Contract,
                }
            );
        }
        let reset = now + MAX_DURATION_SECONDS;
        assert!(matches!(
            resolve_duration(
                now,
                Some(reset),
                None,
                Some(DurationEvidence::contract(MAX_DURATION_SECONDS)),
                None,
            ),
            DurationResolution::Ready {
                duration_seconds: MAX_DURATION_SECONDS,
                source: DurationSource::Contract,
            }
        ));
        assert_eq!(
            resolve_duration(
                now,
                Some(reset),
                None,
                Some(DurationEvidence::contract(MAX_DURATION_SECONDS + 1)),
                None,
            ),
            DurationResolution::Unavailable(DurationUnavailableReason::InvalidEvidence)
        );
    }

    #[test]
    fn copilot_calendar_duration_preserves_28_to_31_day_months() {
        let cases = [
            ("2023-03-01T00:00:00Z", 28 * DAY),
            ("2024-03-01T00:00:00Z", 29 * DAY),
            ("2023-05-01T00:00:00Z", 30 * DAY),
            ("2023-08-01T00:00:00Z", 31 * DAY),
        ];
        for (text, expected) in cases {
            let reset = text.parse::<chrono::DateTime<Utc>>().unwrap().timestamp();
            assert_eq!(copilot_calendar_duration(reset), Some(expected));
        }
        let not_month_start = "2023-08-02T00:00:00Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap()
            .timestamp();
        let not_midnight = "2023-08-01T00:00:01Z"
            .parse::<chrono::DateTime<Utc>>()
            .unwrap()
            .timestamp();
        assert_eq!(copilot_calendar_duration(not_month_start), None);
        assert_eq!(copilot_calendar_duration(not_midnight), None);
    }

    #[test]
    fn observed_requires_stable_old_reset_boundary_and_next_poll_confirmation() {
        let now = 1_000_000;
        let old_reset = now + DAY;
        let new_reset = old_reset + 7 * DAY;
        let first = observe_reset(None, old_reset, now).unwrap();
        assert!(matches!(
            first.state,
            ObservedState::Watching {
                consecutive_count: 1,
                ..
            }
        ));
        let stable = observe_reset(Some(&first.state), old_reset, old_reset - 5 * 60).unwrap();
        assert!(matches!(
            stable.state,
            ObservedState::Watching {
                consecutive_count: 2,
                ..
            }
        ));
        let candidate = observe_reset(Some(&stable.state), new_reset, old_reset + 5 * 60).unwrap();
        assert!(matches!(candidate.state, ObservedState::Candidate { .. }));
        assert_eq!(candidate.duration_seconds, None);
        let ready = observe_reset(Some(&candidate.state), new_reset, old_reset + 10 * 60).unwrap();
        assert!(ready.became_ready);
        assert_eq!(ready.duration_seconds, Some(7 * DAY));
        assert!(matches!(ready.state, ObservedState::Ready { .. }));
    }

    #[test]
    fn candidate_confirms_at_exact_fifteen_minute_boundary() {
        let old_reset = 1_000_000;
        let new_reset = old_reset + 7 * DAY;
        let stable = ObservedState::Watching {
            reset_at: old_reset,
            first_seen_at: old_reset - ROLLOVER_GRACE_SECONDS,
            last_seen_at: old_reset - 1,
            consecutive_count: 2,
        };
        let candidate = observe_reset(Some(&stable), new_reset, old_reset).unwrap();
        let confirmed = observe_reset(
            Some(&candidate.state),
            new_reset,
            old_reset + ROLLOVER_GRACE_SECONDS,
        )
        .unwrap();
        assert!(confirmed.became_ready);
        assert_eq!(confirmed.duration_seconds, Some(7 * DAY));
    }

    #[test]
    fn candidate_confirmation_after_timeout_restarts_watching() {
        let old_reset = 1_000_000;
        let new_reset = old_reset + 7 * DAY;
        let candidate = ObservedState::Candidate {
            old_reset_at: old_reset,
            old_seen_at: old_reset - 60,
            new_reset_at: new_reset,
            first_new_seen_at: old_reset,
        };
        let expired = observe_reset(
            Some(&candidate),
            new_reset,
            old_reset + ROLLOVER_GRACE_SECONDS + 1,
        )
        .unwrap();
        assert_eq!(expired.duration_seconds, None);
        assert!(matches!(
            expired.state,
            ObservedState::Watching {
                reset_at,
                consecutive_count: 1,
                ..
            } if reset_at == new_reset
        ));
    }

    #[test]
    fn persisted_observed_state_validation_rejects_out_of_bounds_timestamps() {
        let invalid_watching = ObservedState::Watching {
            reset_at: 100,
            first_seen_at: 50,
            last_seen_at: 100,
            consecutive_count: 2,
        };
        assert!(!validate_observed_state(&invalid_watching));

        let invalid_candidate = ObservedState::Candidate {
            old_reset_at: 1_000,
            old_seen_at: 900,
            new_reset_at: 1_000 + DAY,
            first_new_seen_at: 1_000 + ROLLOVER_GRACE_SECONDS + 1,
        };
        assert!(!validate_observed_state(&invalid_candidate));

        let late_ready = ObservedState::Ready {
            cycle_started_at: 1_000,
            reset_at: 1_000 + DAY,
            duration_seconds: DAY,
            confirmed_at: 1_000 + ROLLOVER_GRACE_SECONDS + 1,
            last_seen_at: 1_000 + ROLLOVER_GRACE_SECONDS + 1,
        };
        assert!(!validate_observed_state(&late_ready));
        assert!(ready_state(1_000 + DAY, 1_000 + ROLLOVER_GRACE_SECONDS + 1, DAY,).is_none());
    }

    #[test]
    fn duplicate_reset_updates_stability_without_manufacturing_boundary() {
        let now = 1_000_000;
        let reset = now + DAY;
        let first = observe_reset(None, reset, now).unwrap();
        let duplicate = observe_reset(Some(&first.state), reset, now + 1).unwrap();
        assert!(duplicate.duplicate);
        assert!(!duplicate.became_ready);
        assert_eq!(duplicate.duration_seconds, None);
        assert!(matches!(
            duplicate.state,
            ObservedState::Watching {
                consecutive_count: 2,
                ..
            }
        ));
        let ready = ready_state(reset, now, DAY).unwrap();
        let duplicate_ready = observe_reset(Some(&ready), reset, now + 1).unwrap();
        assert!(duplicate_ready.duplicate);
        assert!(!duplicate_ready.became_ready);
        assert_eq!(duplicate_ready.duration_seconds, Some(DAY));
    }

    #[test]
    fn sliding_backward_and_missed_boundaries_restart_learning() {
        let now = 1_000_000;
        let old = now + DAY;
        let stable = ObservedState::Watching {
            reset_at: old,
            first_seen_at: old - 10 * 60,
            last_seen_at: old - 5 * 60,
            consecutive_count: 2,
        };
        let sliding = observe_reset(Some(&stable), old + DAY, old - DAY).unwrap();
        assert!(
            matches!(sliding.state, ObservedState::Watching { reset_at, consecutive_count: 1, .. } if reset_at == old + DAY)
        );
        let backward = observe_reset(Some(&stable), old - HOUR, old - 2 * HOUR).unwrap();
        assert!(
            matches!(backward.state, ObservedState::Watching { reset_at, consecutive_count: 1, .. } if reset_at == old - HOUR)
        );

        let missed =
            observe_reset(Some(&stable), old + DAY, old + ROLLOVER_GRACE_SECONDS + 1).unwrap();
        assert!(
            matches!(missed.state, ObservedState::Watching { reset_at, consecutive_count: 1, .. } if reset_at == old + DAY)
        );
    }

    #[test]
    fn candidate_change_does_not_divide_or_guess_cycle_count() {
        let old = 1_000_000;
        let candidate = ObservedState::Candidate {
            old_reset_at: old,
            old_seen_at: old - 60,
            new_reset_at: old + 7 * DAY,
            first_new_seen_at: old + 60,
        };
        let changed = observe_reset(Some(&candidate), old + 14 * DAY, old + 120).unwrap();
        assert!(
            matches!(changed.state, ObservedState::Watching { reset_at, .. } if reset_at == old + 14 * DAY)
        );
        assert_eq!(changed.duration_seconds, None);
    }

    #[test]
    fn ready_state_validation_keeps_exact_duration() {
        let reset = 10_000_000;
        let state = ready_state(reset, reset - 5 * HOUR + 10 * 60, 5 * HOUR).unwrap();
        assert!(validate_observed_state(&state));
        assert_eq!(observed_duration(&state), Some(5 * HOUR));
    }
}
