use anyhow::{Context as _, bail};
use chrono::Utc;
use fabro_types::{ReviewReadiness, ReviewReadinessStatus};
use futures::stream::{self, StreamExt as _};
use serde::Deserialize;

use super::{
    AuthoredPullRequest, GitHubContext, GitHubCredentials, HttpClient, HttpMethod, github_headers,
};

const MAX_PAGES: usize = 100;
const MAX_CONCURRENT_PULL_REQUESTS: usize = 8;

const READINESS_QUERY: &str = r"
query ReviewReadiness(
  $owner: String!,
  $repo: String!,
  $number: Int!,
  $threadCursor: String
) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      headRefOid
      author { id }
      reviewThreads(first: 100, after: $threadCursor) {
        nodes {
          id
          isResolved
          comments(last: 100) {
            nodes { author { id isBot } }
            pageInfo { hasPreviousPage startCursor }
          }
        }
        pageInfo { hasNextPage endCursor }
      }
      commits(last: 1) {
        nodes {
          commit {
            oid
            statusCheckRollup {
              state
              contexts(first: 1) { totalCount }
            }
          }
        }
      }
    }
  }
}
";

const THREAD_COMMENTS_QUERY: &str = r"
query ReviewThreadComments($threadId: ID!, $before: String) {
  node(id: $threadId) {
    ... on PullRequestReviewThread {
      comments(last: 100, before: $before) {
        nodes { author { id isBot } }
        pageInfo { hasPreviousPage startCursor }
      }
    }
  }
}
";

const TOP_LEVEL_COMMENTS_QUERY: &str = r"
query PullRequestComments(
  $owner: String!,
  $repo: String!,
  $number: Int!,
  $before: String
) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      comments(last: 100, before: $before) {
        nodes { author { id isBot } }
        pageInfo { hasPreviousPage startCursor }
      }
    }
  }
}
";

/// Collect review-readiness evidence for authored pull requests with bounded
/// concurrency. A failure for one pull request produces `Unavailable` for that
/// item and does not hide the rest of the Runs board.
pub async fn review_readiness_for_pull_requests(
    ctx: &GitHubContext<'_>,
    pull_requests: Vec<AuthoredPullRequest>,
) -> anyhow::Result<Vec<ReviewReadiness>> {
    let client = ctx.http_client()?;
    let token = match ctx.creds {
        GitHubCredentials::Pat(token) => token.clone(),
        GitHubCredentials::Installation(token) => token.valid_token()?.to_string(),
        GitHubCredentials::App(_) => {
            bail!("GitHub App credentials cannot inspect a user's authored pull requests")
        }
    };
    let base_url = ctx.base_url;

    Ok(stream::iter(pull_requests)
        .map(|pull_request| {
            let client = client.clone();
            let token = token.clone();
            async move { collect_or_unavailable(&client, &token, base_url, &pull_request).await }
        })
        .buffered(MAX_CONCURRENT_PULL_REQUESTS)
        .collect()
        .await)
}

async fn collect_or_unavailable(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    pull_request: &AuthoredPullRequest,
) -> ReviewReadiness {
    match collect_readiness(client, token, base_url, pull_request).await {
        Ok(readiness) => readiness,
        Err(error) => {
            tracing::debug!(
                owner = %pull_request.owner,
                repo = %pull_request.repo,
                number = pull_request.number.get(),
                error = %error,
                "GitHub review readiness unavailable"
            );
            unavailable()
        }
    }
}

async fn collect_readiness(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    pull_request: &AuthoredPullRequest,
) -> anyhow::Result<ReviewReadiness> {
    let mut accumulator = ReadinessAccumulator::default();
    let mut thread_cursor = None;

    for _ in 0..MAX_PAGES {
        let variables = serde_json::json!({
            "owner": pull_request.owner,
            "repo": pull_request.repo,
            "number": pull_request.number.get(),
            "threadCursor": thread_cursor,
        });
        let response: GraphqlResponse<ReadinessData> =
            graphql(client, token, base_url, READINESS_QUERY, variables).await?;
        let data = complete_data(response)?;
        let pull_request_data = data
            .repository
            .and_then(|repository| repository.pull_request)
            .context("GitHub did not return the pull request")?;
        let page = ReadinessPage::try_from(pull_request_data)?;
        accumulator.add_page(&page)?;
        for thread in &page.threads.nodes {
            if thread_awaits_response(client, token, base_url, thread, &page.author_id).await? {
                accumulator.feedback_awaiting_response += 1;
            }
        }

        if !page.threads.page_info.has_next_page {
            if top_level_discussion_awaits_response(
                client,
                token,
                base_url,
                pull_request,
                &page.author_id,
            )
            .await?
            {
                accumulator.feedback_awaiting_response += 1;
            }
            return accumulator.finish();
        }
        thread_cursor = next_cursor(&page.threads)?;
    }

    bail!("GitHub review-readiness pagination exceeded {MAX_PAGES} pages")
}

async fn thread_awaits_response(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    thread: &ReviewThread,
    pull_request_author_id: &str,
) -> anyhow::Result<bool> {
    Ok(latest_meaningful_author(
        client,
        token,
        base_url,
        &thread.comments,
        pull_request_author_id,
        |before| serde_json::json!({ "threadId": thread.id, "before": before }),
        THREAD_COMMENTS_QUERY,
    )
    .await?
    .is_some_and(|author_id| author_id != pull_request_author_id))
}

async fn top_level_discussion_awaits_response(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    pull_request: &AuthoredPullRequest,
    pull_request_author_id: &str,
) -> anyhow::Result<bool> {
    let first_page =
        pull_request_comments_page(client, token, base_url, pull_request, None).await?;
    Ok(latest_meaningful_author(
        client,
        token,
        base_url,
        &first_page,
        pull_request_author_id,
        |before| {
            serde_json::json!({
                "owner": pull_request.owner,
                "repo": pull_request.repo,
                "number": pull_request.number.get(),
                "before": before,
            })
        },
        TOP_LEVEL_COMMENTS_QUERY,
    )
    .await?
    .is_some_and(|author_id| author_id != pull_request_author_id))
}

async fn latest_meaningful_author(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    first_page: &ReverseConnection<ReviewComment>,
    pull_request_author_id: &str,
    variables: impl Fn(Option<String>) -> serde_json::Value,
    query: &str,
) -> anyhow::Result<Option<String>> {
    let mut page = first_page.clone();
    for _ in 0..MAX_PAGES {
        if let Some(author) = page
            .nodes
            .iter()
            .rev()
            .find_map(|comment| comment.meaningful_author(pull_request_author_id))
        {
            return Ok(Some(author.to_string()));
        }
        if !page.page_info.has_previous_page {
            return Ok(None);
        }
        let before = previous_cursor(&page)?;
        page = comments_page(client, token, base_url, query, variables(Some(before))).await?;
    }
    bail!("GitHub review-feedback pagination exceeded {MAX_PAGES} pages")
}

async fn pull_request_comments_page(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    pull_request: &AuthoredPullRequest,
    before: Option<String>,
) -> anyhow::Result<ReverseConnection<ReviewComment>> {
    let variables = serde_json::json!({
        "owner": pull_request.owner,
        "repo": pull_request.repo,
        "number": pull_request.number.get(),
        "before": before,
    });
    comments_page(client, token, base_url, TOP_LEVEL_COMMENTS_QUERY, variables).await
}

async fn comments_page(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    query: &str,
    variables: serde_json::Value,
) -> anyhow::Result<ReverseConnection<ReviewComment>> {
    let response: GraphqlResponse<CommentsData> =
        graphql(client, token, base_url, query, variables).await?;
    let data = complete_data(response)?;
    data.node
        .and_then(|node| node.comments)
        .or_else(|| {
            data.repository
                .and_then(|repository| repository.pull_request)
                .map(|pull_request| pull_request.comments)
        })
        .context("GitHub did not return review comments")
}

fn previous_cursor<T>(connection: &ReverseConnection<T>) -> anyhow::Result<String> {
    connection
        .page_info
        .start_cursor
        .clone()
        .context("GitHub pagination omitted a start cursor")
}

fn next_cursor<T>(connection: &Connection<T>) -> anyhow::Result<Option<String>> {
    connection
        .page_info
        .end_cursor
        .clone()
        .map(Some)
        .context("GitHub pagination omitted an end cursor")
}

async fn graphql<T: for<'de> Deserialize<'de>>(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    query: &str,
    variables: serde_json::Value,
) -> anyhow::Result<GraphqlResponse<T>> {
    let auth = format!("Bearer {token}");
    let url = format!("{base_url}/graphql");
    let body = serde_json::json!({ "query": query, "variables": variables });
    let response = client
        .request(HttpMethod::Post, &url, &github_headers(&auth), Some(&body))
        .await
        .context("Failed to query GitHub review readiness")?;
    if response.status != 200 {
        bail!(
            "GitHub returned status {} querying review readiness",
            response.status
        );
    }
    response
        .json()
        .context("Failed to parse GitHub review readiness")
}

fn complete_data<T>(response: GraphqlResponse<T>) -> anyhow::Result<T> {
    if response.errors.is_some_and(|errors| !errors.is_empty()) {
        bail!("GitHub returned a partial GraphQL response")
    }
    response
        .data
        .context("GitHub GraphQL response omitted data")
}

fn unavailable() -> ReviewReadiness {
    ReviewReadiness {
        status: ReviewReadinessStatus::Unavailable,
        head_sha: None,
        unresolved_thread_count: None,
        feedback_awaiting_response_count: None,
        checked_at: Utc::now(),
    }
}

#[derive(Default)]
struct ReadinessAccumulator {
    head_sha: Option<String>,
    author_id: Option<String>,
    unresolved_threads: u64,
    feedback_awaiting_response: u64,
    ci: Option<CiEvidence>,
}

impl ReadinessAccumulator {
    fn add_page(&mut self, page: &ReadinessPage) -> anyhow::Result<()> {
        if !valid_sha(&page.head_sha) || page.commit_sha != page.head_sha {
            bail!("GitHub returned inconsistent pull request head data")
        }
        if self
            .head_sha
            .as_ref()
            .is_some_and(|head_sha| head_sha != &page.head_sha)
        {
            bail!("Pull request head changed while readiness was loading")
        }
        if self
            .author_id
            .as_ref()
            .is_some_and(|author_id| author_id != &page.author_id)
        {
            bail!("Pull request author changed while readiness was loading")
        }
        self.head_sha.get_or_insert_with(|| page.head_sha.clone());
        self.author_id.get_or_insert_with(|| page.author_id.clone());
        self.ci = Some(page.ci.clone());
        self.unresolved_threads += page
            .threads
            .nodes
            .iter()
            .filter(|thread| !thread.is_resolved)
            .count() as u64;
        Ok(())
    }

    fn finish(self) -> anyhow::Result<ReviewReadiness> {
        let head_sha = self
            .head_sha
            .context("GitHub returned no pull request head")?;
        let ci = self
            .ci
            .context("GitHub returned no check-rollup evidence")?
            .state()?;
        let status = match ci {
            CiState::NoChecks => ReviewReadinessStatus::NoChecks,
            CiState::Failed => ReviewReadinessStatus::CiFailed,
            CiState::Running => ReviewReadinessStatus::CiRunning,
            CiState::Green if self.unresolved_threads > 0 => {
                ReviewReadinessStatus::UnresolvedComments
            }
            CiState::Green => ReviewReadinessStatus::Ready,
        };
        Ok(ReviewReadiness {
            status,
            head_sha: Some(head_sha),
            unresolved_thread_count: Some(self.unresolved_threads),
            feedback_awaiting_response_count: Some(self.feedback_awaiting_response),
            checked_at: Utc::now(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CiState {
    Green,
    Running,
    Failed,
    NoChecks,
}

#[derive(Clone)]
enum CiEvidence {
    NoChecks,
    /// GitHub's `StatusCheckRollup` is the authoritative latest view across
    /// both check runs and commit statuses. Using its state avoids ordering
    /// overlapping reruns by completion time, which is not run recency.
    Rollup {
        state:          String,
        reported_count: u64,
    },
}

impl CiEvidence {
    fn state(&self) -> anyhow::Result<CiState> {
        match self {
            Self::NoChecks
            | Self::Rollup {
                reported_count: 0, ..
            } => Ok(CiState::NoChecks),
            Self::Rollup { state, .. } => match state.as_str() {
                "SUCCESS" => Ok(CiState::Green),
                "PENDING" | "EXPECTED" => Ok(CiState::Running),
                "ERROR" | "FAILURE" => Ok(CiState::Failed),
                _ => bail!("GitHub returned an unknown check-rollup state"),
            },
        }
    }
}

fn valid_sha(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

struct ReadinessPage {
    head_sha:   String,
    author_id:  String,
    commit_sha: String,
    threads:    Connection<ReviewThread>,
    ci:         CiEvidence,
}

impl TryFrom<PullRequestData> for ReadinessPage {
    type Error = anyhow::Error;

    fn try_from(pull_request: PullRequestData) -> Result<Self, Self::Error> {
        let commit = pull_request
            .commits
            .nodes
            .into_iter()
            .next()
            .context("GitHub returned no current pull request commit")?
            .commit;
        let ci = commit
            .status_check_rollup
            .map_or(CiEvidence::NoChecks, |rollup| CiEvidence::Rollup {
                state:          rollup.state,
                reported_count: rollup.contexts.total_count,
            });
        Ok(Self {
            head_sha: pull_request.head_ref_oid,
            author_id: pull_request.author.id,
            commit_sha: commit.oid,
            threads: pull_request.review_threads,
            ci,
        })
    }
}

#[derive(Deserialize)]
struct GraphqlResponse<T> {
    data:   Option<T>,
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct ReadinessData {
    repository: Option<RepositoryData>,
}

#[derive(Deserialize)]
struct RepositoryData {
    #[serde(rename = "pullRequest")]
    pull_request: Option<PullRequestData>,
}

#[derive(Deserialize)]
struct PullRequestData {
    #[serde(rename = "headRefOid")]
    head_ref_oid:   String,
    author:         Actor,
    #[serde(rename = "reviewThreads")]
    review_threads: Connection<ReviewThread>,
    commits:        CommitConnection,
}

#[derive(Deserialize)]
struct CommitConnection {
    nodes: Vec<CommitNode>,
}

#[derive(Deserialize)]
struct CommitNode {
    commit: CommitData,
}

#[derive(Deserialize)]
struct CommitData {
    oid:                 String,
    #[serde(rename = "statusCheckRollup")]
    status_check_rollup: Option<StatusCheckRollup>,
}

#[derive(Deserialize)]
struct StatusCheckRollup {
    state:    String,
    contexts: ContextSummary,
}

#[derive(Deserialize)]
struct ContextSummary {
    #[serde(rename = "totalCount")]
    total_count: u64,
}

#[derive(Deserialize)]
struct Connection<T> {
    nodes:     Vec<T>,
    #[serde(rename = "pageInfo")]
    page_info: PageInfo,
}

#[derive(Deserialize)]
struct PageInfo {
    #[serde(rename = "hasNextPage")]
    has_next_page: bool,
    #[serde(rename = "endCursor")]
    end_cursor:    Option<String>,
}

#[derive(Deserialize)]
struct ReviewThread {
    id:          String,
    #[serde(rename = "isResolved")]
    is_resolved: bool,
    comments:    ReverseConnection<ReviewComment>,
}

#[derive(Clone, Deserialize)]
struct ReviewComment {
    author: Option<Actor>,
}

impl ReviewComment {
    fn meaningful_author(&self, pull_request_author_id: &str) -> Option<&str> {
        self.author.as_ref().and_then(|author| {
            (!author.is_bot || author.id == pull_request_author_id).then_some(author.id.as_str())
        })
    }
}

#[derive(Clone, Deserialize)]
struct Actor {
    id:     String,
    #[serde(default, rename = "isBot")]
    is_bot: bool,
}

#[derive(Clone, Deserialize)]
struct ReverseConnection<T> {
    nodes:     Vec<T>,
    #[serde(rename = "pageInfo")]
    page_info: ReversePageInfo,
}

#[derive(Clone, Deserialize)]
struct ReversePageInfo {
    #[serde(rename = "hasPreviousPage")]
    has_previous_page: bool,
    #[serde(rename = "startCursor")]
    start_cursor:      Option<String>,
}

#[derive(Deserialize)]
struct CommentsData {
    node:       Option<CommentsNode>,
    repository: Option<CommentsRepository>,
}

#[derive(Deserialize)]
struct CommentsNode {
    comments: Option<ReverseConnection<ReviewComment>>,
}

#[derive(Deserialize)]
struct CommentsRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<CommentsPullRequest>,
}

#[derive(Deserialize)]
struct CommentsPullRequest {
    comments: ReverseConnection<ReviewComment>,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use chrono::TimeZone as _;

    use super::*;
    use crate::tests_mock::MockHttpClient;

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const NEXT_HEAD: &str = "2222222222222222222222222222222222222222";

    fn pull_request() -> AuthoredPullRequest {
        AuthoredPullRequest {
            owner:      "acme".to_string(),
            repo:       "widgets".to_string(),
            number:     NonZeroU64::new(42).expect("42 is non-zero"),
            title:      "Test readiness".to_string(),
            html_url:   "https://github.com/acme/widgets/pull/42".to_string(),
            draft:      true,
            updated_at: Utc
                .with_ymd_and_hms(2026, 11, 18, 12, 0, 0)
                .single()
                .expect("valid timestamp"),
        }
    }

    fn response(
        head: &str,
        threads: &[bool],
        rollup_state: Option<&str>,
        reported_count: u64,
    ) -> String {
        let status_check_rollup = rollup_state.map(|state| {
            serde_json::json!({
                "state": state,
                "contexts": { "totalCount": reported_count }
            })
        });
        serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": head,
                        "author": { "id": "author-id" },
                        "reviewThreads": {
                            "nodes": threads.iter()
                                .enumerate()
                                .map(|(index, resolved)| serde_json::json!({
                                    "id": format!("thread-{index}"),
                                    "isResolved": resolved,
                                    "comments": {
                                        "nodes": [{
                                            "author": {
                                                "id": "author-id",
                                                "isBot": false
                                            }
                                        }],
                                        "pageInfo": {
                                            "hasPreviousPage": false,
                                            "startCursor": null
                                        }
                                    }
                                }))
                                .collect::<Vec<_>>(),
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        },
                        "comments": {
                            "nodes": [],
                            "pageInfo": {
                                "hasPreviousPage": false,
                                "startCursor": null
                            }
                        },
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": head,
                                    "statusCheckRollup": status_check_rollup
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string()
    }

    async fn collect(body: &str) -> anyhow::Result<ReviewReadiness> {
        let mock = MockHttpClient::new().on(HttpMethod::Post, "/graphql", 200, body);
        collect_readiness(&mock, "secret", "", &pull_request()).await
    }

    #[tokio::test]
    async fn green_checks_and_zero_unresolved_threads_are_ready() {
        let body = response(HEAD, &[true], Some("SUCCESS"), 2);

        let readiness = collect(&body).await.expect("readiness should load");

        assert_eq!(readiness.status, ReviewReadinessStatus::Ready);
        assert_eq!(readiness.head_sha.as_deref(), Some(HEAD));
        assert_eq!(readiness.unresolved_thread_count, Some(0));
    }

    #[tokio::test]
    async fn unresolved_then_resolved_review_threads_change_readiness() {
        let unresolved = collect(&response(HEAD, &[false, true], Some("SUCCESS"), 1))
            .await
            .expect("unresolved evidence should load");
        let resolved = collect(&response(HEAD, &[true, true], Some("SUCCESS"), 1))
            .await
            .expect("resolved evidence should load");

        assert_eq!(unresolved.status, ReviewReadinessStatus::UnresolvedComments);
        assert_eq!(unresolved.unresolved_thread_count, Some(1));
        assert_eq!(resolved.status, ReviewReadinessStatus::Ready);
    }

    #[tokio::test]
    async fn pending_and_failing_checks_are_blockers() {
        let pending = collect(&response(HEAD, &[], Some("PENDING"), 1))
            .await
            .expect("pending evidence should load");
        let failing = collect(&response(HEAD, &[], Some("FAILURE"), 2))
            .await
            .expect("failing evidence should load");

        assert_eq!(pending.status, ReviewReadinessStatus::CiRunning);
        assert_eq!(failing.status, ReviewReadinessStatus::CiFailed);
    }

    #[tokio::test]
    async fn authoritative_rollup_keeps_a_newer_rerun_from_being_hidden() {
        // GitHub's rollup has already applied stable run identity and latest
        // commit-status semantics. A newer queued rerun must therefore stay
        // pending even if an older overlapping attempt completed successfully.
        let body = response(HEAD, &[], Some("PENDING"), 2);

        let readiness = collect(&body).await.expect("rerun evidence should load");

        assert_eq!(readiness.status, ReviewReadinessStatus::CiRunning);
    }

    #[tokio::test]
    async fn no_reported_checks_is_not_green() {
        let no_rollup = collect(&response(HEAD, &[], None, 0))
            .await
            .expect("empty evidence should load");
        let empty_rollup = collect(&response(HEAD, &[], Some("SUCCESS"), 0))
            .await
            .expect("empty rollup should load");

        assert_eq!(no_rollup.status, ReviewReadinessStatus::NoChecks);
        assert_eq!(empty_rollup.status, ReviewReadinessStatus::NoChecks);
    }

    #[test]
    fn changed_head_invalidates_collected_green_evidence() {
        let page_data: PullRequestData = serde_json::from_value(serde_json::json!({
            "headRefOid": HEAD,
            "author": { "id": "author-id" },
            "reviewThreads": {
                "nodes": [],
                "pageInfo": { "hasNextPage": false, "endCursor": null }
            },
            "commits": {
                "nodes": [{
                    "commit": {
                        "oid": HEAD,
                        "statusCheckRollup": {
                            "state": "SUCCESS",
                            "contexts": { "totalCount": 1 }
                        }
                    }
                }]
            }
        }))
        .expect("page fixture should deserialize");
        let page = ReadinessPage::try_from(page_data).expect("page should be valid");
        let mut accumulator = ReadinessAccumulator::default();
        accumulator.add_page(&page).expect("page should accumulate");

        let changed_page_data: PullRequestData = serde_json::from_value(serde_json::json!({
            "headRefOid": NEXT_HEAD,
            "author": { "id": "author-id" },
            "reviewThreads": {
                "nodes": [],
                "pageInfo": { "hasNextPage": false, "endCursor": null }
            },
            "commits": {
                "nodes": [{
                    "commit": {
                        "oid": NEXT_HEAD,
                        "statusCheckRollup": null
                    }
                }]
            }
        }))
        .expect("changed-head fixture should deserialize");
        let changed_page =
            ReadinessPage::try_from(changed_page_data).expect("changed page should be valid");

        assert!(accumulator.add_page(&changed_page).is_err());
    }

    #[tokio::test]
    async fn unknown_partial_incomplete_and_transport_failures_are_not_ready() {
        let unknown = collect(&response(HEAD, &[], Some("MYSTERY"), 1)).await;
        let partial = serde_json::json!({
            "data": { "repository": null },
            "errors": [{ "message": "reviewThreads forbidden" }]
        })
        .to_string();
        let partial = collect(&partial).await;
        let incomplete = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "id": "author-id" },
                        "reviewThreads": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": true, "endCursor": null }
                        },
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": HEAD,
                                    "statusCheckRollup": null
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string();
        let incomplete = collect(&incomplete).await;
        let transport =
            MockHttpClient::new().fails(HttpMethod::Post, "/graphql", "connection closed");
        let transport = collect_readiness(&transport, "secret", "", &pull_request()).await;

        assert!(unknown.is_err());
        assert!(partial.is_err());
        assert!(incomplete.is_err());
        assert!(transport.is_err());
        assert_eq!(unavailable().status, ReviewReadinessStatus::Unavailable);
    }

    #[tokio::test]
    async fn missing_requested_threads_or_rollup_contexts_are_unavailable() {
        let missing_threads = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "reviewThreads": null,
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": HEAD,
                                    "statusCheckRollup": {
                                        "state": "SUCCESS",
                                        "contexts": { "totalCount": 1 }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string();
        let missing_contexts = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "id": "author-id" },
                        "reviewThreads": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        },
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": HEAD,
                                    "statusCheckRollup": {
                                        "state": "SUCCESS",
                                        "contexts": null
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string();

        let missing_threads =
            MockHttpClient::new().on(HttpMethod::Post, "/graphql", 200, &missing_threads);
        let missing_contexts =
            MockHttpClient::new().on(HttpMethod::Post, "/graphql", 200, &missing_contexts);

        assert_eq!(
            collect_or_unavailable(&missing_threads, "secret", "", &pull_request())
                .await
                .status,
            ReviewReadinessStatus::Unavailable
        );
        assert_eq!(
            collect_or_unavailable(&missing_contexts, "secret", "", &pull_request())
                .await
                .status,
            ReviewReadinessStatus::Unavailable
        );
    }

    #[tokio::test]
    async fn missing_contexts_on_a_later_thread_page_discard_earlier_green() {
        let first_page = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "id": "author-id" },
                        "reviewThreads": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": true, "endCursor": "threads-1" }
                        },
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": HEAD,
                                    "statusCheckRollup": {
                                        "state": "SUCCESS",
                                        "contexts": { "totalCount": 1 }
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string();
        let malformed_second_page = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "id": "author-id" },
                        "reviewThreads": {
                            "nodes": [],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        },
                        "commits": {
                            "nodes": [{
                                "commit": {
                                    "oid": HEAD,
                                    "statusCheckRollup": {
                                        "state": "SUCCESS",
                                        "contexts": null
                                    }
                                }
                            }]
                        }
                    }
                }
            }
        })
        .to_string();
        let mock = MockHttpClient::new()
            .on_once(HttpMethod::Post, "/graphql", 200, &first_page)
            .on_once(HttpMethod::Post, "/graphql", 200, &malformed_second_page);

        let readiness = collect_or_unavailable(&mock, "secret", "", &pull_request()).await;

        assert_eq!(readiness.status, ReviewReadinessStatus::Unavailable);
        assert_eq!(mock.request_count(), 2);
    }
}
