//! Types for the two-tier `keymux gamemode` override system.
//!
//! Tier 1 is the programmed detection heuristics (`niri::gamemode_detection`).
//! Tier 2 (`WindowOverride`) is a temporary, per-app_id override set via
//! `keymux gamemode window on|off|toggle|auto`. Tier 3 (`GlobalOverride`) is a
//! single system-wide kill-switch set via `keymux gamemode global`.
//!
//! Both tiers live only in the running root daemon's memory - they are not
//! persisted to disk and reset to "no override" (programmed rules apply) on
//! every daemon restart. This is intentional: the override system is meant
//! for quick, session-scoped adjustments, not a permanent rule change.

use serde::{Deserialize, Serialize};

/// A temporary override of the programmed detection rules for one specific
/// app_id.
///
/// Absence of an entry (not this enum) means "no override - use programmed
/// rules", so this type only ever represents an explicit choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum WindowOverride {
    /// Force game mode ON for this app_id, regardless of what the
    /// programmed rules would decide.
    On,
    /// Force game mode OFF for this app_id, regardless of what the
    /// programmed rules would decide.
    Off,
}

/// A system-wide override that, when not `Auto`, ignores every per-window
/// decision (both programmed rules and `WindowOverride`s) entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum GlobalOverride {
    /// Default: defer to window overrides, then programmed detection rules.
    #[default]
    Auto,
    /// Force game mode ON everywhere.
    AlwaysOn,
    /// Force game mode OFF everywhere.
    AlwaysOff,
}
