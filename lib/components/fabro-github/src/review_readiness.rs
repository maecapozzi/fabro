use anyhow::{Context as _, bail};
use chrono::{DateTime, Utc};
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
  viewer { login }
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      headRefOid
      author { login __typename }
      reviewThreads(first: 100, after: $threadCursor) {
        nodes {
          id
          isResolved
          comments(last: 100) {
            nodes { databaseId body createdAt updatedAt author { login __typename } }
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
        nodes { databaseId body createdAt updatedAt author { login __typename } }
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
        nodes { databaseId body createdAt updatedAt author { login __typename } }
        pageInfo { hasPreviousPage startCursor }
      }
    }
  }
}
";

const TOP_LEVEL_REVIEWS_QUERY: &str = r"
query PullRequestReviews(
  $owner: String!,
  $repo: String!,
  $number: Int!,
  $before: String
) {
  repository(owner: $owner, name: $repo) {
    pullRequest(number: $number) {
      reviews(last: 100, before: $before) {
        nodes { databaseId body createdAt updatedAt author { login __typename } }
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
        let factory_login = data.viewer.login;
        let pull_request_data = data
            .repository
            .and_then(|repository| repository.pull_request)
            .context("GitHub did not return the pull request")?;
        let page = ReadinessPage::try_from((pull_request_data, factory_login))?;
        accumulator.add_page(&page)?;
        for thread in &page.threads.nodes {
            if thread_awaits_response(client, token, base_url, thread, &page.response_identities)
                .await?
            {
                accumulator.feedback_awaiting_response += 1;
            }
        }

        if !page.threads.page_info.has_next_page {
            accumulator.feedback_awaiting_response += top_level_feedback_awaiting_response_count(
                client,
                token,
                base_url,
                pull_request,
                &page.response_identities,
            )
            .await?;
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
    response_identities: &ResponseIdentities,
) -> anyhow::Result<bool> {
    let history = feedback_history(
        client,
        token,
        base_url,
        &thread.comments,
        response_identities,
        FeedbackKind::Inline,
        |before| serde_json::json!({ "threadId": thread.id, "before": before }),
        THREAD_COMMENTS_QUERY,
    )
    .await?;
    let Some(latest_feedback) = history.latest_feedback()? else {
        return Ok(false);
    };

    if history.is_handled(latest_feedback)? {
        return Ok(false);
    }

    // Resolution clears the thread's existing obligation, but GitHub can leave
    // it resolved when a reviewer follows up. A follow-up after Factory's
    // recognized response is a new obligation.
    Ok(!thread.is_resolved || history.has_response_before(latest_feedback)?)
}

async fn top_level_feedback_awaiting_response_count(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    pull_request: &AuthoredPullRequest,
    response_identities: &ResponseIdentities,
) -> anyhow::Result<u64> {
    let first_page =
        pull_request_comments_page(client, token, base_url, pull_request, None).await?;
    let mut comment_history = feedback_history(
        client,
        token,
        base_url,
        &first_page,
        response_identities,
        FeedbackKind::Comment,
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
    .await?;
    let first_review_page =
        pull_request_reviews_page(client, token, base_url, pull_request, None).await?;
    let review_history = feedback_history(
        client,
        token,
        base_url,
        &first_review_page,
        response_identities,
        FeedbackKind::Review,
        |before| {
            serde_json::json!({
                "owner": pull_request.owner,
                "repo": pull_request.repo,
                "number": pull_request.number.get(),
                "before": before,
            })
        },
        TOP_LEVEL_REVIEWS_QUERY,
    )
    .await?;
    comment_history.extend(review_history);
    comment_history.outstanding_count()
}

async fn feedback_history(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    first_page: &ReverseConnection<ReviewComment>,
    response_identities: &ResponseIdentities,
    feedback_kind: FeedbackKind,
    variables: impl Fn(Option<String>) -> serde_json::Value,
    query: &str,
) -> anyhow::Result<FeedbackHistory> {
    let mut page = first_page.clone();
    let mut history = FeedbackHistory::default();
    for _ in 0..MAX_PAGES {
        record_feedback_items(
            &page.nodes,
            response_identities,
            feedback_kind,
            &mut history,
        )?;
        if !page.page_info.has_previous_page {
            return Ok(history);
        }
        let before = previous_cursor(&page)?;
        let variables = variables(Some(before));
        page = if query == TOP_LEVEL_REVIEWS_QUERY {
            reviews_page(client, token, base_url, variables).await?
        } else {
            comments_page(client, token, base_url, query, variables).await?
        };
    }
    bail!("GitHub review-feedback pagination exceeded {MAX_PAGES} pages")
}

fn record_feedback_items(
    comments: &[ReviewComment],
    response_identities: &ResponseIdentities,
    feedback_kind: FeedbackKind,
    history: &mut FeedbackHistory,
) -> anyhow::Result<()> {
    for comment in comments {
        if comment.is_reviewer_feedback(response_identities) {
            history.feedback_items.push(FeedbackItem {
                kind:    feedback_kind,
                comment: comment.clone(),
            });
        }
        if comment.is_factory_response(response_identities) {
            let targets = factory_feedback_targets(&comment.body).collect::<Vec<_>>();
            if !targets.is_empty() {
                history.responses.push(FactoryResponse {
                    created_at: comment.created_at()?,
                    targets,
                });
            }
        }
    }
    Ok(())
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

async fn pull_request_reviews_page(
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
    reviews_page(client, token, base_url, variables).await
}

async fn reviews_page(
    client: &impl HttpClient,
    token: &str,
    base_url: &str,
    variables: serde_json::Value,
) -> anyhow::Result<ReverseConnection<ReviewComment>> {
    let response: GraphqlResponse<ReviewData> =
        graphql(client, token, base_url, TOP_LEVEL_REVIEWS_QUERY, variables).await?;
    complete_data(response)?
        .repository
        .and_then(|repository| repository.pull_request)
        .map(|pull_request| pull_request.reviews)
        .context("GitHub did not return pull request reviews")
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
    response_identities: Option<ResponseIdentities>,
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
            .response_identities
            .as_ref()
            .is_some_and(|identities| identities != &page.response_identities)
        {
            bail!("Pull request response identities changed while readiness was loading")
        }
        self.head_sha.get_or_insert_with(|| page.head_sha.clone());
        self.response_identities
            .get_or_insert_with(|| page.response_identities.clone());
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
    head_sha:            String,
    response_identities: ResponseIdentities,
    commit_sha:          String,
    threads:             Connection<ReviewThread>,
    ci:                  CiEvidence,
}

impl TryFrom<(PullRequestData, String)> for ReadinessPage {
    type Error = anyhow::Error;

    fn try_from(
        (pull_request, factory_login): (PullRequestData, String),
    ) -> Result<Self, Self::Error> {
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
            response_identities: ResponseIdentities {
                pull_request_author_login: pull_request.author.login,
                factory_login,
            },
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
    viewer:     Viewer,
    repository: Option<RepositoryData>,
}

#[derive(Deserialize)]
struct Viewer {
    login: String,
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
    #[serde(rename = "databaseId")]
    database_id: u64,
    body:        String,
    #[serde(rename = "createdAt")]
    created_at:  Option<DateTime<Utc>>,
    #[serde(rename = "updatedAt")]
    updated_at:  Option<DateTime<Utc>>,
    author:      Option<Actor>,
}

impl ReviewComment {
    fn is_reviewer_feedback(&self, response_identities: &ResponseIdentities) -> bool {
        self.author
            .as_ref()
            .is_some_and(|author| !response_identities.contains(&author.login))
            && !self.is_routine_status_noise()
    }

    fn is_factory_response(&self, response_identities: &ResponseIdentities) -> bool {
        self.author
            .as_ref()
            .is_some_and(|author| response_identities.contains(&author.login))
    }

    fn created_at(&self) -> anyhow::Result<DateTime<Utc>> {
        self.created_at
            .context("GitHub omitted a Factory response creation timestamp")
    }

    fn revision(&self) -> anyhow::Result<DateTime<Utc>> {
        self.updated_at.with_context(|| {
            format!(
                "GitHub omitted reviewer feedback revision timestamp for database ID {}",
                self.database_id
            )
        })
    }

    fn is_routine_status_noise(&self) -> bool {
        let body = visible_review_text(&self.body);
        body.is_empty()
            || (self.author.as_ref().is_some_and(Actor::is_bot)
                && !has_substantive_code_review_signal(&body))
    }
}

/// Bots also emit routine progress and status messages. Treat a bot comment as
/// review feedback only when its content identifies a concrete code concern or
/// asks for a change, rather than relying on the bot's name or exact phrasing.
fn has_substantive_code_review_signal(body: &str) -> bool {
    const CONCRETE_CONCERNS: &[&str] = &[
        "authentication",
        "authorization",
        "deadlock",
        "injection",
        "memory leak",
        "null dereference",
        "out of bounds",
        "panic",
        "path traversal",
        "race condition",
        "regression",
        "security",
        "sql injection",
        "unsafe",
        "use after free",
        "vulnerab",
        "xss",
    ];
    const ACTION_REQUESTS: &[&str] = &[
        "please add ",
        "please change ",
        "please fix ",
        "please handle ",
        "please remove ",
        "please use ",
        "please validate ",
        "should add ",
        "should change ",
        "should fix ",
        "should handle ",
        "should not ",
        "should use ",
        "should validate ",
    ];
    const BEHAVIORAL_CONCERNS: &[&str] = &[
        "does not ",
        "doesn't ",
        "fails to ",
        "failed to ",
        "is missing",
        "are missing",
        "permits ",
        "without validat",
        "without checking",
        "before it validat",
        "before validating",
    ];

    let text = visible_review_text(body).to_ascii_lowercase();
    let text = text.trim();
    if text.is_empty() {
        return false;
    }

    let requests_change = ACTION_REQUESTS.iter().any(|request| text.contains(request))
        || BEHAVIORAL_CONCERNS
            .iter()
            .any(|concern| text.contains(concern));
    if requests_change {
        return true;
    }

    !is_routine_status_message(text)
        && CONCRETE_CONCERNS
            .iter()
            .any(|concern| text.contains(concern))
}

/// A bot's status is noise only when its *whole* visible message has the
/// limited vocabulary of a successful check or explicit no-findings report.
/// This deliberately keeps mixed messages: a status clause must never veto a
/// concrete finding elsewhere in the same review.
fn is_routine_status_message(text: &str) -> bool {
    const ROUTINE_STATUS_WORDS: &[&str] = &[
        "a",
        "all",
        "analysis",
        "and",
        "an",
        "build",
        "check",
        "checks",
        "complete",
        "completed",
        "deployment",
        "details",
        "detected",
        "done",
        "failed",
        "findings",
        "finished",
        "full",
        "has",
        "have",
        "in",
        "is",
        "log",
        "logs",
        "no",
        "of",
        "on",
        "passed",
        "path",
        "previous",
        "report",
        "results",
        "review",
        "run",
        "scan",
        "security",
        "see",
        "status",
        "succeeded",
        "success",
        "successful",
        "successfully",
        "test",
        "the",
        "this",
        "traversal",
        "vulnerabilities",
        "vulnerability",
        "was",
        "were",
        "workflow",
        "found",
        "sql",
        "injection",
    ];

    let words = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>();
    if words.is_empty()
        || words
            .iter()
            .any(|word| !ROUTINE_STATUS_WORDS.contains(word))
    {
        return false;
    }

    let reports_no_findings = words.windows(2).any(|words| words == ["no", "findings"])
        || (words.first() == Some(&"no")
            && words
                .last()
                .is_some_and(|word| matches!(*word, "found" | "detected"))
            && words
                .iter()
                .any(|word| matches!(*word, "vulnerability" | "vulnerabilities")));
    let reports_success = words.iter().any(|word| {
        matches!(
            *word,
            "complete"
                | "completed"
                | "done"
                | "finished"
                | "passed"
                | "succeeded"
                | "success"
                | "successful"
                | "successfully"
        )
    }) && words.iter().any(|word| {
        matches!(
            *word,
            "analysis"
                | "build"
                | "check"
                | "checks"
                | "deployment"
                | "review"
                | "scan"
                | "test"
                | "workflow"
        )
    });

    reports_no_findings || reports_success
}

/// Hide machine metadata and URLs before classifying a review. GitHub bots
/// commonly prepend HTML markers and link to dashboard status pages; neither
/// should mask a visible finding or create one from URL path text.
fn visible_review_text(body: &str) -> String {
    let mut visible = String::with_capacity(body.len());
    let mut remaining = body;
    while let Some(comment_start) = remaining.find("<!--") {
        visible.push_str(&remaining[..comment_start]);
        let after_start = &remaining[comment_start + "<!--".len()..];
        match after_start.find("-->") {
            Some(comment_end) => remaining = &after_start[comment_end + "-->".len()..],
            None => return visible,
        }
    }
    visible.push_str(remaining);

    let mut outside_code = String::with_capacity(visible.len());
    let mut in_code = false;
    for character in visible.chars() {
        if character == '`' {
            in_code = !in_code;
        } else if !in_code {
            outside_code.push(character);
        }
    }

    outside_code
        .lines()
        .map(|line| {
            line.split_whitespace()
                .filter(|part| {
                    !part.contains("http://")
                        && !part.contains("https://")
                        && !part.contains("www.")
                })
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ResponseIdentities {
    pull_request_author_login: String,
    factory_login:             String,
}

impl ResponseIdentities {
    fn contains(&self, login: &str) -> bool {
        login == self.pull_request_author_login || login == self.factory_login
    }
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, strum::EnumString)]
#[strum(serialize_all = "lowercase")]
enum FeedbackKind {
    Comment,
    Inline,
    Review,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct FeedbackKey {
    kind:        FeedbackKind,
    database_id: u64,
}

#[derive(Clone)]
struct FeedbackItem {
    kind:    FeedbackKind,
    comment: ReviewComment,
}

impl FeedbackItem {
    fn key(&self) -> FeedbackKey {
        FeedbackKey {
            kind:        self.kind,
            database_id: self.comment.database_id,
        }
    }
}

struct FactoryResponse {
    created_at: DateTime<Utc>,
    targets:    Vec<FeedbackKey>,
}

#[derive(Default)]
struct FeedbackHistory {
    feedback_items: Vec<FeedbackItem>,
    responses:      Vec<FactoryResponse>,
}

impl FeedbackHistory {
    fn extend(&mut self, other: Self) {
        self.feedback_items.extend(other.feedback_items);
        self.responses.extend(other.responses);
    }

    fn outstanding_count(&self) -> anyhow::Result<u64> {
        self.feedback_items.iter().try_fold(0, |count, feedback| {
            Ok(count + u64::from(!self.is_handled(feedback)?))
        })
    }

    fn latest_feedback(&self) -> anyhow::Result<Option<&FeedbackItem>> {
        self.feedback_items
            .iter()
            .try_fold(None::<&FeedbackItem>, |latest, feedback| {
                let feedback_revision = feedback.comment.revision()?;
                Ok(match latest {
                    Some(current) if current.comment.revision()? >= feedback_revision => {
                        Some(current)
                    }
                    _ => Some(feedback),
                })
            })
    }

    fn is_handled(&self, feedback: &FeedbackItem) -> anyhow::Result<bool> {
        let revision = feedback.comment.revision()?;
        let key = feedback.key();
        Ok(self
            .responses
            .iter()
            .any(|response| response.created_at >= revision && response.targets.contains(&key)))
    }

    fn has_response_before(&self, feedback: &FeedbackItem) -> anyhow::Result<bool> {
        let revision = feedback.comment.revision()?;
        for response in &self.responses {
            if response.created_at >= revision {
                continue;
            }
            for target in &response.targets {
                if target.kind != feedback.kind {
                    continue;
                }
                for prior_feedback in &self.feedback_items {
                    if prior_feedback.key() == *target
                        && prior_feedback.comment.revision()? <= response.created_at
                    {
                        return Ok(true);
                    }
                }
            }
        }
        Ok(false)
    }
}

/// Extract typed REST database IDs from Factory's deliberately narrow GitHub
/// comment marker. Ordinary prose, malformed markers, unknown kinds, and
/// GraphQL node IDs are not response signals.
fn factory_feedback_targets(body: &str) -> impl Iterator<Item = FeedbackKey> + '_ {
    body.split("<!-- factory-feedback:")
        .skip(1)
        .filter_map(|tail| tail.split_once(" -->"))
        .filter_map(|(marker, _)| {
            let (kind, database_id) = marker.split_once(':')?;
            if database_id.contains(':') {
                return None;
            }
            let kind = kind.parse().ok()?;
            let database_id = database_id.parse().ok()?;
            (database_id > 0).then_some(FeedbackKey { kind, database_id })
        })
}

#[derive(Clone, Deserialize)]
struct Actor {
    login:     String,
    #[serde(rename = "__typename")]
    type_name: String,
}

impl Actor {
    fn is_bot(&self) -> bool {
        self.type_name == "Bot"
    }
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

#[derive(Deserialize)]
struct ReviewData {
    repository: Option<ReviewRepository>,
}

#[derive(Deserialize)]
struct ReviewRepository {
    #[serde(rename = "pullRequest")]
    pull_request: Option<ReviewPullRequest>,
}

#[derive(Deserialize)]
struct ReviewPullRequest {
    reviews: ReverseConnection<ReviewComment>,
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
                "viewer": { "login": "factory" },
                "repository": {
                    "pullRequest": {
                        "headRefOid": head,
                        "author": {
                            "login": "author",
                            "__typename": "User"
                        },
                        "reviewThreads": {
                            "nodes": threads.iter()
                                .enumerate()
                                .map(|(index, resolved)| serde_json::json!({
                                    "id": format!("thread-{index}"),
                                    "isResolved": resolved,
                                    "comments": {
                                        "nodes": [{
                                            "databaseId": 1_000 + index,
                                            "body": "",
                                            "author": {
                                                "login": "author",
                                                "__typename": "User"
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
                        "reviews": {
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
            "author": { "login": "author", "__typename": "User" },
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
        let page = ReadinessPage::try_from((page_data, "factory".to_string()))
            .expect("page should be valid");
        let mut accumulator = ReadinessAccumulator::default();
        accumulator.add_page(&page).expect("page should accumulate");

        let changed_page_data: PullRequestData = serde_json::from_value(serde_json::json!({
            "headRefOid": NEXT_HEAD,
            "author": { "login": "author", "__typename": "User" },
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
        let changed_page = ReadinessPage::try_from((changed_page_data, "factory".to_string()))
            .expect("changed page should be valid");

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
                "viewer": { "login": "factory" },
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "login": "author", "__typename": "User" },
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
                "viewer": { "login": "factory" },
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "login": "author", "__typename": "User" },
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
                "viewer": { "login": "factory" },
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "login": "author", "__typename": "User" },
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
                "viewer": { "login": "factory" },
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "login": "author", "__typename": "User" },
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
                "viewer": { "login": "factory" },
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "login": "author", "__typename": "User" },
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

    #[tokio::test]
    async fn collector_counts_mixed_threads_and_typed_top_level_feedback() {
        let body = serde_json::json!({
            "data": {
                "viewer": { "login": "factory" },
                "repository": {
                    "pullRequest": {
                        "headRefOid": HEAD,
                        "author": { "login": "author", "__typename": "User" },
                        "reviewThreads": {
                            "nodes": [
                                {
                                    "id": "handled-thread",
                                    "isResolved": false,
                                    "comments": {
                                        "nodes": [
                                            {
                                                "databaseId": 301,
                                                "body": "Please change this.",
                                                "createdAt": "2026-11-20T12:00:00Z",
                                                "updatedAt": "2026-11-20T12:00:00Z",
                                                "author": {
                                                    "login": "reviewer",
                                                    "__typename": "User"
                                                }
                                            },
                                            {
                                                "databaseId": 401,
                                                "body": "<!-- factory-feedback:inline:301 -->",
                                                "createdAt": "2026-11-20T12:01:00Z",
                                                "updatedAt": "2026-11-20T12:01:00Z",
                                                "author": {
                                                    "login": "factory",
                                                    "__typename": "Bot"
                                                }
                                            }
                                        ],
                                        "pageInfo": {
                                            "hasPreviousPage": false,
                                            "startCursor": null
                                        }
                                    }
                                },
                                {
                                    "id": "awaiting-thread",
                                    "isResolved": false,
                                    "comments": {
                                        "nodes": [{
                                            "databaseId": 302,
                                            "body": "One more concern.",
                                            "createdAt": "2026-11-20T12:00:00Z",
                                            "updatedAt": "2026-11-20T12:00:00Z",
                                            "author": {
                                                "login": "reviewer",
                                                "__typename": "User"
                                            }
                                        }],
                                        "pageInfo": {
                                            "hasPreviousPage": false,
                                            "startCursor": null
                                        }
                                    }
                                },
                                {
                                    "id": "resolved-thread",
                                    "isResolved": true,
                                    "comments": {
                                        "nodes": [{
                                            "databaseId": 303,
                                            "body": "Resolved without a reply.",
                                            "createdAt": "2026-11-20T12:00:00Z",
                                            "updatedAt": "2026-11-20T12:00:00Z",
                                            "author": {
                                                "login": "reviewer",
                                                "__typename": "User"
                                            }
                                        }],
                                        "pageInfo": {
                                            "hasPreviousPage": false,
                                            "startCursor": null
                                        }
                                    }
                                },
                                {
                                    "id": "noise-thread",
                                    "isResolved": false,
                                    "comments": {
                                        "nodes": [{
                                            "databaseId": 304,
                                            "body": "Automated note.",
                                            "createdAt": "2026-11-20T12:00:00Z",
                                            "updatedAt": "2026-11-20T12:00:00Z",
                                            "author": {
                                                "login": "dependabot",
                                                "__typename": "Bot"
                                            }
                                        }],
                                        "pageInfo": {
                                            "hasPreviousPage": false,
                                            "startCursor": null
                                        }
                                    }
                                }
                            ],
                            "pageInfo": { "hasNextPage": false, "endCursor": null }
                        },
                        "comments": {
                            "nodes": [
                                {
                                    "databaseId": 101,
                                    "body": "Top-level concern.",
                                    "createdAt": "2026-11-20T12:00:00Z",
                                    "updatedAt": "2026-11-20T12:00:00Z",
                                    "author": {
                                        "login": "reviewer",
                                        "__typename": "User"
                                    }
                                },
                                {
                                    "databaseId": 102,
                                    "body": "Handled concern.",
                                    "createdAt": "2026-11-20T12:00:00Z",
                                    "updatedAt": "2026-11-20T12:00:00Z",
                                    "author": {
                                        "login": "reviewer",
                                        "__typename": "User"
                                    }
                                },
                                {
                                    "databaseId": 501,
                                    "body": "<!-- factory-feedback:comment:102 --> \
                                             <!-- factory-feedback:review:101 -->",
                                    "createdAt": "2026-11-20T12:01:00Z",
                                    "updatedAt": "2026-11-20T12:01:00Z",
                                    "author": {
                                        "login": "author",
                                        "__typename": "User"
                                    }
                                }
                            ],
                            "pageInfo": {
                                "hasPreviousPage": false,
                                "startCursor": null
                            }
                        },
                        "reviews": {
                            "nodes": [
                                {
                                    "databaseId": 101,
                                    "body": "Review summary with the same REST ID.",
                                    "createdAt": "2026-11-20T12:00:00Z",
                                    "updatedAt": "2026-11-20T12:00:00Z",
                                    "author": {
                                        "login": "reviewer",
                                        "__typename": "User"
                                    }
                                },
                                {
                                    "databaseId": 103,
                                    "body": "",
                                    "author": {
                                        "login": "reviewer",
                                        "__typename": "User"
                                    }
                                }
                            ],
                            "pageInfo": {
                                "hasPreviousPage": false,
                                "startCursor": null
                            }
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
        let mock = MockHttpClient::new().on(HttpMethod::Post, "/graphql", 200, &body);

        let readiness = collect_readiness(&mock, "secret", "", &pull_request())
            .await
            .expect("mixed review evidence should load");

        assert_eq!(readiness.status, ReviewReadinessStatus::UnresolvedComments);
        assert_eq!(readiness.unresolved_thread_count, Some(3));
        assert_eq!(readiness.feedback_awaiting_response_count, Some(2));
        assert_eq!(mock.request_count(), 3);
    }

    #[tokio::test]
    async fn collector_follows_every_successful_pagination_connection() {
        let readiness_page = |has_next_page: bool, thread_nodes: serde_json::Value| {
            serde_json::json!({
                "data": {
                    "viewer": { "login": "factory" },
                    "repository": {
                        "pullRequest": {
                            "headRefOid": HEAD,
                            "author": { "login": "author", "__typename": "User" },
                            "reviewThreads": {
                                "nodes": thread_nodes,
                                "pageInfo": {
                                    "hasNextPage": has_next_page,
                                    "endCursor": has_next_page.then_some("threads-2")
                                }
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
            .to_string()
        };
        let first_readiness_page = readiness_page(true, serde_json::json!([]));
        let second_readiness_page = readiness_page(
            false,
            serde_json::json!([{
                "id": "thread-1",
                "isResolved": false,
                "comments": {
                    "nodes": [{
                        "databaseId": 401,
                        "body": "<!-- factory-feedback:inline:301 -->",
                        "createdAt": "2026-11-20T12:01:00Z",
                        "updatedAt": "2026-11-20T12:01:00Z",
                        "author": { "login": "factory", "__typename": "Bot" }
                    }],
                    "pageInfo": {
                        "hasPreviousPage": true,
                        "startCursor": "inline-previous"
                    }
                }
            }]),
        );
        let previous_inline_page = serde_json::json!({
            "data": {
                "node": {
                    "comments": {
                        "nodes": [{
                            "databaseId": 301,
                            "body": "Inline concern.",
                            "createdAt": "2026-11-20T12:00:00Z",
                            "updatedAt": "2026-11-20T12:00:00Z",
                            "author": { "login": "reviewer", "__typename": "User" }
                        }],
                        "pageInfo": {
                            "hasPreviousPage": false,
                            "startCursor": null
                        }
                    }
                }
            }
        })
        .to_string();
        let latest_comment_page = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "comments": {
                            "nodes": [{
                                "databaseId": 501,
                                "body": "<!-- factory-feedback:comment:101 -->",
                                "createdAt": "2026-11-20T12:01:00Z",
                                "updatedAt": "2026-11-20T12:01:00Z",
                                "author": { "login": "author", "__typename": "User" }
                            }],
                            "pageInfo": {
                                "hasPreviousPage": true,
                                "startCursor": "comment-previous"
                            }
                        }
                    }
                }
            }
        })
        .to_string();
        let previous_comment_page = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "comments": {
                            "nodes": [{
                                "databaseId": 101,
                                "body": "Top-level concern.",
                                "createdAt": "2026-11-20T12:00:00Z",
                                "updatedAt": "2026-11-20T12:00:00Z",
                                "author": { "login": "reviewer", "__typename": "User" }
                            }],
                            "pageInfo": {
                                "hasPreviousPage": false,
                                "startCursor": null
                            }
                        }
                    }
                }
            }
        })
        .to_string();
        let latest_review_page = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviews": {
                            "nodes": [{
                                "databaseId": 601,
                                "body": "<!-- factory-feedback:review:201 -->",
                                "createdAt": "2026-11-20T12:01:00Z",
                                "updatedAt": "2026-11-20T12:01:00Z",
                                "author": { "login": "factory", "__typename": "Bot" }
                            }],
                            "pageInfo": {
                                "hasPreviousPage": true,
                                "startCursor": "review-previous"
                            }
                        }
                    }
                }
            }
        })
        .to_string();
        let previous_review_page = serde_json::json!({
            "data": {
                "repository": {
                    "pullRequest": {
                        "reviews": {
                            "nodes": [{
                                "databaseId": 201,
                                "body": "Review summary.",
                                "createdAt": "2026-11-20T12:00:00Z",
                                "updatedAt": "2026-11-20T12:00:00Z",
                                "author": { "login": "reviewer", "__typename": "User" }
                            }],
                            "pageInfo": {
                                "hasPreviousPage": false,
                                "startCursor": null
                            }
                        }
                    }
                }
            }
        })
        .to_string();
        let mock = MockHttpClient::new()
            .on_once(HttpMethod::Post, "/graphql", 200, &first_readiness_page)
            .on_once(HttpMethod::Post, "/graphql", 200, &second_readiness_page)
            .on_once(HttpMethod::Post, "/graphql", 200, &previous_inline_page)
            .on_once(HttpMethod::Post, "/graphql", 200, &latest_comment_page)
            .on_once(HttpMethod::Post, "/graphql", 200, &previous_comment_page)
            .on_once(HttpMethod::Post, "/graphql", 200, &latest_review_page)
            .on_once(HttpMethod::Post, "/graphql", 200, &previous_review_page);

        let readiness = collect_readiness(&mock, "secret", "", &pull_request())
            .await
            .expect("paginated review evidence should load");

        assert_eq!(readiness.unresolved_thread_count, Some(1));
        assert_eq!(readiness.feedback_awaiting_response_count, Some(0));
        assert_eq!(mock.request_count(), 7);
    }

    fn comment(database_id: u64, login: &str, type_name: &str, body: &str) -> ReviewComment {
        comment_at(
            database_id,
            login,
            type_name,
            body,
            "2026-11-20T12:00:00Z",
            "2026-11-20T12:00:00Z",
        )
    }

    fn comment_at(
        database_id: u64,
        login: &str,
        type_name: &str,
        body: &str,
        created_at: &str,
        updated_at: &str,
    ) -> ReviewComment {
        ReviewComment {
            database_id,
            body: body.to_string(),
            created_at: Some(created_at.parse().expect("valid creation timestamp")),
            updated_at: Some(updated_at.parse().expect("valid revision timestamp")),
            author: Some(Actor {
                login:     login.to_string(),
                type_name: type_name.to_string(),
            }),
        }
    }

    fn response_identities() -> ResponseIdentities {
        ResponseIdentities {
            pull_request_author_login: "author".to_string(),
            factory_login:             "factory".to_string(),
        }
    }

    #[test]
    fn unrelated_author_comment_does_not_handle_top_level_feedback() {
        let comments = [
            comment(101, "reviewer", "User", "Please fix this."),
            comment(201, "author", "User", "I updated the README."),
        ];
        let mut history = FeedbackHistory::default();

        record_feedback_items(
            &comments,
            &response_identities(),
            FeedbackKind::Comment,
            &mut history,
        )
        .expect("feedback should classify");

        assert_eq!(history.outstanding_count().unwrap(), 1);
    }

    #[test]
    fn author_and_factory_markers_handle_only_exact_typed_feedback_items() {
        let comments = [
            comment(101, "reviewer", "User", "First concern."),
            comment(102, "reviewer", "User", "Second concern."),
            comment(
                201,
                "factory",
                "Bot",
                "<!-- factory-feedback:comment:101 -->",
            ),
            comment(
                202,
                "author",
                "User",
                "<!-- factory-feedback:review:102 -->",
            ),
        ];
        let mut history = FeedbackHistory::default();

        record_feedback_items(
            &comments,
            &response_identities(),
            FeedbackKind::Comment,
            &mut history,
        )
        .expect("feedback should classify");

        assert_eq!(history.outstanding_count().unwrap(), 1);
        assert!(history.responses[1].targets.contains(&FeedbackKey {
            kind:        FeedbackKind::Review,
            database_id: 102,
        }));
    }

    #[test]
    fn malformed_unknown_and_graphql_id_markers_are_ignored() {
        let targets = factory_feedback_targets(
            "<!-- factory-feedback:inline:501 --> \
             <!-- factory-feedback:thread:502 --> \
             <!-- factory-feedback:comment:MDExOlB1bGxSZXF1ZXN0 --> \
             <!-- factory-feedback:review:0 --> \
             <!-- factory-feedback:comment:503:extra -->",
        )
        .collect::<Vec<_>>();

        assert_eq!(targets, [FeedbackKey {
            kind:        FeedbackKind::Inline,
            database_id: 501,
        }]);
    }

    #[test]
    fn substantive_automated_feedback_counts_while_routine_status_noise_does_not() {
        let comments = [
            comment(
                101,
                "code-review-bot",
                "Bot",
                "This branch still has a SQL injection risk.",
            ),
            comment(
                102,
                "code-review-bot",
                "Bot",
                "The authorization check is missing.",
            ),
            comment(103, "status-bot", "Bot", "✅ All checks have passed."),
            comment(104, "reviewer", "User", ""),
            comment(
                105,
                "factory",
                "Bot",
                "<!-- factory-feedback:review:102 -->",
            ),
            comment(106, "author", "User", "Self activity."),
        ];
        let mut inline_history = FeedbackHistory::default();
        record_feedback_items(
            &comments,
            &response_identities(),
            FeedbackKind::Inline,
            &mut inline_history,
        )
        .expect("feedback should classify");
        assert_eq!(inline_history.feedback_items.len(), 2);

        let reviews = [
            comment(
                201,
                "code-review-bot",
                "Bot",
                "This automation review found a regression.",
            ),
            comment(102, "author", "User", "self"),
        ];
        let mut review_history = FeedbackHistory::default();

        record_feedback_items(
            &reviews,
            &response_identities(),
            FeedbackKind::Review,
            &mut review_history,
        )
        .expect("feedback should classify");

        assert_eq!(review_history.outstanding_count().unwrap(), 1);
    }

    #[test]
    fn generic_bot_status_is_noise_but_source_specific_bot_feedback_counts() {
        let comments = [
            comment(101, "dependabot", "Bot", "Automated note."),
            comment(
                102,
                "code-review-bot",
                "Bot",
                "src/auth.rs accepts the token before it validates its signature.",
            ),
            comment(103, "ci-bot", "Bot", "Deployment status: complete."),
        ];
        let mut history = FeedbackHistory::default();

        record_feedback_items(
            &comments,
            &response_identities(),
            FeedbackKind::Inline,
            &mut history,
        )
        .expect("bot activity should classify");

        assert_eq!(history.feedback_items.len(), 1);
        assert_eq!(history.feedback_items[0].key().database_id, 102);
    }

    #[test]
    fn bot_review_classification_ignores_status_urls_and_markup_but_keeps_visible_findings() {
        let comments = [
            comment(
                101,
                "deploy-bot",
                "Bot",
                "Deployment status: complete. See https://ci.example.test/runs/42/issues/failed.",
            ),
            comment(
                102,
                "review-bot",
                "Bot",
                "No findings. Full report: https://review.example.test/pull/42/issue/1",
            ),
            comment(
                103,
                "review-bot",
                "Bot",
                "`scripts/review/check.sh` should run after deployment.",
            ),
            comment(
                104,
                "review-bot",
                "Bot",
                "<!-- review metadata: {\"status\":\"complete\"} -->\n\
                 ## [P1] Missing authorization\n\
                 `src/auth.rs` accepts the token without validating its signature.",
            ),
            comment(
                105,
                "review-bot",
                "Bot",
                "<!-- generated by CodeRabbit -->\n\
                 ### Security finding\n\
                 The authorization check is missing before this request is processed.",
            ),
            comment(
                106,
                "review-bot",
                "Bot",
                "<!-- review metadata: {\"status\":\"complete\"} -->\n\
                 Review completed: [P1] the authorization check is missing.",
            ),
            comment(
                107,
                "review-bot",
                "Bot",
                "No findings in the previous run; this change permits SQL injection.",
            ),
            comment(
                108,
                "review-bot",
                "Bot",
                "<!-- review metadata: {\"status\":\"complete\"} -->\n\
                 Review completed: no findings.",
            ),
            comment(
                109,
                "review-bot",
                "Bot",
                "No findings in the previous run; deployment succeeded.",
            ),
            comment(
                110,
                "review-bot",
                "Bot",
                "<!-- metadata --> Review completed: [P1] SQL injection in query construction.",
            ),
            comment(
                111,
                "review-bot",
                "Bot",
                "No findings in the previous run; [P1] path traversal in archive extraction.",
            ),
            comment(
                112,
                "review-bot",
                "Bot",
                "<!-- metadata --> Review completed: security scan passed.",
            ),
            comment(
                113,
                "review-bot",
                "Bot",
                "No findings in this run; path traversal scan completed successfully.",
            ),
            comment(
                114,
                "review-bot",
                "Bot",
                "Analysis completed: no security vulnerabilities were found.",
            ),
            comment(
                115,
                "review-bot",
                "Bot",
                "No findings in the previous run\n[P1] unsafe archive extraction.",
            ),
            comment(
                116,
                "review-bot",
                "Bot",
                "No findings in the previous run, but [P1] SQL injection in query construction.",
            ),
            comment(
                117,
                "review-bot",
                "Bot",
                "Security scan passed, but [P1] path traversal remains in archive extraction.",
            ),
            comment(
                118,
                "review-bot",
                "Bot",
                "No findings in the previous run, and deployment succeeded.",
            ),
            comment(
                119,
                "review-bot",
                "Bot",
                "Security scan passed, and deployment completed.",
            ),
            comment(
                120,
                "review-bot",
                "Bot",
                "Security scan passed: [P1] path traversal remains in archive extraction.",
            ),
            comment(
                121,
                "review-bot",
                "Bot",
                "No findings in the previous run, however SQL injection remains in query construction.",
            ),
            comment(
                122,
                "review-bot",
                "Bot",
                "Security scan passed, although path traversal remains in archive extraction.",
            ),
            comment(
                123,
                "review-bot",
                "Bot",
                "Security scan passed ([P1] SQL injection remains in query construction).",
            ),
            comment(
                124,
                "review-bot",
                "Bot",
                "Security scan passed\n- [P1] path traversal remains in archive extraction",
            ),
            comment(
                125,
                "review-bot",
                "Bot",
                "Security scan passed [P1] SQL injection remains in query construction",
            ),
            comment(
                126,
                "review-bot",
                "Bot",
                "No SQL injection vulnerabilities were found.",
            ),
            comment(
                127,
                "review-bot",
                "Bot",
                "Path traversal scan completed successfully.",
            ),
        ];
        let mut history = FeedbackHistory::default();

        record_feedback_items(
            &comments,
            &response_identities(),
            FeedbackKind::Comment,
            &mut history,
        )
        .expect("bot review comments should classify");

        assert_eq!(
            history
                .feedback_items
                .iter()
                .map(|item| item.key().database_id)
                .collect::<Vec<_>>(),
            [
                104, 105, 106, 107, 110, 111, 115, 116, 117, 120, 121, 122, 123, 124, 125,
            ]
        );
    }

    #[test]
    fn routine_status_classifier_requires_the_entire_message_to_be_routine() {
        for routine_status in [
            "No findings.",
            "No findings. Full report: https://review.example.test/pull/42",
            "No SQL injection vulnerabilities were found.",
            "Analysis completed: no security vulnerabilities were found.",
            "Path traversal scan completed successfully.",
            "Deployment status: complete. See https://ci.example.test/runs/42",
        ] {
            assert!(
                is_routine_status_message(
                    &visible_review_text(routine_status).to_ascii_lowercase()
                ),
                "{routine_status:?} should be routine status noise"
            );
        }

        for finding in [
            "No findings however SQL injection vulnerability remains in query construction.",
            "No findings SQL injection vulnerability remains in query construction.",
            "No findings, but [P1] SQL injection vulnerability remains in query construction.",
            "Security scan passed, however the authorization check is missing.",
            "Security scan passed. The path traversal vulnerability remains in archive extraction.",
        ] {
            assert!(
                !is_routine_status_message(&visible_review_text(finding).to_ascii_lowercase()),
                "{finding:?} must remain visible as review feedback"
            );
            assert!(
                has_substantive_code_review_signal(finding),
                "{finding:?} should count as review feedback"
            );
        }
    }

    #[tokio::test]
    async fn resolved_thread_handles_feedback_without_a_marker() {
        let thread = ReviewThread {
            id:          "thread-1".to_string(),
            is_resolved: true,
            comments:    ReverseConnection {
                nodes:     vec![comment(301, "reviewer", "User", "Please change this.")],
                page_info: ReversePageInfo {
                    has_previous_page: false,
                    start_cursor:      None,
                },
            },
        };

        let awaits_response = thread_awaits_response(
            &MockHttpClient::new(),
            "secret",
            "",
            &thread,
            &response_identities(),
        )
        .await
        .expect("resolved thread should be handled");

        assert!(!awaits_response);
    }

    #[tokio::test]
    async fn reviewer_follow_up_after_factory_reply_awaits_another_response() {
        let comments = vec![
            comment_at(
                301,
                "reviewer",
                "User",
                "Please change this.",
                "2026-11-20T12:00:00Z",
                "2026-11-20T12:00:00Z",
            ),
            comment_at(
                401,
                "factory",
                "Bot",
                "Done. <!-- factory-feedback:inline:301 -->",
                "2026-11-20T12:01:00Z",
                "2026-11-20T12:01:00Z",
            ),
        ];
        let mut thread = ReviewThread {
            id:          "thread-1".to_string(),
            is_resolved: false,
            comments:    ReverseConnection {
                nodes:     comments,
                page_info: ReversePageInfo {
                    has_previous_page: false,
                    start_cursor:      None,
                },
            },
        };

        let handled = thread_awaits_response(
            &MockHttpClient::new(),
            "secret",
            "",
            &thread,
            &response_identities(),
        )
        .await
        .expect("thread should load");
        thread.comments.nodes.push(comment_at(
            302,
            "reviewer",
            "User",
            "One more thing.",
            "2026-11-20T12:02:00Z",
            "2026-11-20T12:02:00Z",
        ));
        let followed_up = thread_awaits_response(
            &MockHttpClient::new(),
            "secret",
            "",
            &thread,
            &response_identities(),
        )
        .await
        .expect("thread should load");

        assert!(!handled);
        assert!(followed_up);
    }

    #[tokio::test]
    async fn resolved_thread_reopens_after_a_factory_response_and_reviewer_follow_up() {
        let thread = ReviewThread {
            id:          "thread-1".to_string(),
            is_resolved: true,
            comments:    ReverseConnection {
                nodes:     vec![
                    comment_at(
                        301,
                        "reviewer",
                        "User",
                        "Please change this.",
                        "2026-11-20T12:00:00Z",
                        "2026-11-20T12:00:00Z",
                    ),
                    comment_at(
                        401,
                        "factory",
                        "Bot",
                        "<!-- factory-feedback:inline:301 -->",
                        "2026-11-20T12:01:00Z",
                        "2026-11-20T12:01:00Z",
                    ),
                    comment_at(
                        302,
                        "reviewer",
                        "User",
                        "One more thing.",
                        "2026-11-20T12:02:00Z",
                        "2026-11-20T12:02:00Z",
                    ),
                ],
                page_info: ReversePageInfo {
                    has_previous_page: false,
                    start_cursor:      None,
                },
            },
        };

        assert!(
            thread_awaits_response(
                &MockHttpClient::new(),
                "secret",
                "",
                &thread,
                &response_identities(),
            )
            .await
            .expect("resolved follow-up should load")
        );
    }

    #[tokio::test]
    async fn resolved_thread_does_not_reopen_for_an_unrelated_recognized_marker() {
        let thread = ReviewThread {
            id:          "thread-1".to_string(),
            is_resolved: true,
            comments:    ReverseConnection {
                nodes:     vec![
                    comment_at(
                        301,
                        "reviewer",
                        "User",
                        "Please change this.",
                        "2026-11-20T12:00:00Z",
                        "2026-11-20T12:00:00Z",
                    ),
                    comment_at(
                        401,
                        "factory",
                        "Bot",
                        "<!-- factory-feedback:inline:999 -->",
                        "2026-11-20T12:01:00Z",
                        "2026-11-20T12:01:00Z",
                    ),
                    comment_at(
                        302,
                        "reviewer",
                        "User",
                        "One more thing.",
                        "2026-11-20T12:02:00Z",
                        "2026-11-20T12:02:00Z",
                    ),
                ],
                page_info: ReversePageInfo {
                    has_previous_page: false,
                    start_cursor:      None,
                },
            },
        };

        assert!(
            !thread_awaits_response(
                &MockHttpClient::new(),
                "secret",
                "",
                &thread,
                &response_identities(),
            )
            .await
            .expect("resolved thread should load")
        );
    }

    #[tokio::test]
    async fn latest_inline_feedback_is_the_only_current_thread_obligation() {
        let thread = ReviewThread {
            id:          "thread-1".to_string(),
            is_resolved: false,
            comments:    ReverseConnection {
                nodes:     vec![
                    comment_at(
                        301,
                        "reviewer-a",
                        "User",
                        "First concern.",
                        "2026-11-20T12:00:00Z",
                        "2026-11-20T12:00:00Z",
                    ),
                    comment_at(
                        302,
                        "reviewer-b",
                        "User",
                        "Current concern.",
                        "2026-11-20T12:01:00Z",
                        "2026-11-20T12:01:00Z",
                    ),
                    comment_at(
                        401,
                        "factory",
                        "Bot",
                        "<!-- factory-feedback:inline:302 -->",
                        "2026-11-20T12:02:00Z",
                        "2026-11-20T12:02:00Z",
                    ),
                ],
                page_info: ReversePageInfo {
                    has_previous_page: false,
                    start_cursor:      None,
                },
            },
        };

        assert!(
            !thread_awaits_response(
                &MockHttpClient::new(),
                "secret",
                "",
                &thread,
                &response_identities(),
            )
            .await
            .expect("latest inline feedback should load")
        );
    }

    #[test]
    fn marker_responses_only_handle_the_feedback_revision_they_follow() {
        for kind in [
            FeedbackKind::Comment,
            FeedbackKind::Inline,
            FeedbackKind::Review,
        ] {
            let marker_kind = match kind {
                FeedbackKind::Comment => "comment",
                FeedbackKind::Inline => "inline",
                FeedbackKind::Review => "review",
            };
            let feedback = comment_at(
                301,
                "reviewer",
                "User",
                "Please change this.",
                "2026-11-20T12:00:00Z",
                "2026-11-20T12:00:00Z",
            );
            let response = comment_at(
                401,
                "factory",
                "Bot",
                &format!("<!-- factory-feedback:{marker_kind}:301 -->"),
                "2026-11-20T12:01:00Z",
                "2026-11-20T12:01:00Z",
            );
            let mut history = FeedbackHistory::default();
            record_feedback_items(
                &[feedback.clone(), response],
                &response_identities(),
                kind,
                &mut history,
            )
            .expect("initial response should classify");
            assert_eq!(history.outstanding_count().unwrap(), 0);

            let edited = comment_at(
                301,
                "reviewer",
                "User",
                "Please change this, with more detail.",
                "2026-11-20T12:00:00Z",
                "2026-11-20T12:02:00Z",
            );
            let newer_response = comment_at(
                402,
                "factory",
                "Bot",
                &format!("<!-- factory-feedback:{marker_kind}:301 -->"),
                "2026-11-20T12:03:00Z",
                "2026-11-20T12:03:00Z",
            );
            let mut edited_history = FeedbackHistory::default();
            record_feedback_items(
                &[edited.clone()],
                &response_identities(),
                kind,
                &mut edited_history,
            )
            .expect("edited feedback should classify");
            edited_history.responses.push(FactoryResponse {
                created_at: "2026-11-20T12:01:00Z"
                    .parse()
                    .expect("valid response timestamp"),
                targets:    vec![FeedbackKey {
                    kind,
                    database_id: 301,
                }],
            });
            assert_eq!(edited_history.outstanding_count().unwrap(), 1);

            record_feedback_items(
                &[newer_response],
                &response_identities(),
                kind,
                &mut edited_history,
            )
            .expect("newer response should classify");
            assert_eq!(edited_history.outstanding_count().unwrap(), 0);
        }
    }
}
