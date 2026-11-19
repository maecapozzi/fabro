#![allow(
    dead_code,
    reason = "Tool DTO fields are consumed by serde and schema generation even when not read \
              directly."
)]

mod common;
mod create;
mod events;
pub mod fabro_client;
mod gather;
mod get;
mod interact;
mod manifest;
mod pair;
mod search;
mod workflow_version;

pub use common::{
    FABRO_RUN_CREATE_TOOL_NAME, FABRO_RUN_EVENTS_TOOL_NAME, FABRO_RUN_GATHER_TOOL_NAME,
    FABRO_RUN_GET_TOOL_NAME, FABRO_RUN_INTERACT_TOOL_NAME, FABRO_RUN_PAIR_TOOL_NAME,
    FABRO_RUN_SEARCH_TOOL_NAME, FABRO_WORKFLOW_VERSION_CREATE_TOOL_NAME,
    FABRO_WORKFLOW_VERSION_LOOKUP_TOOL_NAME, FabroToolBackend, RunSummaryResult, ToolDefinition,
    ToolError, ToolResult, tool_definitions,
};
pub use create::{
    CreateRunOptions, CreateRunSpec, CreateRunsResult, CreatedRunResult, FabroRunCreateParams,
    create_runs, create_runs_text, create_runs_with_options,
};
pub use events::{
    FabroRunEventsParams, RunEventResult, RunEventsAction, RunEventsResult, ValidatedRunEvents,
    run_events, run_events_text,
};
pub use gather::{
    FabroRunGatherParams, GatherRunsResult, ValidatedGatherRuns, gather_runs, gather_runs_text,
};
pub use get::{FabroRunGetParams, RunGetResult, ValidatedRunGet, run_get, run_get_text};
pub use interact::{
    AnswerValue, FabroRunInteractParams, InteractRunResult, RunInteractAction,
    ValidatedInteractAction, ValidatedInteractRun, interact_run, interact_run_text,
};
pub use manifest::json_to_toml_value;
pub use pair::{
    FabroRunPairParams, PairRunResult, RunPairAction, ValidatedPairAction, ValidatedPairRun,
    pair_run, pair_run_text,
};
pub use search::{
    FabroRunSearchParams, SearchRunSummaryResult, SearchRunsResult, ValidatedSearchRuns,
    search_runs, search_runs_text,
};
pub use workflow_version::{
    FabroWorkflowVersionCreateParams, FabroWorkflowVersionLookupParams,
    ValidatedWorkflowVersionCreate, WorkflowVersionLookupResult, WorkflowVersionPackager,
    create_workflow_version, lookup_workflow_version, workflow_version_create_text,
    workflow_version_lookup_text,
};
