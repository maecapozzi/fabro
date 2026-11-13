use std::sync::Arc;

use fabro_types::AuthMethod;
use serde::Deserialize;

use super::super::{
    ApiError, AppState, IntoResponse, Json, Query, RequiredUser, Response, ReviewRequest,
    ReviewRequestKind, ReviewRequestListMeta, ReviewRequestListResponse, ReviewRequestTeamCoverage,
    Router, State, StatusCode, Utc, get,
};

#[derive(Deserialize)]
struct ReviewRequestParams {
    reviewer: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
enum ReviewerSelectionError {
    Required,
    Invalid,
}

fn selected_reviewer(
    user: &fabro_types::UserPrincipal,
    explicit: Option<&str>,
) -> Result<String, ReviewerSelectionError> {
    if user.auth_method == AuthMethod::Github {
        return Ok(user.login.clone());
    }
    let explicit = explicit.ok_or(ReviewerSelectionError::Required)?;
    fabro_github::review_queue::validate_reviewer(explicit)
        .map(str::to_string)
        .map_err(|_| ReviewerSelectionError::Invalid)
}

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/review-requests", get(list_review_requests))
}

async fn list_review_requests(
    RequiredUser(user): RequiredUser,
    State(state): State<Arc<AppState>>,
    Query(params): Query<ReviewRequestParams>,
) -> Response {
    let reviewer = match selected_reviewer(&user, params.reviewer.as_deref()) {
        Ok(reviewer) => reviewer,
        Err(ReviewerSelectionError::Required) => {
            return ApiError::with_code(
                StatusCode::BAD_REQUEST,
                "Enter the GitHub login whose review requests should be shown.",
                "github_reviewer_required",
            )
            .into_response();
        }
        Err(ReviewerSelectionError::Invalid) => {
            return ApiError::with_code(
                StatusCode::BAD_REQUEST,
                "Reviewer must be a valid GitHub login of 1 to 39 letters, numbers, or hyphens.",
                "github_reviewer_invalid",
            )
            .into_response();
        }
    };

    let settings = state.server_settings();
    if !settings.server.integrations.github.enabled {
        return ApiError::with_code(
            StatusCode::SERVICE_UNAVAILABLE,
            "GitHub integration is not enabled.",
            "github_not_configured",
        )
        .into_response();
    }
    let credentials = match state
        .github_credentials(&settings.server.integrations.github)
        .await
    {
        Ok(Some(credentials)) => credentials,
        Ok(None) => {
            return ApiError::with_code(
                StatusCode::SERVICE_UNAVAILABLE,
                "GitHub credentials are not configured.",
                "github_credentials_unavailable",
            )
            .into_response();
        }
        Err(error) => {
            tracing::error!(error = ?error, "Failed to load GitHub credentials for review queue");
            return ApiError::with_code(
                StatusCode::SERVICE_UNAVAILABLE,
                "GitHub credentials are unavailable.",
                "github_credentials_unavailable",
            )
            .into_response();
        }
    };
    let github = fabro_github::GitHubContext::new(&credentials, &state.github_api_base_url);
    let queue = match fabro_github::review_queue::review_queue(&github, &reviewer).await {
        Ok(queue) => queue,
        Err(fabro_github::ReviewQueueError::Authentication) => {
            return ApiError::with_code(
                StatusCode::SERVICE_UNAVAILABLE,
                "GitHub rejected the configured credentials.",
                "github_authentication_failed",
            )
            .into_response();
        }
        Err(fabro_github::ReviewQueueError::RateLimited) => {
            return ApiError::with_code(
                StatusCode::TOO_MANY_REQUESTS,
                "GitHub rate limited the review queue. Try again later.",
                "github_rate_limited",
            )
            .into_response();
        }
        Err(fabro_github::ReviewQueueError::Access) => {
            return ApiError::with_code(
                StatusCode::FORBIDDEN,
                "The configured GitHub connection cannot read review requests.",
                "github_review_requests_forbidden",
            )
            .into_response();
        }
        Err(error) => {
            tracing::error!(error = ?error, reviewer, "Failed to load GitHub review queue");
            return ApiError::with_code(
                StatusCode::BAD_GATEWAY,
                "GitHub review requests could not be loaded.",
                "github_review_requests_failed",
            )
            .into_response();
        }
    };

    Json(ReviewRequestListResponse {
        data: queue
            .items
            .into_iter()
            .map(|item| ReviewRequest {
                owner:        item.owner,
                repository:   item.repository,
                number:       i64::try_from(item.number)
                    .expect("GitHub pull request number should fit in i64"),
                title:        item.title,
                author:       item.author,
                html_url:     item.html_url,
                draft:        item.draft,
                request_kind: match item.request_kind {
                    fabro_github::ReviewRequestKind::Direct => ReviewRequestKind::Direct,
                    fabro_github::ReviewRequestKind::Team => ReviewRequestKind::Team,
                    fabro_github::ReviewRequestKind::DirectAndTeam => {
                        ReviewRequestKind::DirectAndTeam
                    }
                },
                teams:        item.teams,
                updated_at:   item.updated_at,
            })
            .collect(),
        meta: ReviewRequestListMeta {
            reviewer:      queue.reviewer,
            fetched_at:    Utc::now(),
            incomplete:    queue.incomplete,
            team_coverage: match queue.team_coverage {
                fabro_github::TeamCoverage::Complete => ReviewRequestTeamCoverage::Complete,
                fabro_github::TeamCoverage::Limited => ReviewRequestTeamCoverage::Limited,
            },
        },
    })
    .into_response()
}

#[cfg(test)]
mod tests {
    use fabro_types::{AuthMethod, IdpIdentity, Principal};

    use super::{ReviewerSelectionError, selected_reviewer};

    fn user(login: &str, auth_method: AuthMethod) -> fabro_types::UserPrincipal {
        let Principal::User(user) = Principal::user(
            IdpIdentity::new("https://issuer.example", login)
                .expect("test identity should be valid"),
            login.to_string(),
            auth_method,
        ) else {
            unreachable!("user constructor should return a user principal");
        };
        user
    }

    #[test]
    fn github_session_uses_its_own_login_and_ignores_an_explicit_override() {
        assert_eq!(
            selected_reviewer(&user("maecapozzi", AuthMethod::Github), None).as_deref(),
            Ok("maecapozzi")
        );
        assert_eq!(
            selected_reviewer(
                &user("maecapozzi", AuthMethod::Github),
                Some("another-user")
            )
            .as_deref(),
            Ok("maecapozzi")
        );
    }

    #[test]
    fn non_github_session_requires_an_explicit_reviewer() {
        let user = user("operator", AuthMethod::DevToken);
        assert_eq!(
            selected_reviewer(&user, None),
            Err(ReviewerSelectionError::Required)
        );
        assert_eq!(
            selected_reviewer(&user, Some("  maecapozzi ")).as_deref(),
            Ok("maecapozzi")
        );
    }

    #[test]
    fn non_github_session_rejects_invalid_explicit_logins() {
        let user = user("operator", AuthMethod::DevToken);
        for reviewer in ["", "owner/name", "mae--capozzi", &"a".repeat(40)] {
            assert_eq!(
                selected_reviewer(&user, Some(reviewer)),
                Err(ReviewerSelectionError::Invalid)
            );
        }
    }
}
