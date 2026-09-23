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

    // No `#[from]`: the only conversion from `reqwest::Error` goes through
    // `client::http_error`, which calls `without_url()` first. Every request
    // URL carries `u`, `t` and `s` (the auth token and its salt) and
    // `reqwest::Error`'s `Display` prints the URL it failed on, so a derived
    // `From` is a standing invitation to paint the account's token into
    // whatever renders the error.
    #[error("http error: {0}")]
    Http(reqwest::Error),

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

    /// Worth trying again: the failure says nothing about the request itself.
    /// A 4xx, a bad URL or wrong credentials will fail identically forever, so
    /// only transport faults and the server's own 5xx/429 qualify.
    pub fn is_transient(&self) -> bool {
        match self {
            Error::Http(e) => {
                if let Some(status) = e.status() {
                    return status.is_server_error()
                        || status == reqwest::StatusCode::TOO_MANY_REQUESTS;
                }
                e.is_timeout() || e.is_connect() || e.is_request()
            }
            // The spec's catch-all: Navidrome answers with it for transient
            // internal faults as well as for genuine bad requests, and the
            // message is the only thing telling them apart, so it is retried.
            Error::Api {
                code: ApiErrorCode::Generic,
                ..
            } => true,
            _ => false,
        }
    }

    /// A sentence for the UI: no URL (every request URL carries the auth token
    /// and salt), no library internals, and an action where there is one.
    pub fn user_message(&self) -> String {
        match self {
            Error::Api { code, message } => match code {
                ApiErrorCode::WrongCredentials | ApiErrorCode::TokenAuthNotSupported => {
                    "Wrong username or password.".into()
                }
                ApiErrorCode::NotAuthorized => "Your account is not allowed to do that.".into(),
                ApiErrorCode::NotFound => "The server has no record of that.".into(),
                ApiErrorCode::ClientTooOld => {
                    "This server needs a newer client than Scirè speaks.".into()
                }
                ApiErrorCode::ServerTooOld => {
                    "The server is too old for Scirè — Subsonic 1.16.1 or newer is needed.".into()
                }
                ApiErrorCode::TrialExpired => "The server's trial period is over.".into(),
                ApiErrorCode::MissingParameter => {
                    "The server rejected the request as incomplete.".into()
                }
                ApiErrorCode::Generic | ApiErrorCode::Other(_) => match message.trim() {
                    "" => "The server reported an error.".into(),
                    m => format!("The server reported an error: {m}"),
                },
            },
            Error::Http(e) => {
                if e.is_timeout() {
                    "The server took too long to respond.".into()
                } else if e.is_connect() {
                    "Can't reach the server. Check that it is running and the address is right."
                        .into()
                } else if let Some(status) = e.status() {
                    match status.as_u16() {
                        401 | 403 => "The server refused the request.".into(),
                        404 => "The server has no such endpoint — is the address right?".into(),
                        502..=504 => "The server is not answering requests right now.".into(),
                        other => format!("The server answered with HTTP {other}."),
                    }
                } else if e.is_decode() {
                    "The server's reply could not be read.".into()
                } else {
                    "Network error while talking to the server.".into()
                }
            }
            Error::InvalidUrl(_) => "That server address is not a valid URL.".into(),
            Error::UnexpectedResponse(_) => "The server sent a reply Scirè did not expect.".into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn credentials_failures_are_named_and_never_retried() {
        let e = Error::Api {
            code: ApiErrorCode::WrongCredentials,
            message: "Wrong username or password.".into(),
        };
        assert!(e.is_auth_failure());
        assert!(!e.is_transient());
        assert_eq!(e.user_message(), "Wrong username or password.");
    }

    #[test]
    fn the_servers_own_message_survives_a_generic_error() {
        let e = Error::Api {
            code: ApiErrorCode::Other(99),
            message: "index is rebuilding".into(),
        };
        assert_eq!(
            e.user_message(),
            "The server reported an error: index is rebuilding"
        );
    }

    #[test]
    fn a_generic_code_is_retried_and_a_missing_record_is_not() {
        let generic = Error::Api {
            code: ApiErrorCode::Generic,
            message: String::new(),
        };
        let missing = Error::Api {
            code: ApiErrorCode::NotFound,
            message: String::new(),
        };
        assert!(generic.is_transient());
        assert!(!missing.is_transient());
    }

    #[test]
    fn a_bad_url_says_so_without_quoting_it() {
        let e = Error::InvalidUrl("https://host/?u=me&t=secret: bad".into());
        assert!(!e.user_message().contains("secret"));
    }
}
