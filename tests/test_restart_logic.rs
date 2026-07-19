//! Tests for the auto-restart state machine.
//!
//! These test the state transitions that govern backend-owned restart:
//! - restart_generation increment on start
//! - restart_generation bump on stop (invalidates pending restart)
//! - stale generation rejection
//! - crash-loop counter (same policy for MC and Terraria)
//! - profile_id retention across restart

use std::collections::VecDeque;

// We test the logic patterns used in lib.rs by simulating the
// ServerManager fields and the restart decision code.

/// Simulated server state for testing restart logic.
struct RestartState {
    pub stop_requested: bool,
    pub status: ServerStatus,
    pub restart_generation: u64,
    pub profile_id: Option<String>,
    pub server_dir: Option<String>,
    pub recent_auto_restarts: VecDeque<std::time::Instant>,
}

#[derive(Debug, Clone, PartialEq)]
enum ServerStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
}

impl RestartState {
    fn new() -> Self {
        Self {
            stop_requested: false,
            status: ServerStatus::Stopped,
            restart_generation: 0,
            profile_id: None,
            server_dir: None,
            recent_auto_restarts: VecDeque::new(),
        }
    }

    /// Simulates do_start_server's state capture.
    fn start_server(&mut self, profile_id: String, server_dir: String) {
        self.status = ServerStatus::Starting;
        self.stop_requested = false;
        self.profile_id = Some(profile_id);
        self.server_dir = Some(server_dir);
        self.restart_generation = self.restart_generation.wrapping_add(1);
    }

    /// Simulates graceful_or_force_stop_server's generation bump.
    fn stop_server(&mut self) {
        self.stop_requested = true;
        self.restart_generation = self.restart_generation.wrapping_add(1);
    }

    /// Simulates restore_backup's generation bump.
    fn begin_restore(&mut self) {
        self.restart_generation = self.restart_generation.wrapping_add(1);
    }

    /// Simulates the exit handler's restart decision.
    /// Returns (should_restart, captured_generation, captured_profile_id).
    fn capture_restart_context(&self) -> (bool, u64, Option<String>) {
        (
            !self.stop_requested && self.status == ServerStatus::Stopped,
            self.restart_generation,
            self.profile_id.clone(),
        )
    }

    /// Simulates the delayed restart's staleness check.
    /// Returns true if the restart should proceed.
    fn validate_restart(
        &self,
        captured_generation: u64,
        max_restarts: usize,
        window: std::time::Duration,
    ) -> bool {
        // Check stop_requested and status
        if self.stop_requested || self.status != ServerStatus::Stopped {
            return false;
        }
        // Check generation
        if self.restart_generation != captured_generation {
            return false;
        }
        // Check crash-loop
        let now = std::time::Instant::now();
        let mut count = 0;
        for &t in &self.recent_auto_restarts {
            if now.duration_since(t) <= window {
                count += 1;
            }
        }
        count < max_restarts
    }

    fn record_restart(&mut self) {
        self.recent_auto_restarts
            .push_back(std::time::Instant::now());
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[test]
fn test_restart_generation_increments_on_start() {
    let mut state = RestartState::new();
    assert_eq!(state.restart_generation, 0);

    state.start_server("profile-1".into(), "/srv/mc".into());
    assert_eq!(state.restart_generation, 1);

    state.status = ServerStatus::Stopped;
    state.start_server("profile-1".into(), "/srv/mc".into());
    assert_eq!(state.restart_generation, 2);
}

#[test]
fn test_restart_generation_bumps_on_stop() {
    let mut state = RestartState::new();
    state.start_server("profile-1".into(), "/srv/mc".into());
    assert_eq!(state.restart_generation, 1);

    state.stop_server();
    assert_eq!(state.restart_generation, 2);
    assert!(state.stop_requested);
}

#[test]
fn test_stale_restart_rejected_after_manual_stop() {
    let mut state = RestartState::new();
    state.start_server("profile-1".into(), "/srv/mc".into());
    let gen_at_start = state.restart_generation;

    // Server crashes — capture context
    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, captured_gen, _) = state.capture_restart_context();
    assert_eq!(captured_gen, gen_at_start);

    // User manually stops (bumps generation)
    state.stop_server();
    assert_eq!(state.restart_generation, gen_at_start + 1);

    // Delayed restart should be rejected (generation mismatch)
    state.stop_requested = false; // server already stopped
    assert!(
        !state.validate_restart(captured_gen, 3, std::time::Duration::from_secs(300)),
        "Stale restart must be rejected after manual stop"
    );
}

#[test]
fn test_stale_restart_rejected_after_new_session() {
    let mut state = RestartState::new();
    state.start_server("profile-1".into(), "/srv/mc".into());
    let gen_first = state.restart_generation;

    // Server crashes
    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, captured_gen, _) = state.capture_restart_context();

    // User starts a new session (increments generation)
    state.start_server("profile-1".into(), "/srv/mc".into());
    assert_eq!(state.restart_generation, gen_first + 1);

    // Original restart should be rejected
    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    assert!(
        !state.validate_restart(captured_gen, 3, std::time::Duration::from_secs(300)),
        "Stale restart must be rejected after new session starts"
    );
}

#[test]
fn test_stale_restart_rejected_after_restore() {
    let mut state = RestartState::new();
    state.start_server("profile-1".into(), "/srv/mc".into());

    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, captured_gen, _) = state.capture_restart_context();

    // Restore bumps generation
    state.begin_restore();

    assert!(
        !state.validate_restart(captured_gen, 3, std::time::Duration::from_secs(300)),
        "Stale restart must be rejected after restore begins"
    );
}

#[test]
fn test_original_profile_retained() {
    let mut state = RestartState::new();
    state.start_server("profile-abc".into(), "/srv/terraria".into());

    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, _, captured_pid) = state.capture_restart_context();

    assert_eq!(
        captured_pid,
        Some("profile-abc".into()),
        "Original profile_id must be captured for restart"
    );
}

#[test]
fn test_original_server_dir_retained() {
    let mut state = RestartState::new();
    state.start_server("p1".into(), "/srv/my-server".into());

    assert_eq!(
        state.server_dir,
        Some("/srv/my-server".into()),
        "Server dir must be captured at start"
    );
}

#[test]
fn test_valid_restart_proceeds() {
    let mut state = RestartState::new();
    state.start_server("profile-1".into(), "/srv/mc".into());

    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, captured_gen, _) = state.capture_restart_context();

    // No intervening actions — restart should proceed
    assert!(
        state.validate_restart(captured_gen, 3, std::time::Duration::from_secs(300)),
        "Valid restart with matching generation should proceed"
    );
}

#[test]
fn test_crash_loop_blocks_after_threshold() {
    let mut state = RestartState::new();
    state.start_server("p1".into(), "/srv/mc".into());
    state.status = ServerStatus::Stopped;

    // Record MAX_RESTARTS_IN_WINDOW (3) recent restarts
    for _ in 0..3 {
        state.record_restart();
    }

    let (_, captured_gen, _) = state.capture_restart_context();
    assert!(
        !state.validate_restart(captured_gen, 3, std::time::Duration::from_secs(300)),
        "Crash-loop guard must block after 3 restarts in window"
    );
}

#[test]
fn test_crash_loop_resets_after_window_expires() {
    let mut state = RestartState::new();

    // Simulate old restarts (outside the window)
    let old = std::time::Instant::now() - std::time::Duration::from_secs(600);
    state.recent_auto_restarts.push_back(old);
    state.recent_auto_restarts.push_back(old);
    state.recent_auto_restarts.push_back(old);

    state.start_server("p1".into(), "/srv/mc".into());
    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, captured_gen, _) = state.capture_restart_context();

    assert!(
        state.validate_restart(captured_gen, 3, std::time::Duration::from_secs(300)),
        "Crash-loop guard must allow restart after window expires"
    );
}

#[test]
fn test_crash_loop_same_policy_for_terraria_and_minecraft() {
    // Both use the same RestartState, same constants.
    // This test documents that the policy is identical.
    let max_restarts = 3;
    let window = std::time::Duration::from_secs(300);

    let mut mc_state = RestartState::new();
    mc_state.start_server("mc-profile".into(), "/srv/mc".into());
    mc_state.status = ServerStatus::Stopped;
    for _ in 0..max_restarts {
        mc_state.record_restart();
    }
    let (_, mc_gen, _) = mc_state.capture_restart_context();

    let mut terraria_state = RestartState::new();
    terraria_state.start_server("terraria-profile".into(), "/srv/terraria".into());
    terraria_state.status = ServerStatus::Stopped;
    for _ in 0..max_restarts {
        terraria_state.record_restart();
    }
    let (_, t_gen, _) = terraria_state.capture_restart_context();

    assert_eq!(
        mc_state.validate_restart(mc_gen, max_restarts, window),
        terraria_state.validate_restart(t_gen, max_restarts, window),
        "MC and Terraria must use the same crash-loop policy"
    );
}

#[test]
fn test_stop_requested_blocks_restart() {
    let mut state = RestartState::new();
    state.start_server("p1".into(), "/srv/mc".into());
    state.status = ServerStatus::Stopped;
    state.stop_requested = true; // user requested stop

    let (_, captured_gen, _) = state.capture_restart_context();
    // capture_restart_context returns false because stop_requested is true
    assert!(
        !state.capture_restart_context().0,
        "stop_requested must prevent restart capture"
    );
}

#[test]
fn test_running_status_blocks_restart() {
    let mut state = RestartState::new();
    state.start_server("p1".into(), "/srv/mc".into());
    // Server is still running
    state.status = ServerStatus::Running;
    state.stop_requested = false;

    assert!(
        !state.capture_restart_context().0,
        "Running server must not trigger restart"
    );
}

#[test]
fn test_generation_wraps_at_max() {
    let mut state = RestartState::new();
    state.restart_generation = u64::MAX;
    state.start_server("p1".into(), "/srv/mc".into());
    assert_eq!(
        state.restart_generation, 0,
        "Generation must wrap at u64::MAX"
    );
}

#[test]
fn test_multiple_crashes_then_manual_stop() {
    let mut state = RestartState::new();
    state.start_server("p1".into(), "/srv/mc".into());

    // Crash 1
    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, gen1, _) = state.capture_restart_context();
    state.record_restart();

    // Crash 2
    state.start_server("p1".into(), "/srv/mc".into());
    state.status = ServerStatus::Stopped;
    state.stop_requested = false;
    let (_, gen2, _) = state.capture_restart_context();
    state.record_restart();

    // Manual stop before crash 3 restart
    state.stop_server();

    // Crash 3 restart should be blocked by stop_requested + generation bump
    state.status = ServerStatus::Stopped;
    state.stop_requested = false; // server is stopped, but generation changed
    assert!(
        !state.validate_restart(gen2, 3, std::time::Duration::from_secs(300)),
        "Manual stop after crashes must invalidate pending restart"
    );
}
