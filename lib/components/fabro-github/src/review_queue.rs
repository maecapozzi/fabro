//! Read-only discovery of pull requests awaiting one GitHub user's review.
//!
//! GitHub documents `review-requested:USERNAME` and
//! `team-review-requested:ORG/TEAM` as separate issue-search qualifiers. The
//! former must not be treated as including team requests. Candidates from
//! both searches are therefore combined and then checked against the current
//! `GET /repos/{owner}/{repo}/pulls/{number}/requested_reviewers` response.
//! See <https://docs.github.com/search-github/searching-on-github/searching-issues-and-pull-requests>
//! and <https://docs.github.com/rest/pulls/review-requests>.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
use serde::Deserialize;

use crate::{
    GitHubContext, GitHubCredentials, HttpClient, HttpMethod, github_headers, sign_app_jwt,
};

const PAGE_SIZE: usize = 100;
const SEARCH_RESULT_LIMIT: usize = 1_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewRequestKind {
    Direct,
    Team,
    DirectAndTeam,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TeamCoverage {
    Complete,
    Limited,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewQueueItem {
    pub owner:        String,
    pub repository:   String,
    pub number:       u64,
    pub title:        String,
    pub author:       String,
    pub html_url:     String,
    pub draft:        bool,
    pub request_kind: ReviewRequestKind,
    pub teams:        Vec<String>,
    pub updated_at:   DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewQueue {
    pub reviewer:      String,
    pub items:         Vec<ReviewQueueItem>,
    pub incomplete:    bool,
    pub team_coverage: TeamCoverage,
}

#[derive(Debug, thiserror::Error)]
pub enum ReviewQueueError {
    #[error("GitHub rejected the configured credentials")]
    Authentication,
    #[error("GitHub rate limited the review queue request")]
    RateLimited,
    #[error("GitHub denied access to review requests")]
    Access,
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Clone)]
struct AccessToken {
    token:            String,
    can_resolve_user: bool,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchUser {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SearchItem {
    repository_url: String,
    number:         u64,
}

#[derive(Deserialize)]
struct SearchResponse {
    total_count:        usize,
    incomplete_results: bool,
    items:              Vec<SearchItem>,
}

#[derive(Debug, Clone, Deserialize)]
struct TeamOrganization {
    login: String,
}

#[derive(Debug, Clone, Deserialize)]
struct Team {
    slug:         String,
    organization: TeamOrganization,
}

impl Team {
    fn full_name(&self) -> String {
        format!("{}/{}", self.organization.login, self.slug)
    }
}

#[derive(Deserialize)]
struct RequestedUser {
    login: String,
}

#[derive(Deserialize)]
struct RequestedTeam {
    slug:         String,
    organization: Option<TeamOrganization>,
}

#[derive(Deserialize)]
struct RequestedReviewers {
    users: Vec<RequestedUser>,
    teams: Vec<RequestedTeam>,
}

#[derive(Deserialize)]
struct LivePullRequest {
    html_url:   String,
    number:     u64,
    title:      String,
    user:       SearchUser,
    state:      String,
    draft:      bool,
    merged_at:  Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

#[derive(Deserialize)]
struct Installation {
    id: u64,
}

#[derive(Deserialize)]
struct InstallationTokenResponse {
    token: String,
}

fn valid_reviewer(reviewer: &str) -> anyhow::Result<&str> {
    let reviewer = reviewer.trim();
    if reviewer.is_empty()
        || reviewer.len() > 39
        || reviewer.starts_with('-')
        || reviewer.ends_with('-')
        || reviewer
            .chars()
            .any(|ch| !(ch.is_ascii_alphanumeric() || ch == '-'))
    {
        bail!("reviewer must be a valid GitHub login");
    }
    Ok(reviewer)
}

async fn request(
    client: &impl HttpClient,
    method: HttpMethod,
    url: &str,
    token: &str,
    body: Option<&serde_json::Value>,
) -> Result<crate::HttpResponse, ReviewQueueError> {
    let auth = format!("Bearer {token}");
    let response = client
        .request(method, url, &github_headers(&auth), body)
        .await
        .context("GitHub review queue request failed")?;
    match response.status {
        200 | 201 => Ok(response),
        401 => Err(ReviewQueueError::Authentication),
        403 if response.text().to_ascii_lowercase().contains("rate limit") => {
            Err(ReviewQueueError::RateLimited)
        }
        403 | 404 => Err(ReviewQueueError::Access),
        429 => Err(ReviewQueueError::RateLimited),
        status => Err(ReviewQueueError::Other(anyhow::anyhow!(
            "unexpected GitHub review queue status {status}"
        ))),
    }
}

async fn app_tokens(
    client: &impl HttpClient,
    creds: &crate::GitHubAppCredentials,
    base_url: &str,
) -> Result<Vec<AccessToken>, ReviewQueueError> {
    let jwt = sign_app_jwt(&creds.app_id, &creds.private_key_pem)?;
    let mut installations = Vec::new();
    for page in 1.. {
        let url = format!("{base_url}/app/installations?per_page={PAGE_SIZE}&page={page}");
        let response = request(client, HttpMethod::Get, &url, &jwt, None).await?;
        let batch: Vec<Installation> = response
            .json()
            .context("failed to parse GitHub App installations")?;
        let done = batch.len() < PAGE_SIZE;
        installations.extend(batch);
        if done {
            break;
        }
    }

    let mut tokens = Vec::with_capacity(installations.len());
    for installation in installations {
        let url = format!(
            "{base_url}/app/installations/{}/access_tokens",
            installation.id
        );
        // Omitting repository ids gives the token access to every repository
        // selected for this installation. Request only the read permission
        // needed by search and requested-reviewer endpoints.
        let body = serde_json::json!({ "permissions": { "pull_requests": "read" } });
        let response = request(client, HttpMethod::Post, &url, &jwt, Some(&body)).await?;
        let token: InstallationTokenResponse = response
            .json()
            .context("failed to parse GitHub installation token")?;
        tokens.push(AccessToken {
            token:            token.token,
            can_resolve_user: false,
        });
    }
    Ok(tokens)
}

async fn access_tokens(
    client: &impl HttpClient,
    context: &GitHubContext<'_>,
) -> Result<Vec<AccessToken>, ReviewQueueError> {
    match context.creds {
        GitHubCredentials::Pat(token) => Ok(vec![AccessToken {
            token:            token.clone(),
            can_resolve_user: true,
        }]),
        GitHubCredentials::Installation(token) => Ok(vec![AccessToken {
            token:            token.valid_token()?.to_string(),
            can_resolve_user: false,
        }]),
        GitHubCredentials::App(creds) => app_tokens(client, creds, context.base_url).await,
    }
}

async fn token_teams(
    client: &impl HttpClient,
    base_url: &str,
    token: &AccessToken,
    reviewer: &str,
) -> Result<Option<Vec<Team>>, ReviewQueueError> {
    if !token.can_resolve_user {
        return Ok(None);
    }
    let identity = crate::identity::lookup_token_identity(client, &token.token, base_url)
        .await
        .map_err(|error| ReviewQueueError::Other(error.context("resolving review queue user")))?;
    if !identity.login.eq_ignore_ascii_case(reviewer) {
        return Ok(None);
    }

    let mut teams = Vec::new();
    for page in 1.. {
        let url = format!("{base_url}/user/teams?per_page={PAGE_SIZE}&page={page}");
        let response = match request(client, HttpMethod::Get, &url, &token.token, None).await {
            Ok(response) => response,
            Err(ReviewQueueError::Access) => return Ok(None),
            Err(error) => return Err(error),
        };
        let batch: Vec<Team> = response.json().context("failed to parse GitHub teams")?;
        let done = batch.len() < PAGE_SIZE;
        teams.extend(batch);
        if done {
            break;
        }
    }
    Ok(Some(teams))
}

async fn search(
    client: &impl HttpClient,
    base_url: &str,
    token: &str,
    qualifier: &str,
) -> Result<(Vec<SearchItem>, bool), ReviewQueueError> {
    let query = format!("is:pr is:open {qualifier}");
    let mut items = Vec::new();
    let mut incomplete = false;
    for page in 1..=SEARCH_RESULT_LIMIT / PAGE_SIZE {
        let mut url = fabro_http::Url::parse(&format!("{base_url}/search/issues"))
            .context("invalid GitHub API base URL")?;
        url.query_pairs_mut()
            .append_pair("q", &query)
            .append_pair("per_page", &PAGE_SIZE.to_string())
            .append_pair("page", &page.to_string());
        let response = request(client, HttpMethod::Get, url.as_str(), token, None).await?;
        let result: SearchResponse = response
            .json()
            .context("failed to parse GitHub review search")?;
        incomplete |= result.incomplete_results;
        let done = result.items.len() < PAGE_SIZE;
        items.extend(result.items);
        if done {
            incomplete |= result.total_count > items.len();
            break;
        }
        if page == SEARCH_RESULT_LIMIT / PAGE_SIZE {
            incomplete |= result.total_count > items.len();
        }
    }
    Ok((items, incomplete))
}

fn repository_parts(base_url: &str, repository_url: &str) -> Option<(String, String)> {
    let prefix = format!("{}/repos/", base_url.trim_end_matches('/'));
    let slug = repository_url.strip_prefix(&prefix)?;
    let (owner, repository) = slug.split_once('/')?;
    Some((owner.to_string(), repository.to_string()))
}

fn requested_team_name(team: &RequestedTeam, repository_owner: &str) -> String {
    team.organization.as_ref().map_or_else(
        || format!("{repository_owner}/{}", team.slug),
        |organization| format!("{}/{}", organization.login, team.slug),
    )
}

fn known_team_matches(known: &str, requested: &str) -> bool {
    known.eq_ignore_ascii_case(requested)
}

/// Discover current, open review requests for `reviewer`.
///
/// Search is candidate discovery only. Every result is checked against the
/// pull request's current requested reviewers, so fulfilled or removed
/// requests disappear and re-requests return independently of aggregate
/// review state.
pub async fn review_queue(
    context: &GitHubContext<'_>,
    reviewer: &str,
) -> Result<ReviewQueue, ReviewQueueError> {
    let client = context.http_client()?;
    review_queue_with_client(&client, context, reviewer).await
}

pub async fn review_queue_with_client(
    client: &impl HttpClient,
    context: &GitHubContext<'_>,
    reviewer: &str,
) -> Result<ReviewQueue, ReviewQueueError> {
    let reviewer = valid_reviewer(reviewer)?.to_string();
    let tokens = access_tokens(client, context).await?;
    if tokens.is_empty() {
        return Err(ReviewQueueError::Access);
    }

    let mut candidates = BTreeMap::<(String, String, u64), SearchItem>::new();
    let mut known_teams = BTreeSet::new();
    let mut incomplete = false;
    let mut complete_team_coverage = false;

    for token in &tokens {
        let (direct, direct_incomplete) = search(
            client,
            context.base_url,
            &token.token,
            &format!("review-requested:{reviewer}"),
        )
        .await?;
        incomplete |= direct_incomplete;
        for item in direct {
            if let Some((owner, repository)) =
                repository_parts(context.base_url, &item.repository_url)
            {
                candidates
                    .entry((owner, repository, item.number))
                    .or_insert(item);
            }
        }

        if let Some(teams) = token_teams(client, context.base_url, token, &reviewer).await? {
            complete_team_coverage = true;
            for team in teams {
                let team_name = team.full_name();
                known_teams.insert(team_name.clone());
                let (team_items, team_incomplete) = search(
                    client,
                    context.base_url,
                    &token.token,
                    &format!("team-review-requested:{team_name}"),
                )
                .await?;
                incomplete |= team_incomplete;
                for item in team_items {
                    if let Some((owner, repository)) =
                        repository_parts(context.base_url, &item.repository_url)
                    {
                        candidates
                            .entry((owner, repository, item.number))
                            .or_insert(item);
                    }
                }
            }
        }
    }

    let mut items = Vec::new();
    for ((owner, repository, number), _) in candidates {
        let mut live = None;
        let mut current = None;
        for token in &tokens {
            let pull_url = format!(
                "{}/repos/{owner}/{repository}/pulls/{number}",
                context.base_url
            );
            match request(client, HttpMethod::Get, &pull_url, &token.token, None).await {
                Ok(response) => {
                    live = Some(
                        response
                            .json::<LivePullRequest>()
                            .context("failed to parse live pull request")?,
                    );
                }
                Err(ReviewQueueError::Access) => continue,
                Err(error) => return Err(error),
            }
            let url = format!(
                "{}/repos/{owner}/{repository}/pulls/{number}/requested_reviewers",
                context.base_url
            );
            match request(client, HttpMethod::Get, &url, &token.token, None).await {
                Ok(response) => {
                    current = Some(
                        response
                            .json::<RequestedReviewers>()
                            .context("failed to parse requested reviewers")?,
                    );
                    break;
                }
                Err(ReviewQueueError::Access) => {}
                Err(error) => return Err(error),
            }
        }
        let (Some(live), Some(current)) = (live, current) else {
            incomplete = true;
            continue;
        };
        if live.state != "open" || live.merged_at.is_some() {
            continue;
        }
        let direct = current
            .users
            .iter()
            .any(|user| user.login.eq_ignore_ascii_case(&reviewer));
        let teams: Vec<_> = current
            .teams
            .iter()
            .map(|team| requested_team_name(team, &owner))
            .filter(|team| {
                known_teams
                    .iter()
                    .any(|known| known_team_matches(known, team))
            })
            .collect();
        let request_kind = match (direct, teams.is_empty()) {
            (true, true) => ReviewRequestKind::Direct,
            (true, false) => ReviewRequestKind::DirectAndTeam,
            (false, false) => ReviewRequestKind::Team,
            (false, true) => continue,
        };
        items.push(ReviewQueueItem {
            owner,
            repository,
            number: live.number,
            title: live.title,
            author: live.user.login,
            html_url: live.html_url,
            draft: live.draft,
            request_kind,
            teams,
            updated_at: live.updated_at,
        });
    }
    items.sort_by(|left, right| right.updated_at.cmp(&left.updated_at));

    Ok(ReviewQueue {
        reviewer,
        items,
        incomplete,
        team_coverage: if complete_team_coverage {
            TeamCoverage::Complete
        } else {
            TeamCoverage::Limited
        },
    })
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};

    use super::*;
    use crate::tests_mock::MockHttpClient;

    fn search_item(number: u64, draft: bool) -> serde_json::Value {
        serde_json::json!({
            "repository_url": "https://api.test/repos/acme/widgets",
            "html_url": format!("https://github.com/acme/widgets/pull/{number}"),
            "number": number,
            "title": format!("Change {number}"),
            "user": { "login": "author" },
            "draft": draft,
            "updated_at": "2026-11-13T12:00:00Z"
        })
    }

    fn search_body(items: Vec<serde_json::Value>) -> String {
        serde_json::json!({
            "total_count": items.len(),
            "incomplete_results": false,
            "items": items
        })
        .to_string()
    }

    fn live_pull(number: u64, state: &str, draft: bool, merged: bool) -> String {
        serde_json::json!({
            "html_url": format!("https://github.com/acme/widgets/pull/{number}"),
            "number": number,
            "title": format!("Live change {number}"),
            "user": { "login": "live-author" },
            "state": state,
            "draft": draft,
            "merged_at": merged.then_some("2026-11-13T12:30:00Z"),
            "updated_at": "2026-11-13T13:00:00Z"
        })
        .to_string()
    }

    #[tokio::test]
    async fn keeps_only_current_direct_or_member_team_requests_and_deduplicates() {
        let direct_path =
            "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3Amae&per_page=100&page=1";
        let team_path = "/search/issues?q=is%3Apr+is%3Aopen+team-review-requested%3Aacme%2Fplatform&per_page=100&page=1";
        let mock = MockHttpClient::new()
            .on(HttpMethod::Get, "/user", 200, r#"{"login":"mae","id":7}"#)
            .on(
                HttpMethod::Get,
                "/user/teams?per_page=100&page=1",
                200,
                r#"[{"slug":"platform","organization":{"login":"acme"}}]"#,
            )
            .on(
                HttpMethod::Get,
                direct_path,
                200,
                &search_body(vec![search_item(1, false), search_item(2, false)]),
            )
            .on(
                HttpMethod::Get,
                team_path,
                200,
                &search_body(vec![search_item(1, false), search_item(3, true)]),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/1",
                200,
                &live_pull(1, "open", false, false),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/1/requested_reviewers",
                200,
                r#"{"users":[{"login":"mae"}],"teams":[{"slug":"platform","organization":{"login":"acme"}}]}"#,
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/2",
                200,
                &live_pull(2, "open", false, false),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/2/requested_reviewers",
                200,
                r#"{"users":[{"login":"someone-else"}],"teams":[]}"#,
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/3",
                200,
                &live_pull(3, "open", true, false),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/3/requested_reviewers",
                200,
                r#"{"users":[],"teams":[{"slug":"platform"}]}"#,
            );
        let credentials = GitHubCredentials::Pat("token".to_string());
        let context = GitHubContext::new(&credentials, "https://api.test");

        let queue = review_queue_with_client(&mock, &context, "mae")
            .await
            .expect("queue should load");

        assert_eq!(queue.team_coverage, TeamCoverage::Complete);
        assert!(!queue.incomplete);
        assert_eq!(queue.items.len(), 2);
        assert_eq!(queue.items[0].number, 1);
        assert_eq!(
            queue.items[0].request_kind,
            ReviewRequestKind::DirectAndTeam
        );
        assert_eq!(queue.items[0].teams, ["acme/platform"]);
        assert_eq!(queue.items[1].number, 3);
        assert!(queue.items[1].draft);
        assert_eq!(queue.items[1].request_kind, ReviewRequestKind::Team);
        assert_eq!(
            queue.items[0].updated_at,
            Utc.with_ymd_and_hms(2026, 11, 13, 13, 0, 0)
                .single()
                .expect("timestamp should be valid")
        );
    }

    #[tokio::test]
    async fn paginates_search_and_marks_the_github_result_cap_incomplete() {
        let first_page: Vec<_> = (1..=100).map(|_| search_item(9, false)).collect();
        let first_body = serde_json::json!({
            "total_count": 1001,
            "incomplete_results": false,
            "items": first_page
        })
        .to_string();
        let empty_body = search_body(Vec::new());
        let mut mock = MockHttpClient::new()
            .on(HttpMethod::Get, "/user", 200, r#"{"login":"mae","id":7}"#)
            .on(
                HttpMethod::Get,
                "/user/teams?per_page=100&page=1",
                403,
                "{}",
            );
        for page in 1..=10 {
            let path = format!(
                "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3Amae&per_page=100&page={page}"
            );
            mock = mock.on(HttpMethod::Get, &path, 200, &first_body);
        }
        mock = mock.on(
            HttpMethod::Get,
            "/repos/acme/widgets/pulls/9",
            200,
            &live_pull(9, "open", false, false),
        );
        mock = mock.on(
            HttpMethod::Get,
            "/repos/acme/widgets/pulls/9/requested_reviewers",
            200,
            r#"{"users":[{"login":"mae"}],"teams":[]}"#,
        );
        // The empty body is deliberately retained to make fixture intent
        // obvious if the pagination cap changes.
        assert!(empty_body.contains("\"items\":[]"));
        let credentials = GitHubCredentials::Pat("token".to_string());
        let context = GitHubContext::new(&credentials, "https://api.test");

        let queue = review_queue_with_client(&mock, &context, "mae")
            .await
            .expect("queue should load");

        assert!(queue.incomplete);
        assert_eq!(queue.team_coverage, TeamCoverage::Limited);
        assert_eq!(queue.items.len(), 1);
    }

    #[tokio::test]
    async fn live_state_filters_closed_and_merged_candidates_and_refreshes_draft_metadata() {
        let direct_path =
            "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3Amae&per_page=100&page=1";
        let mock = MockHttpClient::new()
            .on(HttpMethod::Get, "/user", 200, r#"{"login":"mae","id":7}"#)
            .on(
                HttpMethod::Get,
                "/user/teams?per_page=100&page=1",
                403,
                "{}",
            )
            .on(
                HttpMethod::Get,
                direct_path,
                200,
                &search_body(vec![
                    search_item(1, false),
                    search_item(2, false),
                    search_item(3, false),
                ]),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/1",
                200,
                &live_pull(1, "open", true, false),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/1/requested_reviewers",
                200,
                r#"{"users":[{"login":"mae"}],"teams":[]}"#,
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/2",
                200,
                &live_pull(2, "closed", false, false),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/2/requested_reviewers",
                200,
                r#"{"users":[{"login":"mae"}],"teams":[]}"#,
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/3",
                200,
                &live_pull(3, "closed", false, true),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/3/requested_reviewers",
                200,
                r#"{"users":[{"login":"mae"}],"teams":[]}"#,
            );
        let credentials = GitHubCredentials::Pat("token".to_string());
        let context = GitHubContext::new(&credentials, "https://api.test");

        let queue = review_queue_with_client(&mock, &context, "mae")
            .await
            .expect("queue should load");

        assert_eq!(queue.items.len(), 1);
        assert_eq!(queue.items[0].number, 1);
        assert!(queue.items[0].draft);
        assert_eq!(queue.items[0].title, "Live change 1");
        assert_eq!(queue.items[0].author, "live-author");
    }

    #[tokio::test]
    async fn bare_requested_team_slug_is_scoped_to_the_repository_owner() {
        let direct_path =
            "/search/issues?q=is%3Apr+is%3Aopen+review-requested%3Amae&per_page=100&page=1";
        let team_path = "/search/issues?q=is%3Apr+is%3Aopen+team-review-requested%3Aother-org%2Fplatform&per_page=100&page=1";
        let mock = MockHttpClient::new()
            .on(HttpMethod::Get, "/user", 200, r#"{"login":"mae","id":7}"#)
            .on(
                HttpMethod::Get,
                "/user/teams?per_page=100&page=1",
                200,
                r#"[{"slug":"platform","organization":{"login":"other-org"}}]"#,
            )
            .on(HttpMethod::Get, direct_path, 200, &search_body(Vec::new()))
            .on(
                HttpMethod::Get,
                team_path,
                200,
                &search_body(vec![search_item(4, false)]),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/4",
                200,
                &live_pull(4, "open", false, false),
            )
            .on(
                HttpMethod::Get,
                "/repos/acme/widgets/pulls/4/requested_reviewers",
                200,
                r#"{"users":[],"teams":[{"slug":"platform"}]}"#,
            );
        let credentials = GitHubCredentials::Pat("token".to_string());
        let context = GitHubContext::new(&credentials, "https://api.test");

        let queue = review_queue_with_client(&mock, &context, "mae")
            .await
            .expect("queue should load");

        assert!(queue.items.is_empty());
        assert_eq!(queue.team_coverage, TeamCoverage::Complete);
    }

    #[test]
    fn rejects_missing_or_invalid_explicit_reviewer() {
        assert!(valid_reviewer("").is_err());
        assert!(valid_reviewer("owner/name").is_err());
        assert_eq!(valid_reviewer("mae-capozzi").unwrap(), "mae-capozzi");
    }
}
