//! The configured user's credentials and the Subsonic ways of presenting them.

use super::Params;
use super::response::ApiError;
use crate::web::same_token;
use anyhow::{Result, bail};
use md5::{Digest, Md5};
use std::fmt;

/// The environment variable that names the user.
pub const USER_VAR: &str = "RADISH_SUBSONIC_USER";
/// The environment variable that holds the password.
pub const PASSWORD_VAR: &str = "RADISH_SUBSONIC_PASSWORD";

/// The username and password every request must present.
///
/// Token authentication hashes the password with a salt the client picks, so
/// the server has to hold the password itself.
pub struct Credentials {
    user: String,
    password: String,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Credentials")
            .field("user", &self.user)
            .finish_non_exhaustive()
    }
}

impl Credentials {
    /// Checks both values: each must be set, and the user must not be blank.
    ///
    /// # Errors
    ///
    /// Returns an error naming the variable that is missing.
    pub fn new(user: Option<String>, password: Option<String>) -> Result<Self> {
        let Some(user) = user.filter(|user| !user.trim().is_empty()) else {
            bail!("{USER_VAR} is not set; the Subsonic API needs a username and a password");
        };
        let Some(password) = password.filter(|password| !password.is_empty()) else {
            bail!("{PASSWORD_VAR} is not set; the Subsonic API needs a username and a password");
        };
        Ok(Self { user, password })
    }

    /// Reads them from [`USER_VAR`] and [`PASSWORD_VAR`], which `.env` may set.
    ///
    /// # Errors
    ///
    /// Returns an error naming the variable that is missing.
    pub fn from_env() -> Result<Self> {
        Self::new(
            std::env::var(USER_VAR).ok(),
            std::env::var(PASSWORD_VAR).ok(),
        )
    }

    /// Checks a request's login: `u` with a password `p` (plain, or `enc:`
    /// and its hex), or `u` with a token `t = md5(password + s)` and its
    /// salt `s`. Comparisons take the same time wherever the values differ.
    pub(crate) fn authenticate(&self, params: &Params) -> Result<(), ApiError> {
        if params.get("apiKey").is_some() {
            return Err(ApiError::unsupported_auth());
        }
        let user = params.required("u")?;
        let (password, token, salt) = (params.get("p"), params.get("t"), params.get("s"));
        if password.is_some() && (token.is_some() || salt.is_some()) {
            return Err(ApiError::conflicting_auth());
        }
        let secret_matches = match (password, token, salt) {
            (Some(password), _, _) => decode_password(password)
                .is_some_and(|password| same_token(&self.password, &password)),
            (None, Some(token), Some(salt)) => same_token(
                &md5_hex(&format!("{}{salt}", self.password)),
                &token.to_ascii_lowercase(),
            ),
            (None, Some(_), None) => return Err(ApiError::missing("s")),
            (None, None, _) => return Err(ApiError::missing("p")),
        };
        if same_token(&self.user, user) && secret_matches {
            Ok(())
        } else {
            Err(ApiError::wrong_credentials())
        }
    }
}

/// A `p` value: plain text, or `enc:` and the hex of its UTF-8 bytes.
fn decode_password(value: &str) -> Option<String> {
    match value.strip_prefix("enc:") {
        None => Some(value.to_string()),
        Some(hex) => String::from_utf8(super::decode_hex(hex)?).ok(),
    }
}

/// The MD5 of `text` as lowercase hex.
fn md5_hex(text: &str) -> String {
    super::hex(Md5::digest(text.as_bytes()).as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(pairs: &[(&str, &str)]) -> Result<(), ApiError> {
        let credentials = Credentials::new(Some("joe".into()), Some("sesame".into())).unwrap();
        credentials.authenticate(&Params::from_pairs(pairs))
    }

    #[test]
    fn the_spec_token_example_is_accepted_in_either_case() {
        assert_eq!(md5_hex("sesamec19b2d"), "26719a1196d2a940705a59634eb18eab");
        let token = "26719a1196d2a940705a59634eb18eab";
        assert_eq!(
            check(&[("u", "joe"), ("t", token), ("s", "c19b2d")]),
            Ok(())
        );
        let upper = token.to_ascii_uppercase();
        assert_eq!(
            check(&[("u", "joe"), ("t", &upper), ("s", "c19b2d")]),
            Ok(())
        );
        assert_eq!(
            check(&[("u", "joe"), ("t", token), ("s", "other1")]),
            Err(ApiError::wrong_credentials())
        );
    }

    #[test]
    fn passwords_may_be_plain_or_hex_encoded() {
        assert_eq!(check(&[("u", "joe"), ("p", "sesame")]), Ok(()));
        assert_eq!(check(&[("u", "joe"), ("p", "enc:736573616d65")]), Ok(()));
        let wrong = Err(ApiError::wrong_credentials());
        assert_eq!(check(&[("u", "joe"), ("p", "enc:zz")]), wrong);
        assert_eq!(check(&[("u", "joe"), ("p", "wrong")]), wrong);
        assert_eq!(check(&[("u", "eve"), ("p", "sesame")]), wrong);
    }

    #[test]
    fn malformed_logins_get_their_own_codes() {
        let code = |pairs: &[(&str, &str)]| check(pairs).unwrap_err().code;
        assert_eq!(code(&[("p", "sesame")]), 10);
        assert_eq!(code(&[("u", "joe")]), 10);
        assert_eq!(code(&[("u", "joe"), ("t", "x")]), 10);
        assert_eq!(
            code(&[("u", "joe"), ("p", "sesame"), ("t", "x"), ("s", "y")]),
            43
        );
        assert_eq!(code(&[("apiKey", "k")]), 42);
    }

    #[test]
    fn credentials_must_be_set_and_never_show_the_password() {
        assert!(Credentials::new(None, Some("p".into())).is_err());
        assert!(Credentials::new(Some(" ".into()), Some("p".into())).is_err());
        assert!(Credentials::new(Some("u".into()), Some(String::new())).is_err());
        assert!(Credentials::new(Some("u".into()), None).is_err());
        let credentials = Credentials::new(Some("joe".into()), Some("sesame".into())).unwrap();
        assert!(!format!("{credentials:?}").contains("sesame"));
    }
}
