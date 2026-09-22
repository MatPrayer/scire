//! Turning failures into something a user can read and act on.
//!
//! Everything in the app used to render `format!("{e:#}")` straight into the
//! view. Two problems with that, and the first is the serious one:
//!
//! * **Credentials.** Every Subsonic request URL carries `u`, `t` (the auth
//!   token) and `s` (the salt), and the HTTP layers print the URL they failed
//!   on. A dropped connection therefore painted the account's token across the
//!   album grid. The `subsonic` crate now strips the URL at the source and
//!   `scrub` is the belt-and-braces pass for anything that still quotes one.
//! * **Wording.** "http error: error sending request" names a library, not a
//!   cause. `subsonic::Error::user_message` says what happened and, where there
//!   is one, what to do about it.
//!
//! `retryable` is the other half: an error line the user can only stare at is
//! a dead end, so the views draw a Retry button — but only for failures where
//! trying again could plausibly work.

use playback::scrub_urls;

/// A failure as a view holds it: the sentence to draw, and whether trying the
/// same request again is worth offering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorNote {
    pub text: String,
    pub retryable: bool,
}

impl ErrorNote {
    pub fn new(e: &anyhow::Error) -> Self {
        Self {
            text: error_text(e),
            retryable: retryable(e),
        }
    }
}

/// A sentence for the UI, free of URLs, query strings and library jargon.
pub fn error_text(e: &anyhow::Error) -> String {
    // anyhow contexts added by the app ("loading album …") are worth keeping,
    // so the head of the chain is kept and only the tail — the library's own
    // wording — is replaced. `anyhow::Error::to_string` is the outermost layer,
    // which for an uncontexted error is the subsonic error itself; comparing
    // the two is how "no context was added" is told from "some was".
    let mut msg = match e.chain().find_map(|c| c.downcast_ref::<subsonic::Error>()) {
        Some(api) => {
            let head = e.to_string();
            match head.is_empty() || head == api.to_string() {
                true => api.user_message(),
                false => format!("{head}: {}", api.user_message()),
            }
        }
        None => format!("{e:#}"),
    };
    msg = scrub(&msg);
    capitalize(&msg)
}

/// True when trying the same request again could plausibly succeed: a timeout,
/// a refused connection, a 5xx. Wrong credentials, a bad URL or a 404 fail
/// identically forever, and offering a button that cannot work is worse than
/// offering none.
pub fn retryable(e: &anyhow::Error) -> bool {
    match e.chain().find_map(|c| c.downcast_ref::<subsonic::Error>()) {
        Some(api) => api.is_transient(),
        // Not a Subsonic failure: an IO or decode error the app wrapped. Those
        // are usually local and permanent, so the button stays off.
        None => false,
    }
}

/// Recast a playback engine failure in terms of the track the user was trying
/// to play.
///
/// The engine's messages come from rodio, symphonia and reqwest by way of
/// stream-download, and they are written for whoever is reading a log: "error
/// sending request", "end of stream", "the format of the data has not been
/// recognized". On the player bar, under a song title, the useful shape is
/// *which* track and *why*, so the track is named and the handful of causes a
/// user can act on are said in words. Anything unrecognised is passed through
/// rather than replaced with a vaguer sentence — an odd message beats none.
pub fn playback_error(msg: &str, title: Option<&str>) -> String {
    let lower = msg.to_ascii_lowercase();
    let reason = if lower.contains("timed out") || lower.contains("timeout") {
        "the server did not respond in time".to_string()
    } else if lower.contains("connect")
        || lower.contains("dns")
        || lower.contains("error sending request")
    {
        "the server could not be reached".to_string()
    } else if lower.contains("404") || lower.contains("not found") {
        "the server no longer has this file".to_string()
    } else if lower.contains("401") || lower.contains("403") {
        "the server refused the request".to_string()
    } else if lower.contains("no such file") || lower.contains("os error 2") {
        "the file is missing from disk".to_string()
    } else {
        scrub(msg)
    };
    match title {
        Some(title) if !title.is_empty() => format!("Couldn't play “{title}”: {reason}"),
        _ => capitalize(&reason),
    }
}

/// Strip query strings out of any URL in `msg` — the auth token and salt live
/// there — and drop a trailing period the sentence will supply itself.
pub fn scrub(msg: &str) -> String {
    scrub_urls(msg).trim().to_string()
}

fn capitalize(msg: &str) -> String {
    let mut chars = msg.chars();
    match chars.next() {
        Some(first) if first.is_lowercase() => {
            first.to_uppercase().collect::<String>() + chars.as_str()
        }
        _ => msg.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_subsonic_error_is_rendered_in_its_own_words() {
        let e = anyhow::Error::from(subsonic::Error::Api {
            code: subsonic::ApiErrorCode::WrongCredentials,
            message: "Wrong username or password.".into(),
        });
        assert_eq!(error_text(&e), "Wrong username or password.");
    }

    #[test]
    fn a_context_chain_keeps_the_context_and_replaces_the_tail() {
        let e = anyhow::Error::from(subsonic::Error::Api {
            code: subsonic::ApiErrorCode::NotFound,
            message: "not found".into(),
        })
        .context("loading album");
        let text = error_text(&e);
        assert!(text.starts_with("Loading album: "), "{text}");
        assert!(text.contains("no record"), "{text}");
    }

    #[test]
    fn an_auth_token_is_never_rendered() {
        let e = anyhow::anyhow!(
            "error sending request for url (https://music.example.com/rest/getAlbumList2?u=me&t=9f8e7d&s=abc)"
        );
        let text = error_text(&e);
        assert!(!text.contains("t=9f8e7d"), "{text}");
        assert!(!text.contains("s=abc"), "{text}");
        assert!(
            text.contains("music.example.com/rest/getAlbumList2"),
            "{text}"
        );
    }

    #[test]
    fn a_playback_failure_names_the_track_and_the_cause() {
        let text = playback_error(
            "error sending request for url (https://music.example.com/rest/stream?u=me&t=secret)",
            Some("Lateralus"),
        );
        assert_eq!(
            text,
            "Couldn't play “Lateralus”: the server could not be reached"
        );
        assert!(!text.contains("secret"));
    }

    #[test]
    fn an_unrecognised_playback_failure_is_passed_through() {
        let text = playback_error("the format of the data has not been recognized", None);
        assert_eq!(text, "The format of the data has not been recognized");
    }

    #[test]
    fn only_transient_failures_offer_a_retry() {
        let not_found = anyhow::Error::from(subsonic::Error::Api {
            code: subsonic::ApiErrorCode::NotFound,
            message: String::new(),
        });
        let generic = anyhow::Error::from(subsonic::Error::Api {
            code: subsonic::ApiErrorCode::Generic,
            message: String::new(),
        });
        assert!(!retryable(&not_found));
        assert!(retryable(&generic));
        assert!(!retryable(&anyhow::anyhow!("no such file")));
    }
}
