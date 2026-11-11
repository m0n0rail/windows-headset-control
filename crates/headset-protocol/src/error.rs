use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("expected a {expected}-byte report, got {actual}")]
    UnexpectedLength { expected: usize, actual: usize },

    #[error("expected report id {expected:#04x}, got {actual:#04x}")]
    UnexpectedReportId { expected: u8, actual: u8 },

    #[error(
        "parameter {param:#04x} is not in the observed {} allowlist; \
         only identifiers seen on the wire may be sent",
        if *write { "write" } else { "read" }
    )]
    NotAllowlisted { param: u8, write: bool },

    #[error("parameter {param:#04x} does not take an index operand")]
    UnexpectedIndex { param: u8 },

    #[error(
        "parameter {param:#04x} was observed being written with {expected} payload byte(s), \
         not {actual}"
    )]
    WrongWritePayloadLen {
        param: u8,
        expected: usize,
        actual: usize,
    },

    #[error(
        "parameter {param:#04x} was never observed carrying {value:#04x} in payload byte \
         {byte}; only values seen on the wire may be sent"
    )]
    UnobservedValue { param: u8, byte: usize, value: u8 },

    #[error("payload of {actual} bytes exceeds the {max} that fit before the checksum")]
    PayloadTooLong { max: usize, actual: usize },

    #[error("checksum mismatch: computed {expected:#04x}, frame carries {actual:#04x}")]
    ChecksumMismatch { expected: u8, actual: u8 },

    #[error("implausible data_size {data_size}")]
    ImplausibleDataSize { data_size: usize },

    #[error(
        "payload length disagreement: byte 12 declares {declared}, data_size implies {implied}"
    )]
    LengthDisagreement { declared: u8, implied: usize },

    #[error("unknown role byte {role:#04x}")]
    UnknownRole { role: u8 },

    #[error("malformed message at byte {offset}: {why}")]
    MalformedMessage { offset: usize, why: &'static str },
}
