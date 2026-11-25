//! Spawn admission policy: whether a spawn may register over an existing entry.
//! Pure code motion out of `agent/mod.rs` (anti-monolith ceiling).

/// Registration policy of a spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SpawnAdmission {
    /// Ordinary spawn: registers unconditionally (replacing any entry).
    Always,
    /// Shell fallback of a reaped handle: register only if the id is still
    /// vacant. The reaper's `cleanup_agent` proved ownership only at the instant
    /// it removed its own entry; a competing spawn may publish a successor
    /// before this spawn is admitted to the transport lane, and the fallback
    /// must never overwrite it.
    VacantOnly,
}

/// Returned (inside `anyhow::Error`) by a [`SpawnAdmission::VacantOnly`] spawn
/// that found the id already taken by someone else. It is not a spawn failure:
/// the caller must not emit crash/clean-exit events or tear down by name.
#[derive(Debug)]
pub(super) struct SlotOccupied;

impl std::fmt::Display for SlotOccupied {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("registry slot already taken by a successor; spawn not admitted")
    }
}

impl std::error::Error for SlotOccupied {}
