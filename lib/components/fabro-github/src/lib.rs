use std::collections::HashSet;
use std::num::NonZeroU64;

use anyhow::{Context as _, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use chrono::{DateTime, Utc};
use fabro_redact::DisplaySafeUrl;
use fabro_static::EnvVars;
use fabro_types::PullRequestGithubDetail;
use fabro_types::settings::run::MergeStrategy;
use serde::Deserialize;
use tokio::process::Command;

use crate::token_source::SecretString;

pub mod access;
pub mod identity;
pub mod token_source;

#[cfg(any(test, feature = "test-support"))]
pub mod test_support;
#[cfg(test)]
pub(crate) mod tests_mock;

pub use access::GitHubRepositoryAccess;
pub use identity::GitHubAccountIdentity;
use identity::lookup_token_identity;

pub const GITHUB_API_BASE_URL: &str = "https://api.github.com";

/// Git config key that routes github.com HTTPS credentials through
/// [`GITHUB_CREDENTIAL_HELPER`].
pub const GITHUB_CREDENTIAL_HELPER_KEY: &str = "credential.https://github.com.helper";

/// Secret-free git credential helper: reads `$GITHUB_TOKEN` from the
/// invoking git process's environment at invocation time, so the token never
/// lands in git configuration, argv, or rendered errors. Non-`get`
/// operations (`store`, `erase`) are ignored. The one definition shared by
/// the runtime git bridge, server preflight probes, and the live contract
/// test, so the probes always exercise exactly what the bridge configures.
pub const GITHUB_CREDENTIAL_HELPER: &str = r#"!f() { if [ "$1" = get ]; then echo username=x-access-token; echo "password=$GITHUB_TOKEN"; fi; }; f"#;

/// Configure a `git` invocation to authenticate to github.com through
/// [`GITHUB_CREDENTIAL_HELPER`] with `token`, isolated from user/system git
/// configuration and terminal prompts. The token is passed only through the
/// child process environment.
pub fn apply_probe_git_env(command: &mut Command, token: &str) {
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env(EnvVars::GITHUB_TOKEN, token)
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", GITHUB_CREDENTIAL_HELPER_KEY)
        .env("GIT_CONFIG_VALUE_0", GITHUB_CREDENTIAL_HELPER);
}

/// Returns the GitHub API base URL, allowing override via `GITHUB_BASE_URL` env
/// var.
#[expect(
    clippy::disallowed_methods,
    reason = "GitHub API client exposes a documented process-env base URL override."
)]
pub fn github_api_base_url() -> String {
    std::env::var(EnvVars::GITHUB_BASE_URL).unwrap_or_else(|_| GITHUB_API_BASE_URL.to_string())
}

/// Bundle of GitHub credentials and the API base URL, threaded through every
/// authenticated GitHub call. Lets call sites pass one parameter instead of
/// two, and keeps the auth/endpoint pair from drifting apart.
#[derive(Debug, Clone)]
pub struct GitHubContext<'a> {
    creds:       &'a GitHubCredentials,
    base_url:    &'a str,
    http_client: Option<fabro_http::HttpClient>,
}

impl<'a> GitHubContext<'a> {
    pub fn new(creds: &'a GitHubCredentials, base_url: &'a str) -> Self {
        Self {
            creds,
            base_url,
            http_client: None,
        }
    }

    pub fn with_http_client(
        creds: &'a GitHubCredentials,
        base_url: &'a str,
        http_client: fabro_http::HttpClient,
    ) -> Self {
        Self {
            creds,
            base_url,
            http_client: Some(http_client),
        }
    }

    fn http_client(&self) -> anyhow::Result<fabro_http::HttpClient> {
        self.http_client.clone().map_or_else(http_client, Ok)
    }
}

/// Errors returned by pull-request endpoints. Callers branch on `NotFound` to
/// distinguish a missing PR from any other failure.
#[derive(Debug, thiserror::Error)]
pub enum PullRequestApiError {
    #[error("Pull request #{number} not found in {owner}/{repo}")]
    NotFound {
        owner:  String,
        repo:   String,
        number: u64,
    },
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// One open pull request returned by the authenticated user's GitHub search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredPullRequest {
    pub owner:      String,
    pub repo:       String,
    pub number:     NonZeroU64,
    pub title:      String,
    pub html_url:   String,
    pub draft:      bool,
    pub updated_at: DateTime<Utc>,
}

/// A complete or partially-complete authored pull request search.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthoredPullRequestSearch {
    pub author:  String,
    pub items:   Vec<AuthoredPullRequest>,
    pub partial: bool,
    pub warning: Option<String>,
}

fn http_client() -> anyhow::Result<fabro_http::HttpClient> {
    fabro_http::http_client().map_err(Into::into)
}

/// Owner information for a GitHub App.
#[derive(Debug, Clone, Deserialize)]
pub struct AppOwner {
    pub login: String,
}

/// Information about a GitHub App from the authenticated `/app` endpoint.
#[derive(Debug, Clone, Deserialize)]
pub struct AppInfo {
    pub slug:  String,
    pub owner: AppOwner,
}

/// Credentials for authenticating as a GitHub App.
#[derive(Clone, Debug)]
pub struct GitHubAppCredentials {
    pub app_id:          String,
    pub private_key_pem: String,
    pub slug:            Option<String>,
}

impl GitHubAppCredentials {
    #[expect(
        clippy::disallowed_methods,
        reason = "GitHub App credentials support a documented private-key env source."
    )]
    pub fn private_key_from_env() -> Result<Option<String>, String> {
        let Ok(raw) = std::env::var(EnvVars::GITHUB_APP_PRIVATE_KEY) else {
            return Ok(None);
        };
        decode_pem_env(EnvVars::GITHUB_APP_PRIVATE_KEY, &raw).map(Some)
    }

    pub fn from_env(app_id: Option<&str>) -> Result<Option<Self>, String> {
        Self::from_env_with_slug(app_id, None)
    }

    pub fn from_env_with_slug(
        app_id: Option<&str>,
        slug: Option<&str>,
    ) -> Result<Option<Self>, String> {
        let Some(app_id) = app_id else {
            return Ok(None);
        };
        let Some(private_key_pem) = Self::private_key_from_env()? else {
            return Ok(None);
        };
        Ok(Some(Self {
            app_id: app_id.to_string(),
            private_key_pem,
            slug: slug
                .map(str::trim)
                .filter(|slug| !slug.is_empty())
                .map(str::to_string),
        }))
    }

    pub fn installation_url(&self, owner: &str) -> Option<String> {
        self.slug.as_ref().map(|slug| {
            format!("https://github.com/organizations/{owner}/settings/apps/{slug}/installations")
        })
    }

    pub async fn mint_installation_token(
        &self,
        client: &impl HttpClient,
        owner: &str,
        repo: &str,
        base_url: &str,
        permissions: serde_json::Value,
        install_url: Option<&str>,
    ) -> anyhow::Result<InstallationToken> {
        self.mint_installation_token_for_repositories(
            client,
            owner,
            &[repo.to_string()],
            base_url,
            permissions,
            install_url,
        )
        .await
    }

    /// Mint one installation token scoped to every repository in
    /// `repository_names` (names within `owner`'s installation, primary
    /// first) with the shared `permissions`.
    pub async fn mint_installation_token_for_repositories(
        &self,
        client: &impl HttpClient,
        owner: &str,
        repository_names: &[String],
        base_url: &str,
        permissions: serde_json::Value,
        install_url: Option<&str>,
    ) -> anyhow::Result<InstallationToken> {
        let jwt = sign_app_jwt(&self.app_id, &self.private_key_pem)?;
        let default_install_url = self.installation_url(owner);
        let install_url = install_url.or(default_install_url.as_deref());
        mint_installation_token_with_jwt(
            client,
            &jwt,
            owner,
            repository_names,
            base_url,
            permissions,
            install_url,
        )
        .await
    }
}

#[derive(Clone, Debug)]
pub struct InstallationToken {
    pub token:      String,
    pub expires_at: DateTime<Utc>,
}

impl InstallationToken {
    pub fn near_expiry(&self, threshold: std::time::Duration) -> bool {
        let threshold = chrono::Duration::from_std(threshold).unwrap_or(chrono::Duration::MAX);
        self.expires_at <= Utc::now() + threshold
    }

    pub fn valid_token(&self) -> anyhow::Result<&str> {
        if self.expires_at <= Utc::now() {
            bail!(
                "GitHub installation access token expired at {}",
                self.expires_at
            );
        }
        Ok(&self.token)
    }
}

#[derive(Clone, Debug)]
pub enum GitHubCredentials {
    App(GitHubAppCredentials),
    Pat(String),
    Installation(InstallationToken),
}

impl GitHubCredentials {
    /// Whether resolving these credentials mints a new installation token.
    #[must_use]
    pub fn mints_installation_token(&self) -> bool {
        matches!(self, Self::App(_))
    }

    pub fn from_env(app_id: Option<&str>) -> Result<Option<Self>, String> {
        Ok(GitHubAppCredentials::from_env(app_id)?.map(Self::App))
    }

    pub fn from_env_with_slug(
        app_id: Option<&str>,
        slug: Option<&str>,
    ) -> Result<Option<Self>, String> {
        Ok(GitHubAppCredentials::from_env_with_slug(app_id, slug)?.map(Self::App))
    }

    pub async fn resolve_bearer_token(
        &self,
        client: &impl HttpClient,
        owner: &str,
        repo: &str,
        base_url: &str,
        permissions: serde_json::Value,
    ) -> anyhow::Result<String> {
        match self {
            Self::App(creds) => {
                let install_url = creds.installation_url(owner);
                creds
                    .mint_installation_token(
                        client,
                        owner,
                        repo,
                        base_url,
                        permissions,
                        install_url.as_deref(),
                    )
                    .await
                    .map(|token| token.token)
            }
            Self::Pat(token) => Ok(token.clone()),
            Self::Installation(token) => token.valid_token().map(str::to_owned),
        }
    }
}

#[derive(Deserialize)]
struct SearchIssuesResponse {
    total_count:        u64,
    #[serde(default)]
    incomplete_results: bool,
    items:              Vec<SearchIssue>,
}

#[derive(Deserialize)]
struct SearchIssue {
    repository_url: String,
    number:         u64,
    title:          String,
    html_url:       String,
    state:          String,
    #[serde(default)]
    draft:          bool,
    updated_at:     DateTime<Utc>,
    pull_request:   SearchIssuePullRequest,
}

#[derive(Deserialize)]
struct SearchIssuePullRequest {
    merged_at: Option<DateTime<Utc>>,
}

const SEARCH_PAGE_SIZE: usize = 100;
const SEARCH_RESULT_LIMIT: u64 = 1_000;

/// Search every repository visible to the configured user credential for open
/// pull requests authored by that user. GitHub's search API caps a query at
/// 1,000 results; reaching that cap is reported as a partial result.
pub async fn search_open_authored_pull_requests(
    ctx: &GitHubContext<'_>,
    signed_in_github_login: Option<&str>,
) -> anyhow::Result<AuthoredPullRequestSearch> {
    let token = match ctx.creds {
        GitHubCredentials::Pat(token) => token.clone(),
        GitHubCredentials::Installation(token) => token.valid_token()?.to_string(),
        GitHubCredentials::App(_) => {
            bail!(
                "GitHub App credentials cannot identify or search a user's authored pull requests"
            )
        }
    };
    let author = match signed_in_github_login
        .map(str::trim)
        .filter(|login| !login.is_empty())
    {
        Some(login) => login.to_string(),
        None => {
            lookup_token_identity(&ctx.http_client()?, &token, ctx.base_url)
                .await?
                .login
        }
    };
    validate_search_login(&author)?;
    search_open_authored_pull_requests_with_client(
        &ctx.http_client()?,
        &token,
        ctx.base_url,
        author,
    )
    .await
}

fn validate_search_login(login: &str) -> anyhow::Result<()> {
    if login.is_empty()
        || !login
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '-')
    {
        bail!("GitHub returned an invalid login for authored pull request search");
    }
    Ok(())
}

async fn search_open_authored_pull_requests_with_client(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    author: String,
) -> anyhow::Result<AuthoredPullRequestSearch> {
    let auth = format!("Bearer {token}");
    let mut page = 1_u64;
    let mut items = Vec::new();
    let mut seen = HashSet::new();
    let mut expected = None;
    let mut partial = false;
    let mut warning = None;

    loop {
        let url = format!(
            "{base_url}/search/issues?q=is%3Apr+is%3Aopen+author%3A{author}&per_page={SEARCH_PAGE_SIZE}&page={page}"
        );
        let response = match client
            .request(HttpMethod::Get, &url, &github_headers(&auth), None)
            .await
        {
            Ok(response) => response,
            Err(_err) if page > 1 => {
                partial = true;
                warning = Some(format!("GitHub search stopped after page {}.", page - 1));
                break;
            }
            Err(err) => return Err(err).context("Failed to search GitHub pull requests"),
        };
        if response.status != 200 {
            let message = format!(
                "GitHub returned status {} searching pull requests",
                response.status
            );
            if page == 1 {
                bail!("{message}");
            }
            partial = true;
            warning = Some(message);
            break;
        }

        let search: SearchIssuesResponse = match response.json() {
            Ok(search) => search,
            Err(_err) if page > 1 => {
                partial = true;
                warning = Some(format!(
                    "GitHub returned an invalid pull request search page {page}."
                ));
                break;
            }
            Err(err) => return Err(err).context("Failed to parse GitHub pull request search"),
        };
        expected.get_or_insert(search.total_count);
        if search.incomplete_results {
            partial = true;
            warning = Some(
                "GitHub reported incomplete search results; some open pull requests may be missing."
                    .to_string(),
            );
        }
        let page_len = search.items.len();
        for item in search.items {
            if item.state != "open" || item.pull_request.merged_at.is_some() {
                continue;
            }
            let Some(number) = NonZeroU64::new(item.number) else {
                partial = true;
                warning = Some(
                    "GitHub returned a pull request with an invalid pull request number."
                        .to_string(),
                );
                continue;
            };
            let Some((owner, repo)) = repository_coordinates(&item.repository_url) else {
                partial = true;
                warning = Some(
                    "GitHub returned a pull request with an invalid repository URL.".to_string(),
                );
                continue;
            };
            if seen.insert((owner.clone(), repo.clone(), number)) {
                items.push(AuthoredPullRequest {
                    owner,
                    repo,
                    number,
                    title: item.title,
                    html_url: item.html_url,
                    draft: item.draft,
                    updated_at: item.updated_at,
                });
            }
        }

        let total_count = expected.unwrap_or_default();
        if page_len < SEARCH_PAGE_SIZE || page * SEARCH_PAGE_SIZE as u64 >= total_count {
            break;
        }
        if page * SEARCH_PAGE_SIZE as u64 >= SEARCH_RESULT_LIMIT {
            partial = true;
            warning = Some(
                "GitHub search returned more than 1,000 open pull requests; showing the first 1,000."
                    .to_string(),
            );
            break;
        }
        page += 1;
    }

    items.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));
    Ok(AuthoredPullRequestSearch {
        author,
        items,
        partial,
        warning,
    })
}

fn repository_coordinates(repository_url: &str) -> Option<(String, String)> {
    let path = repository_url.split_once("/repos/")?.1;
    let (owner, repo) = path.split_once('/')?;
    (!owner.is_empty() && !repo.is_empty() && !repo.contains('/'))
        .then(|| (owner.to_string(), repo.to_string()))
}

pub fn validate_static_github_token(token: &str) -> anyhow::Result<()> {
    if token.starts_with("ghs_") {
        bail!(
            "GitHub installation access token (ghs_*) cannot be configured as a static token \
             because it expires quickly; use a PAT or GitHub App credentials instead"
        );
    }
    Ok(())
}

pub async fn gh_auth_token() -> anyhow::Result<String> {
    let output = Command::new("gh")
        .args(["auth", "token"])
        .output()
        .await
        .context("Failed to run `gh auth token`")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let message = if stderr.is_empty() {
            format!("`gh auth token` exited with status {}", output.status)
        } else {
            stderr
        };
        bail!("Failed to get GitHub CLI token: {message}");
    }

    let token =
        String::from_utf8(output.stdout).context("`gh auth token` returned invalid UTF-8")?;
    let token = token.trim().to_string();
    if token.is_empty() {
        bail!("`gh auth token` returned an empty token");
    }
    Ok(token)
}

fn decode_pem_env(name: &str, raw: &str) -> Result<String, String> {
    if raw.starts_with("-----") {
        return Ok(raw.to_string());
    }
    let pem_bytes = STANDARD
        .decode(raw)
        .map_err(|err| format!("{name} is not valid PEM or base64: {err}"))?;
    String::from_utf8(pem_bytes)
        .map_err(|err| format!("{name} base64 decoded to invalid UTF-8: {err}"))
}

/// HTTP method used in GitHub API calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
    Put,
    Patch,
}

/// A minimal HTTP response for testability.
pub struct HttpResponse {
    pub status: u16,
    body:       String,
}

impl HttpResponse {
    pub fn new(status: u16, body: String) -> Self {
        Self { status, body }
    }

    pub fn json<T: for<'de> Deserialize<'de>>(&self) -> anyhow::Result<T> {
        serde_json::from_str(&self.body).context("Failed to parse response")
    }

    pub fn text(&self) -> &str {
        &self.body
    }
}

/// Abstract HTTP client for GitHub API calls.
///
/// Implemented for `fabro_http::HttpClient` in production; tests use a mock
/// to avoid TCP/process overhead.
pub trait HttpClient: Send + Sync {
    fn request(
        &self,
        method: HttpMethod,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> impl std::future::Future<Output = anyhow::Result<HttpResponse>> + Send;
}

impl HttpClient for fabro_http::HttpClient {
    async fn request(
        &self,
        method: HttpMethod,
        url: &str,
        headers: &[(&str, &str)],
        body: Option<&serde_json::Value>,
    ) -> anyhow::Result<HttpResponse> {
        let mut builder = match method {
            HttpMethod::Get => self.get(url),
            HttpMethod::Post => self.post(url),
            HttpMethod::Put => self.put(url),
            HttpMethod::Patch => self.patch(url),
        };
        for &(key, value) in headers {
            builder = builder.header(key, value);
        }
        if let Some(json_body) = body {
            builder = builder.json(json_body);
        }
        let resp = builder.send().await.map_err(anyhow::Error::new)?;
        let status = resp.status().as_u16();
        let text = resp.text().await.map_err(anyhow::Error::new)?;
        Ok(HttpResponse::new(status, text))
    }
}

/// Parse `owner` and `repo` from a GitHub HTTPS URL.
///
/// Accepts URLs like:
/// - `https://github.com/owner/repo.git`
/// - `https://github.com/owner/repo`
/// - `https://github.com/owner/repo/`
/// - `https://x-access-token:TOKEN@github.com/owner/repo.git`
pub fn parse_github_owner_repo(url: &str) -> anyhow::Result<(String, String)> {
    // Strip credentials from URLs like https://x-access-token:TOKEN@github.com/...
    let stripped = url.strip_prefix("https://").and_then(|rest| {
        rest.split_once('@')
            .map(|(_, after)| format!("https://{after}"))
    });
    let url = stripped.as_deref().unwrap_or(url);
    let display_url = redacted_url_for_error(url);
    let path = url
        .strip_prefix("https://github.com/")
        .ok_or_else(|| anyhow!("Not a GitHub HTTPS URL: {display_url}"))?;

    let path = path.trim_end_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);

    let mut parts = path.split('/');
    let owner = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Missing owner in GitHub URL: {display_url}"))?;
    let repo = parts
        .next()
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("Missing repo in GitHub URL: {display_url}"))?;
    if parts.next().is_some() {
        bail!("GitHub URL must identify one repository: {display_url}");
    }

    Ok((owner.to_string(), repo.to_string()))
}

fn redacted_url_for_error(url: &str) -> String {
    DisplaySafeUrl::parse(url)
        .map_or_else(|_| "<invalid url>".to_string(), |url| url.redacted_string())
}

/// Create a signed JWT for GitHub App authentication (RS256).
///
/// The JWT is valid for 10 minutes with a 60-second clock skew allowance.
pub fn sign_app_jwt(app_id: &str, private_key_pem: &str) -> anyhow::Result<String> {
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use serde::Serialize;

    #[derive(Serialize)]
    struct Claims {
        iss: String,
        iat: i64,
        exp: i64,
    }

    let now = chrono::Utc::now().timestamp();
    let claims = Claims {
        iss: app_id.to_string(),
        iat: now - 60,
        exp: now + 600,
    };

    let key =
        EncodingKey::from_rsa_pem(private_key_pem.as_bytes()).context("Invalid RSA private key")?;

    let jwt =
        encode(&Header::new(Algorithm::RS256), &claims, &key).context("Failed to sign JWT")?;
    Ok(jwt)
}

/// Standard GitHub API headers for authenticated requests.
pub(crate) fn github_headers(auth: &str) -> [(&str, &str); 3] {
    [
        ("Authorization", auth),
        ("Accept", "application/vnd.github+json"),
        ("User-Agent", "fabro"),
    ]
}

/// Request a scoped Installation Access Token for a specific repository.
///
/// Uses the App JWT to find the installation for `owner/repo`, then requests
/// a token scoped to the given `permissions` on that single repository.
pub async fn create_installation_access_token_with_permissions(
    client: &impl HttpClient,
    jwt: &str,
    owner: &str,
    repo: &str,
    base_url: &str,
    permissions: serde_json::Value,
) -> anyhow::Result<String> {
    create_installation_access_token_with_permissions_and_install_url(
        client,
        jwt,
        owner,
        repo,
        base_url,
        permissions,
        None,
    )
    .await
}

pub async fn create_installation_access_token_with_permissions_and_install_url(
    client: &impl HttpClient,
    jwt: &str,
    owner: &str,
    repo: &str,
    base_url: &str,
    permissions: serde_json::Value,
    install_url: Option<&str>,
) -> anyhow::Result<String> {
    mint_installation_token_with_jwt(
        client,
        jwt,
        owner,
        &[repo.to_string()],
        base_url,
        permissions,
        install_url,
    )
    .await
    .map(|token| token.token)
}

/// Outcome of one GitHub App installation lookup
/// (`GET /repos/{owner}/{repo}/installation`).
///
/// Status interpretation stays with callers because their user guidance
/// differs: the mint path speaks about the owner's App installation, the
/// multi-repository resolution names the specific repository.
pub(crate) enum InstallationLookup {
    Found(u64),
    /// 404: the App cannot see the repository (or is not installed at all).
    NotFound,
    /// Any other non-200 status.
    Failed(u16),
}

/// Look up the App installation covering `owner/repo` and parse its id.
/// Transport and parse failures carry no caller-specific context; callers
/// attach their own.
pub(crate) async fn lookup_installation(
    client: &impl HttpClient,
    jwt: &str,
    base_url: &str,
    owner: &str,
    repo: &str,
) -> anyhow::Result<InstallationLookup> {
    #[derive(Deserialize)]
    struct Installation {
        id: u64,
    }

    let endpoint = format!("{base_url}/repos/{owner}/{repo}/installation");
    let auth = format!("Bearer {jwt}");
    let resp = client
        .request(HttpMethod::Get, &endpoint, &github_headers(&auth), None)
        .await?;
    match resp.status {
        200 => {
            let installation: Installation = resp
                .json()
                .context("Failed to parse installation response")?;
            Ok(InstallationLookup::Found(installation.id))
        }
        404 => Ok(InstallationLookup::NotFound),
        status => Ok(InstallationLookup::Failed(status)),
    }
}

async fn mint_installation_token_with_jwt(
    client: &impl HttpClient,
    jwt: &str,
    owner: &str,
    repos: &[String],
    base_url: &str,
    permissions: serde_json::Value,
    install_url: Option<&str>,
) -> anyhow::Result<InstallationToken> {
    let Some(primary_repo) = repos.first() else {
        bail!("installation token mint requires at least one repository");
    };

    // Step 1: Find the installation via the primary repository. Multi-
    // repository callers resolve every repository's installation up front
    // (`GitHubRepositoryAccess::resolve_shared_installation`), so the
    // primary stands for the whole set here.
    let installation_id = match lookup_installation(client, jwt, base_url, owner, primary_repo)
        .await
        .context("Failed to look up GitHub App installation")?
    {
        InstallationLookup::Found(id) => id,
        InstallationLookup::NotFound => {
            let install_url = install_url.map_or_else(
                || format!("https://github.com/organizations/{owner}/settings/installations"),
                str::to_string,
            );
            bail!(
                "GitHub App is not installed for {owner}, or its installation does not \
                 include {owner}/{primary_repo}. Check repository access at {install_url}"
            );
        }
        InstallationLookup::Failed(403) => {
            bail!(
                "GitHub App installation is suspended. \
                 Re-enable it in your organization's GitHub App settings."
            );
        }
        InstallationLookup::Failed(401) => {
            bail!(
                "GitHub App authentication failed. \
                 Check that app_id and GITHUB_APP_PRIVATE_KEY are correct."
            );
        }
        InstallationLookup::Failed(status) => {
            bail!("Unexpected status {status} looking up GitHub App installation");
        }
    };

    mint_installation_token_for_id_with_jwt(
        client,
        jwt,
        installation_id,
        repos,
        base_url,
        permissions,
    )
    .await
}

/// Create a repository-scoped token for an installation already resolved by
/// the caller.
pub(crate) async fn mint_installation_token_for_id_with_jwt(
    client: &impl HttpClient,
    jwt: &str,
    installation_id: u64,
    repos: &[String],
    base_url: &str,
    permissions: serde_json::Value,
) -> anyhow::Result<InstallationToken> {
    #[derive(Deserialize)]
    struct AccessToken {
        token:      String,
        expires_at: DateTime<Utc>,
    }

    let auth = format!("Bearer {jwt}");
    let token_url = format!("{base_url}/app/installations/{installation_id}/access_tokens");
    let body = serde_json::json!({
        "repositories": repos,
        "permissions": permissions,
    });

    let token_resp = client
        .request(
            HttpMethod::Post,
            &token_url,
            &github_headers(&auth),
            Some(&body),
        )
        .await
        .context("Failed to create installation access token")?;

    match token_resp.status {
        201 => {}
        422 => {
            bail!(
                "GitHub App does not have access to every requested repository ({}). \
                 Update the installation's repository permissions to include them.",
                repos.join(", ")
            );
        }
        401 => {
            bail!(
                "GitHub App authentication failed. \
                 Check that app_id and GITHUB_APP_PRIVATE_KEY are correct."
            );
        }
        _ => {
            bail!(
                "Unexpected status {} creating installation access token",
                token_resp.status
            );
        }
    }

    let access_token: AccessToken = token_resp
        .json()
        .context("Failed to parse access token response")?;

    Ok(InstallationToken {
        token:      access_token.token,
        expires_at: access_token.expires_at,
    })
}

/// Request a scoped Installation Access Token with `contents: write`.
pub async fn create_installation_access_token(
    client: &impl HttpClient,
    jwt: &str,
    owner: &str,
    repo: &str,
    base_url: &str,
) -> anyhow::Result<String> {
    create_installation_access_token_with_permissions(
        client,
        jwt,
        owner,
        repo,
        base_url,
        serde_json::json!({ "contents": "write" }),
    )
    .await
}

/// Request a scoped Installation Access Token with `contents: write`
/// and `pull_requests: write`. Used for creating pull requests.
pub async fn create_installation_access_token_for_pr(
    client: &impl HttpClient,
    jwt: &str,
    owner: &str,
    repo: &str,
    base_url: &str,
) -> anyhow::Result<String> {
    create_installation_access_token_with_permissions(
        client,
        jwt,
        owner,
        repo,
        base_url,
        serde_json::json!({ "contents": "write", "pull_requests": "write" }),
    )
    .await
}

/// Pull request created on, or reconciled from, GitHub.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreatedPullRequest {
    pub html_url: String,
    pub number:   u64,
    pub node_id:  String,
    pub title:    String,
}

/// Find an open pull request that already carries the expected head commit.
///
/// This supports recovery when GitHub created a pull request but the caller
/// stopped before it could persist the result locally.
pub async fn find_open_pull_request(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
    expected_head_sha: &str,
) -> anyhow::Result<Option<CreatedPullRequest>> {
    let client = ctx.http_client()?;
    find_open_pull_request_with_client(&client, ctx, owner, repo, base, head, expected_head_sha)
        .await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Pull request reconciliation needs explicit repo, branch, and commit coordinates."
)]
pub async fn find_open_pull_request_with_client(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
    expected_head_sha: &str,
) -> anyhow::Result<Option<CreatedPullRequest>> {
    #[derive(Deserialize)]
    struct PullRequestHead {
        sha: String,
    }

    #[derive(Deserialize)]
    struct PullRequestListItem {
        html_url: String,
        number:   u64,
        node_id:  String,
        title:    String,
        head:     PullRequestHead,
    }

    let token = ctx
        .creds
        .resolve_bearer_token(
            client,
            owner,
            repo,
            ctx.base_url,
            serde_json::json!({ "contents": "write", "pull_requests": "write" }),
        )
        .await?;
    let mut url = DisplaySafeUrl::parse(&format!("{}/repos/{owner}/{repo}/pulls", ctx.base_url))
        .context("Failed to build pull request reconciliation URL")?;
    url.query_pairs_mut()
        .append_pair("state", "open")
        .append_pair("base", base)
        .append_pair("head", &format!("{owner}:{head}"));
    let auth = format!("Bearer {token}");
    let resp = client
        .request(
            HttpMethod::Get,
            &url.raw_string(),
            &github_headers(&auth),
            None,
        )
        .await
        .context("Failed to find an existing pull request")?;
    if resp.status != 200 {
        bail!(
            "Unexpected status {} finding an existing pull request: {}",
            resp.status,
            resp.text()
        );
    }

    let pull_requests = resp
        .json::<Vec<PullRequestListItem>>()
        .context("Failed to parse existing pull request response")?;
    Ok(pull_requests
        .into_iter()
        .find(|pull_request| pull_request.head.sha == expected_head_sha)
        .map(|pull_request| CreatedPullRequest {
            html_url: pull_request.html_url,
            number:   pull_request.number,
            node_id:  pull_request.node_id,
            title:    pull_request.title,
        }))
}

/// Create a pull request on GitHub.
///
/// Signs a JWT, obtains a PR-scoped installation token, and POSTs to the
/// GitHub pulls API.
#[allow(
    clippy::too_many_arguments,
    reason = "Creating a pull request needs explicit repo, branch, and body fields."
)]
pub async fn create_pull_request(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> anyhow::Result<CreatedPullRequest> {
    let client = ctx.http_client()?;
    create_pull_request_with_client(&client, ctx, owner, repo, base, head, title, body, draft).await
}

#[allow(
    clippy::too_many_arguments,
    reason = "Creating a pull request needs explicit repo, branch, and body fields."
)]
pub async fn create_pull_request_with_client(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    base: &str,
    head: &str,
    title: &str,
    body: &str,
    draft: bool,
) -> anyhow::Result<CreatedPullRequest> {
    #[derive(Deserialize)]
    struct PullRequestResponse {
        html_url: String,
        number:   u64,
        node_id:  String,
    }

    let token = ctx
        .creds
        .resolve_bearer_token(
            client,
            owner,
            repo,
            ctx.base_url,
            serde_json::json!({ "contents": "write", "pull_requests": "write" }),
        )
        .await?;

    tracing::info!(title = %title, head = %head, base = %base, draft, "Creating pull request");

    let pr_body = serde_json::json!({
        "title": title,
        "head": head,
        "base": base,
        "body": body,
        "draft": draft,
    });

    let url = format!("{}/repos/{owner}/{repo}/pulls", ctx.base_url);
    let auth = format!("Bearer {token}");
    let resp = HttpClient::request(
        client,
        HttpMethod::Post,
        &url,
        &github_headers(&auth),
        Some(&pr_body),
    )
    .await
    .context("Failed to create pull request")?;

    match resp.status {
        201 => {}
        422 => {
            bail!("Pull request could not be created (422): {}", resp.text());
        }
        401 | 403 => {
            bail!(
                "Authentication failed creating pull request ({})",
                resp.status
            );
        }
        _ => {
            bail!(
                "Unexpected status {} creating pull request: {}",
                resp.status,
                resp.text()
            );
        }
    }

    let pr: PullRequestResponse = resp
        .json()
        .context("Failed to parse pull request response")?;

    Ok(CreatedPullRequest {
        html_url: pr.html_url,
        number:   pr.number,
        node_id:  pr.node_id,
        title:    title.to_string(),
    })
}

fn merge_method_as_graphql_value(method: MergeStrategy) -> &'static str {
    match method {
        MergeStrategy::Merge => "MERGE",
        MergeStrategy::Squash => "SQUASH",
        MergeStrategy::Rebase => "REBASE",
    }
}

/// Enable auto-merge on a pull request via GitHub's GraphQL API.
///
/// Requires the PR's `node_id` (from the REST API response) and a merge method.
/// The repository must have auto-merge enabled in its settings.
pub async fn enable_auto_merge(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    pr_node_id: &str,
    merge_method: MergeStrategy,
) -> anyhow::Result<()> {
    let client = ctx.http_client()?;
    enable_auto_merge_with_client(&client, ctx, owner, repo, pr_node_id, merge_method).await
}

pub async fn enable_auto_merge_with_client(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    pr_node_id: &str,
    merge_method: MergeStrategy,
) -> anyhow::Result<()> {
    let token = ctx
        .creds
        .resolve_bearer_token(
            client,
            owner,
            repo,
            ctx.base_url,
            serde_json::json!({ "contents": "write", "pull_requests": "write" }),
        )
        .await?;

    let graphql_value = merge_method_as_graphql_value(merge_method);
    let query = format!(
        r#"mutation {{
  enablePullRequestAutoMerge(input: {{pullRequestId: "{pr_node_id}", mergeMethod: {graphql_value}}}) {{
    pullRequest {{
      autoMergeRequest {{
        enabledAt
        mergeMethod
      }}
    }}
  }}
}}"#,
    );

    tracing::debug!(
        pr_node_id,
        merge_method = graphql_value,
        "Enabling auto-merge"
    );

    let graphql_url = format!("{}/graphql", ctx.base_url);
    let auth = format!("Bearer {token}");
    let graphql_body = serde_json::json!({ "query": query });
    let resp = HttpClient::request(
        client,
        HttpMethod::Post,
        &graphql_url,
        &[("Authorization", auth.as_str()), ("User-Agent", "fabro")],
        Some(&graphql_body),
    )
    .await
    .context("Failed to enable auto-merge")?;

    let status = resp.status;
    let body: serde_json::Value = resp.json().context("Failed to parse auto-merge response")?;

    if !(200..300).contains(&status) {
        bail!("Auto-merge request failed ({status}): {body}");
    }

    if let Some(errors) = body.get("errors") {
        bail!("Auto-merge GraphQL error: {errors}");
    }

    tracing::info!(pr_node_id, "Auto-merge enabled");
    Ok(())
}

/// Convert a Git SSH URL to HTTPS format for token-based authentication.
///
/// SSH URLs like `git@github.com:owner/repo.git` become
/// `https://github.com/owner/repo.git`. URLs that are already HTTPS
/// (or any other non-SSH format) are returned unchanged.
pub fn ssh_url_to_https(url: &str) -> String {
    // Match `git@<host>:<path>` (standard SSH URL format)
    if let Some(rest) = url.strip_prefix("git@") {
        if let Some((host, path)) = rest.split_once(':') {
            return format!("https://{host}/{path}");
        }
    }
    // Match `ssh://git@<host>/<path>`
    if let Some(rest) = url.strip_prefix("ssh://git@") {
        return format!("https://{rest}");
    }
    url.to_string()
}

pub fn normalize_repo_origin_url(url: &str) -> String {
    let https = ssh_url_to_https(url.trim());
    let without_credentials = strip_https_credentials(&https);
    let normalized = normalize_https_host_path(&without_credentials);
    let normalized = normalized.trim_end_matches('/');
    normalized
        .strip_suffix(".git")
        .unwrap_or(normalized)
        .to_string()
}

fn strip_https_credentials(url: &str) -> String {
    let Some(rest) = url.strip_prefix("https://") else {
        return url.to_string();
    };

    match rest.split_once('@') {
        Some((before, after)) if !before.contains('/') => format!("https://{after}"),
        _ => url.to_string(),
    }
}

fn normalize_https_host_path(url: &str) -> String {
    let Some(rest) = url.strip_prefix("https://") else {
        return url.to_string();
    };

    match rest.split_once(':') {
        Some((host, path)) if !host.contains('/') && !path.starts_with('/') => {
            format!("https://{host}/{path}")
        }
        _ => url.to_string(),
    }
}

/// Return the commit SHA at the head of a GitHub branch.
///
/// Returns `None` when the branch does not exist.
pub async fn branch_head_sha(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    branch: &str,
) -> anyhow::Result<Option<String>> {
    let client = ctx.http_client()?;
    branch_head_sha_with_client(&client, ctx, owner, repo, branch).await
}

async fn branch_head_sha_with_client(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    branch: &str,
) -> anyhow::Result<Option<String>> {
    #[derive(Deserialize)]
    struct BranchResponse {
        commit: BranchCommit,
    }

    #[derive(Deserialize)]
    struct BranchCommit {
        sha: String,
    }

    let token = ctx
        .creds
        .resolve_bearer_token(
            client,
            owner,
            repo,
            ctx.base_url,
            serde_json::json!({ "contents": "read" }),
        )
        .await?;

    let url = format!("{}/repos/{owner}/{repo}/branches/{branch}", ctx.base_url);
    let auth = format!("Bearer {token}");
    let resp = client
        .request(HttpMethod::Get, &url, &github_headers(&auth), None)
        .await
        .context("Failed to read remote branch head")?;

    match resp.status {
        200 => {
            let branch: BranchResponse = resp
                .json()
                .context("Failed to parse remote branch response")?;
            Ok(Some(branch.commit.sha))
        }
        404 => Ok(None),
        status => bail!("Unexpected status {status} reading branch '{branch}'"),
    }
}

/// Check whether a GitHub App is installed for a specific repository.
///
/// Uses the App JWT to query `GET /repos/{owner}/{repo}/installation`.
/// Returns `Ok(true)` on 200, `Ok(false)` on 404.
pub async fn check_app_installed(
    client: &impl HttpClient,
    jwt: &str,
    owner: &str,
    repo: &str,
    base_url: &str,
) -> anyhow::Result<bool> {
    let lookup = lookup_installation(client, jwt, base_url, owner, repo)
        .await
        .context("Failed to check GitHub App installation")?;

    match lookup {
        InstallationLookup::Found(_) => Ok(true),
        InstallationLookup::NotFound => Ok(false),
        InstallationLookup::Failed(401) => bail!(
            "GitHub App authentication failed. \
             Check that app_id and GITHUB_APP_PRIVATE_KEY are correct."
        ),
        InstallationLookup::Failed(403) => bail!(
            "GitHub App installation is suspended. \
             Re-enable it in your organization's GitHub App settings."
        ),
        InstallationLookup::Failed(status) => {
            bail!("Unexpected status {status} checking GitHub App installation")
        }
    }
}

/// Fetch information about the authenticated GitHub App.
///
/// Uses the App JWT to call `GET /app` and returns the app's slug and owner.
pub async fn get_authenticated_app(
    client: &impl HttpClient,
    jwt: &str,
    base_url: &str,
) -> anyhow::Result<AppInfo> {
    let url = format!("{base_url}/app");
    let auth = format!("Bearer {jwt}");
    let resp = client
        .request(HttpMethod::Get, &url, &github_headers(&auth), None)
        .await
        .context("Failed to fetch GitHub App info")?;

    match resp.status {
        200 => {}
        401 => {
            bail!(
                "GitHub App authentication failed. \
                 Check that app_id and GITHUB_APP_PRIVATE_KEY are correct."
            );
        }
        status => {
            bail!("Unexpected status {status} fetching GitHub App info");
        }
    }

    resp.json::<AppInfo>()
        .context("Failed to parse GitHub App info")
}

/// Update a GitHub App's webhook URL via `PATCH /app/hook/config`.
///
/// Signs an App JWT and sets the webhook endpoint and content type.
pub async fn update_app_webhook_config(
    app_id: &str,
    private_key_pem: &str,
    webhook_url: &str,
) -> anyhow::Result<()> {
    let jwt = sign_app_jwt(app_id, private_key_pem)?;
    let client = http_client()?;
    let url = format!("{}/app/hook/config", github_api_base_url());
    let auth = format!("Bearer {jwt}");
    let body = serde_json::json!({
        "url": webhook_url,
        "content_type": "json",
    });

    let resp = HttpClient::request(
        &client,
        HttpMethod::Patch,
        &url,
        &github_headers(&auth),
        Some(&body),
    )
    .await
    .context("Failed to update GitHub App webhook")?;

    if !(200..300).contains(&resp.status) {
        bail!("GitHub API returned {}: {}", resp.status, resp.text());
    }

    Ok(())
}

/// Required credentials for an authenticated GitHub HTTPS clone.
///
/// The password is redacted from `Debug` output and should only be exposed at
/// the point where it is passed to Git.
#[derive(Clone, Debug)]
pub struct GitCloneCredentials {
    username: String,
    password: SecretString,
}

impl GitCloneCredentials {
    fn from_token(token: String) -> anyhow::Result<Self> {
        if token.is_empty() {
            bail!("GitHub clone credential token is empty");
        }
        Ok(Self {
            username: "x-access-token".to_string(),
            password: SecretString::new(token),
        })
    }

    /// The username passed to Git's HTTPS basic authentication.
    #[must_use]
    pub fn username(&self) -> &str {
        &self.username
    }

    /// The secret passed to Git's HTTPS basic authentication.
    ///
    /// Callers must not log or persist the returned value.
    #[must_use]
    pub fn password(&self) -> &str {
        self.password.expose()
    }
}

/// Resolve Git clone credentials for a GitHub repository.
///
/// Always generates credentials regardless of repository visibility because
/// the token is needed for pushing from the sandbox.
///
/// # Errors
///
/// Returns an error when an installation token is expired, a GitHub App token
/// cannot be minted, the HTTP client cannot be created, or the resolved token
/// is empty.
pub async fn resolve_clone_credentials(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
) -> anyhow::Result<GitCloneCredentials> {
    let token = match ctx.creds {
        GitHubCredentials::Pat(token) => token.clone(),
        GitHubCredentials::Installation(token) => token.valid_token()?.to_string(),
        GitHubCredentials::App(_) => {
            let client = ctx.http_client()?;
            mint_git_token(
                &client,
                ctx,
                owner,
                repo,
                serde_json::json!({ "contents": "write" }),
            )
            .await?
        }
    };
    GitCloneCredentials::from_token(token)
}

/// Resolve credentials for fetching repository contents without granting a
/// GitHub App token permission to push.
///
/// Static PATs and pre-minted installation tokens retain their configured
/// permissions. App credentials mint a repository-scoped token with
/// `contents: read`.
///
/// # Errors
///
/// Returns an error when an installation token is expired, a GitHub App token
/// cannot be minted, the HTTP client cannot be created, or the resolved token
/// is empty.
pub async fn resolve_read_only_clone_credentials(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
) -> anyhow::Result<GitCloneCredentials> {
    let token = match ctx.creds {
        GitHubCredentials::Pat(token) => token.clone(),
        GitHubCredentials::Installation(token) => token.valid_token()?.to_string(),
        GitHubCredentials::App(_) => {
            let client = ctx.http_client()?;
            mint_git_token(
                &client,
                ctx,
                owner,
                repo,
                serde_json::json!({ "contents": "read" }),
            )
            .await?
        }
    };
    GitCloneCredentials::from_token(token)
}

async fn mint_git_token(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    permissions: serde_json::Value,
) -> anyhow::Result<String> {
    ctx.creds
        .resolve_bearer_token(client, owner, repo, ctx.base_url, permissions)
        .await
}

/// Embed a token into an HTTPS URL for authenticated git operations.
///
/// Converts `https://github.com/owner/repo` to
/// `https://x-access-token:<token>@github.com/owner/repo`.
pub fn embed_token_in_url(url: &str, token: &str) -> anyhow::Result<DisplaySafeUrl> {
    let mut url = DisplaySafeUrl::parse(url).context("Failed to parse GitHub HTTPS URL")?;
    if url.scheme() != "https" {
        bail!("GitHub clone URL must use HTTPS: {}", url.redacted_string());
    }
    url.set_username("x-access-token")
        .map_err(|()| anyhow!("Failed to set GitHub token username"))?;
    url.set_password(Some(token))
        .map_err(|()| anyhow!("Failed to set GitHub token password"))?;
    Ok(url)
}

/// Resolve an authenticated HTTPS URL for a GitHub repository.
///
/// Parses owner/repo from the URL, obtains a fresh installation access token,
/// and returns the URL with embedded credentials.
pub async fn resolve_authenticated_url(
    ctx: &GitHubContext<'_>,
    url: &str,
) -> anyhow::Result<DisplaySafeUrl> {
    let (owner, repo) = parse_github_owner_repo(url)?;
    let credentials = resolve_clone_credentials(ctx, &owner, &repo).await?;
    embed_token_in_url(url, credentials.password())
}

/// Fetch detailed information about a pull request.
pub async fn get_pull_request(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<PullRequestGithubDetail, PullRequestApiError> {
    let client = ctx.http_client()?;
    get_pull_request_with_client(&client, ctx, owner, repo, number).await
}

pub async fn get_pull_request_with_client(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<PullRequestGithubDetail, PullRequestApiError> {
    tracing::debug!(owner, repo, number, "Fetching pull request");

    let token = ctx
        .creds
        .resolve_bearer_token(
            client,
            owner,
            repo,
            ctx.base_url,
            serde_json::json!({ "contents": "write", "pull_requests": "write" }),
        )
        .await?;

    let url = format!("{}/repos/{owner}/{repo}/pulls/{number}", ctx.base_url);
    let auth = format!("Bearer {token}");
    let resp = client
        .request(HttpMethod::Get, &url, &github_headers(&auth), None)
        .await
        .context("Failed to fetch pull request")?;

    match resp.status {
        200 => {}
        404 => {
            return Err(PullRequestApiError::NotFound {
                owner: owner.to_string(),
                repo: repo.to_string(),
                number,
            });
        }
        401 | 403 => {
            return Err(anyhow!(
                "Authentication failed fetching pull request ({})",
                resp.status
            )
            .into());
        }
        status => {
            return Err(anyhow!(
                "Unexpected status {status} fetching pull request: {}",
                resp.text()
            )
            .into());
        }
    }

    Ok(resp
        .json::<PullRequestGithubDetail>()
        .context("Failed to parse pull request response")?)
}

/// Merge a pull request.
pub async fn merge_pull_request(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    number: u64,
    method: MergeStrategy,
) -> Result<(), PullRequestApiError> {
    let client = ctx.http_client()?;
    merge_pull_request_with_client(&client, ctx, owner, repo, number, method).await
}

pub async fn merge_pull_request_with_client(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    number: u64,
    method: MergeStrategy,
) -> Result<(), PullRequestApiError> {
    tracing::debug!(owner, repo, number, method = %method, "Merging pull request");

    let token = ctx
        .creds
        .resolve_bearer_token(
            client,
            owner,
            repo,
            ctx.base_url,
            serde_json::json!({ "contents": "write", "pull_requests": "write" }),
        )
        .await?;

    let url = format!("{}/repos/{owner}/{repo}/pulls/{number}/merge", ctx.base_url);
    let body = serde_json::json!({ "merge_method": method });
    let auth = format!("Bearer {token}");

    let resp = client
        .request(HttpMethod::Put, &url, &github_headers(&auth), Some(&body))
        .await
        .context("Failed to merge pull request")?;

    match resp.status {
        200 => Ok(()),
        405 => Err(
            anyhow!("Pull request #{number} is not mergeable (method may not be allowed)").into(),
        ),
        409 => Err(anyhow!("Pull request #{number} has a merge conflict").into()),
        404 => Err(PullRequestApiError::NotFound {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }),
        401 | 403 => Err(anyhow!(
            "Authentication failed merging pull request ({})",
            resp.status
        )
        .into()),
        status => Err(anyhow!(
            "Unexpected status {status} merging pull request: {}",
            resp.text()
        )
        .into()),
    }
}

/// Close a pull request.
pub async fn close_pull_request(
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<(), PullRequestApiError> {
    let client = ctx.http_client()?;
    close_pull_request_with_client(&client, ctx, owner, repo, number).await
}

pub async fn close_pull_request_with_client(
    client: &impl HttpClient,
    ctx: &GitHubContext<'_>,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<(), PullRequestApiError> {
    tracing::debug!(owner, repo, number, "Closing pull request");

    let token = ctx
        .creds
        .resolve_bearer_token(
            client,
            owner,
            repo,
            ctx.base_url,
            serde_json::json!({ "contents": "write", "pull_requests": "write" }),
        )
        .await?;

    let url = format!("{}/repos/{owner}/{repo}/pulls/{number}", ctx.base_url);
    let body = serde_json::json!({ "state": "closed" });
    let auth = format!("Bearer {token}");

    let resp = client
        .request(HttpMethod::Patch, &url, &github_headers(&auth), Some(&body))
        .await
        .context("Failed to close pull request")?;

    match resp.status {
        200 => Ok(()),
        404 => Err(PullRequestApiError::NotFound {
            owner: owner.to_string(),
            repo: repo.to_string(),
            number,
        }),
        401 | 403 => Err(anyhow!(
            "Authentication failed closing pull request ({})",
            resp.status
        )
        .into()),
        status => Err(anyhow!(
            "Unexpected status {status} closing pull request: {}",
            resp.text()
        )
        .into()),
    }
}

/// Request a scoped Installation Access Token with `issues: write`
/// and `organization_projects: write`. Used for GitHub Projects V2.
pub async fn create_installation_access_token_for_projects(
    client: &impl HttpClient,
    jwt: &str,
    owner: &str,
    repo: &str,
    base_url: &str,
) -> anyhow::Result<String> {
    create_installation_access_token_with_permissions(
        client,
        jwt,
        owner,
        repo,
        base_url,
        serde_json::json!({ "issues": "write", "organization_projects": "write" }),
    )
    .await
}

#[cfg(test)]
mod tests {
    use std::io;
    use std::sync::{Arc, Mutex};

    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use tracing::{debug, subscriber};
    use tracing_subscriber::fmt::{self as tracing_fmt, MakeWriter};
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::registry;

    use super::*;

    #[test]
    fn decode_pem_env_accepts_raw_pem() {
        let pem = "-----BEGIN TEST KEY-----\nabc\n-----END TEST KEY-----";
        assert_eq!(decode_pem_env("GITHUB_APP_PRIVATE_KEY", pem).unwrap(), pem);
    }

    #[test]
    fn pull_request_api_error_preserves_other_source_chain() {
        let original = anyhow::Error::new(std::io::Error::other("leaf failure"))
            .context("middle context")
            .context("outer context");
        let original_chain = original
            .chain()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        let err = PullRequestApiError::from(original);
        let wrapped = anyhow::Error::new(err);
        let wrapped_chain = wrapped.chain().map(ToString::to_string).collect::<Vec<_>>();

        assert_eq!(wrapped_chain.len(), original_chain.len());
        for original_cause in original_chain {
            assert!(
                wrapped_chain.iter().any(|cause| cause == &original_cause),
                "missing original cause {original_cause:?} in {wrapped_chain:#?}"
            );
        }
    }

    #[test]
    fn decode_pem_env_accepts_base64_pem() {
        let pem = "-----BEGIN TEST KEY-----\nabc\n-----END TEST KEY-----";
        let encoded = STANDARD.encode(pem);
        assert_eq!(
            decode_pem_env("GITHUB_APP_PRIVATE_KEY", &encoded).unwrap(),
            pem
        );
    }

    #[test]
    fn decode_pem_env_rejects_invalid_base64() {
        let err = decode_pem_env("GITHUB_APP_PRIVATE_KEY", "%%%not-base64%%%").unwrap_err();
        assert!(err.contains("GITHUB_APP_PRIVATE_KEY is not valid PEM or base64"));
    }

    // -----------------------------------------------------------------------
    // parse_github_owner_repo
    // -----------------------------------------------------------------------

    #[test]
    fn parse_https_with_git_suffix() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/owner/repo.git").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_https_without_git_suffix() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/owner/repo").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_https_with_trailing_slash() {
        let (owner, repo) = parse_github_owner_repo("https://github.com/owner/repo/").unwrap();
        assert_eq!(owner, "owner");
        assert_eq!(repo, "repo");
    }

    #[test]
    fn parse_rejects_extra_path_components() {
        let error = parse_github_owner_repo("https://github.com/owner/repo/issues").unwrap_err();

        assert!(
            error.to_string().contains("must identify one repository"),
            "got: {error}"
        );
    }

    // -----------------------------------------------------------------------
    // ssh_url_to_https
    // -----------------------------------------------------------------------

    #[test]
    fn ssh_url_to_https_converts_git_at_syntax() {
        assert_eq!(
            ssh_url_to_https("git@github.com:brynary/arc.git"),
            "https://github.com/brynary/arc.git"
        );
    }

    #[test]
    fn ssh_url_to_https_converts_ssh_protocol() {
        assert_eq!(
            ssh_url_to_https("ssh://git@github.com/brynary/arc.git"),
            "https://github.com/brynary/arc.git"
        );
    }

    #[test]
    fn ssh_url_to_https_passes_through_https() {
        assert_eq!(
            ssh_url_to_https("https://github.com/brynary/arc.git"),
            "https://github.com/brynary/arc.git"
        );
    }

    #[test]
    fn normalize_repo_origin_url_converts_ssh_and_trims_git_suffix() {
        assert_eq!(
            normalize_repo_origin_url("git@github.com:brynary/arc.git"),
            "https://github.com/brynary/arc"
        );
    }

    #[test]
    fn normalize_repo_origin_url_strips_credentials_and_trailing_slash() {
        assert_eq!(
            normalize_repo_origin_url("https://token@github.com/acme/widgets.git/"),
            "https://github.com/acme/widgets"
        );
    }

    #[test]
    fn normalize_repo_origin_url_handles_sanitized_git_at_shape() {
        assert_eq!(
            normalize_repo_origin_url("https://***@github.com:acme/widgets.git"),
            "https://github.com/acme/widgets"
        );
    }

    #[test]
    fn parse_github_url_with_credentials() {
        let (owner, repo) = parse_github_owner_repo(
            "https://x-access-token:ghs_abc123@github.com/acme/widgets.git",
        )
        .unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_github_url_with_credentials_no_password() {
        let (owner, repo) =
            parse_github_owner_repo("https://token@github.com/acme/widgets.git").unwrap();
        assert_eq!(owner, "acme");
        assert_eq!(repo, "widgets");
    }

    #[test]
    fn parse_credentials_non_github_still_errors() {
        let result = parse_github_owner_repo("https://user:pass@gitlab.com/owner/repo");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Not a GitHub HTTPS URL")
        );
    }

    #[test]
    fn embed_token_in_url_redacts_display_and_keeps_raw_access() {
        let url = embed_token_in_url("https://github.com/acme/widgets.git", "ghs_abc123").unwrap();

        assert_eq!(
            url.redacted_string(),
            "https://x-access-token:****@github.com/acme/widgets.git"
        );
        assert_eq!(
            url.raw_string(),
            "https://x-access-token:ghs_abc123@github.com/acme/widgets.git"
        );
    }

    #[test]
    fn logging_embedded_token_url_does_not_emit_token() {
        let output = CapturedTrace::default();
        let subscriber = registry().with(
            tracing_fmt::layer()
                .with_writer(output.clone())
                .without_time()
                .with_target(false),
        );

        subscriber::with_default(subscriber, || {
            let url =
                embed_token_in_url("https://github.com/acme/widgets.git", "ghs_abc123").unwrap();
            debug!(?url, %url, "resolved authenticated GitHub URL");
        });

        let formatted = output.captured_output();
        assert!(formatted.contains("x-access-token:****@"));
        assert!(!formatted.contains("ghs_abc123"));
    }

    #[derive(Clone, Default)]
    struct CapturedTrace {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl CapturedTrace {
        fn captured_output(&self) -> String {
            let buffer = self.buffer.lock().unwrap();
            String::from_utf8(buffer.clone()).unwrap()
        }
    }

    impl<'writer> MakeWriter<'writer> for CapturedTrace {
        type Writer = CapturedTraceWriter;

        fn make_writer(&'writer self) -> Self::Writer {
            CapturedTraceWriter {
                buffer: Arc::clone(&self.buffer),
            }
        }
    }

    struct CapturedTraceWriter {
        buffer: Arc<Mutex<Vec<u8>>>,
    }

    impl io::Write for CapturedTraceWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.buffer.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn parse_non_github_url_errors() {
        let result = parse_github_owner_repo("https://gitlab.com/owner/repo");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Not a GitHub HTTPS URL")
        );
    }

    #[test]
    fn parse_missing_repo_errors() {
        let result = parse_github_owner_repo("https://github.com/owner");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("Missing repo"));
    }

    #[test]
    fn parse_empty_string_errors() {
        let result = parse_github_owner_repo("");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // sign_app_jwt
    // -----------------------------------------------------------------------

    fn test_rsa_key() -> &'static str {
        tests_mock::test_rsa_key()
    }

    #[test]
    fn jwt_is_three_part_string() {
        let pem = test_rsa_key();
        let jwt = sign_app_jwt("12345", pem).unwrap();
        assert_eq!(jwt.split('.').count(), 3);
    }

    #[test]
    fn jwt_has_rs256_header() {
        let pem = test_rsa_key();
        let jwt = sign_app_jwt("12345", pem).unwrap();
        let header_b64 = jwt.split('.').next().unwrap();
        let header_json = URL_SAFE_NO_PAD.decode(header_b64).unwrap();
        let header: serde_json::Value = serde_json::from_slice(&header_json).unwrap();
        assert_eq!(header["alg"], "RS256");
    }

    #[test]
    fn jwt_has_correct_claims() {
        let pem = test_rsa_key();
        let jwt = sign_app_jwt("99999", pem).unwrap();
        let payload_b64 = jwt.split('.').nth(1).unwrap();
        let payload_json = URL_SAFE_NO_PAD.decode(payload_b64).unwrap();
        let claims: serde_json::Value = serde_json::from_slice(&payload_json).unwrap();
        assert_eq!(claims["iss"], "99999");

        let now = chrono::Utc::now().timestamp();
        let iat = claims["iat"].as_i64().unwrap();
        let exp = claims["exp"].as_i64().unwrap();
        // iat should be ~60s before now
        assert!((now - 60 - iat).abs() < 5);
        // exp should be ~10min after now
        assert!((now + 600 - exp).abs() < 5);
    }

    #[test]
    fn jwt_invalid_pem_errors() {
        let result = sign_app_jwt("12345", "not-a-pem");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("Invalid RSA private key")
        );
    }

    // -----------------------------------------------------------------------
    // MockHttpClient
    // -----------------------------------------------------------------------

    use crate::tests_mock::{self, MockHttpClient};

    fn search_issue(
        owner: &str,
        repo: &str,
        number: u64,
        state: &str,
        draft: bool,
        merged_at: Option<&str>,
    ) -> serde_json::Value {
        serde_json::json!({
            "repository_url": format!("https://api.github.test/repos/{owner}/{repo}"),
            "number": number,
            "title": format!("Pull request {number}"),
            "html_url": format!("https://github.test/{owner}/{repo}/pull/{number}"),
            "state": state,
            "draft": draft,
            "updated_at": format!("2026-01-{:02}T00:00:00Z", number % 28 + 1),
            "pull_request": { "merged_at": merged_at },
        })
    }

    #[tokio::test]
    async fn authored_search_uses_author_and_keeps_drafts_while_excluding_closed_and_merged() {
        let body = serde_json::json!({
            "total_count": 4,
            "items": [
                search_issue("acme", "one", 1, "open", false, None),
                search_issue("acme", "two", 2, "open", true, None),
                search_issue("acme", "three", 3, "closed", false, None),
                search_issue("acme", "four", 4, "open", false, Some("2026-01-04T00:00:00Z")),
            ],
        })
        .to_string();
        let mock = MockHttpClient::new().on(
            HttpMethod::Get,
            "/search/issues?q=is%3Apr+is%3Aopen+author%3Aoctocat&per_page=100&page=1",
            200,
            &body,
        );

        let result = search_open_authored_pull_requests_with_client(
            &mock,
            "secret",
            "",
            "octocat".to_string(),
        )
        .await
        .expect("authored search should succeed");

        assert_eq!(result.author, "octocat");
        assert_eq!(result.items.len(), 2);
        assert!(result.items.iter().any(|item| item.draft));
        assert!(!result.partial);
    }

    #[tokio::test]
    async fn authored_search_marks_incomplete_first_page_as_partial() {
        let body = serde_json::json!({
            "total_count": 1,
            "incomplete_results": true,
            "items": [
                search_issue("acme", "one", 1, "open", false, None),
            ],
        })
        .to_string();
        let mock = MockHttpClient::new().on(
            HttpMethod::Get,
            "/search/issues?q=is%3Apr+is%3Aopen+author%3Aoctocat&per_page=100&page=1",
            200,
            &body,
        );

        let result = search_open_authored_pull_requests_with_client(
            &mock,
            "secret",
            "",
            "octocat".to_string(),
        )
        .await
        .expect("incomplete search response should preserve returned items");

        assert_eq!(result.items.len(), 1);
        assert!(result.partial);
        assert_eq!(
            result.warning.as_deref(),
            Some(
                "GitHub reported incomplete search results; some open pull requests may be missing."
            )
        );
    }

    #[tokio::test]
    async fn authored_search_paginates_and_deduplicates_pull_requests() {
        let first_items = (1..=100)
            .map(|number| search_issue("acme", "repo", number, "open", false, None))
            .collect::<Vec<_>>();
        let first = serde_json::json!({
            "total_count": 102,
            "items": first_items,
        })
        .to_string();
        let second = serde_json::json!({
            "total_count": 102,
            "items": [
                search_issue("acme", "repo", 100, "open", false, None),
                search_issue("acme", "repo", 101, "open", true, None),
            ],
        })
        .to_string();
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/search/issues?q=is%3Apr+is%3Aopen+author%3Aoctocat&per_page=100&page=1",
                200,
                &first,
            )
            .on(
                HttpMethod::Get,
                "/search/issues?q=is%3Apr+is%3Aopen+author%3Aoctocat&per_page=100&page=2",
                200,
                &second,
            );

        let result = search_open_authored_pull_requests_with_client(
            &mock,
            "secret",
            "",
            "octocat".to_string(),
        )
        .await
        .expect("paginated search should succeed");

        assert_eq!(result.items.len(), 101);
        assert_eq!(mock.request_count(), 2);
    }

    #[tokio::test]
    async fn authored_search_returns_partial_items_when_a_later_page_fails() {
        let first_items = (1..=100)
            .map(|number| search_issue("acme", "repo", number, "open", false, None))
            .collect::<Vec<_>>();
        let first = serde_json::json!({
            "total_count": 101,
            "items": first_items,
        })
        .to_string();
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/search/issues?q=is%3Apr+is%3Aopen+author%3Aoctocat&per_page=100&page=1",
                200,
                &first,
            )
            .on(
                HttpMethod::Get,
                "/search/issues?q=is%3Apr+is%3Aopen+author%3Aoctocat&per_page=100&page=2",
                502,
                "",
            );

        let result = search_open_authored_pull_requests_with_client(
            &mock,
            "secret",
            "",
            "octocat".to_string(),
        )
        .await
        .expect("later-page failures should preserve earlier results");

        assert_eq!(result.items.len(), 100);
        assert!(result.partial);
        assert!(result.warning.is_some());
    }

    // -----------------------------------------------------------------------
    // create_installation_access_token — success
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn app_credentials_mint_installation_token_preserves_expiry() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 123}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/123/access_tokens",
                201,
                r#"{"token": "ghs_xxx", "expires_at": "2026-01-01T12:00:00Z"}"#,
            )
            .with_req_body(r#"{"permissions":{"contents":"write"},"repositories":["repo"]}"#);

        let creds = GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: test_rsa_key().to_string(),
            slug:            None,
        };

        let token = creds
            .mint_installation_token(
                &mock,
                "owner",
                "repo",
                "",
                serde_json::json!({ "contents": "write" }),
                None,
            )
            .await
            .unwrap();

        assert_eq!(token.token, "ghs_xxx");
        assert_eq!(
            token.expires_at,
            "2026-01-01T12:00:00Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap()
        );
    }

    /// The multi-repository mint sends one request listing every projected
    /// repository name exactly once, primary first, with the shared
    /// permissions; the installation lookup uses the primary repository.
    #[tokio::test]
    async fn multi_repository_mint_lists_every_repository_name_once() {
        let access = GitHubRepositoryAccess::new(
            Some("git@github.com:owner/repo.git"),
            &[
                "owner/keystone".parse().unwrap(),
                "owner/arc".parse().unwrap(),
            ]
            .into_iter()
            .collect(),
            std::collections::HashMap::from([("contents".to_string(), "read".to_string())]),
        )
        .unwrap()
        .expect("origin should produce an access value");

        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 123}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/123/access_tokens",
                201,
                r#"{"token": "ghs_multi", "expires_at": "2026-01-01T12:00:00Z"}"#,
            )
            .with_req_body(
                r#"{"permissions":{"contents":"read"},"repositories":["repo","arc","keystone"]}"#,
            );

        let creds = GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: test_rsa_key().to_string(),
            slug:            None,
        };

        let token = creds
            .mint_installation_token_for_repositories(
                &mock,
                access.owner(),
                &access.repository_names(),
                "",
                access.permissions_json().unwrap(),
                None,
            )
            .await
            .unwrap();

        assert_eq!(token.token, "ghs_multi");
    }

    #[tokio::test]
    async fn create_iat_requests_only_contents_write() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 123}"#,
            )
            .with_req_header("Authorization", "Bearer test-jwt")
            .on(
                HttpMethod::Post,
                "/app/installations/123/access_tokens",
                201,
                r#"{"token": "ghs_xxx", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .with_req_header("Authorization", "Bearer test-jwt")
            .with_req_body(r#"{"permissions":{"contents":"write"},"repositories":["repo"]}"#);

        let token = create_installation_access_token(&mock, "test-jwt", "owner", "repo", "")
            .await
            .unwrap();
        assert_eq!(token, "ghs_xxx");
    }

    // -----------------------------------------------------------------------
    // create_installation_access_token — failure modes
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_iat_not_installed() {
        let mock =
            MockHttpClient::new().on(HttpMethod::Get, "/repos/owner/repo/installation", 404, "");

        let err = create_installation_access_token(&mock, "jwt", "owner", "repo", "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("not installed"), "got: {err}");
        assert!(err.contains("owner"), "got: {err}");
    }

    #[tokio::test]
    async fn create_iat_not_installed_uses_app_specific_install_url() {
        let mock =
            MockHttpClient::new().on(HttpMethod::Get, "/repos/owner/repo/installation", 404, "");
        let install_url =
            "https://github.com/organizations/owner/settings/apps/fabro-test/installations";

        let err = create_installation_access_token_with_permissions_and_install_url(
            &mock,
            "jwt",
            "owner",
            "repo",
            "",
            serde_json::json!({ "contents": "write" }),
            Some(install_url),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(err.contains("not installed"), "got: {err}");
        assert!(err.contains(install_url), "got: {err}");
        assert!(
            !err.contains("https://github.com/organizations/owner/settings/installations"),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn create_iat_suspended() {
        let mock =
            MockHttpClient::new().on(HttpMethod::Get, "/repos/owner/repo/installation", 403, "");

        let err = create_installation_access_token(&mock, "jwt", "owner", "repo", "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("suspended"), "got: {err}");
    }

    #[tokio::test]
    async fn create_iat_no_repo_access() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 123}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/123/access_tokens",
                422,
                "",
            );

        let err = create_installation_access_token(&mock, "jwt", "owner", "repo", "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not have access"), "got: {err}");
        assert!(err.contains("repo"), "got: {err}");
    }

    #[tokio::test]
    async fn find_open_pull_request_matches_the_expected_head_commit() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/pulls?state=open&base=main&head=owner%3Afabro%2Frun%2F1",
                200,
                r#"[
                    {
                        "html_url": "https://github.com/owner/repo/pull/40",
                        "number": 40,
                        "node_id": "PR_wrong",
                        "title": "Old head",
                        "head": {"sha": "old-sha"}
                    },
                    {
                        "html_url": "https://github.com/owner/repo/pull/42",
                        "number": 42,
                        "node_id": "PR_expected",
                        "title": "Expected head",
                        "head": {"sha": "final-sha"}
                    }
                ]"#,
            )
            .with_req_header("Authorization", "Bearer ghu_test");
        let creds = GitHubCredentials::Pat("ghu_test".to_string());
        let ctx = GitHubContext::new(&creds, "https://api.test");

        let found = find_open_pull_request_with_client(
            &mock,
            &ctx,
            "owner",
            "repo",
            "main",
            "fabro/run/1",
            "final-sha",
        )
        .await
        .unwrap()
        .expect("matching pull request should be found");

        assert_eq!(found.number, 42);
        assert_eq!(found.node_id, "PR_expected");
        assert_eq!(found.title, "Expected head");
    }

    #[tokio::test]
    async fn create_iat_auth_failed() {
        let mock =
            MockHttpClient::new().on(HttpMethod::Get, "/repos/owner/repo/installation", 401, "");

        let err = create_installation_access_token(&mock, "jwt", "owner", "repo", "")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("authentication failed"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // create_installation_access_token_for_pr
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn create_iat_for_pr_requests_pr_permissions() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 456}"#,
            )
            .with_req_header("Authorization", "Bearer test-jwt")
            .on(
                HttpMethod::Post,
                "/app/installations/456/access_tokens",
                201,
                r#"{"token": "ghs_pr_token", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .with_req_header("Authorization", "Bearer test-jwt")
            .with_req_body(
                r#"{"permissions":{"contents":"write","pull_requests":"write"},"repositories":["repo"]}"#,
            );

        let token = create_installation_access_token_for_pr(&mock, "test-jwt", "owner", "repo", "")
            .await
            .unwrap();
        assert_eq!(token, "ghs_pr_token");
    }

    // -----------------------------------------------------------------------
    // branch_head_sha
    // -----------------------------------------------------------------------

    fn app_creds() -> GitHubCredentials {
        GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: test_rsa_key().to_string(),
            slug:            None,
        })
    }

    /// Mock the installation-token exchange every App-credentialed call makes
    /// before it reaches the endpoint under test.
    fn mock_with_installation_token() -> MockHttpClient {
        MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
    }

    #[tokio::test]
    async fn branch_head_sha_returns_commit_on_200() {
        let mock = mock_with_installation_token().on(
            HttpMethod::Get,
            "/repos/owner/repo/branches/my-branch",
            200,
            r#"{"name": "my-branch", "commit": {"sha": "abc123"}}"#,
        );

        let creds = app_creds();
        let result = branch_head_sha_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            "my-branch",
        )
        .await;

        assert_eq!(result.unwrap(), Some("abc123".to_string()));
    }

    #[tokio::test]
    async fn branch_head_sha_returns_none_on_404() {
        let mock = mock_with_installation_token().on(
            HttpMethod::Get,
            "/repos/owner/repo/branches/no-such-branch",
            404,
            "",
        );

        let creds = app_creds();
        let result = branch_head_sha_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            "no-such-branch",
        )
        .await;

        assert_eq!(result.unwrap(), None);
    }

    #[tokio::test]
    async fn branch_head_sha_returns_error_on_500() {
        let mock = mock_with_installation_token().on(
            HttpMethod::Get,
            "/repos/owner/repo/branches/broken",
            500,
            "",
        );

        let creds = app_creds();
        let result = branch_head_sha_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            "broken",
        )
        .await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn branch_head_sha_with_token_uses_direct_bearer_token() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/branches/my-branch",
                200,
                r#"{"name": "my-branch", "commit": {"sha": "abc123"}}"#,
            )
            .with_req_header("Authorization", "Bearer ghu_test");

        let creds = GitHubCredentials::Pat("ghu_test".to_string());
        let result = branch_head_sha_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            "my-branch",
        )
        .await;

        assert_eq!(result.unwrap(), Some("abc123".to_string()));
    }

    // -----------------------------------------------------------------------
    // check_app_installed
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn check_app_installed_returns_true_on_200() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .with_req_header("Authorization", "Bearer test-jwt");

        let result = check_app_installed(&mock, "test-jwt", "owner", "repo", "").await;
        assert!(result.unwrap());
    }

    #[tokio::test]
    async fn check_app_installed_returns_false_on_404() {
        let mock =
            MockHttpClient::new().on(HttpMethod::Get, "/repos/owner/repo/installation", 404, "");

        let result = check_app_installed(&mock, "test-jwt", "owner", "repo", "").await;
        assert!(!result.unwrap());
    }

    #[tokio::test]
    async fn check_app_installed_returns_error_on_401() {
        let mock =
            MockHttpClient::new().on(HttpMethod::Get, "/repos/owner/repo/installation", 401, "");

        let result = check_app_installed(&mock, "test-jwt", "owner", "repo", "").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("authentication failed"),
            "expected auth error"
        );
    }

    // -----------------------------------------------------------------------
    // get_authenticated_app
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn get_authenticated_app_success() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/app",
                200,
                r#"{"slug": "my-fabro-app", "owner": {"login": "my-org"}}"#,
            )
            .with_req_header("Authorization", "Bearer test-jwt");

        let info = get_authenticated_app(&mock, "test-jwt", "").await.unwrap();
        assert_eq!(info.slug, "my-fabro-app");
        assert_eq!(info.owner.login, "my-org");
    }

    #[tokio::test]
    async fn get_authenticated_app_auth_failure() {
        let mock = MockHttpClient::new().on(HttpMethod::Get, "/app", 401, "");

        let result = get_authenticated_app(&mock, "bad-jwt", "").await;
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("authentication failed"),
            "expected auth error"
        );
    }

    // -----------------------------------------------------------------------
    // get_pull_request
    // -----------------------------------------------------------------------

    fn mock_pr_json() -> &'static str {
        r#"{
            "number": 42,
            "title": "Fix the bug",
            "body": "Detailed description",
            "state": "open",
            "draft": false,
            "merged": false,
            "merged_at": null,
            "mergeable": true,
            "additions": 10,
            "deletions": 3,
            "changed_files": 2,
            "html_url": "https://github.com/owner/repo/pull/42",
            "user": {"login": "testuser"},
            "head": {"ref": "feature-branch"},
            "base": {"ref": "main"},
            "created_at": "2026-01-01T12:00:00Z",
            "updated_at": "2026-01-02T12:00:00Z"
        }"#
    }

    #[tokio::test]
    async fn get_pr_success() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/pulls/42",
                200,
                mock_pr_json(),
            );

        let pem = test_rsa_key();
        let creds = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: pem.to_string(),
            slug:            None,
        });
        let detail = get_pull_request_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            42,
        )
        .await
        .unwrap();

        assert_eq!(detail.number, 42);
        assert_eq!(detail.title, "Fix the bug");
        assert_eq!(detail.state, "open");
        assert!(!detail.merged);
        assert_eq!(detail.merged_at, None);
        assert_eq!(detail.additions, 10);
        assert_eq!(detail.deletions, 3);
        assert_eq!(detail.changed_files, 2);
        assert_eq!(detail.user.login, "testuser");
        assert_eq!(detail.head.ref_name, "feature-branch");
        assert_eq!(detail.base.ref_name, "main");
    }

    #[tokio::test]
    async fn get_pr_not_found() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .on(HttpMethod::Get, "/repos/owner/repo/pulls/999", 404, "");

        let pem = test_rsa_key();
        let creds = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: pem.to_string(),
            slug:            None,
        });
        let err = get_pull_request_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            999,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                PullRequestApiError::NotFound {
                    number: 999,
                    ref owner,
                    ref repo,
                } if owner == "owner" && repo == "repo"
            ),
            "got: {err}"
        );
    }

    #[tokio::test]
    async fn get_pr_with_token_uses_direct_bearer_token() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/pulls/42",
                200,
                mock_pr_json(),
            )
            .with_req_header("Authorization", "Bearer ghu_test");

        let creds = GitHubCredentials::Pat("ghu_test".to_string());
        let detail = get_pull_request_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            42,
        )
        .await
        .unwrap();

        assert_eq!(detail.number, 42);
    }

    #[tokio::test]
    async fn resolve_clone_credentials_returns_token_for_token_credentials() {
        let creds = GitHubCredentials::Pat("ghu_test".to_string());

        let credentials =
            resolve_clone_credentials(&GitHubContext::new(&creds, ""), "owner", "repo")
                .await
                .unwrap();

        assert_eq!(credentials.username(), "x-access-token");
        assert_eq!(credentials.password(), "ghu_test");
        assert!(!format!("{credentials:?}").contains("ghu_test"));
    }

    #[tokio::test]
    async fn resolve_read_only_clone_credentials_returns_static_token_unchanged() {
        let creds = GitHubCredentials::Pat("ghu_test".to_string());

        let credentials =
            resolve_read_only_clone_credentials(&GitHubContext::new(&creds, ""), "owner", "repo")
                .await
                .unwrap();

        assert_eq!(credentials.username(), "x-access-token");
        assert_eq!(credentials.password(), "ghu_test");
    }

    #[tokio::test]
    async fn clone_token_requests_only_contents_write() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 123}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/123/access_tokens",
                201,
                r#"{"token": "ghs_xxx", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .with_req_body(r#"{"permissions":{"contents":"write"},"repositories":["repo"]}"#);
        let credentials = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: test_rsa_key().to_string(),
            slug:            None,
        });
        let context = GitHubContext::new(&credentials, "");
        let token = mint_git_token(
            &mock,
            &context,
            "owner",
            "repo",
            serde_json::json!({ "contents": "write" }),
        )
        .await
        .unwrap();

        assert_eq!(token, "ghs_xxx");
    }

    #[tokio::test]
    async fn read_only_clone_token_requests_only_contents_read() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 123}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/123/access_tokens",
                201,
                r#"{"token": "ghs_xxx", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .with_req_body(r#"{"permissions":{"contents":"read"},"repositories":["repo"]}"#);
        let credentials = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: test_rsa_key().to_string(),
            slug:            None,
        });
        let context = GitHubContext::new(&credentials, "");
        let token = mint_git_token(
            &mock,
            &context,
            "owner",
            "repo",
            serde_json::json!({ "contents": "read" }),
        )
        .await
        .unwrap();

        assert_eq!(token, "ghs_xxx");
    }

    #[test]
    fn installation_token_valid_token_rejects_expired_tokens() {
        let expired = InstallationToken {
            token:      "ghs_expired".to_string(),
            expires_at: chrono::Utc::now() - chrono::Duration::seconds(1),
        };
        assert!(expired.valid_token().is_err());

        let fresh = InstallationToken {
            token:      "ghs_fresh".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
        };
        assert_eq!(fresh.valid_token().unwrap(), "ghs_fresh");
        assert!(!fresh.near_expiry(std::time::Duration::from_mins(15)));
    }

    #[test]
    fn only_app_credentials_mint_installation_tokens() {
        let app = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: test_rsa_key().to_string(),
            slug:            None,
        });
        let pat = GitHubCredentials::Pat("ghp_personal".to_string());
        let installation = GitHubCredentials::Installation(InstallationToken {
            token:      "ghs_installation".to_string(),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(30),
        });

        assert!(app.mints_installation_token());
        assert!(!pat.mints_installation_token());
        assert!(!installation.mints_installation_token());
    }

    #[test]
    fn validate_static_github_token_rejects_installation_tokens() {
        validate_static_github_token("ghp_personal").unwrap();
        validate_static_github_token("gho_oauth").unwrap();
        validate_static_github_token("ghu_user").unwrap();

        let err = validate_static_github_token("ghs_installation")
            .unwrap_err()
            .to_string();
        assert!(err.contains("installation access token"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // merge_pull_request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn merge_pr_success() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .on(
                HttpMethod::Put,
                "/repos/owner/repo/pulls/42/merge",
                200,
                r#"{"merged": true}"#,
            );

        let pem = test_rsa_key();
        let creds = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: pem.to_string(),
            slug:            None,
        });
        merge_pull_request_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            42,
            MergeStrategy::Squash,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn merge_pr_not_mergeable() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .on(HttpMethod::Put, "/repos/owner/repo/pulls/42/merge", 405, "");

        let pem = test_rsa_key();
        let creds = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: pem.to_string(),
            slug:            None,
        });
        let err = merge_pull_request_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            42,
            MergeStrategy::Squash,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not mergeable"), "got: {err}");
    }

    #[tokio::test]
    async fn merge_pr_conflict() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .on(HttpMethod::Put, "/repos/owner/repo/pulls/42/merge", 409, "");

        let pem = test_rsa_key();
        let creds = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: pem.to_string(),
            slug:            None,
        });
        let err = merge_pull_request_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            42,
            MergeStrategy::Squash,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("merge conflict"), "got: {err}");
    }

    // -----------------------------------------------------------------------
    // close_pull_request
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn close_pr_success() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .on(
                HttpMethod::Patch,
                "/repos/owner/repo/pulls/42",
                200,
                mock_pr_json(),
            );

        let pem = test_rsa_key();
        let creds = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: pem.to_string(),
            slug:            None,
        });
        close_pull_request_with_client(&mock, &GitHubContext::new(&creds, ""), "owner", "repo", 42)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn close_pr_not_found() {
        let mock = MockHttpClient::new()
            .on(
                HttpMethod::Get,
                "/repos/owner/repo/installation",
                200,
                r#"{"id": 1}"#,
            )
            .on(
                HttpMethod::Post,
                "/app/installations/1/access_tokens",
                201,
                r#"{"token": "ghs_test", "expires_at": "2099-01-01T00:00:00Z"}"#,
            )
            .on(HttpMethod::Patch, "/repos/owner/repo/pulls/999", 404, "");

        let pem = test_rsa_key();
        let creds = GitHubCredentials::App(GitHubAppCredentials {
            app_id:          "test".to_string(),
            private_key_pem: pem.to_string(),
            slug:            None,
        });
        let err = close_pull_request_with_client(
            &mock,
            &GitHubContext::new(&creds, ""),
            "owner",
            "repo",
            999,
        )
        .await
        .unwrap_err();
        assert!(
            matches!(
                err,
                PullRequestApiError::NotFound {
                    number: 999,
                    ref owner,
                    ref repo,
                } if owner == "owner" && repo == "repo"
            ),
            "got: {err}"
        );
    }
}
