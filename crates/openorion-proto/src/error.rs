use thiserror::Error;

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ProtoError {
    /// A value is outside the range the device accepts.
    #[error("{what} out of range: {value} (allowed {min}..={max})")]
    OutOfRange {
        what: &'static str,
        value: i64,
        min: i64,
        max: i64,
    },
    /// A raw byte does not map to any known enum variant.
    #[error("unknown {what} value {value}")]
    UnknownValue { what: &'static str, value: i64 },
    /// A reply body or report is shorter than its layout needs.
    #[error("{what}: need {need} bytes, got {got}")]
    Short {
        what: &'static str,
        need: usize,
        got: usize,
    },
    /// The payload would not fit in one 320-byte report.
    #[error("payload too long: {0} bytes")]
    TooLong(usize),
    /// `Report::raw_checked` refused a payload (pid not whitelisted, or a
    /// forbidden get).
    #[error("refused: {0}")]
    Refused(String),
    #[error("malformed {what}: {detail}")]
    Malformed { what: &'static str, detail: String },
}
