//! Bounded, deliberately small protocol used across the privilege boundary.

use std::fmt;

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

pub const ELEVATION_PROTOCOL_VERSION: u16 = 1;
/// Hard limit for the complete pipe message, including its four-byte length prefix.
pub const MAX_ELEVATION_FRAME_BYTES: usize = 64 * 1024;
pub const SESSION_SECRET_BYTES: usize = 32;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ElevatedEngine {
    Xray,
    Mihomo,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ElevationCommand {
    StartXray { generation_id: u64 },
    StartMihomo { generation_id: u64 },
    Stop { engine: ElevatedEngine },
    Diagnostics,
    Shutdown,
}

impl ElevationCommand {
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::StartXray { generation_id } | Self::StartMihomo { generation_id }
                if *generation_id == 0 =>
            {
                Err(ProtocolError::InvalidGeneration)
            }
            _ => Ok(()),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BrokerErrorCode {
    NotReady,
    Unsupported,
    InvalidRequest,
    AuthenticationFailed,
    TimedOut,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ElevationResponse {
    Authenticated,
    Stopped,
    Diagnostics {
        xray_running: bool,
        mihomo_running: bool,
    },
    ShuttingDown,
    Error {
        code: BrokerErrorCode,
    },
}

#[derive(Clone, Eq, PartialEq)]
pub struct SessionSecret([u8; SESSION_SECRET_BYTES]);

impl SessionSecret {
    pub fn generate() -> Result<Self, getrandom::Error> {
        let mut bytes = [0_u8; SESSION_SECRET_BYTES];
        getrandom::fill(&mut bytes)?;
        Ok(Self(bytes))
    }

    #[must_use]
    pub const fn from_bytes(bytes: [u8; SESSION_SECRET_BYTES]) -> Self {
        Self(bytes)
    }

    #[must_use]
    pub const fn authentication(&self) -> Authentication {
        Authentication { secret: self.0 }
    }

    #[must_use]
    pub fn verifies(&self, authentication: &Authentication) -> bool {
        self.0
            .iter()
            .zip(authentication.secret.iter())
            .fold(0_u8, |difference, (left, right)| {
                difference | (left ^ right)
            })
            == 0
    }
}

impl fmt::Debug for SessionSecret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SessionSecret([REDACTED])")
    }
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Authentication {
    secret: [u8; SESSION_SECRET_BYTES],
}

impl fmt::Debug for Authentication {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Authentication { secret: [REDACTED] }")
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProtocolError {
    #[error("elevation frame has an invalid length")]
    InvalidLength,
    #[error("elevation frame is truncated")]
    Truncated,
    #[error("elevation frame has trailing bytes")]
    TrailingBytes,
    #[error("elevation frame is not valid UTF-8")]
    InvalidUtf8,
    #[error("elevation frame schema is invalid")]
    InvalidSchema,
    #[error("elevation protocol version is unsupported")]
    UnknownVersion,
    #[error("runtime generation identifier is invalid")]
    InvalidGeneration,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Envelope<T> {
    version: u16,
    body: T,
}

pub fn encode_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    let payload = serde_json::to_vec(&Envelope {
        version: ELEVATION_PROTOCOL_VERSION,
        body: value,
    })
    .map_err(|_| ProtocolError::InvalidSchema)?;
    if payload.is_empty() || payload.len() > MAX_ELEVATION_FRAME_BYTES - 4 {
        return Err(ProtocolError::InvalidLength);
    }
    let length = u32::try_from(payload.len()).map_err(|_| ProtocolError::InvalidLength)?;
    let mut frame = Vec::with_capacity(4 + payload.len());
    frame.extend_from_slice(&length.to_le_bytes());
    frame.extend_from_slice(&payload);
    Ok(frame)
}

pub fn decode_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, ProtocolError> {
    let header: [u8; 4] = frame
        .get(..4)
        .ok_or(ProtocolError::Truncated)?
        .try_into()
        .map_err(|_| ProtocolError::Truncated)?;
    let length = u32::from_le_bytes(header) as usize;
    if length == 0 || length > MAX_ELEVATION_FRAME_BYTES - 4 {
        return Err(ProtocolError::InvalidLength);
    }
    let expected = 4_usize
        .checked_add(length)
        .ok_or(ProtocolError::InvalidLength)?;
    if frame.len() < expected {
        return Err(ProtocolError::Truncated);
    }
    if frame.len() > expected {
        return Err(ProtocolError::TrailingBytes);
    }
    let payload = frame.get(4..expected).ok_or(ProtocolError::Truncated)?;
    std::str::from_utf8(payload).map_err(|_| ProtocolError::InvalidUtf8)?;
    let envelope: Envelope<T> =
        serde_json::from_slice(payload).map_err(|_| ProtocolError::InvalidSchema)?;
    if envelope.version != ELEVATION_PROTOCOL_VERSION {
        return Err(ProtocolError::UnknownVersion);
    }
    Ok(envelope.body)
}

pub fn decode_command(frame: &[u8]) -> Result<ElevationCommand, ProtocolError> {
    let command: ElevationCommand = decode_frame(frame)?;
    command.validate()?;
    Ok(command)
}
