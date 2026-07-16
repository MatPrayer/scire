use md5::{Digest, Md5};
use rand::Rng;

/// Credentials for Subsonic token authentication.
///
/// The plaintext password must be retained because each request uses a fresh
/// salt: `t = md5(password + salt)`.
#[derive(Debug, Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

/// A single-use authentication token (salt + md5 hash).
#[derive(Debug, Clone)]
pub(crate) struct AuthToken {
    pub salt: String,
    pub token: String,
}

impl Credentials {
    pub fn new(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: username.into(),
            password: password.into(),
        }
    }

    /// Generate a fresh salt and derive the request token.
    pub(crate) fn token(&self) -> AuthToken {
        let salt = random_salt(12);
        self.token_with_salt(&salt)
    }

    /// Derive the token for a known salt (separated for testability).
    pub(crate) fn token_with_salt(&self, salt: &str) -> AuthToken {
        let mut hasher = Md5::new();
        hasher.update(self.password.as_bytes());
        hasher.update(salt.as_bytes());
        let token = format!("{:x}", hasher.finalize());
        AuthToken {
            salt: salt.to_string(),
            token,
        }
    }
}

fn random_salt(len: usize) -> String {
    const CHARS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    (0..len)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_matches_subsonic_spec_example() {
        // Example from the Subsonic API docs: password "sesame", salt "c19b2d",
        // expected token md5("sesamec19b2d").
        let creds = Credentials::new("joe", "sesame");
        let auth = creds.token_with_salt("c19b2d");
        assert_eq!(auth.token, "26719a1196d2a940705a59634eb18eab");
        assert_eq!(auth.salt, "c19b2d");
    }

    #[test]
    fn fresh_salt_per_token() {
        let creds = Credentials::new("u", "p");
        let a = creds.token();
        let b = creds.token();
        assert_ne!(a.salt, b.salt);
        assert_eq!(a.salt.len(), 12);
    }
}
