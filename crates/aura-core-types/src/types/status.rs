//! Agent lifecycle status (persisted in store metadata).

/// Agent status values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum AgentStatus {
    /// Agent is active and processing
    #[default]
    Active = 0,
    /// Agent is paused
    Paused = 1,
    /// Agent is permanently stopped
    Dead = 2,
}

impl AgentStatus {
    /// Convert to byte.
    #[must_use]
    pub const fn as_byte(self) -> u8 {
        self as u8
    }

    /// Parse from byte.
    #[must_use]
    pub const fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Self::Active),
            1 => Some(Self::Paused),
            2 => Some(Self::Dead),
            _ => None,
        }
    }
}
