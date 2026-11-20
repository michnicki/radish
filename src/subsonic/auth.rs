//! The configured user's credentials and the Subsonic ways of presenting them.

use super::Params;
use super::response::ApiError;
use crate::output::Clean;
use crate::web::same_token;
use anyhow::{Result, bail};
use md5::{Digest, Md5};
use std::fmt::{self, Write as _};
use std::net::IpAddr;

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

    /// The configured username.
    pub(crate) fn user(&self) -> &str {
        &self.user
    }

    /// Checks a request's login: `u` with a password `p` (plain, or `enc:`
    /// and its hex), or `u` with a token `t = md5(password + s)` and its
    /// salt `s`. Comparisons take the same time wherever the values differ.
    pub(crate) fn authenticate(&self, params: &Params) -> Result<(), Refusal> {
        if params.get("apiKey").is_some() {
            return Err(ApiError::unsupported_auth().into());
        }
        let user = params.required("u")?;
        let (password, token, salt) = (params.get("p"), params.get("t"), params.get("s"));
        if password.is_some() && (token.is_some() || salt.is_some()) {
            return Err(ApiError::conflicting_auth().into());
        }
        let secret = match (password, token, salt) {
            (Some(password), _, _) => match decode_password(password) {
                Some(password) => Ok(same_token(&self.password, &password)),
                None => Err("the enc: password is not hex-encoded UTF-8"),
            },
            (None, Some(token), Some(salt)) => Ok(same_token(
                &md5_hex(&format!("{}{salt}", self.password)),
                &token.to_ascii_lowercase(),
            )),
            (None, Some(_), None) => return Err(ApiError::missing("s").into()),
            (None, None, _) => return Err(ApiError::missing("p").into()),
        };
        // A token can't tell a wrong password from a wrong salt or hash, so
        // both kinds of login report the password.
        let reason = match (same_token(&self.user, user), secret) {
            (true, Ok(true)) => return Ok(()),
            (false, Ok(true)) => "wrong username",
            (true, Ok(false)) => "wrong password",
            (false, Ok(false)) => "wrong username and password",
            (_, Err(malformed)) => malformed,
        };
        Err(Refusal {
            error: ApiError::wrong_credentials(),
            reason: reason.to_string(),
        })
    }
}

/// A refused login: the error the client gets, and why, for the server's log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    /// What the client is told. Code 40 doesn't say which value was wrong.
    pub(crate) error: ApiError,
    /// Why the login failed, for the server's log only.
    pub(crate) reason: String,
}

impl From<ApiError> for Refusal {
    /// A refusal whose error message says all there is to say.
    fn from(error: ApiError) -> Self {
        Self {
            reason: error.message.clone(),
            error,
        }
    }
}

/// The server log's line for a refused login.
///
/// It names the user, how the request logged in, the caller's address,
/// the client and the method, so the operator can see which device sends
/// what. It never shows the password, token or salt.
pub(crate) fn refusal_line(
    refusal: &Refusal,
    method: &str,
    params: &Params,
    peer: Option<IpAddr>,
) -> String {
    // Quoted, so a stray space or a capital letter in the username shows.
    let user = params.get("u").map_or_else(
        || "no user".to_string(),
        |user| format!("user \"{}\"", Clean(user)),
    );
    let mut line = format!("login refused: {user}, {}", login_kind(params));
    if let Some(peer) = peer {
        let _ = write!(line, ", from {peer}");
    }
    if let Some(client) = params.get("c") {
        let _ = write!(line, ", client \"{}\"", Clean(client));
    }
    let _ = write!(
        line,
        ", method {}: {} (code {})",
        Clean(method),
        refusal.reason,
        refusal.error.code
    );
    line
}

/// How a request tries to log in, in a few words.
fn login_kind(params: &Params) -> &'static str {
    if params.get("apiKey").is_some() {
        "API key"
    } else if let Some(password) = params.get("p") {
        if password.starts_with("enc:") {
            "hex password"
        } else {
            "password"
        }
    } else if params.get("t").is_some() {
        "token"
    } else {
        "no password"
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

    fn refuse(pairs: &[(&str, &str)]) -> Result<(), Refusal> {
        let credentials = Credentials::new(Some("joe".into()), Some("sesame".into())).unwrap();
        credentials.authenticate(&Params::from_pairs(pairs))
    }

    fn check(pairs: &[(&str, &str)]) -> Result<(), ApiError> {
        refuse(pairs).map_err(|refusal| refusal.error)
    }

    fn reason(pairs: &[(&str, &str)]) -> String {
        refuse(pairs).unwrap_err().reason
    }

    #[test]
    fn the_log_says_which_value_was_wrong_and_the_client_does_not() {
        assert_eq!(reason(&[("u", "Joe"), ("p", "sesame")]), "wrong username");
        assert_eq!(reason(&[("u", "joe"), ("p", "sesame ")]), "wrong password");
        assert_eq!(
            reason(&[("u", "joe"), ("t", "0123"), ("s", "c19b2d")]),
            "wrong password"
        );
        assert_eq!(
            reason(&[("u", "eve"), ("p", "x")]),
            "wrong username and password"
        );
        assert_eq!(
            reason(&[("u", "joe"), ("p", "enc:zz")]),
            "the enc: password is not hex-encoded UTF-8"
        );
        assert_eq!(reason(&[("u", "joe")]), "Required parameter is missing: p");
        for pairs in [
            &[("u", "Joe"), ("p", "sesame")][..],
            &[("u", "joe"), ("p", "x")],
        ] {
            assert_eq!(check(pairs), Err(ApiError::wrong_credentials()));
        }
    }

    #[test]
    fn the_log_line_names_the_caller_but_no_secret() {
        let params = Params::from_pairs(&[
            ("u", "Joe"),
            ("t", "26719a1196d2a940705a59634eb18eab"),
            ("s", "c19b2d"),
            ("c", "Symfonium"),
        ]);
        let refusal = Credentials::new(Some("joe".into()), Some("sesame".into()))
            .unwrap()
            .authenticate(&params)
            .unwrap_err();
        let peer = Some("192.168.1.20".parse().unwrap());
        let line = refusal_line(&refusal, "ping", &params, peer);
        assert_eq!(
            line,
            "login refused: user \"Joe\", token, from 192.168.1.20, client \"Symfonium\", \
             method ping: wrong username (code 40)"
        );
        assert!(!line.contains("26719a") && !line.contains("c19b2d"));

        let params = Params::from_pairs(&[("p", "enc:736573616d65")]);
        let refusal = Refusal::from(ApiError::missing("u"));
        assert_eq!(
            refusal_line(&refusal, "get\u{1b}Song", &params, None),
            "login refused: no user, hex password, method get\\u{1b}Song: \
             Required parameter is missing: u (code 10)"
        );
        let params = Params::from_pairs(&[("u", "joe\n"), ("apiKey", "k")]);
        let line = refusal_line(
            &Refusal::from(ApiError::unsupported_auth()),
            "ping",
            &params,
            None,
        );
        assert!(line.starts_with("login refused: user \"joe\\u{a}\", API key, method ping: "));
        let line = refusal_line(&refusal, "ping", &Params::from_pairs(&[("u", "joe")]), None);
        assert!(line.starts_with("login refused: user \"joe\", no password, "));
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
        assert_eq!(check(&[("u", "joe"), ("p", "enc:ff")]), wrong, "not UTF-8");
        assert_eq!(check(&[("u", "joe"), ("p", "wrong")]), wrong);
        assert_eq!(check(&[("u", "eve"), ("p", "sesame")]), wrong);
    }

    #[test]
    fn malformed_logins_get_their_own_codes() {
        let code = |pairs: &[(&str, &str)]| check(pairs).unwrap_err().code;
        assert_eq!(code(&[("p", "sesame")]), 10);
        assert_eq!(code(&[("u", "joe")]), 10);
        assert_eq!(code(&[("u", "joe"), ("t", "x")]), 10);
        assert_eq!(code(&[("u", "joe"), ("s", "salt")]), 10);
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
