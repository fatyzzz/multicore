use thiserror::Error;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ConnectionState {
    #[default]
    Empty,
    Ready,
    Connecting,
    Connected,
    Error,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum StateError {
    #[error("invalid connection state transition from {from:?} to {to:?}")]
    InvalidTransition {
        from: ConnectionState,
        to: ConnectionState,
    },
}

#[derive(Debug, Default)]
pub struct StateMachine {
    state: ConnectionState,
}

impl StateMachine {
    pub fn current(&self) -> ConnectionState {
        self.state
    }

    pub fn transition(&mut self, next: ConnectionState) -> Result<(), StateError> {
        let valid = matches!(
            (self.state, next),
            (
                ConnectionState::Empty,
                ConnectionState::Ready | ConnectionState::Error
            ) | (
                ConnectionState::Ready,
                ConnectionState::Connecting | ConnectionState::Empty
            ) | (
                ConnectionState::Connecting,
                ConnectionState::Connected | ConnectionState::Error | ConnectionState::Ready
            ) | (
                ConnectionState::Connected,
                ConnectionState::Ready | ConnectionState::Error
            ) | (
                ConnectionState::Error,
                ConnectionState::Connecting | ConnectionState::Ready | ConnectionState::Empty
            )
        );
        if !valid {
            return Err(StateError::InvalidTransition {
                from: self.state,
                to: next,
            });
        }
        self.state = next;
        Ok(())
    }
}
