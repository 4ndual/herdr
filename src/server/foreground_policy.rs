//! fork: pure foreground-assignment policy for the headless multi-client server.
//!
//! The headless server pins the shared pane PTYs to a single "foreground"
//! client's size (see `HeadlessServer::effective_size`). With more than one
//! client attached, pinning to the wrong client silently clips the other
//! client's bottom rows (the usage strip / plugin footer paints over the
//! pane's last rows). These helpers decide *who* should hold foreground given
//! per-client activity timestamps, with no PTYs or async involved, so the
//! policy is unit-testable in isolation. The headless server owns the wiring
//! and side effects; this module owns only the decisions.

use std::time::{Duration, Instant};

/// Typing (keys / mouse / paste) from a non-foreground client only steals
/// foreground once the current foreground client has been quiet for at least
/// this long. Without a gap, two people typing at once would thrash shared-pane
/// resizes on every keystroke. Two seconds is long enough to absorb a normal
/// typing cadence yet short enough that reclaiming panes from an idle/stale
/// client feels immediate.
pub(crate) const FOREGROUND_STEAL_HYSTERESIS: Duration = Duration::from_secs(2);

/// A foreground client with no activity for longer than this is demoted so a
/// stale attach (e.g. a leftover QA client at a smaller size) cannot keep the
/// shared panes pinned to its geometry forever while a live client only reads
/// output without typing.
pub(crate) const FOREGROUND_IDLE_DEMOTE_AFTER: Duration = Duration::from_secs(600);

/// Snapshot of one app client's activity used for foreground decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ClientActivity {
    pub(crate) client_id: u64,
    /// Wall-clock time of this client's most recent activity: real input,
    /// window focus, resize, or initial attach. Monotonic per client.
    pub(crate) last_active_at: Instant,
}

/// Decide whether typing from `candidate_id` should take foreground from the
/// current holder.
///
/// - No current holder: the candidate takes it.
/// - Candidate already holds it: no change.
/// - Otherwise: steal only once the holder has been quiet for `>= hysteresis`.
pub(crate) fn should_steal_foreground(
    foreground: Option<ClientActivity>,
    candidate_id: u64,
    now: Instant,
    hysteresis: Duration,
) -> bool {
    match foreground {
        None => true,
        Some(fg) if fg.client_id == candidate_id => false,
        Some(fg) => now.saturating_duration_since(fg.last_active_at) >= hysteresis,
    }
}

/// Pick the client foreground should move to when the current holder has gone
/// idle, or `None` to leave it in place.
///
/// Returns a target only when the holder has been idle for `>= idle_timeout`
/// *and* some other client has been active more recently than the holder. The
/// "more recently active" guard keeps demotion from thrashing or from handing
/// foreground to an even-more-idle client when everyone is quiet.
pub(crate) fn idle_demotion_target(
    foreground: ClientActivity,
    others: &[ClientActivity],
    now: Instant,
    idle_timeout: Duration,
) -> Option<u64> {
    if now.saturating_duration_since(foreground.last_active_at) < idle_timeout {
        return None;
    }
    others
        .iter()
        .filter(|other| other.last_active_at > foreground.last_active_at)
        .max_by_key(|other| other.last_active_at)
        .map(|other| other.client_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(base: Instant, secs: u64) -> Instant {
        base + Duration::from_secs(secs)
    }

    #[test]
    fn no_holder_lets_any_client_take_foreground() {
        let now = Instant::now();
        assert!(should_steal_foreground(
            None,
            7,
            now,
            FOREGROUND_STEAL_HYSTERESIS
        ));
    }

    #[test]
    fn single_client_typing_is_a_no_op() {
        // The one client is already foreground: its own input never reassigns.
        let base = Instant::now();
        let fg = ClientActivity {
            client_id: 1,
            last_active_at: base,
        };
        assert!(!should_steal_foreground(
            Some(fg),
            1,
            at(base, 30),
            FOREGROUND_STEAL_HYSTERESIS
        ));
        // And there is no one to demote to.
        assert_eq!(
            idle_demotion_target(fg, &[], at(base, 3600), FOREGROUND_IDLE_DEMOTE_AFTER),
            None
        );
    }

    #[test]
    fn hysteresis_window_blocks_then_allows_steal() {
        let base = Instant::now();
        let fg = ClientActivity {
            client_id: 1,
            last_active_at: base,
        };
        // Just under the window: holder keeps foreground.
        assert!(!should_steal_foreground(
            Some(fg),
            2,
            base + Duration::from_millis(1999),
            FOREGROUND_STEAL_HYSTERESIS
        ));
        // At/after the window: the other client steals it.
        assert!(should_steal_foreground(
            Some(fg),
            2,
            base + Duration::from_secs(2),
            FOREGROUND_STEAL_HYSTERESIS
        ));
    }

    #[test]
    fn two_clients_alternating_input_do_not_thrash() {
        // Client 1 holds foreground and keeps typing every second. Client 2's
        // interleaved keystrokes must not steal foreground while 1 stays active
        // within the hysteresis window.
        let base = Instant::now();
        let mut foreground = 1u64;
        let mut fg_last_active = base;

        // t=0.0 client 1 types (already foreground) -> stays.
        // t=0.5 client 2 types -> holder quiet 0.5s < 2s -> no steal.
        assert!(!should_steal_foreground(
            Some(ClientActivity {
                client_id: foreground,
                last_active_at: fg_last_active,
            }),
            2,
            base + Duration::from_millis(500),
            FOREGROUND_STEAL_HYSTERESIS
        ));
        // t=1.0 client 1 types again -> refreshes holder activity.
        fg_last_active = base + Duration::from_millis(1000);
        // t=1.5 client 2 types -> holder quiet 0.5s -> still no steal.
        assert!(!should_steal_foreground(
            Some(ClientActivity {
                client_id: foreground,
                last_active_at: fg_last_active,
            }),
            2,
            base + Duration::from_millis(1500),
            FOREGROUND_STEAL_HYSTERESIS
        ));
        // Now client 1 goes quiet. t=3.1 client 2 types -> holder quiet 2.1s.
        assert!(should_steal_foreground(
            Some(ClientActivity {
                client_id: foreground,
                last_active_at: fg_last_active,
            }),
            2,
            base + Duration::from_millis(3100),
            FOREGROUND_STEAL_HYSTERESIS
        ));
        foreground = 2;
        assert_eq!(foreground, 2);
    }

    #[test]
    fn idle_demotion_hands_off_to_most_recently_active_other() {
        let base = Instant::now();
        let fg = ClientActivity {
            client_id: 1,
            last_active_at: base,
        };
        let others = [
            ClientActivity {
                client_id: 2,
                last_active_at: at(base, 120),
            },
            ClientActivity {
                client_id: 3,
                last_active_at: at(base, 300),
            },
        ];
        // Before the idle timeout: no demotion.
        assert_eq!(
            idle_demotion_target(fg, &others, at(base, 300), FOREGROUND_IDLE_DEMOTE_AFTER),
            None
        );
        // After the timeout: hands off to the most recently active other (3).
        assert_eq!(
            idle_demotion_target(fg, &others, at(base, 601), FOREGROUND_IDLE_DEMOTE_AFTER),
            Some(3)
        );
    }

    #[test]
    fn idle_demotion_skips_when_no_other_is_more_recent() {
        // Holder is idle past the timeout but is still the most recently active
        // of everyone, so demoting to a more-idle client would be pointless.
        let base = Instant::now();
        let fg = ClientActivity {
            client_id: 1,
            last_active_at: at(base, 500),
        };
        let others = [ClientActivity {
            client_id: 2,
            last_active_at: at(base, 100),
        }];
        assert_eq!(
            idle_demotion_target(fg, &others, at(base, 5000), FOREGROUND_IDLE_DEMOTE_AFTER),
            None
        );
    }
}
