use std::fmt;

/// Failures that abort a whole batch. A failure of one item is not one of
/// these: it is recorded in that item's `ItemScore::error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecideError {
    /// sdd or bun cannot be found or started: nothing was asked of the model.
    Unavailable(String),
    /// The sdd job failed with no usable answers (a budget refusal, a timeout,
    /// a crashed script). The text is sdd's reason, bounded and printable.
    Failed(String),
    /// The caller passed something unusable (duplicate ids, empty question).
    InvalidInput(String),
}

impl fmt::Display for DecideError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(m) | Self::Failed(m) => f.write_str(m),
            Self::InvalidInput(m) => write!(f, "invalid input: {m}"),
        }
    }
}

impl std::error::Error for DecideError {}
