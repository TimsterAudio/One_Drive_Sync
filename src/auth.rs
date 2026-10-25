//! Signing in to a personal Microsoft account: auth code + PKCE through the
//! system browser and a loopback redirect, with the refresh token kept in the
//! OS keychain.

use std::collections::HashMap;
use std::fmt;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use percent_encoding::{percent_decode_str, utf8_percent_encode, NON_ALPHANUMERIC};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::graph::{self, DriveScope, TokenSource};
use crate::remote::{RemoteError, RemoteResult};

const AUTHORITY: &str = "https://login.microsoftonline.com/consumers/oauth2/v2.0";

/// Where the refresh token is kept between runs.
pub trait TokenStore: Send + Sync {
    fn load(&self) -> Option<String>;
    fn save(&self, token: &str) -> io::Result<()>;
    fn clear(&self);
}

/// The OS keychain: Keychain on macOS, Credential Manager on Windows, the
/// Secret Service (GNOME Keyring, KWallet) on Linux.
pub struct KeyringStore {
    service: String,
    user: String,
}

impl KeyringStore {
    /// `service` names the app in the keychain; one token per client id.
    pub fn new(service: &str, client_id: &str) -> Self {
        Self {
            service: service.to_string(),
            user: client_id.to_string(),
        }
    }

    /// Entry 0 is the token, or a count of the parts in entries 1..=n.
    fn entry(&self, part: usize) -> io::Result<keyring::Entry> {
        let user = if part == 0 {
            self.user.clone()
        } else {
            format!("{}#{part}", self.user)
        };
        keyring::Entry::new(&self.service, &user).map_err(io::Error::other)
    }

    fn read(&self, part: usize) -> Option<String> {
        let bytes = self.entry(part).ok()?.get_secret().ok()?;
        String::from_utf8(bytes).ok()
    }

    fn write(&self, part: usize, value: &str) -> io::Result<()> {
        self.entry(part)?
            .set_secret(value.as_bytes())
            .map_err(io::Error::other)
    }
}

/// Windows' Credential Manager holds at most 2560 bytes a secret; refresh
/// tokens can come close, so longer ones are split.
const KEYCHAIN_PART: usize = 2000;
const PARTS_PREFIX: &str = "parts:";

impl TokenStore for KeyringStore {
    fn load(&self) -> Option<String> {
        let first = self.read(0)?;
        let Some(n) = first.strip_prefix(PARTS_PREFIX) else {
            return Some(first);
        };
        let n: usize = n.parse().ok()?;
        (1..=n).map(|i| self.read(i)).collect()
    }

    fn save(&self, token: &str) -> io::Result<()> {
        self.clear();
        if token.len() <= KEYCHAIN_PART {
            return self.write(0, token);
        }
        // Tokens are ASCII, so byte chunks are whole characters.
        let parts: Vec<&str> = token
            .as_bytes()
            .chunks(KEYCHAIN_PART)
            .map(|c| std::str::from_utf8(c).map_err(io::Error::other))
            .collect::<io::Result<_>>()?;
        for (i, part) in parts.iter().enumerate() {
            self.write(i + 1, part)?;
        }
        self.write(0, &format!("{PARTS_PREFIX}{}", parts.len()))
    }

    fn clear(&self) {
        let parts = self
            .read(0)
            .and_then(|f| f.strip_prefix(PARTS_PREFIX)?.parse::<usize>().ok())
            .unwrap_or(0);
        for i in 0..=parts {
            if let Ok(e) = self.entry(i) {
                let _ = e.delete_credential();
            }
        }
    }
}

/// Keeps the token in memory only, for tests or a machine without a keychain.
#[derive(Default)]
pub struct MemoryStore(Mutex<Option<String>>);

impl TokenStore for MemoryStore {
    fn load(&self) -> Option<String> {
        self.0.lock().unwrap().clone()
    }

    fn save(&self, token: &str) -> io::Result<()> {
        *self.0.lock().unwrap() = Some(token.to_string());
        Ok(())
    }

    fn clear(&self) {
        *self.0.lock().unwrap() = None;
    }
}

#[derive(Debug)]
pub enum AuthError {
    /// The browser couldn't be opened, or the loopback port couldn't be.
    Io(io::Error),
    /// The sign-in was refused or abandoned in the browser.
    Denied(String),
    TimedOut,
    Cancelled,
    Remote(RemoteError),
}

impl fmt::Display for AuthError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AuthError::Io(e) => write!(f, "{e}"),
            AuthError::Denied(m) => write!(f, "sign-in refused: {m}"),
            AuthError::TimedOut => write!(f, "sign-in timed out"),
            AuthError::Cancelled => write!(f, "sign-in cancelled"),
            AuthError::Remote(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for AuthError {}

impl From<io::Error> for AuthError {
    fn from(e: io::Error) -> Self {
        AuthError::Io(e)
    }
}

#[derive(Deserialize)]
struct TokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
    error: Option<String>,
    error_description: Option<String>,
}

struct Cached {
    token: String,
    expires: Instant,
}

/// The app's sign-in. Shared between threads; hands out access tokens to
/// [`GraphDrive`](crate::graph::GraphDrive).
pub struct Auth {
    client_id: String,
    scope: DriveScope,
    store: Box<dyn TokenStore>,
    agent: ureq::Agent,
    cached: Mutex<Option<Cached>>,
}

impl Auth {
    /// `client_id` is the Azure app registration's (public client, personal
    /// accounts, `http://localhost` redirect under "Mobile and desktop").
    pub fn new(client_id: &str, scope: DriveScope, store: Box<dyn TokenStore>) -> Self {
        Self {
            client_id: client_id.trim().to_string(),
            scope,
            store,
            agent: graph::agent(),
            cached: Mutex::new(None),
        }
    }

    pub fn scope(&self) -> DriveScope {
        self.scope
    }

    /// True if a refresh token is saved (it may still have been withdrawn).
    pub fn is_signed_in(&self) -> bool {
        self.store.load().is_some()
    }

    pub fn sign_out(&self) {
        self.store.clear();
        *self.cached.lock().unwrap() = None;
    }

    /// Signs in through the browser. `open_url` opens the sign-in page; the
    /// call waits (up to five minutes, or until `cancel` is set) for the
    /// browser to come back to the loopback address.
    pub fn sign_in(
        &self,
        open_url: &mut dyn FnMut(&str) -> io::Result<()>,
        cancel: &AtomicBool,
    ) -> Result<(), AuthError> {
        let v4 = TcpListener::bind("127.0.0.1:0")?;
        let port = v4.local_addr()?.port();
        // "localhost" may resolve to either; listen on both where possible.
        let v6 = TcpListener::bind(("::1", port)).ok();
        let listeners: Vec<TcpListener> = std::iter::once(v4).chain(v6).collect();
        for l in &listeners {
            l.set_nonblocking(true)?;
        }

        let redirect = format!("http://localhost:{port}");
        let verifier = random_token();
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        let state = random_token();
        let url = format!(
            "{AUTHORITY}/authorize?client_id={}&response_type=code&redirect_uri={}\
             &response_mode=query&scope={}&code_challenge={challenge}\
             &code_challenge_method=S256&state={state}&prompt=select_account",
            enc(&self.client_id),
            enc(&redirect),
            enc(self.scope.scopes()),
        );
        open_url(&url)?;

        let deadline = Instant::now() + Duration::from_secs(300);
        let code = loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(AuthError::Cancelled);
            }
            if Instant::now() > deadline {
                return Err(AuthError::TimedOut);
            }
            let mut got = None;
            for l in &listeners {
                match l.accept() {
                    Ok((stream, _)) => {
                        got = Some(stream);
                        break;
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            let Some(stream) = got else {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            };
            if let Some(result) = handle_redirect(stream, &state) {
                break result?;
            }
        };

        let resp = self.token_request(&[
            ("client_id", &self.client_id),
            ("grant_type", "authorization_code"),
            ("code", &code),
            ("redirect_uri", &redirect),
            ("code_verifier", &verifier),
            ("scope", self.scope.scopes()),
        ]);
        self.take_tokens(resp.map_err(AuthError::Remote)?)
            .map_err(AuthError::Remote)
    }

    fn token_request(&self, form: &[(&str, &str)]) -> RemoteResult<TokenResponse> {
        let mut resp = self
            .agent
            .post(format!("{AUTHORITY}/token"))
            .send_form(form.iter().copied())
            .map_err(graph::transport_error)?;
        resp.body_mut()
            .read_json::<TokenResponse>()
            .map_err(graph::transport_error)
    }

    fn take_tokens(&self, resp: TokenResponse) -> RemoteResult<()> {
        let Some(access) = resp.access_token else {
            let error = resp.error.unwrap_or_default();
            let desc = resp.error_description.unwrap_or_default();
            if error == "invalid_grant" || error == "interaction_required" {
                self.sign_out();
            }
            let first = desc.lines().next().unwrap_or("").to_string();
            return Err(RemoteError::Auth(
                format!("{error} {first}").trim().to_string(),
            ));
        };
        if let Some(refresh) = &resp.refresh_token {
            self.store.save(refresh).map_err(|e| {
                RemoteError::Other(format!("couldn't save the sign-in to the keychain: {e}"))
            })?;
        }
        let life = resp.expires_in.unwrap_or(3600).saturating_sub(120);
        *self.cached.lock().unwrap() = Some(Cached {
            token: access,
            expires: Instant::now() + Duration::from_secs(life),
        });
        Ok(())
    }
}

impl TokenSource for Auth {
    fn access_token(&self) -> RemoteResult<String> {
        if let Some(c) = &*self.cached.lock().unwrap() {
            if Instant::now() < c.expires {
                return Ok(c.token.clone());
            }
        }
        let Some(refresh) = self.store.load() else {
            return Err(RemoteError::Auth("not signed in".into()));
        };
        let resp = self.token_request(&[
            ("client_id", &self.client_id),
            ("grant_type", "refresh_token"),
            ("refresh_token", &refresh),
            ("scope", self.scope.scopes()),
        ])?;
        self.take_tokens(resp)?;
        let cached = self.cached.lock().unwrap();
        Ok(cached.as_ref().map(|c| c.token.clone()).unwrap_or_default())
    }

    fn invalidate(&self) {
        *self.cached.lock().unwrap() = None;
    }
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

/// 32 random bytes, base64url: a PKCE verifier or a state value.
fn random_token() -> String {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).expect("no system randomness");
    URL_SAFE_NO_PAD.encode(bytes)
}

/// Parses `a=1&b=x%20y` into a map.
fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&')
        .filter_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let dec = |s: &str| {
                percent_decode_str(&s.replace('+', " "))
                    .decode_utf8_lossy()
                    .into_owned()
            };
            (!k.is_empty()).then(|| (dec(k), dec(v)))
        })
        .collect()
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Signed in</title>\
<body style='font-family:system-ui;margin:3em'><h2>Signed in to OneDrive</h2>\
<p>You can close this tab and go back to the app.</p>";

/// Answers one request to the loopback address. `None` if it wasn't the
/// redirect (a favicon request, say); otherwise the code or the refusal.
fn handle_redirect(mut stream: TcpStream, state: &str) -> Option<Result<String, AuthError>> {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buf = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") && buf.len() < 16 * 1024 {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
    let request = String::from_utf8_lossy(&buf);
    let target = request.split_whitespace().nth(1).unwrap_or("");
    let query = parse_query(target.split_once('?').map(|(_, q)| q).unwrap_or(""));

    let (outcome, page) = if let Some(error) = query.get("error") {
        let desc = query.get("error_description").cloned().unwrap_or_default();
        let msg = format!("{error} {desc}").trim().to_string();
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>Not signed in</title>\
             <body style='font-family:system-ui;margin:3em'><h2>Not signed in</h2><p>{}</p>",
            html_escape(&msg)
        );
        (Some(Err(AuthError::Denied(msg))), page)
    } else if let Some(code) = query.get("code") {
        if query.get("state").map(String::as_str) != Some(state) {
            (
                Some(Err(AuthError::Denied(
                    "the reply didn't match this sign-in".into(),
                ))),
                "Sign-in mismatch; try again from the app.".to_string(),
            )
        } else {
            (Some(Ok(code.clone())), DONE_PAGE.to_string())
        }
    } else {
        (None, String::new())
    };

    let status = if outcome.is_some() {
        "200 OK"
    } else {
        "404 Not Found"
    };
    let _ = write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{page}",
        page.len()
    );
    let _ = stream.flush();
    outcome
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_parsing() {
        let q = parse_query("code=M.C1%2Fx&state=abc&error_description=a+b");
        assert_eq!(q["code"], "M.C1/x");
        assert_eq!(q["state"], "abc");
        assert_eq!(q["error_description"], "a b");
    }

    #[test]
    fn loopback_takes_the_code() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.write_all(b"GET /?code=abc&state=xyz HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut reply = String::new();
            s.read_to_string(&mut reply).unwrap();
            reply
        });
        let (stream, _) = listener.accept().unwrap();
        let got = handle_redirect(stream, "xyz").unwrap().unwrap();
        assert_eq!(got, "abc");
        assert!(client.join().unwrap().starts_with("HTTP/1.1 200"));
    }

    #[test]
    fn pkce_values_are_urlsafe() {
        let t = random_token();
        assert_eq!(t.len(), 43);
        assert!(t
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    }
}
