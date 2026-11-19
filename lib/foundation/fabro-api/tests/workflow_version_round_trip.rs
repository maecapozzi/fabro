use std::any::{TypeId, type_name};

use fabro_api::types::{
    CreateWorkflowVersionResponse, WorkflowPath as ApiWorkflowPath,
    WorkflowVersion as ApiWorkflowVersion, WorkflowVersionId as ApiWorkflowVersionId,
    WorkflowVersionLookupResponse,
};
use fabro_types::{WorkflowPath, WorkflowVersion, WorkflowVersionId};
use serde_json::json;

const DEPENDENCY_ID: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn workflow_version_schemas_reuse_domain_types() {
    assert_same_type::<ApiWorkflowPath, WorkflowPath>();
    assert_same_type::<ApiWorkflowVersionId, WorkflowVersionId>();
    assert_same_type::<ApiWorkflowVersion, WorkflowVersion>();
}

#[test]
fn workflow_version_round_trips_exact_wire_shape() {
    let value = json!({
        "entrypoint": "workflow.fabro",
        "files": {
            "prompts/goal.md": "Ship it",
            "workflow.fabro": "digraph W { start [shape=Mdiamond] exit [shape=Msquare] start -> exit }"
        },
        "workflow_dependencies": { "children/check.fabro": DEPENDENCY_ID }
    });

    let version: ApiWorkflowVersion = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(version).unwrap(), value);
}

#[test]
fn create_workflow_version_response_round_trips_exact_wire_shape() {
    let value = json!({ "workflow_version_id": DEPENDENCY_ID });

    let response: CreateWorkflowVersionResponse = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&response).unwrap(), value);
}

#[test]
fn workflow_version_lookup_response_round_trips_exact_wire_shape() {
    let value = json!({ "workflow_version_id": DEPENDENCY_ID });

    let response: WorkflowVersionLookupResponse = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(&response).unwrap(), value);
}

#[test]
fn workflow_version_id_accepts_any_case_and_emits_lowercase() {
    let id = serde_json::from_value::<ApiWorkflowVersionId>(json!(DEPENDENCY_ID.to_uppercase()))
        .unwrap();
    let emitted = serde_json::to_value(id).unwrap();
    assert_eq!(emitted, json!(DEPENDENCY_ID));

    let text = emitted.as_str().unwrap();
    assert_eq!(text.len(), 64);
    assert!(
        text.bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    );
}

#[test]
fn workflow_version_replacement_rejects_unknown_fields() {
    let value = json!({
        "entrypoint": "workflow.fabro",
        "files": {
            "workflow.fabro": "digraph W {}"
        },
        "workflow_dependencies": {},
        "metadata": {}
    });

    assert!(serde_json::from_value::<ApiWorkflowVersion>(value).is_err());
}

fn assert_same_type<T: 'static, U: 'static>() {
    assert_eq!(
        TypeId::of::<T>(),
        TypeId::of::<U>(),
        "{} and {} should be the same type",
        type_name::<T>(),
        type_name::<U>()
    );
}
