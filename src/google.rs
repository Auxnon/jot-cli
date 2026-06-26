//! Google Tasks integration: OAuth2 (loopback flow) and a thin REST client.
//!
//! Only compiled when the `google` feature is enabled. Credentials are read
//! from a config file the user creates; the OAuth token is cached next to it.

use std::{
    fs,
    io::{BufRead, BufReader, Write},
    net::TcpListener,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

const AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const TASKS_API: &str = "https://tasks.googleapis.com/tasks/v1";
const SCOPE: &str = "https://www.googleapis.com/auth/tasks";

/// OAuth client identity, created by the user in Google Cloud Console
/// (a "Desktop app" OAuth client with the Tasks API enabled).
#[derive(Debug, Clone, Deserialize)]
pub struct Credentials {
    pub client_id: String,
    pub client_secret: String,
}

/// Accepts either a flat `{client_id, client_secret}` object or the file you
/// download straight from Google Cloud Console, which nests the secrets under
/// an `installed` (Desktop app) or `web` key. Extra fields are ignored.
#[derive(Debug, Deserialize)]
struct CredentialsFile {
    installed: Option<Credentials>,
    web: Option<Credentials>,
    client_id: Option<String>,
    client_secret: Option<String>,
}

impl CredentialsFile {
    fn resolve(self) -> Option<Credentials> {
        if let Some(creds) = self.installed.or(self.web) {
            return Some(creds);
        }
        match (self.client_id, self.client_secret) {
            (Some(client_id), Some(client_secret)) => Some(Credentials {
                client_id,
                client_secret,
            }),
            _ => None,
        }
    }
}

/// Cached OAuth token. `expires_at` is a Unix timestamp (seconds).
#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenCache {
    access_token: String,
    refresh_token: String,
    expires_at: u64,
}

/// A Google task as returned by / sent to the Tasks API. Only the fields we
/// use are modelled; unknown fields are ignored on deserialize.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Task {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deleted: Option<bool>,
}

impl Task {
    /// Google encodes completion as a status string; map it to a bool.
    pub fn is_done(&self) -> bool {
        self.status.as_deref() == Some("completed")
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn credentials_path() -> PathBuf {
    crate::config_dir().join("google-credentials.json")
}

fn token_path() -> PathBuf {
    crate::config_dir().join("google-token.json")
}

pub fn load_credentials() -> Result<Credentials, String> {
    let path = credentials_path();
    let contents = fs::read_to_string(&path).map_err(|err| {
        format!(
            "could not read Google credentials at {}: {err}. \
             Save your downloaded OAuth client JSON there, or a flat \
             {{\"client_id\": …, \"client_secret\": …}} object.",
            path.display()
        )
    })?;
    let file: CredentialsFile = serde_json::from_str(&contents)
        .map_err(|err| format!("invalid Google credentials file {}: {err}", path.display()))?;
    file.resolve().ok_or_else(|| {
        format!(
            "Google credentials file {} has no client_id/client_secret \
             (expected a flat object or the downloaded {{\"installed\": {{…}}}} file)",
            path.display()
        )
    })
}

/// An authenticated Tasks API client.
pub struct GoogleClient {
    http: reqwest::blocking::Client,
    creds: Credentials,
    token: TokenCache,
}

impl GoogleClient {
    /// Build a client, reusing a cached token when possible, refreshing it if
    /// expired, or running the interactive consent flow on first use.
    pub fn connect() -> Result<Self, String> {
        let creds = load_credentials()?;
        let http = reqwest::blocking::Client::builder()
            .build()
            .map_err(|err| format!("failed to build HTTP client: {err}"))?;

        let token = match load_token() {
            Some(token) => token,
            None => authorize(&http, &creds)?,
        };

        let mut client = Self { http, creds, token };
        client.ensure_fresh()?;
        Ok(client)
    }

    /// Refresh the access token if it is expired (or about to be).
    fn ensure_fresh(&mut self) -> Result<(), String> {
        if self.token.expires_at > now_unix() + 60 {
            return Ok(());
        }
        let refreshed = refresh_token(&self.http, &self.creds, &self.token.refresh_token)?;
        self.token = refreshed;
        save_token(&self.token);
        Ok(())
    }

    fn auth_header(&self) -> String {
        format!("Bearer {}", self.token.access_token)
    }

    /// List every task in `tasklist`, including completed/hidden ones, across
    /// all pages.
    pub fn list_tasks(&self, tasklist: &str) -> Result<Vec<Task>, String> {
        let mut all = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut req = self
                .http
                .get(format!("{TASKS_API}/lists/{tasklist}/tasks"))
                .header("Authorization", self.auth_header())
                .query(&[
                    ("showCompleted", "true"),
                    ("showHidden", "true"),
                    ("maxResults", "100"),
                ]);
            if let Some(token) = &page_token {
                req = req.query(&[("pageToken", token)]);
            }

            let resp = req
                .send()
                .map_err(|err| format!("list tasks request failed: {err}"))?;
            let resp = check_status(resp)?;
            let page: TaskList = resp
                .json()
                .map_err(|err| format!("could not parse task list: {err}"))?;

            if let Some(items) = page.items {
                all.extend(items);
            }
            match page.next_page_token {
                Some(token) => page_token = Some(token),
                None => break,
            }
        }
        Ok(all)
    }

    /// Create a task in `tasklist`. When `parent` is set, the task is created
    /// as a child of that task.
    pub fn insert_task(
        &self,
        tasklist: &str,
        title: &str,
        done: bool,
        parent: Option<&str>,
    ) -> Result<Task, String> {
        let body = Task {
            title: Some(title.to_string()),
            status: Some(status_str(done).to_string()),
            ..Task::default()
        };
        let mut req = self
            .http
            .post(format!("{TASKS_API}/lists/{tasklist}/tasks"))
            .header("Authorization", self.auth_header())
            .json(&body);
        if let Some(parent) = parent {
            req = req.query(&[("parent", parent)]);
        }
        let resp = req
            .send()
            .map_err(|err| format!("insert task request failed: {err}"))?;
        let resp = check_status(resp)?;
        resp.json()
            .map_err(|err| format!("could not parse inserted task: {err}"))
    }

    /// Patch a task's title and/or completion state.
    pub fn patch_task(
        &self,
        tasklist: &str,
        task_id: &str,
        title: Option<&str>,
        done: Option<bool>,
    ) -> Result<(), String> {
        let body = Task {
            title: title.map(str::to_string),
            status: done.map(|d| status_str(d).to_string()),
            ..Task::default()
        };
        let resp = self
            .http
            .patch(format!("{TASKS_API}/lists/{tasklist}/tasks/{task_id}"))
            .header("Authorization", self.auth_header())
            .json(&body)
            .send()
            .map_err(|err| format!("patch task request failed: {err}"))?;
        check_status(resp)?;
        Ok(())
    }
}

fn status_str(done: bool) -> &'static str {
    if done { "completed" } else { "needsAction" }
}

#[derive(Debug, Deserialize)]
struct TaskList {
    items: Option<Vec<Task>>,
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

fn check_status(resp: reqwest::blocking::Response) -> Result<reqwest::blocking::Response, String> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().unwrap_or_default();
    Err(format!("Google API error {status}: {body}"))
}

// --- OAuth -----------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    expires_in: u64,
    refresh_token: Option<String>,
}

fn load_token() -> Option<TokenCache> {
    let contents = fs::read_to_string(token_path()).ok()?;
    serde_json::from_str(&contents).ok()
}

fn save_token(token: &TokenCache) {
    let path = token_path();
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    if let Ok(payload) = serde_json::to_string_pretty(token) {
        let _ = fs::write(&path, payload);
    }
}

/// Run the interactive OAuth2 loopback flow: open the consent page in the
/// browser, catch the redirect on a local port, and exchange the code.
fn authorize(http: &reqwest::blocking::Client, creds: &Credentials) -> Result<TokenCache, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|err| format!("could not open a local port for OAuth redirect: {err}"))?;
    let port = listener
        .local_addr()
        .map_err(|err| format!("could not read local OAuth port: {err}"))?
        .port();
    let redirect_uri = format!("http://127.0.0.1:{port}");
    // A loopback CSRF token; uniqueness is enough here.
    let state = format!("jot{}", now_unix());

    let auth_url = reqwest::Url::parse_with_params(
        AUTH_URL,
        &[
            ("client_id", creds.client_id.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("response_type", "code"),
            ("scope", SCOPE),
            ("access_type", "offline"),
            ("prompt", "consent"),
            ("state", state.as_str()),
        ],
    )
    .map_err(|err| format!("could not build auth URL: {err}"))?;

    println!("Opening your browser to authorize Google Tasks access.");
    println!("If it doesn't open, visit:\n{auth_url}\n");
    let _ = open::that(auth_url.as_str());

    let (code, returned_state) = wait_for_code(&listener)?;
    if returned_state != state {
        return Err(String::from("OAuth state mismatch; aborting for safety"));
    }

    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("code", code.as_str()),
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .map_err(|err| format!("token exchange request failed: {err}"))?;
    let resp = check_status(resp)?;
    let token: TokenResponse = resp
        .json()
        .map_err(|err| format!("could not parse token response: {err}"))?;

    let refresh_token = token.refresh_token.ok_or_else(|| {
        String::from("Google did not return a refresh token; revoke the app's access and retry")
    })?;
    let cache = TokenCache {
        access_token: token.access_token,
        refresh_token,
        expires_at: now_unix() + token.expires_in,
    };
    save_token(&cache);
    Ok(cache)
}

/// Block until the browser redirects back with `?code=...&state=...`.
fn wait_for_code(listener: &TcpListener) -> Result<(String, String), String> {
    let (stream, _) = listener
        .accept()
        .map_err(|err| format!("failed to accept OAuth redirect: {err}"))?;
    let mut reader = BufReader::new(&stream);
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|err| format!("failed to read OAuth redirect: {err}"))?;

    // Request line looks like: GET /?code=XYZ&state=jot123 HTTP/1.1
    let path = request_line
        .split_whitespace()
        .nth(1)
        .ok_or_else(|| String::from("malformed OAuth redirect request"))?;
    let url = reqwest::Url::parse(&format!("http://localhost{path}"))
        .map_err(|err| format!("could not parse OAuth redirect URL: {err}"))?;

    let mut code = None;
    let mut state = None;
    for (key, value) in url.query_pairs() {
        match key.as_ref() {
            "code" => code = Some(value.into_owned()),
            "state" => state = Some(value.into_owned()),
            "error" => return Err(format!("authorization denied: {value}")),
            _ => {}
        }
    }

    let body = "<html><body><h2>jot-cli is connected.</h2>\
                You can close this tab and return to the terminal.</body></html>";
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{}",
        body.len(),
        body
    );
    let mut stream = stream;
    let _ = stream.write_all(response.as_bytes());

    match (code, state) {
        (Some(code), Some(state)) => Ok((code, state)),
        _ => Err(String::from("OAuth redirect missing code/state")),
    }
}

fn refresh_token(
    http: &reqwest::blocking::Client,
    creds: &Credentials,
    refresh_token: &str,
) -> Result<TokenCache, String> {
    let resp = http
        .post(TOKEN_URL)
        .form(&[
            ("client_id", creds.client_id.as_str()),
            ("client_secret", creds.client_secret.as_str()),
            ("refresh_token", refresh_token),
            ("grant_type", "refresh_token"),
        ])
        .send()
        .map_err(|err| format!("token refresh request failed: {err}"))?;
    let resp = check_status(resp)?;
    let token: TokenResponse = resp
        .json()
        .map_err(|err| format!("could not parse refreshed token: {err}"))?;

    Ok(TokenCache {
        access_token: token.access_token,
        // A refresh response usually omits the refresh token; keep the old one.
        refresh_token: token
            .refresh_token
            .unwrap_or_else(|| refresh_token.to_string()),
        expires_at: now_unix() + token.expires_in,
    })
}

#[cfg(test)]
mod tests {
    use super::CredentialsFile;

    fn resolve(json: &str) -> Option<(String, String)> {
        let file: CredentialsFile = serde_json::from_str(json).unwrap();
        file.resolve().map(|c| (c.client_id, c.client_secret))
    }

    #[test]
    fn parses_flat_credentials() {
        let got = resolve(r#"{"client_id": "abc", "client_secret": "xyz"}"#);
        assert_eq!(got, Some(("abc".into(), "xyz".into())));
    }

    #[test]
    fn parses_downloaded_installed_file_with_extra_fields() {
        // Shape of the file Google Cloud Console hands you for a Desktop app.
        let json = r#"{
            "installed": {
                "client_id": "abc.apps.googleusercontent.com",
                "project_id": "my-proj",
                "auth_uri": "https://accounts.google.com/o/oauth2/auth",
                "token_uri": "https://oauth2.googleapis.com/token",
                "client_secret": "xyz",
                "redirect_uris": ["http://localhost"]
            }
        }"#;
        let got = resolve(json);
        assert_eq!(
            got,
            Some(("abc.apps.googleusercontent.com".into(), "xyz".into()))
        );
    }

    #[test]
    fn parses_web_wrapper() {
        let json = r#"{"web": {"client_id": "w", "client_secret": "s"}}"#;
        assert_eq!(resolve(json), Some(("w".into(), "s".into())));
    }

    #[test]
    fn json_without_recognized_credentials_resolves_to_none() {
        // Valid JSON, but no flat fields and no installed/web wrapper.
        assert_eq!(resolve(r#"{"project_id": "my-proj"}"#), None);
    }
}
