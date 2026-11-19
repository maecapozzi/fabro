use std::sync::Arc;

use axum::extract::DefaultBodyLimit;
use axum::extract::rejection::JsonRejection;
use fabro_api::types::{
    CreateWorkflowVersionResponse, WorkflowVersion, WorkflowVersionId,
    WorkflowVersionLookupResponse,
};
use fabro_types::MAX_WORKFLOW_VERSION_BYTES;
use fabro_util::error;
use fabro_workflow_version::{
    ValidatedWorkflowVersion, WorkflowVersionStore, WorkflowVersionStoreError,
};
use serde::Deserialize;

use super::super::{
    ApiError, AppState, IntoResponse, Json, Query, Response, Router, State, StatusCode, get, post,
};
use crate::principal_middleware::RequiredRunManagementActor;

const INVALID_JSON_CODE: &str = "invalid_json";
const INVALID_VERSION_CODE: &str = "workflow_version_invalid";
const DEPENDENCY_NOT_FOUND_CODE: &str = "workflow_version_dependency_not_found";
const VERSION_TOO_LARGE_CODE: &str = "workflow_version_too_large";

pub(super) fn routes() -> Router<Arc<AppState>> {
    Router::new().route(
        "/workflow-versions",
        get(lookup_workflow_version).post(
            post(create_workflow_version).layer(DefaultBodyLimit::max(MAX_WORKFLOW_VERSION_BYTES)),
        ),
    )
}

#[derive(Deserialize)]
struct WorkflowVersionLookupQuery {
    workflow_version_id: WorkflowVersionId,
}

async fn lookup_workflow_version(
    _auth: RequiredRunManagementActor,
    State(state): State<Arc<AppState>>,
    Query(WorkflowVersionLookupQuery {
        workflow_version_id,
    }): Query<WorkflowVersionLookupQuery>,
) -> Result<Response, ApiError> {
    let store = WorkflowVersionStore::new(state.store_ref().blobs());
    let Some(_) = store
        .get_closure(&workflow_version_id)
        .await
        .map_err(store_error)?
    else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "workflow version was not found",
        ));
    };

    Ok(Json(WorkflowVersionLookupResponse {
        workflow_version_id,
    })
    .into_response())
}

async fn create_workflow_version(
    _auth: RequiredRunManagementActor,
    State(state): State<Arc<AppState>>,
    payload: Result<Json<WorkflowVersion>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(version) = payload.map_err(json_rejection)?;
    let version = ValidatedWorkflowVersion::new(version).map_err(|err| {
        ApiError::with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            err.to_string(),
            INVALID_VERSION_CODE,
        )
    })?;
    let blobs = state.store_ref().blobs();
    let store = WorkflowVersionStore::new(blobs);
    let workflow_version_id = store.put(&version).await.map_err(store_error)?;

    Ok((
        StatusCode::CREATED,
        Json(CreateWorkflowVersionResponse {
            workflow_version_id,
        }),
    )
        .into_response())
}

fn json_rejection(rejection: JsonRejection) -> ApiError {
    if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
        return ApiError::with_code(
            StatusCode::PAYLOAD_TOO_LARGE,
            "workflow version request exceeds 2 MiB",
            VERSION_TOO_LARGE_CODE,
        );
    }

    match rejection {
        JsonRejection::JsonDataError(err) => ApiError::with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            err.body_text(),
            INVALID_VERSION_CODE,
        ),
        other => ApiError::with_code(
            StatusCode::BAD_REQUEST,
            other.body_text(),
            INVALID_JSON_CODE,
        ),
    }
}

fn store_error(err: WorkflowVersionStoreError) -> ApiError {
    match err {
        WorkflowVersionStoreError::InvalidVersion(_)
        | WorkflowVersionStoreError::InvalidShape(_)
        | WorkflowVersionStoreError::Decode { .. }
        | WorkflowVersionStoreError::NonCanonical { .. } => ApiError::with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            "stored workflow version is invalid or non-canonical",
            INVALID_VERSION_CODE,
        ),
        // The top-level message names the offending dependency without its
        // internal source chain, so it is safe to surface to the caller.
        err @ (WorkflowVersionStoreError::DependencyNotFound { .. }
        | WorkflowVersionStoreError::DependencyInvalid { .. }) => ApiError::with_code(
            StatusCode::UNPROCESSABLE_ENTITY,
            err.to_string(),
            DEPENDENCY_NOT_FOUND_CODE,
        ),
        err => {
            tracing::error!(
                error = %err,
                error_chain = ?error::collect_chain(&err),
                "Workflow version store operation failed"
            );
            internal_store_error()
        }
    }
}

fn internal_store_error() -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        "workflow version store operation failed",
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use axum::response::IntoResponse;
    use fabro_types::{BlobHash, WorkflowVersion, WorkflowVersionId};
    use serde_json::{Value, json};
    use tower::ServiceExt;

    use super::{
        DEPENDENCY_NOT_FOUND_CODE, INVALID_JSON_CODE, INVALID_VERSION_CODE,
        MAX_WORKFLOW_VERSION_BYTES, VERSION_TOO_LARGE_CODE, store_error,
    };
    use crate::server;
    use crate::test_support::{self, TestAppStateBuilder};

    const GRAPH: &str = "digraph W { start [shape=Mdiamond] exit [shape=Msquare] start -> exit }";

    fn request(body: impl Into<Body>) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/api/v1/workflow-versions")
            .header(header::CONTENT_TYPE, "application/json")
            .body(body.into())
            .unwrap()
    }

    fn lookup_request(workflow_version_id: &WorkflowVersionId) -> Request<Body> {
        Request::builder()
            .method(Method::GET)
            .uri(format!(
                "/api/v1/workflow-versions?workflow_version_id={workflow_version_id}"
            ))
            .body(Body::empty())
            .unwrap()
    }

    fn version(graph: &str) -> Value {
        json!({
            "entrypoint": "workflow.fabro",
            "files": { "workflow.fabro": graph },
            "workflow_dependencies": {}
        })
    }

    async fn response_json(response: axum::response::Response) -> Value {
        let bytes = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    fn error_code(body: &Value) -> &str {
        body["errors"][0]["code"].as_str().unwrap()
    }

    #[tokio::test]
    async fn create_requires_authenticated_user() {
        let state = TestAppStateBuilder::new().build();
        let app = server::build_router(state, test_support::test_auth_mode());
        let response = app
            .oneshot(request(serde_json::to_vec(&version(GRAPH)).unwrap()))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn valid_and_equivalent_requests_return_the_same_id() {
        let state = TestAppStateBuilder::new().build();
        let app = test_support::build_test_router(Arc::clone(&state));
        let first = app
            .clone()
            .oneshot(request(serde_json::to_vec(&version(GRAPH)).unwrap()))
            .await
            .unwrap();
        assert_eq!(first.status(), StatusCode::CREATED);
        let first = response_json(first).await;
        assert_eq!(first.as_object().unwrap().len(), 1);

        let reordered = format!(
            r#"{{"workflow_dependencies":{{}},"files":{{"workflow.fabro":{}}},"entrypoint":"workflow.fabro"}}"#,
            serde_json::to_string(GRAPH).unwrap()
        );
        let second = app.oneshot(request(reordered)).await.unwrap();
        assert_eq!(second.status(), StatusCode::CREATED);
        assert_eq!(response_json(second).await, first);

        let id = first["workflow_version_id"]
            .as_str()
            .unwrap()
            .parse::<WorkflowVersionId>()
            .unwrap();
        assert!(
            state
                .store_ref()
                .blobs()
                .read(&id.into())
                .await
                .unwrap()
                .is_some()
        );
    }

    #[tokio::test]
    async fn lookup_verifies_complete_registered_closure_and_reports_a_cache_miss() {
        let app = test_support::build_test_router(TestAppStateBuilder::new().build());
        let created = app
            .clone()
            .oneshot(request(serde_json::to_vec(&version(GRAPH)).unwrap()))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED);
        let workflow_version_id = response_json(created).await["workflow_version_id"]
            .as_str()
            .unwrap()
            .parse::<WorkflowVersionId>()
            .unwrap();

        let hit = app
            .clone()
            .oneshot(lookup_request(&workflow_version_id))
            .await
            .unwrap();
        assert_eq!(hit.status(), StatusCode::OK);
        assert_eq!(
            response_json(hit).await,
            json!({"workflow_version_id": workflow_version_id})
        );

        let missing = WorkflowVersionId::from(BlobHash::new(b"missing workflow closure"));
        let miss = app.oneshot(lookup_request(&missing)).await.unwrap();
        assert_eq!(miss.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn lookup_rejects_invalid_and_noncanonical_roots_without_internal_details() {
        let state = TestAppStateBuilder::new().build();
        let app = test_support::build_test_router(Arc::clone(&state));
        let invalid = state
            .store_ref()
            .blobs()
            .write(b"not a workflow version")
            .await
            .unwrap();
        let noncanonical = state
            .store_ref()
            .blobs()
            .write(&serde_json::to_vec_pretty(&version(GRAPH)).unwrap())
            .await
            .unwrap();

        for root_id in [invalid, noncanonical].map(WorkflowVersionId::from) {
            let response = app.clone().oneshot(lookup_request(&root_id)).await.unwrap();
            let body = fabro_test::expect_axum_json(
                response,
                StatusCode::UNPROCESSABLE_ENTITY,
                "GET /api/v1/workflow-versions with invalid stored root",
            )
            .await;

            assert_eq!(error_code(&body), INVALID_VERSION_CODE);
            assert_eq!(
                body["errors"][0]["detail"],
                "stored workflow version is invalid or non-canonical"
            );
            assert!(!body.to_string().contains("cannot be decoded"));
        }
    }

    #[tokio::test]
    async fn lookup_rejects_invalid_and_noncanonical_dependencies_without_internal_details() {
        let state = TestAppStateBuilder::new().build();
        let app = test_support::build_test_router(Arc::clone(&state));
        let invalid = state
            .store_ref()
            .blobs()
            .write(b"not a workflow version")
            .await
            .unwrap();
        let noncanonical = state
            .store_ref()
            .blobs()
            .write(&serde_json::to_vec_pretty(&version(GRAPH)).unwrap())
            .await
            .unwrap();

        for dependency_id in [invalid, noncanonical].map(WorkflowVersionId::from) {
            let root = serde_json::from_value::<WorkflowVersion>(json!({
                "entrypoint": "workflow.fabro",
                "files": {
                    "workflow.fabro":
                        "digraph W { child [stack.child_workflow=\"child.fabro\"] }"
                },
                "workflow_dependencies": { "child.fabro": dependency_id }
            }))
            .unwrap();
            let root_id = WorkflowVersionId::from(
                state
                    .store_ref()
                    .blobs()
                    .write(&root.canonical_bytes().unwrap())
                    .await
                    .unwrap(),
            );

            let response = app.clone().oneshot(lookup_request(&root_id)).await.unwrap();
            let body = fabro_test::expect_axum_json(
                response,
                StatusCode::UNPROCESSABLE_ENTITY,
                "GET /api/v1/workflow-versions with invalid stored dependency",
            )
            .await;

            assert_eq!(error_code(&body), DEPENDENCY_NOT_FOUND_CODE);
            assert!(
                body["errors"][0]["detail"]
                    .as_str()
                    .unwrap()
                    .contains("child.fabro")
            );
            assert!(!body.to_string().contains("cannot be decoded"));
            assert!(!body.to_string().contains("not canonical"));
        }
    }

    #[tokio::test]
    async fn invalid_json_and_domain_content_have_distinct_codes() {
        let app = test_support::build_test_router(TestAppStateBuilder::new().build());
        let malformed = app.clone().oneshot(request("{")).await.unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            error_code(&response_json(malformed).await),
            INVALID_JSON_CODE
        );

        let unknown = json!({
            "entrypoint": "workflow.fabro",
            "files": { "workflow.fabro": GRAPH },
            "workflow_dependencies": {},
            "metadata": {}
        });
        let invalid = app
            .oneshot(request(serde_json::to_vec(&unknown).unwrap()))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            error_code(&response_json(invalid).await),
            INVALID_VERSION_CODE
        );
    }

    #[tokio::test]
    async fn create_rejects_workflow_config_with_missing_goal_file_before_storage() {
        let state = TestAppStateBuilder::new().build();
        let app = test_support::build_test_router(Arc::clone(&state));
        let payload = json!({
            "entrypoint": "workflow.fabro",
            "files": {
                "workflow.fabro": GRAPH,
                "workflow.toml": "_version = 1\n[run.goal]\nfile = \"prompts/goal.md\"\n"
            },
            "workflow_dependencies": {}
        });
        let version = serde_json::from_value::<WorkflowVersion>(payload.clone()).unwrap();
        let id = WorkflowVersionId::from(BlobHash::new(&version.canonical_bytes().unwrap()));

        let response = app
            .oneshot(request(serde_json::to_vec(&payload).unwrap()))
            .await
            .unwrap();
        let body = fabro_test::expect_axum_json(
            response,
            StatusCode::UNPROCESSABLE_ENTITY,
            "POST /api/v1/workflow-versions with missing run goal file",
        )
        .await;

        assert_eq!(error_code(&body), INVALID_VERSION_CODE);
        assert!(!state.store_ref().blobs().exists(&id.into()).await.unwrap());
    }

    #[tokio::test]
    async fn unavailable_dependency_has_specific_code() {
        let state = TestAppStateBuilder::new().build();
        let app = test_support::build_test_router(Arc::clone(&state));
        let missing_id = WorkflowVersionId::from(fabro_types::BlobHash::new(b"missing"));
        let root = json!({
            "entrypoint": "workflow.fabro",
            "files": {
                "workflow.fabro": "digraph W { child [stack.child_workflow=\"child.fabro\"] }"
            },
            "workflow_dependencies": { "child.fabro": missing_id }
        });
        let response = app
            .oneshot(request(serde_json::to_vec(&root).unwrap()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            error_code(&response_json(response).await),
            DEPENDENCY_NOT_FOUND_CODE
        );
    }

    #[tokio::test]
    async fn invalid_stored_dependency_is_a_client_error_without_internals() {
        let state = TestAppStateBuilder::new().build();
        let app = test_support::build_test_router(Arc::clone(&state));
        let dependency_id = WorkflowVersionId::from(
            state
                .store_ref()
                .blobs()
                .write(b"not a workflow version")
                .await
                .unwrap(),
        );
        let root = json!({
            "entrypoint": "workflow.fabro",
            "files": {
                "workflow.fabro": "digraph W { child [stack.child_workflow=\"child.fabro\"] }"
            },
            "workflow_dependencies": { "child.fabro": dependency_id }
        });

        let response = app
            .oneshot(request(serde_json::to_vec(&root).unwrap()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let body = response_json(response).await;
        assert_eq!(error_code(&body), DEPENDENCY_NOT_FOUND_CODE);
        assert!(
            body["errors"][0]["detail"]
                .as_str()
                .unwrap()
                .contains("child.fabro")
        );
        assert!(!body.to_string().contains("cannot be decoded"));
    }

    #[tokio::test]
    async fn stored_child_can_be_pinned_as_a_dependency() {
        let app = test_support::build_test_router(TestAppStateBuilder::new().build());
        let child = app
            .clone()
            .oneshot(request(serde_json::to_vec(&version(GRAPH)).unwrap()))
            .await
            .unwrap();
        assert_eq!(child.status(), StatusCode::CREATED);
        let child_id = response_json(child).await["workflow_version_id"].clone();
        let root = json!({
            "entrypoint": "workflow.fabro",
            "files": {
                "workflow.fabro": "digraph W { child [stack.child_workflow=\"child.fabro\"] }"
            },
            "workflow_dependencies": { "child.fabro": child_id }
        });

        let response = app
            .oneshot(request(serde_json::to_vec(&root).unwrap()))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED);
        assert_eq!(response_json(response).await.as_object().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn body_limit_has_specific_code() {
        let app = test_support::build_test_router(TestAppStateBuilder::new().build());
        let response = app
            .oneshot(request(vec![b' '; MAX_WORKFLOW_VERSION_BYTES + 1]))
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            error_code(&response_json(response).await),
            VERSION_TOO_LARGE_CODE
        );
    }

    #[tokio::test]
    async fn storage_fault_response_is_curated() {
        let response = store_error(fabro_workflow_version::WorkflowVersionStoreError::Storage {
            source: fabro_store::Error::Other("private persistence detail".to_string()),
        })
        .into_response();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);

        let body = response_json(response).await;
        assert_eq!(
            body["errors"][0]["detail"],
            "workflow version store operation failed"
        );
        assert!(!body.to_string().contains("private persistence detail"));
    }
}
