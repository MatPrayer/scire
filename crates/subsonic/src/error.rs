use thiserror::Error;

/// Subsonic API error codes, per the spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiErrorCode {
    /// 0 — generic error
    Generic,
    /// 10 — required parameter missing
    MissingParameter,
    /// 20 — incompatible client protocol version
    ClientTooOld,
    /// 30 — incompatible server protocol version
    ServerTooOld,
    /// 40 — wrong username or password
    WrongCredentials,
    /// 41 — token authentication not supported for LDAP users
    TokenAuthNotSupported,
    /// 50 — user not authorized for the operation
    NotAuthorized,
    /// 60 — trial period over
    TrialExpired,
    /// 70 — requested data not found
    NotFound,
    /// Anything else
    Other(u32),
}

impl From<u32> for ApiErrorCode {
    fn from(code: u32) -> Self {
        match code {
            0 => Self::Generic,
            10 => Self::MissingParameter,
            20 => Self::ClientTooOld,
            30 => Self::ServerTooOld,
            40 => Self::WrongCredentials,
            41 => Self::TokenAuthNotSupported,
            50 => Self::NotAuthorized,
            60 => Self::TrialExpired,
            70 => Self::NotFound,
            other => Self::Other(other),
        }
    }
}

#[derive(Debug, Error)]
pub enum Error {
    #[error("server error {code:?}: {message}")]
    Api { code: ApiErrorCode, message: String },

    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("invalid server url: {0}")]
    InvalidUrl(String),

    #[error("unexpected response: {0}")]
    UnexpectedResponse(String),
}

impl Error {
    /// True when the failure is wrong username/password.
    pub fn is_auth_failure(&self) -> bool {
        matches!(
            self,
            Error::Api {
                code: ApiErrorCode::WrongCredentials | ApiErrorCode::TokenAuthNotSupported,
                ..
            }
        )
    }
}
