//! Fork, rewind, retry and the timeline over Petri runs, through a real
//! server and its worker subprocess.
//!
//! Local-folder runs are checkpointed in their workspace and can retry from
//! the start, but refuse fork and rewind: only a published run branch can
//! seed a new workspace.
//! Git-backed forks are covered by fabro-petri's sandbox/remote integration
//! test.

#![expect(
    clippy::disallowed_methods,
    reason = "these scenarios start a real server subprocess and read its workspaces on disk"
)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use fabro_test::test_context;

use super::petri::{
    RunningServer, run_detached, run_json, wait_for_status, wait_for_success, write_petri_workflow,
};

/// Three command stages that build on each other's files: `one` writes a
/// file, `two` another, `three` checks both and writes its own.
fn three_stage_dot() -> String {
    "digraph Stages {\n  graph [goal=\"Three stages\", default_max_retries=0]\n  start \
     [shape=Mdiamond]\n  exit [shape=Msquare]\n  one [shape=parallelogram, script=\"echo one > \
     one.txt\"]\n  two [shape=parallelogram, script=\"echo two > two.txt\"]\n  three \
     [shape=parallelogram, script=\"test \\\"$(cat one.txt)\\\" = one && test \\\"$(cat \
     two.txt)\\\" = two && echo three > three.txt\"]\n  start -> one -> two -> three -> exit\n}\n"
        .to_string()
}

/// Three stages whose middle one fails the first time it runs and passes
/// the next: the marker outside the workspace remembers the first run. Its
/// failure ends the run (`on_failure=exit`) rather than routing on.
fn flaky_dot(marker: &Path) -> String {
    format!(
        "digraph Flaky {{\n  graph [goal=\"A transient failure\", default_max_retries=0]\n  \
         start [shape=Mdiamond]\n  exit [shape=Msquare]\n  one [shape=parallelogram, \
         script=\"echo one > one.txt\"]\n  flaky [shape=parallelogram, max_retries=0, on_failure=exit, \
         script=\"if [ ! -f {marker} ]; then touch {marker}; exit 1; fi; echo flaky > \
         flaky.txt\"]\n  three [shape=parallelogram, script=\"test \\\"$(cat one.txt)\\\" = one \
         && test \\\"$(cat flaky.txt)\\\" = flaky && echo three > three.txt\"]\n  start -> one \
         -> flaky -> three -> exit\n}}\n",
        marker = marker.display()
    )
}

/// A parallel node with two command branches and a join.
fn parallel_dot() -> String {
    "digraph Branches {\n  graph [goal=\"Two branches\", default_max_retries=0]\n  start \
     [shape=Mdiamond]\n  exit [shape=Msquare]\n  fan [shape=component]\n  a \
     [shape=parallelogram, script=\"echo a > a.txt\"]\n  b [shape=parallelogram, script=\"echo \
     b > b.txt\"]\n  join [shape=tripleoctagon]\n  done [shape=parallelogram, script=\"echo done \
     > done.txt\"]\n  start -> fan\n  fan -> a\n  fan -> b\n  a -> join\n  b -> join\n  join -> \
     done -> exit\n}\n"
        .to_string()
}

/// Run a CLI command against the server; the caller judges the exit.
fn cli(context: &fabro_test::TestContext, server: &RunningServer, args: &[&str]) -> Output {
    let target = server.target();
    context
        .command()
        .args(args)
        .args(["--server", &target])
        .output()
        .expect("the CLI command executes")
}

/// Run a CLI command that must succeed, and parse its stdout as JSON.
fn cli_json(
    context: &fabro_test::TestContext,
    server: &RunningServer,
    args: &[&str],
) -> serde_json::Value {
    let output = cli(context, server, args);
    assert!(
        output.status.success(),
        "`fabro {}` failed\nstdout:\n{}\nstderr:\n{}",
        args.join(" "),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|err| {
        panic!(
            "`fabro {}` printed no JSON: {err}\nstdout:\n{}",
            args.join(" "),
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

async fn timeline(server: &RunningServer, run_id: &str) -> serde_json::Value {
    run_json(server, &format!("runs/{run_id}/timeline")).await
}

/// The timeline's entries as `(node, execution, firing, attempt, sha)`.
fn entries(timeline: &serde_json::Value) -> Vec<(String, u64, u64, u64, Option<String>)> {
    timeline["entries"]
        .as_array()
        .expect("the timeline has entries")
        .iter()
        .map(|entry| {
            (
                entry["node_name"].as_str().unwrap_or_default().to_string(),
                entry["execution"].as_u64().expect("an execution"),
                entry["firing"].as_u64().expect("a firing"),
                entry["attempt"].as_u64().expect("an attempt"),
                entry["run_commit_sha"].as_str().map(str::to_owned),
            )
        })
        .collect()
}

fn nodes(timeline: &serde_json::Value) -> Vec<String> {
    entries(timeline)
        .into_iter()
        .map(|(node, ..)| node)
        .collect()
}

/// The one host workspace of the run.
fn workspace(server: &RunningServer, run_id: &str) -> PathBuf {
    let scopes = server.petri_run_dir(run_id).join("scopes");
    let mut workspaces: Vec<PathBuf> = std::fs::read_dir(&scopes)
        .expect("the scopes directory lists")
        .map(|entry| entry.expect("an entry reads").path().join("work"))
        .collect();
    assert_eq!(workspaces.len(), 1, "one workspace: {workspaces:?}");
    workspaces.remove(0)
}

/// The subjects of the commits on the workspace's branch, oldest first.
fn commit_subjects(workspace: &Path) -> Vec<String> {
    let output = Command::new("git")
        .args(["log", "--reverse", "--format=%s"])
        .current_dir(workspace)
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git log failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(ToOwned::to_owned)
        .collect()
}

fn read(workspace: &Path, name: &str) -> String {
    std::fs::read_to_string(workspace.join(name))
        .unwrap_or_else(|err| panic!("{name} in {}: {err}", workspace.display()))
}

/// A local-folder run cannot fork without a published checkpoint.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_folder_fork_is_refused_without_creating_a_run() {
    let context = test_context!();
    let server = RunningServer::start().await;
    let bundle = write_petri_workflow(&context, &three_stage_dot());
    let source = run_detached(&context, &server, &bundle);
    wait_for_success(&server, &source).await;
    let refused = cli(&context, &server, &["fork", &source, "one", "--json"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("GitHub-backed run"));
    let summary = run_json(&server, &format!("runs/{source}")).await;
    assert!(summary["superseded_by"].is_null());
    assert_eq!(summary["lifecycle"]["archived"], false);
    server.shutdown();
}

/// A retry starts the workflow again in a new workspace. A transient
/// failure can succeed on this second execution.
#[tokio::test(flavor = "multi_thread")]
async fn a_retry_starts_over_and_succeeds_when_the_failure_was_transient() {
    let context = test_context!();
    let server = RunningServer::start().await;
    let marker = context.temp_dir.join("flaky.marker");
    let bundle = write_petri_workflow(&context, &flaky_dot(&marker));
    let source = run_detached(&context, &server, &bundle);
    let status = wait_for_status(&server, &source, &["succeeded", "failed"]).await;
    assert_eq!(status, "failed", "the first run fails on `flaky`");
    assert!(marker.exists(), "the first run left its marker");
    assert_eq!(nodes(&timeline(&server, &source).await), [
        "start", "one", "flaky"
    ]);

    let retried = cli_json(&context, &server, &["retry", &source, "--json"]);
    assert_eq!(retried["source_run_id"], source);
    let retry = retried["run_id"]
        .as_str()
        .expect("the new run id")
        .to_string();
    wait_for_success(&server, &retry).await;

    let retry_timeline = timeline(&server, &retry).await;
    assert_eq!(nodes(&retry_timeline), [
        "start", "one", "flaky", "three", "exit"
    ]);
    assert!(retry_timeline["forked_from"].is_null());
    let retry_workspace = workspace(&server, &retry);
    assert_eq!(read(&retry_workspace, "one.txt"), "one\n");
    assert_eq!(read(&retry_workspace, "flaky.txt"), "flaky\n");
    assert_eq!(read(&retry_workspace, "three.txt"), "three\n");
    assert_eq!(commit_subjects(&retry_workspace), [
        "Start completed (no source changes)",
        "Update source during One",
        "Update source during Flaky",
        "Update source during Three",
        "Exit completed (no source changes)",
    ]);
    let state = run_json(&server, &format!("runs/{retry}/state")).await;
    assert_eq!(state["retried_from"], source);
    assert!(state["spec"]["fork_source_ref"].is_null());

    // The source is untouched, and a second retry is refused for the
    // running or archived cases alone: it is terminal, so it may retry again.
    let summary = run_json(&server, &format!("runs/{source}")).await;
    assert_eq!(summary["lifecycle"]["status"]["kind"], "failed");
    assert!(summary["superseded_by"].is_null());
    server.shutdown();
}

/// Refusing rewind must leave the original run available and unarchived.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_folder_rewind_is_refused_without_archiving_its_source() {
    let context = test_context!();
    let server = RunningServer::start().await;
    let bundle = write_petri_workflow(&context, &three_stage_dot());
    let source = run_detached(&context, &server, &bundle);
    wait_for_success(&server, &source).await;
    let listed = cli_json(&context, &server, &["rewind", &source, "--json"]);
    assert_eq!(listed["entries"].as_array().unwrap().len(), 5);
    let refused = cli(&context, &server, &["rewind", &source, "@3", "--json"]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("GitHub-backed run"));
    let summary = run_json(&server, &format!("runs/{source}")).await;
    assert!(summary["superseded_by"].is_null());
    assert_eq!(summary["lifecycle"]["archived"], false);
    server.shutdown();
}

/// The timeline lists every checkpoint the run recorded, in order, with
/// its position and commit, as the CLI prints it and as the API serves it.
#[tokio::test(flavor = "multi_thread")]
async fn the_timeline_lists_every_checkpoint_with_its_commit() {
    let context = test_context!();
    let server = RunningServer::start().await;
    let bundle = write_petri_workflow(&context, &three_stage_dot());
    let run_id = run_detached(&context, &server, &bundle);
    wait_for_success(&server, &run_id).await;

    let recorded: Vec<(u64, u64, u64, Option<String>)> = server
        .checkpoints(&run_id)
        .await
        .into_iter()
        .map(|(key, sha)| (key.execution, key.firing, u64::from(key.attempt), Some(sha)))
        .collect();
    let listed = cli_json(&context, &server, &["timeline", &run_id, "--json"]);
    let listed_entries: Vec<(u64, u64, u64, Option<String>)> = entries(&listed)
        .into_iter()
        .map(|(_, execution, firing, attempt, sha)| (execution, firing, attempt, sha))
        .collect();
    assert_eq!(listed_entries, recorded);
    assert_eq!(listed_entries.len(), 5);
    let ordinals: Vec<u64> = listed["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["ordinal"].as_u64().expect("ordinal"))
        .collect();
    assert_eq!(ordinals, [1, 2, 3, 4, 5]);
    let stages: Vec<&str> = listed["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|entry| entry["stage"].as_str().unwrap_or("-"))
        .collect();
    assert_eq!(stages, ["start@1", "one@1", "two@1", "three@1", "exit@1"]);
    assert!(listed["forked_from"].is_null());

    let table = cli(&context, &server, &["timeline", &run_id]);
    assert!(table.status.success());
    let stderr = String::from_utf8_lossy(&table.stderr);
    for (ordinal, node) in ["start", "one", "two", "three", "exit"].iter().enumerate() {
        assert!(
            stderr.contains(&format!("@{}", ordinal + 1)) && stderr.contains(node),
            "the table names @{} {node}:\n{stderr}",
            ordinal + 1
        );
    }
    let api = timeline(&server, &run_id).await;
    assert_eq!(nodes(&api), ["start", "one", "two", "three", "exit"]);
    server.shutdown();
}

/// A checkpoint inside a parallel branch is not a fork position: Petri
/// refuses it, and the refusal says why. The join, in the root, is.
#[tokio::test(flavor = "multi_thread")]
async fn a_local_folder_parallel_run_cannot_be_forked() {
    let context = test_context!();
    let server = RunningServer::start().await;
    let bundle = write_petri_workflow(&context, &parallel_dot());
    let source = run_detached(&context, &server, &bundle);
    wait_for_success(&server, &source).await;

    let listed = timeline(&server, &source).await;
    let all = entries(&listed);
    let branch = all
        .iter()
        .zip(1_u64..)
        .find(|((node, execution, ..), _)| *execution != 0 && (node == "a" || node == "b"))
        .map(|(_, ordinal)| ordinal)
        .expect("a branch stage has a checkpoint in a child execution");
    let refused = cli(&context, &server, &["fork", &source, &format!("@{branch}")]);
    assert!(
        !refused.status.success(),
        "a fork inside a branch is refused"
    );
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("GitHub-backed run"),
        "the refusal names the branch:\n{stderr}"
    );
    // Nothing was started for it.
    let runs = run_json(&server, "runs").await;
    let ids: Vec<&str> = runs["data"]
        .as_array()
        .or_else(|| runs.as_array())
        .expect("a run list")
        .iter()
        .filter_map(|run| run["id"].as_str())
        .collect();
    assert!(ids.iter().all(|id| *id == source), "runs: {ids:?}");

    let refused = cli(&context, &server, &["fork", &source, "done"]);
    assert!(!refused.status.success());
    server.shutdown();
}
