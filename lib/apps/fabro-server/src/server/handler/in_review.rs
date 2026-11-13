use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use fabro_types::{AuthMethod, PullRequestLink, Run, RunId};

use super::super::{
    ApiError, AppState, InReviewPullRequest, InReviewPullRequestListMeta,
    InReviewPullRequestListResponse, InReviewPullRequestListStatus, IntoResponse, Json, Response,
    Router, State, StatusCode, get, warn,
};
use crate::principal_middleware::RequiredUser;

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route("/runs/in-review", get(list_in_review_pull_requests))
}

async fn list_in_review_pull_requests(
    RequiredUser(user): RequiredUser,
    State(state): State<Arc<AppState>>,
) -> Response {
    let creds = match super::pull_requests::load_server_github_credentials(&state).await {
        Ok(creds) => creds,
        Err(err) => return err.into_response(),
    };
    let github = match super::pull_requests::server_github_context(&state, &creds) {
        Ok(github) => github,
        Err(err) => return err.into_response(),
    };
    let signed_in_github_login =
        (user.auth_method == AuthMethod::Github).then_some(user.login.as_str());
    let mut search =
        match fabro_github::search_open_authored_pull_requests(&github, signed_in_github_login)
            .await
        {
            Ok(search) => search,
            Err(err) => {
                warn!(error = %err, "Failed to list authored GitHub pull requests");
                return ApiError::with_code(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "Open GitHub pull requests are temporarily unavailable.",
                    "github_pull_requests_unavailable",
                )
                .into_response();
            }
        };

    let runs = match state
        .stores
        .run_summaries
        .list_all(chrono::Utc::now())
        .await
    {
        Ok(runs) => runs,
        Err(err) => {
            warn!(error = %err, "Failed to associate runs with authored pull requests");
            search.partial = true;
            search.warning.get_or_insert_with(|| {
                "Pull requests loaded, but Fabro run links are temporarily unavailable.".to_string()
            });
            Vec::new()
        }
    };
    let run_ids = associated_run_ids(&runs);
    let data = search
        .items
        .into_iter()
        .map(|item| {
            let key = (item.owner.clone(), item.repo.clone(), item.number.get());
            InReviewPullRequest {
                owner:      item.owner,
                repo:       item.repo,
                number:     item.number,
                title:      item.title,
                html_url:   item.html_url,
                draft:      item.draft,
                updated_at: item.updated_at,
                run_ids:    run_ids.get(&key).cloned().unwrap_or_default(),
            }
        })
        .collect();
    let status = if search.partial {
        InReviewPullRequestListStatus::Partial
    } else {
        InReviewPullRequestListStatus::Complete
    };

    Json(InReviewPullRequestListResponse {
        data,
        meta: InReviewPullRequestListMeta {
            author: search.author,
            status,
            warning: search.warning,
        },
    })
    .into_response()
}

fn associated_run_ids(runs: &[Run]) -> HashMap<(String, String, u64), Vec<String>> {
    associated_run_ids_for_links(
        runs.iter()
            .filter_map(|run| run.pull_request.as_ref().map(|link| (&run.id, link))),
    )
}

fn associated_run_ids_for_links<'a>(
    links: impl IntoIterator<Item = (&'a RunId, &'a PullRequestLink)>,
) -> HashMap<(String, String, u64), Vec<String>> {
    let mut associations = HashMap::<_, Vec<String>>::new();
    let mut seen = HashSet::<((String, String, u64), RunId)>::new();
    for (
        run_id,
        PullRequestLink {
            owner,
            repo,
            number,
        },
    ) in links
    {
        let key = (owner.clone(), repo.clone(), *number);
        if seen.insert((key.clone(), *run_id)) {
            associations
                .entry(key)
                .or_default()
                .push(run_id.to_string());
        }
    }
    associations
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn associates_multiple_runs_with_one_pull_request_without_duplicate_links() {
        let first = RunId::new();
        let second = RunId::new();
        let pull_request = PullRequestLink {
            owner:  "acme".to_string(),
            repo:   "widgets".to_string(),
            number: 42,
        };

        let associations = associated_run_ids_for_links([
            (&first, &pull_request),
            (&first, &pull_request),
            (&second, &pull_request),
        ]);

        assert_eq!(
            associations.get(&("acme".to_string(), "widgets".to_string(), 42)),
            Some(&vec![first.to_string(), second.to_string()])
        );
    }
}
