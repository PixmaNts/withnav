use std::time::Duration;

/// What happened on a document, as seen by the trigger policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerEvent {
    /// The user saved the document (`didSave`).
    Save,
    /// The idle timer expired: no edit for `idle_ms`.
    IdleExpired,
}

/// Inputs to the trigger policy: a pure decision function so thresholds can be
/// tuned and table-tested without a clock or an LLM.
#[derive(Debug, Clone, PartialEq)]
pub struct TriggerInput {
    pub event: TriggerEvent,
    /// Changed-line count since the baseline (text at last remark or open).
    pub changed_lines: usize,
    /// Time since the last edit. `None` when never edited.
    pub since_last_edit: Option<Duration>,
    /// Time since the last remark. `None` when no remark yet.
    pub since_last_remark: Option<Duration>,
}

/// Policy decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerOutcome {
    Fire,
    Skip(&'static str),
}

/// The trigger policy: fires on save or idle expiry, never directly on
/// didChange. Skips when disabled handled by the caller; here:
/// - fewer than `min_changed_lines` changed lines since baseline => skip
/// - a remark more recent than `cooldown_ms` => skip
pub fn decide(input: &TriggerInput, config: &super::config::NavigatorConfig) -> TriggerOutcome {
    if input.changed_lines < config.min_changed_lines {
        return TriggerOutcome::Skip("too_few_changed_lines");
    }
    if let Some(since) = input.since_last_remark {
        if since < Duration::from_millis(config.cooldown_ms) {
            return TriggerOutcome::Skip("cooldown");
        }
    }
    match input.event {
        TriggerEvent::Save | TriggerEvent::IdleExpired => TriggerOutcome::Fire,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigator::config::NavigatorConfig;

    fn input(event: TriggerEvent, changed: usize) -> TriggerInput {
        TriggerInput {
            event,
            changed_lines: changed,
            since_last_edit: Some(Duration::from_secs(5)),
            since_last_remark: None,
        }
    }

    #[test]
    fn save_with_enough_lines_fires() {
        let cfg = NavigatorConfig::default();
        assert_eq!(
            decide(&input(TriggerEvent::Save, 3), &cfg),
            TriggerOutcome::Fire
        );
    }

    #[test]
    fn idle_expiry_with_enough_lines_fires() {
        let cfg = NavigatorConfig::default();
        assert_eq!(
            decide(&input(TriggerEvent::IdleExpired, 5), &cfg),
            TriggerOutcome::Fire
        );
    }

    #[test]
    fn small_change_never_fires() {
        let cfg = NavigatorConfig::default();
        for event in [TriggerEvent::Save, TriggerEvent::IdleExpired] {
            assert_eq!(
                decide(&input(event, 2), &cfg),
                TriggerOutcome::Skip("too_few_changed_lines")
            );
        }
    }

    #[test]
    fn cooldown_blocks_remark_within_window() {
        let cfg = NavigatorConfig::default();
        let mut i = input(TriggerEvent::Save, 10);
        i.since_last_remark = Some(Duration::from_millis(19_999));
        assert_eq!(decide(&i, &cfg), TriggerOutcome::Skip("cooldown"));

        i.since_last_remark = Some(Duration::from_millis(20_000));
        assert_eq!(decide(&i, &cfg), TriggerOutcome::Fire);
    }

    #[test]
    fn no_previous_remark_has_no_cooldown() {
        let cfg = NavigatorConfig::default();
        let i = input(TriggerEvent::IdleExpired, 10);
        assert_eq!(decide(&i, &cfg), TriggerOutcome::Fire);
    }
}
