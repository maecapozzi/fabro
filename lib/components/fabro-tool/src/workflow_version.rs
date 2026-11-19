use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;
use fabro_api::types::CreateWorkflowVersionResponse;
use fabro_types::{MAX_WORKFLOW_VERSION_BYTES, WorkflowPath, WorkflowVersionId};
use fabro_workflow_version::CollectedWorkflowClosure;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{FabroToolBackend, ToolError, ToolResult};

/// Caller-supplied source content, before packaging resolves workflow
/// dependencies.
#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FabroWorkflowVersionCreateParams {
    /// Exact package-relative key of the graph or workflow configuration file.
    #[schemars(with = "String")]
    pub entrypoint: WorkflowPath,
    /// All local dependencies, keyed by package-relative path. Values are text
    /// contents. Tool arguments arrive as an already-parsed JSON value on
    /// every production route, so duplicate keys have collapsed (last wins)
    /// before this type sees them; there is no byte-level guard to add here.
    #[schemars(with = "BTreeMap<String, String>")]
    pub files:      BTreeMap<WorkflowPath, String>,
}

/// A supplied source tree whose entrypoint, budgets, and portable path
/// collisions have been checked, so it is safe to stage on a filesystem.
#[derive(Clone, Debug)]
pub struct ValidatedWorkflowVersionCreate {
    pub entrypoint: WorkflowPath,
    pub files:      BTreeMap<WorkflowPath, String>,
}

#[derive(Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FabroWorkflowVersionLookupParams {
    #[schemars(with = "String")]
    pub workflow_version_id: WorkflowVersionId,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WorkflowVersionLookupResult {
    pub workflow_version_id: String,
    pub available:           bool,
}

impl TryFrom<FabroWorkflowVersionCreateParams> for ValidatedWorkflowVersionCreate {
    type Error = ToolError;

    fn try_from(params: FabroWorkflowVersionCreateParams) -> Result<Self, Self::Error> {
        let FabroWorkflowVersionCreateParams { entrypoint, files } = params;
        fabro_types::validate_workflow_files(&entrypoint, &files)
            .map_err(|err| ToolError::message(err.to_string()))?;
        let total: usize = files.values().map(String::len).sum();
        if total > MAX_WORKFLOW_VERSION_BYTES {
            return Err(ToolError::message(format!(
                "workflow source exceeds {} MiB",
                MAX_WORKFLOW_VERSION_BYTES / (1024 * 1024)
            )));
        }
        fabro_types::validate_workflow_source_paths(files.keys())
            .map_err(|_| ToolError::message("workflow source paths collide"))?;
        Ok(Self { entrypoint, files })
    }
}

/// Application seam for packaging supplied content. The manifest crates that
/// own collection depend on this crate, so the packager is injected instead.
/// Implementations confine reads to supplied files and validate the entire
/// closure before returning.
#[async_trait]
pub trait WorkflowVersionPackager: Send + Sync {
    async fn package(
        &self,
        source: ValidatedWorkflowVersionCreate,
    ) -> anyhow::Result<CollectedWorkflowClosure>;
}

pub async fn create_workflow_version(
    backend: Arc<dyn FabroToolBackend>,
    source: ValidatedWorkflowVersionCreate,
) -> ToolResult<CreateWorkflowVersionResponse> {
    let workflow_version_id = backend
        .create_workflow_version(source)
        .await
        .map_err(|err| ToolError::from_anyhow(&err))?;
    Ok(CreateWorkflowVersionResponse {
        workflow_version_id,
    })
}

pub async fn lookup_workflow_version(
    backend: Arc<dyn FabroToolBackend>,
    params: FabroWorkflowVersionLookupParams,
) -> ToolResult<WorkflowVersionLookupResult> {
    let workflow_version_id = params.workflow_version_id;
    let available = backend
        .lookup_workflow_version(&workflow_version_id)
        .await
        .map_err(|err| ToolError::from_anyhow(&err))?;
    Ok(WorkflowVersionLookupResult {
        workflow_version_id: workflow_version_id.to_string(),
        available,
    })
}

#[must_use]
pub fn workflow_version_create_text(result: &CreateWorkflowVersionResponse) -> String {
    format!("Registered workflow version {}", result.workflow_version_id)
}

#[must_use]
pub fn workflow_version_lookup_text(result: &WorkflowVersionLookupResult) -> String {
    if result.available {
        format!(
            "Workflow version {} is available",
            result.workflow_version_id
        )
    } else {
        format!(
            "Workflow version {} is not available",
            result.workflow_version_id
        )
    }
}

#[cfg(test)]
mod tests {
    use fabro_types::{MAX_WORKFLOW_VERSION_FILE_BYTES, MAX_WORKFLOW_VERSION_FILES};
    use serde_json::json;

    use super::*;
    use crate::fabro_client::ClientBackend;

    fn validate(value: serde_json::Value) -> ToolResult<ValidatedWorkflowVersionCreate> {
        let params: FabroWorkflowVersionCreateParams = serde_json::from_value(value).unwrap();
        ValidatedWorkflowVersionCreate::try_from(params)
    }

    #[test]
    fn workflow_version_request_rejects_unknown_fields_and_invalid_paths() {
        let valid = json!({"entrypoint": "workflow", "files": {"workflow": "digraph W {}"}});
        validate(valid.clone()).unwrap();
        for field in [
            "cwd",
            "url",
            "workflow",
            "environment",
            "parent_id",
            "workflow_dependencies",
        ] {
            let mut value = valid.clone();
            value[field] = json!("unexpected");
            assert!(serde_json::from_value::<FabroWorkflowVersionCreateParams>(value).is_err());
        }
        for path in [
            "../workflow",
            "/workflow",
            "a/../workflow",
            "a//b",
            "a\\b",
            "~/workflow",
            "",
        ] {
            for value in [
                json!({"entrypoint":path,"files":{"workflow":"x"}}),
                json!({"entrypoint":"workflow","files":{path:"x"}}),
            ] {
                assert!(serde_json::from_value::<FabroWorkflowVersionCreateParams>(value).is_err());
            }
        }
    }

    #[test]
    fn workflow_version_source_enforces_presence_collisions_and_budgets() {
        assert!(
            validate(json!({"entrypoint":"missing","files":{"workflow":"digraph W {}"}})).is_err()
        );
        for files in [
            json!({"A":"x","a":"y"}),
            json!({"A":"x","a/b.md":"y"}),
            json!({"a":"x","A/b.md":"y"}),
        ] {
            let mut files = files.as_object().unwrap().clone();
            files.insert("workflow".into(), json!("digraph W {}"));
            assert!(validate(json!({"entrypoint":"workflow","files":files})).is_err());
        }
        let oversized_file = FabroWorkflowVersionCreateParams {
            entrypoint: "workflow".parse().unwrap(),
            files:      BTreeMap::from([(
                "workflow".parse().unwrap(),
                "x".repeat(MAX_WORKFLOW_VERSION_FILE_BYTES + 1),
            )]),
        };
        assert!(ValidatedWorkflowVersionCreate::try_from(oversized_file).is_err());
        let mut too_many_files = FabroWorkflowVersionCreateParams {
            entrypoint: "workflow".parse().unwrap(),
            files:      (0..MAX_WORKFLOW_VERSION_FILES)
                .map(|i| (format!("file{i}").parse().unwrap(), String::new()))
                .collect(),
        };
        too_many_files
            .files
            .insert(too_many_files.entrypoint.clone(), String::new());
        assert!(ValidatedWorkflowVersionCreate::try_from(too_many_files).is_err());
        let mut oversized_total = FabroWorkflowVersionCreateParams {
            entrypoint: "workflow".parse().unwrap(),
            files:      (0..5)
                .map(|i| {
                    (
                        format!("file{i}").parse().unwrap(),
                        "x".repeat(MAX_WORKFLOW_VERSION_FILE_BYTES),
                    )
                })
                .collect(),
        };
        oversized_total
            .files
            .insert(oversized_total.entrypoint.clone(), String::new());
        assert!(ValidatedWorkflowVersionCreate::try_from(oversized_total).is_err());
    }

    #[tokio::test]
    async fn workflow_version_same_run_backend_denies_before_packaging() {
        let client = fabro_client::Client::new_no_proxy("http://127.0.0.1:1").unwrap();
        let backend = ClientBackend::new(Arc::new(client))
            .with_workflow_version_packager(Arc::new(UnreachablePackager))
            .with_run_scope("01KRBZW4DW0000000000000002".parse().unwrap());
        let source =
            validate(json!({"entrypoint":"workflow","files":{"workflow":"digraph W {}"}})).unwrap();
        let error = create_workflow_version(Arc::new(backend), source)
            .await
            .unwrap_err();
        assert!(error.as_str().contains("run scope"));
    }

    struct UnreachablePackager;
    #[async_trait]
    impl WorkflowVersionPackager for UnreachablePackager {
        async fn package(
            &self,
            _: ValidatedWorkflowVersionCreate,
        ) -> anyhow::Result<CollectedWorkflowClosure> {
            panic!("scoped backend must not invoke the packager")
        }
    }
}
