//! Operation-scoped credentials. No credential helper, shell, key discovery,
//! certificate override, or on-disk secret storage is used here.

use std::{fmt, net::Ipv6Addr, path::PathBuf};

use git2::{Cred, CredentialType, Error, ErrorClass, ErrorCode};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthProtocol {
    Https,
    Ssh,
}

/// Safe information for an application-owned authentication prompt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthChallenge {
    pub protocol: AuthProtocol,
    /// Host and optional nondefault port; never URL userinfo, path, or query.
    pub host: String,
    pub username: Option<String>,
    /// An explicitly configured key path, for refilling a retry prompt only.
    pub private_key: Option<PathBuf>,
}

/// Credentials supplied for one remote operation, retained only in memory.
///
/// Clone starts a fresh attempt sequence. Bind each clone to the actual fetch
/// or push URL before installing its `credentials` method in a callback. Do
/// not serialize this type, include it in logs, or reuse it for another remote.
#[derive(Default)]
pub struct RemoteAuth {
    username: Option<String>,
    password: Option<String>,
    private_key: Option<PathBuf>,
    passphrase: Option<String>,
    expected_origin: Option<(AuthProtocol, String)>,
    origin: Option<Origin>,
    username_hint: Option<String>,
    attempts: Attempts,
}

impl Clone for RemoteAuth {
    fn clone(&self) -> Self {
        Self {
            username: self.username.clone(),
            password: self.password.clone(),
            private_key: self.private_key.clone(),
            passphrase: self.passphrase.clone(),
            expected_origin: self.expected_origin.clone(),
            origin: self.origin.clone(),
            username_hint: self.username_hint.clone(),
            attempts: Attempts::default(),
        }
    }
}

impl fmt::Debug for RemoteAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RemoteAuth")
            .field("username_supplied", &self.username.is_some())
            .field("password_supplied", &self.password.is_some())
            .field("private_key_supplied", &self.private_key.is_some())
            .field("passphrase_supplied", &self.passphrase.is_some())
            .finish_non_exhaustive()
    }
}

#[derive(Default)]
struct Attempts {
    username: bool,
    password: bool,
    agent: bool,
    key: bool,
    default: bool,
}

impl RemoteAuth {
    /// Whether an operation already supplied its own credential choices.
    pub(crate) fn is_explicit(&self) -> bool {
        self.username.is_some()
            || self.password.is_some()
            || self.private_key.is_some()
            || self.passphrase.is_some()
            || self.expected_origin.is_some()
    }

    /// HTTPS Basic authentication; `password` may be a provider's PAT.
    pub fn user_password(username: impl Into<String>, password: impl Into<String>) -> Self {
        Self {
            username: Some(username.into()),
            password: Some(password.into()),
            ..Self::default()
        }
    }

    /// Use only the explicitly selected private key, without searching ~/.ssh.
    /// The transport reads the key; this constructor performs no file I/O.
    pub fn ssh_key(
        username: impl Into<String>,
        private_key: PathBuf,
        passphrase: Option<String>,
    ) -> Self {
        Self {
            username: Some(username.into()),
            private_key: Some(private_key),
            passphrase,
            ..Self::default()
        }
    }

    /// Supply the username needed by an agent when the URL does not contain it.
    pub fn with_username(mut self, username: impl Into<String>) -> Self {
        self.username = Some(username.into());
        self
    }

    /// Restrict a prompt's credentials to the origin that displayed the prompt.
    /// A remote URL changed while the user was entering credentials must never
    /// become their destination. Clones retain this restriction and must bind
    /// the actual selected URL before credentials can be used.
    pub fn with_challenge(mut self, challenge: &AuthChallenge) -> Self {
        self.expected_origin = Some((challenge.protocol, challenge.host.clone()));
        self.origin = None;
        self.username_hint = None;
        self.attempts = Attempts::default();
        self
    }

    /// Pin credentials to the original operation's scheme, host and port.
    ///
    /// Local/file remotes are valid but never receive credentials. Plain HTTP
    /// can remain usable anonymously; an authentication request over HTTP is
    /// refused rather than transmitting a password or Windows credentials.
    pub fn bind_remote_url(&mut self, url: &str) -> Result<(), Error> {
        self.origin = None;
        self.username_hint = None;
        self.attempts = Attempts::default();
        let (origin, username) = parse_remote(url)?;
        if let Some((protocol, host)) = &self.expected_origin {
            let scheme = match protocol {
                AuthProtocol::Https => Scheme::Https,
                AuthProtocol::Ssh => Scheme::Ssh,
            };
            if !origin
                .as_ref()
                .is_some_and(|origin| origin.scheme == scheme && origin.label() == *host)
            {
                return Err(auth_error(
                    "Authentication destination changed since the prompt; credentials were withheld",
                ));
            }
        }
        self.origin = origin;
        self.username_hint = username;
        Ok(())
    }

    pub fn challenge(&self) -> Option<AuthChallenge> {
        let origin = self.origin.as_ref()?;
        let protocol = match origin.scheme {
            Scheme::Https => AuthProtocol::Https,
            Scheme::Ssh => AuthProtocol::Ssh,
            Scheme::Http => return None,
        };
        Some(AuthChallenge {
            protocol,
            host: origin.label(),
            username: credential_username(
                origin.scheme,
                self.username.as_deref(),
                self.username_hint.as_deref(),
                None,
                None,
            )
            .map(str::to_owned),
            private_key: (protocol == AuthProtocol::Ssh)
                .then(|| self.private_key.clone())
                .flatten(),
        })
    }

    /// For `RemoteCallbacks::credentials`; the callback is already `FnMut`.
    ///
    /// Each strategy is returned at most once. Repeated server rejection must
    /// return to the UI instead of retrying the same PAT/key indefinitely.
    pub fn credentials(
        &mut self,
        url: &str,
        username_from_url: Option<&str>,
        allowed: CredentialType,
    ) -> Result<Cred, Error> {
        let (requested, requested_username) = parse_remote(url)?;
        let origin = self.origin.as_ref().ok_or_else(authentication_required)?;
        if requested.as_ref() != Some(origin) {
            return Err(auth_error(
                "Authentication origin changed; credentials were withheld",
            ));
        }
        if origin.scheme == Scheme::Http {
            return Err(auth_error("Password authentication requires HTTPS"));
        }
        if self.username_hint.is_none() {
            self.username_hint = username_from_url.map(str::to_owned);
        }
        let username = credential_username(
            origin.scheme,
            self.username.as_deref(),
            self.username_hint.as_deref(),
            username_from_url,
            requested_username.as_deref(),
        );

        if origin.scheme == Scheme::Https {
            if allowed.contains(CredentialType::USER_PASS_PLAINTEXT)
                && !self.attempts.password
                && let (Some(username), Some(password)) = (username, self.password.as_deref())
            {
                self.attempts.password = true;
                return Cred::userpass_plaintext(username, password)
                    .map_err(|_| authentication_required());
            }
            if allowed.contains(CredentialType::DEFAULT) && !self.attempts.default {
                self.attempts.default = true;
                return Cred::default().map_err(|_| authentication_required());
            }
        } else if allowed.contains(CredentialType::SSH_KEY)
            && let Some(username) = username
        {
            if let Some(private_key) = self.private_key.as_deref() {
                if !self.attempts.key {
                    self.attempts.key = true;
                    return Cred::ssh_key(username, None, private_key, self.passphrase.as_deref())
                        .map_err(|_| authentication_required());
                }
            } else if !self.attempts.agent {
                self.attempts.agent = true;
                return Cred::ssh_key_from_agent(username).map_err(|_| authentication_required());
            }
        }
        if allowed.contains(CredentialType::USERNAME)
            && !self.attempts.username
            && let Some(username) = username
        {
            self.attempts.username = true;
            return Cred::username(username).map_err(|_| authentication_required());
        }
        Err(authentication_required())
    }
}

fn credential_username<'a>(
    scheme: Scheme,
    explicit: Option<&'a str>,
    bound: Option<&'a str>,
    callback: Option<&'a str>,
    requested: Option<&'a str>,
) -> Option<&'a str> {
    let explicit = explicit.filter(|value| !value.is_empty());
    let bound = bound.filter(|value| !value.is_empty());
    let callback = callback.filter(|value| !value.is_empty());
    if scheme == Scheme::Ssh {
        // libgit2 requires the credential's username to match an SSH URL's
        // embedded username. Explicit/default names only fill an absent one.
        requested
            .filter(|value| !value.is_empty())
            .or(bound)
            .or(callback)
            .or(explicit)
    } else {
        explicit.or(callback).or(bound)
    }
}

pub(crate) fn ssh_url_username(url: &str) -> Option<String> {
    let (origin, username) = parse_remote(url).ok()?;
    (origin?.scheme == Scheme::Ssh)
        .then_some(username)
        .flatten()
}

/// Identify supported authenticated transports without exposing URL credentials.
/// Local, anonymous and malformed addresses have no authentication protocol.
pub fn remote_protocol(url: &str) -> Option<AuthProtocol> {
    let (origin, _) = parse_remote(url).ok()?;
    match origin?.scheme {
        Scheme::Https => Some(AuthProtocol::Https),
        Scheme::Ssh => Some(AuthProtocol::Ssh),
        Scheme::Http => None,
    }
}

/// Configuration forms must never persist passwords or token query strings.
pub(crate) fn validate_settings_url(url: &str) -> Result<(), Error> {
    let invalid = || auth_error("Remote address must not contain credentials or malformed values");
    if url.is_empty() || url.trim() != url || url.chars().any(char::is_control) {
        return Err(invalid());
    }
    let (origin, _) = parse_remote(url)?;
    if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git+ssh" | "git" | "file"
        ) {
            return Err(invalid());
        }
        if rest.is_empty() || url.contains(['?', '#', '\\']) {
            return Err(invalid());
        }
        let authority = rest.split('/').next().unwrap_or_default();
        if scheme.eq_ignore_ascii_case("file") {
            if authority.contains('@') {
                return Err(invalid());
            }
        } else {
            if authority.is_empty() || authority.chars().any(char::is_whitespace) {
                return Err(invalid());
            }
            if let Some((username, _)) = authority.rsplit_once('@')
                && (username.is_empty() || username.contains([':', '%', '@']))
            {
                return Err(invalid());
            }
            if scheme.eq_ignore_ascii_case("git") {
                // Anonymous Git still needs an unambiguous network authority.
                parse_remote(&format!("https://{rest}"))?;
            }
        }
    } else if origin.is_some() {
        let authority = url.split(':').next().unwrap_or_default();
        if let Some((username, _)) = authority.rsplit_once('@')
            && (username.is_empty() || username.contains(['%', '@']))
        {
            return Err(invalid());
        }
    }
    Ok(())
}

fn authentication_required() -> Error {
    auth_error("Authentication required or supplied credentials were rejected")
}

fn auth_error(message: &'static str) -> Error {
    Error::new(ErrorCode::Auth, ErrorClass::Callback, message)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Scheme {
    Https,
    Http,
    Ssh,
}

impl Scheme {
    fn port(self) -> u16 {
        match self {
            Self::Https => 443,
            Self::Http => 80,
            Self::Ssh => 22,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Origin {
    scheme: Scheme,
    host: String,
    port: u16,
}

impl Origin {
    fn label(&self) -> String {
        let host = if self.host.contains(':') {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        if self.port == self.scheme.port() {
            host
        } else {
            format!("{host}:{}", self.port)
        }
    }
}

/// Parse only the authentication origin; never retain a raw URL or its password.
/// Ambiguous authorities are refused rather than interpreted differently from
/// the transport. DNS names must be ASCII (international names use punycode).
fn parse_remote(url: &str) -> Result<(Option<Origin>, Option<String>), Error> {
    let invalid = || auth_error("Remote authentication address is invalid");
    if url.chars().any(char::is_control) {
        return Err(invalid());
    }
    let (scheme, authority) = if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = match scheme.to_ascii_lowercase().as_str() {
            "https" => Scheme::Https,
            "http" => Scheme::Http,
            "ssh" | "git+ssh" => Scheme::Ssh,
            _ => return Ok((None, None)),
        };
        if url.contains('\\') {
            return Err(invalid());
        }
        (scheme, rest.split(['/', '?', '#']).next().unwrap_or(""))
    } else {
        // Preserve local paths, including Windows drive letters and UNC paths.
        if url.contains('\\') || url.starts_with('/') {
            return Ok((None, None));
        }
        let separator = if let Some(open) = url.find('[') {
            url[open..]
                .find(']')
                .map(|close| open + close + 1)
                .filter(|close| url.as_bytes().get(*close) == Some(&b':'))
        } else {
            url.find(':')
        };
        let Some(separator) = separator else {
            return Ok((None, None));
        };
        let authority = &url[..separator];
        if authority.contains('/')
            || (authority.len() == 1 && authority.as_bytes()[0].is_ascii_alphabetic())
        {
            return Ok((None, None));
        }
        (Scheme::Ssh, authority)
    };
    if authority.is_empty() || authority.chars().any(char::is_whitespace) {
        return Err(invalid());
    }
    let (host_port, username) = if let Some((userinfo, host)) = authority.rsplit_once('@') {
        if userinfo.contains('@') {
            return Err(invalid());
        }
        let username = userinfo.split(':').next().filter(|name| !name.is_empty());
        (host, username.map(str::to_owned))
    } else {
        (authority, None)
    };
    let (host, port) = if let Some(bracketed) = host_port.strip_prefix('[') {
        let (host, suffix) = bracketed.split_once(']').ok_or_else(invalid)?;
        let host = host.parse::<Ipv6Addr>().map_err(|_| invalid())?.to_string();
        let port = if suffix.is_empty() {
            scheme.port()
        } else {
            suffix
                .strip_prefix(':')
                .ok_or_else(invalid)?
                .parse::<u16>()
                .map_err(|_| invalid())?
        };
        (host, port)
    } else {
        let (host, port) = if let Some((host, port)) = host_port.split_once(':') {
            (host, port.parse::<u16>().map_err(|_| invalid())?)
        } else {
            (host_port, scheme.port())
        };
        if host.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            return Err(invalid());
        }
        (host.to_ascii_lowercase(), port)
    };
    if port == 0 {
        return Err(invalid());
    }
    Ok((Some(Origin { scheme, host, port }), username))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_detection_recognizes_ssh_and_https_without_accepting_malformed_hosts() {
        for url in [
            "git@example.test:team/repo.git",
            "ssh://git@example.test/repo.git",
            "git+ssh://example.test/repo.git",
        ] {
            assert_eq!(remote_protocol(url), Some(AuthProtocol::Ssh));
        }
        assert_eq!(
            remote_protocol("https://example.test/repo.git"),
            Some(AuthProtocol::Https)
        );
        for url in [
            "http://example.test/repo.git",
            "https://good.test\\@evil.test/repo.git",
            "./local",
            "C:/local",
        ] {
            assert_eq!(remote_protocol(url), None);
        }
    }

    #[test]
    fn https_prompt_uses_only_safe_host_and_username() {
        let mut auth = RemoteAuth::default();
        auth.bind_remote_url("https://alice:must-not-leak@EXAMPLE.test:8443/repo?token=hidden")
            .unwrap();
        assert_eq!(
            auth.challenge(),
            Some(AuthChallenge {
                protocol: AuthProtocol::Https,
                host: "example.test:8443".into(),
                username: Some("alice".into()),
                private_key: None,
            })
        );
        let display = format!("{:?}", auth.challenge());
        assert!(!display.contains("must-not-leak"));
        assert!(!display.contains("hidden"));
    }

    #[test]
    fn manual_pat_is_allowed_once_for_the_bound_https_origin() {
        let mut auth = RemoteAuth::user_password("alice", "test-pat");
        auth.bind_remote_url("https://example.test/repo").unwrap();
        assert!(
            auth.credentials(
                "https://EXAMPLE.test:443/repo/info/refs",
                None,
                CredentialType::USER_PASS_PLAINTEXT
            )
            .is_ok()
        );
        assert_eq!(
            auth.credentials(
                "https://example.test/repo",
                None,
                CredentialType::USER_PASS_PLAINTEXT
            )
            .err()
            .unwrap()
            .code(),
            ErrorCode::Auth
        );
        let mut retry = auth.clone();
        assert!(
            retry
                .credentials(
                    "https://example.test/repo",
                    None,
                    CredentialType::USER_PASS_PLAINTEXT
                )
                .is_ok()
        );
    }

    #[test]
    fn credentials_are_withheld_after_host_port_or_scheme_change() {
        for destination in [
            "https://other.test/repo",
            "https://example.test:8443/repo",
            "http://example.test/repo",
            "ssh://example.test/repo",
        ] {
            let mut auth = RemoteAuth::user_password("alice", "test-pat");
            auth.bind_remote_url("https://example.test/repo").unwrap();
            let error = auth
                .credentials(destination, None, CredentialType::USER_PASS_PLAINTEXT)
                .err()
                .unwrap();
            assert_eq!(error.code(), ErrorCode::Auth);
            assert!(!error.message().contains("test-pat"));
            assert!(!error.message().contains(destination));
        }
    }

    #[test]
    fn prompt_retry_rejects_changed_destinations_before_binding_credentials() {
        let mut initial = RemoteAuth::default();
        initial
            .bind_remote_url("https://EXAMPLE.test/repo")
            .unwrap();
        let challenge = initial.challenge().unwrap();
        let retry = RemoteAuth::user_password("alice", "prompt-secret").with_challenge(&challenge);
        for destination in [
            "https://other.test/repo",
            "https://example.test:8443/repo",
            "http://example.test/repo",
            "ssh://example.test/repo",
            "./local-repository",
        ] {
            let mut auth = retry.clone();
            // Start from a successful binding, as a subsequent clone or retry
            // may already carry one. A failed rebind must also discard it.
            auth.bind_remote_url("https://example.test/repo").unwrap();
            let error = auth.bind_remote_url(destination).unwrap_err();
            assert_eq!(error.code(), ErrorCode::Auth);
            assert!(!error.message().contains("prompt-secret"));
            assert!(!error.message().contains(destination));
            assert!(auth.challenge().is_none());
            assert!(
                auth.credentials(destination, None, CredentialType::USER_PASS_PLAINTEXT)
                    .is_err()
            );
            assert!(
                auth.credentials(
                    "https://example.test/repo",
                    None,
                    CredentialType::USER_PASS_PLAINTEXT
                )
                .is_err()
            );
        }
    }

    #[test]
    fn prompt_retry_accepts_normalized_original_origin_and_clones_keep_constraint() {
        let mut initial = RemoteAuth::default();
        initial
            .bind_remote_url("https://example.test/repo")
            .unwrap();
        let challenge = initial.challenge().unwrap();
        let mut auth =
            RemoteAuth::user_password("alice", "prompt-secret").with_challenge(&challenge);
        auth.bind_remote_url("https://EXAMPLE.test:443/renamed-repo")
            .unwrap();
        assert!(
            auth.credentials(
                "https://example.test/renamed-repo/info/refs",
                None,
                CredentialType::USER_PASS_PLAINTEXT
            )
            .is_ok()
        );
        let mut cloned = auth.clone();
        assert!(
            cloned
                .credentials(
                    "https://example.test/renamed-repo",
                    None,
                    CredentialType::USER_PASS_PLAINTEXT
                )
                .is_ok()
        );
        assert!(cloned.bind_remote_url("https://other.test/repo").is_err());
        // Adding a prompt restriction to an already bound value must also
        // require a fresh binding, so the builder cannot bypass the check.
        let mut constrained = auth.with_challenge(&challenge);
        assert!(
            constrained
                .credentials(
                    "https://example.test/renamed-repo",
                    None,
                    CredentialType::USER_PASS_PLAINTEXT
                )
                .is_err()
        );
        assert!(
            RemoteAuth::default()
                .with_challenge(&challenge)
                .is_explicit()
        );
    }

    #[test]
    fn ssh_prompt_retry_accepts_equivalent_scp_origin_and_refuses_other_ports() {
        let mut initial = RemoteAuth::default();
        initial
            .bind_remote_url("git@example.test:team/repo.git")
            .unwrap();
        let challenge = initial.challenge().unwrap();
        let mut auth = RemoteAuth::default()
            .with_username("git")
            .with_challenge(&challenge);
        auth.bind_remote_url("git+ssh://git@EXAMPLE.test:22/team/repo.git")
            .unwrap();
        assert!(
            auth.credentials(
                "ssh://git@example.test/team/repo.git",
                Some("git"),
                CredentialType::USERNAME
            )
            .is_ok()
        );
        assert!(
            auth.bind_remote_url("ssh://git@example.test:2222/team/repo.git")
                .is_err()
        );
    }

    #[test]
    fn plaintext_http_and_unbound_credentials_are_refused() {
        let mut auth = RemoteAuth::user_password("alice", "test-pat");
        assert!(
            auth.credentials("https://example.test/repo", None, CredentialType::all())
                .is_err()
        );
        auth.bind_remote_url("http://example.test/repo").unwrap();
        assert!(auth.challenge().is_none());
        assert!(
            auth.credentials("http://example.test/repo", None, CredentialType::all())
                .is_err()
        );
    }

    #[test]
    fn ssh_username_challenge_and_scp_origin_agree_without_agent_or_file_access() {
        let mut auth = RemoteAuth::default();
        auth.bind_remote_url("git@example.test:group/repo.git")
            .unwrap();
        assert_eq!(
            auth.challenge(),
            Some(AuthChallenge {
                protocol: AuthProtocol::Ssh,
                host: "example.test".into(),
                username: Some("git".into()),
                private_key: None,
            })
        );
        assert!(
            auth.credentials(
                "ssh://git@example.test/group/repo.git",
                Some("git"),
                CredentialType::USERNAME
            )
            .is_ok()
        );
    }

    #[test]
    fn ssh_url_username_wins_over_a_conflicting_default_but_https_explicit_username_wins() {
        for url in [
            "git@example.test:team/repo.git",
            "ssh://git@example.test/team/repo.git",
        ] {
            let mut auth = RemoteAuth::ssh_key("alice", PathBuf::from("unused-key"), None);
            auth.bind_remote_url(url).unwrap();
            assert_eq!(auth.challenge().unwrap().username.as_deref(), Some("git"));
            // Cred intentionally has no safe username getter. Verify the same
            // selection used by its USERNAME constructor before constructing it.
            assert_eq!(
                credential_username(
                    Scheme::Ssh,
                    auth.username.as_deref(),
                    auth.username_hint.as_deref(),
                    Some("git"),
                    ssh_url_username(url).as_deref()
                ),
                Some("git")
            );
            let cred = auth
                .credentials(url, Some("git"), CredentialType::USERNAME)
                .unwrap();
            assert!(cred.has_username());
        }
        let mut auth = RemoteAuth::user_password("alice@example.test", "test-secret");
        let url = "https://url-user@example.test/team/repo.git";
        auth.bind_remote_url(url).unwrap();
        assert_eq!(
            auth.challenge().unwrap().username.as_deref(),
            Some("alice@example.test")
        );
        assert_eq!(
            credential_username(
                Scheme::Https,
                auth.username.as_deref(),
                auth.username_hint.as_deref(),
                Some("url-user"),
                Some("url-user")
            ),
            Some("alice@example.test")
        );
        assert!(
            auth.credentials(url, Some("url-user"), CredentialType::USER_PASS_PLAINTEXT)
                .is_ok()
        );
    }

    #[test]
    fn debug_never_formats_credentials_or_private_key_paths() {
        let auth = RemoteAuth::ssh_key(
            "private-user",
            PathBuf::from("private-key-location"),
            Some("private-passphrase".into()),
        );
        let debug = format!("{auth:?}");
        for secret in ["private-user", "private-key-location", "private-passphrase"] {
            assert!(!debug.contains(secret));
        }
        let auth = RemoteAuth::user_password("private-user", "private-pat");
        assert!(!format!("{auth:?}").contains("private-pat"));
    }

    #[test]
    fn origin_parser_handles_ipv6_and_local_paths_and_rejects_ambiguous_authorities() {
        for url in [
            r"C:\repo",
            "C:/repo",
            r"\\server\repo",
            "file:///C:/repo",
            "./repo",
        ] {
            assert!(parse_remote(url).unwrap().0.is_none());
        }
        let mut auth = RemoteAuth::default();
        auth.bind_remote_url("ssh://git@[2001:db8::1]:2222/repo")
            .unwrap();
        assert_eq!(auth.challenge().unwrap().host, "[2001:db8::1]:2222");
        for url in [
            "https://good.test\\@other.test/repo",
            "https://name@good.test@other.test/repo",
            "https://good.test%2fother.test/repo",
            "https://good.test:0/repo",
            "https://good.test\n/repo",
            "https://[2001:db8::1]invalid/repo",
        ] {
            assert!(parse_remote(url).is_err());
        }
    }
}
