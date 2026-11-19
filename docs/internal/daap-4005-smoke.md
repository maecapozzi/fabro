# DAAP-4005 isolated acceptance smoke

Run this smoke only against an isolated candidate server. It exercises three
Factory paths:

- `cold`: empty prepared binding, so Factory packages and registers without a
  lookup;
- `synthetic-miss`: an explicitly synthetic, in-memory negative fixture whose
  version ID and package digest are zero, so lookup misses and Factory takes
  the safe registration fallback;
- `warm`: the valid operator-prepared binding, so lookup hits and Factory
  launches without clone, package, or registration work.

The procedure never auto-approves a child. It records observed values only;
leave a measurement unrecorded if its run has not completed.

## Provenance

The Fabro source base for this work is
`d8b9e3c5bbaf7a035b3891f63b91a25fb2d93e44`. Fabro pins the Petri companion
`b769e386d51b349f7f995a286496ede07d153479`, based on verified Petri main
`31fbe1bc20850599a5a31e133f8d68ca9265964d`.

Comparing that Petri base with Fabro's former lock
`f30a12df140fb60f55a7c6cff3d7f45673c7a7c5` shows no changes under
`crates/**/src/**`; the root `Cargo.toml` change is only a comment about the
existing sandbox-driver lock. Companion implementation commit `b769e386...`
has tree `b74d8c34ee140f02ab89f46210603e5fc1217c8f`. The host reports that
published test commit `645e544e940b6fc24c854d507ebcae188ec07f0f`
has the same tree and passed Petri's native gate; GitHub CI was still pending
at handoff.

Record the candidate's final Fabro commit and server version with the results.

## Prepare exact Factory inputs

Use `prepare_orchestrate.py` from the private
[`maecapozzi/factory` candidate documentation](https://github.com/maecapozzi/factory/blob/be5cee0f9532280f9b4876eef2386c07c42afc43/docs/orchestrate.md)
at revision `be5cee0f9532280f9b4876eef2386c07c42afc43` to produce the binding
for the exact tuple under test:

- Factory revision (`factory_ref`);
- route;
- target repository;
- worker environment;
- repository-specific package variant, if any.

The tested invocation supplies every cache selector and the native transport
JSON used to reach the isolated server. The transport file is not Factory's
private cache directory:

```bash
umask 077
export FACTORY_ROOT='/absolute/path/to/clean/factory-candidate'
export FACTORY_ROUTE='change'
export WORKER_ENVIRONMENT='default'
export TARGET_REPOSITORY='owner/repository'
export TRANSPORT_PATH='/absolute/private/native-transport.json'
export FACTORY_REF
FACTORY_REF=$(git -C "$FACTORY_ROOT" rev-parse --verify HEAD^{commit})
test "$FACTORY_REF" = be5cee0f9532280f9b4876eef2386c07c42afc43

PREPARED_OUTPUT=$(mktemp)
python3 "$FACTORY_ROOT/prepare_orchestrate.py" \
  --factory-root "$FACTORY_ROOT" \
  --factory-revision "$FACTORY_REF" \
  --route "$FACTORY_ROUTE" \
  --environment "$WORKER_ENVIRONMENT" \
  --target-repository "$TARGET_REPOSITORY" \
  --transport "$TRANSPORT_PATH" > "$PREPARED_OUTPUT"

export PREPARED_FACTORY_REF
PREPARED_FACTORY_REF=$(python3 -c \
  'import json,sys; print(json.load(open(sys.argv[1]))["fabro_inputs"][0].split("=",1)[1])' \
  "$PREPARED_OUTPUT")
export PREPARED_WORKFLOW_BINDING
PREPARED_WORKFLOW_BINDING=$(python3 -c \
  'import json,sys; print(json.load(open(sys.argv[1]))["fabro_inputs"][1].split("=",1)[1])' \
  "$PREPARED_OUTPUT")
test "$PREPARED_FACTORY_REF" = "$FACTORY_REF"
```

Do not hand-author or partially copy the normal binding. It is canonical
base64url-encoded JSON. Remove `"$PREPARED_OUTPUT"` after recording redacted
binding metadata.

Normal preparation cannot produce a different canonical version ID for the
same selectors on the same server. For the negative probe only, derive an
in-memory fixture from the valid binding by replacing
`workflow_version_id` and `package_digest` with 64 zeros. This fixture must
not be written to or treated as an entry in the private preparation cache.

The regular `cold` case uses an empty binding and therefore does not perform
lookup. The synthetic miss is a denied-launch negative probe, not a normally
prepared binding and not a successful measurement run. A genuinely fresh
separate isolated server using the valid binding is an alternative way to
exercise a real miss.

Configure an authenticated Fabro CLI target for the isolated server before
running `fabro events`. Then set:

```bash
set +x
set -euo pipefail

export FABRO_SERVER_URL=http://127.0.0.1:32284
export FABRO_TOKEN='<isolated-server-dev-or-user-token>'
export FACTORY_WORKFLOW_VERSION_ID='<registered-factory-orchestrate-version-id>'
export FACTORY_GOAL='<exact-child-task-goal>'
export TEST_COMMAND='<real-repository-check-command>'
export ENVIRONMENT_ID='<isolated-server-environment-id>'
export TARGET_JSON='{"kind":"git","repo":"owner/repository","branch":"candidate","sha":"0123456789abcdef0123456789abcdef01234567"}'

# Exact Factory node IDs, used only to classify observed model calls.
export DISPATCH_STAGE='<dispatch-node-id>'
export STATUS_STAGE='<status-node-id>'

export RESULTS_DIR
RESULTS_DIR=$(mktemp -d "${TMPDIR:-/tmp}/daap-4005-smoke.XXXXXX")
printf 'results_dir=%s\n' "$RESULTS_DIR"

export EXPECTED_CHILD_VERSION_ID
EXPECTED_CHILD_VERSION_ID=$(python3 -c '
import base64,json,sys
value=sys.argv[1]
decoded=base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))
print(json.loads(decoded)["workflow_version_id"])
' "$PREPARED_WORKFLOW_BINDING")

SYNTHETIC_MISS_BINDING=$(python3 -c '
import base64,json,sys
value=sys.argv[1]
decoded=base64.urlsafe_b64decode(value + "=" * (-len(value) % 4))
binding=json.loads(decoded)
binding["workflow_version_id"]="0" * 64
binding["package_digest"]="0" * 64
canonical=json.dumps(binding,sort_keys=True,separators=(",",":")).encode()
print(base64.urlsafe_b64encode(canonical).decode().rstrip("="))
' "$PREPARED_WORKFLOW_BINDING")
readonly SYNTHETIC_MISS_BINDING
```

The valid prepared version must be present. The all-zero synthetic ID must be
absent:

```bash
api() {
  curl --fail-with-body -sS \
    -H "Authorization: Bearer $FABRO_TOKEN" \
    -H 'Content-Type: application/json' \
    "$@"
}

api "$FABRO_SERVER_URL/api/v1/workflow-versions?workflow_version_id=$EXPECTED_CHILD_VERSION_ID" \
  > "$RESULTS_DIR/prepared-hit.json"
jq -e --arg id "$EXPECTED_CHILD_VERSION_ID" '
  keys == ["workflow_version_id"] and .workflow_version_id == $id
' "$RESULTS_DIR/prepared-hit.json" >/dev/null

synthetic_miss_status=$(curl -sS \
  -o "$RESULTS_DIR/synthetic-prepared-miss.json" -w '%{http_code}' \
  -H "Authorization: Bearer $FABRO_TOKEN" \
  "$FABRO_SERVER_URL/api/v1/workflow-versions?workflow_version_id=$(printf '0%.0s' {1..64})")
test "$synthetic_miss_status" = 404
```

Save provenance. Do not put the token or opaque bindings in this file:

```bash
{
  printf 'recorded_at=%s\n' "$(date -u +%Y-%m-%dT%H:%M:%SZ)"
  printf 'fabro_commit=%s\n' "$(git rev-parse HEAD)"
  printf 'fabro_version=%s\n' "$(fabro --version)"
  printf 'server_url=%s\n' "$FABRO_SERVER_URL"
  printf 'factory_workflow_version_id=%s\n' "$FACTORY_WORKFLOW_VERSION_ID"
  printf 'factory_ref=%s\n' "$FACTORY_REF"
  printf 'expected_child_version_id=%s\n' "$EXPECTED_CHILD_VERSION_ID"
  printf 'parent_environment_id=%s\n' "$ENVIRONMENT_ID"
  printf 'worker_environment=%s\n' "$WORKER_ENVIRONMENT"
  printf 'target=%s\n' "$TARGET_JSON"
} > "$RESULTS_DIR/provenance.txt"
```

## Create the positive parents and negative probe

This helper supplies the actual Factory inputs. In particular, it does not
leave `goal`, `factory_ref`, `test_command`, or
`prepared_workflow_binding` implicit. `environment_id` selects the parent
run's placement; `args.inputs.worker_environment` independently selects the
child worker environment and must match the prepared binding:

```bash
create_parent() {
  local label=$1
  local binding=$2
  local intent="$RESULTS_DIR/$label-parent-intent.json"
  local created="$RESULTS_DIR/$label-parent-created.json"
  local parent_id

  jq -n \
    --arg workflow_version_id "$FACTORY_WORKFLOW_VERSION_ID" \
    --arg environment_id "$ENVIRONMENT_ID" \
    --arg goal "$FACTORY_GOAL" \
    --arg factory_ref "$FACTORY_REF" \
    --arg test_command "$TEST_COMMAND" \
    --arg prepared_workflow_binding "$binding" \
    --arg worker_environment "$WORKER_ENVIRONMENT" \
    --arg label "$label" \
    --argjson target "$TARGET_JSON" \
    '{
      workflow_version_id: $workflow_version_id,
      target: $target,
      environment_id: $environment_id,
      goal: $goal,
      args: {
        inputs: {
          factory_ref: $factory_ref,
          test_command: $test_command,
          prepared_workflow_binding: $prepared_workflow_binding,
          worker_environment: $worker_environment
        },
        labels: {"daap-4005-smoke": $label}
      }
    }' > "$intent"

  api -X POST --data-binary @"$intent" \
    "$FABRO_SERVER_URL/api/v1/runs" > "$created"
  parent_id=$(jq -er '.id' "$created")
  api -X POST "$FABRO_SERVER_URL/api/v1/runs/$parent_id/start" \
    > "$RESULTS_DIR/$label-parent-started.json"
  printf '%s\n' "$parent_id"
}
```

Run one case at a time and collect its evidence before creating the next
parent:

```bash
COLD_PARENT_ID=$(create_parent cold '')
printf 'cold_parent_id=%s\n' "$COLD_PARENT_ID"
```

After collecting cold, run the explicitly synthetic negative probe:

```bash
SYNTHETIC_MISS_PARENT_ID=$(create_parent \
  synthetic-miss "$SYNTHETIC_MISS_BINDING")
printf 'synthetic_miss_parent_id=%s\n' "$SYNTHETIC_MISS_PARENT_ID"
```

After collecting the denied synthetic probe, run warm:

```bash
WARM_PARENT_ID=$(create_parent warm "$PREPARED_WORKFLOW_BINDING")
printf 'warm_parent_id=%s\n' "$WARM_PARENT_ID"
```

For each positive parent (`cold`, then `warm`), use the run UI to wait until
exactly one child requests approval. Set `LABEL` and `PARENT_ID` to the active
case, then verify the tagged lifecycle shape:

```bash
LABEL=cold
PARENT_ID=$COLD_PARENT_ID

api "$FABRO_SERVER_URL/api/v1/runs?parent_id=$PARENT_ID&limit=10" \
  > "$RESULTS_DIR/$LABEL-children-before-approval.json"
test "$(jq '.data | length' \
  "$RESULTS_DIR/$LABEL-children-before-approval.json")" -eq 1
CHILD_ID=$(jq -er '.data[0].id' \
  "$RESULTS_DIR/$LABEL-children-before-approval.json")

api "$FABRO_SERVER_URL/api/v1/runs/$CHILD_ID" \
  > "$RESULTS_DIR/$LABEL-child-pending.json"
jq -e '
  .lifecycle.status.kind == "pending" and
  .lifecycle.status.reason == "approval_required" and
  .lifecycle.approval.state == "pending"
' "$RESULTS_DIR/$LABEL-child-pending.json" >/dev/null
```

The operator approves the child explicitly:

```bash
read -r -p "Approve child $CHILD_ID now? Type yes: " answer
test "$answer" = yes
api -X POST "$FABRO_SERVER_URL/api/v1/runs/$CHILD_ID/approve" \
  > "$RESULTS_DIR/$LABEL-child-approved.json"
```

After approval, return to the parent human gate and answer the exact-parent
recheck with `R`. This is required; approving the child alone intentionally
leaves the parent waiting at its native human gate. Then wait in the UI until
both runs are terminal and collect:

```bash
read -r -p "After answering the parent recheck with R and both runs finish, type done: " answer
test "$answer" = done

api "$FABRO_SERVER_URL/api/v1/runs/$CHILD_ID" \
  > "$RESULTS_DIR/$LABEL-child-final.json"
api "$FABRO_SERVER_URL/api/v1/runs/$PARENT_ID" \
  > "$RESULTS_DIR/$LABEL-parent-final.json"
jq -e '.lifecycle.status.kind == "succeeded"' \
  "$RESULTS_DIR/$LABEL-child-final.json" >/dev/null
jq -e '.lifecycle.status.kind == "succeeded"' \
  "$RESULTS_DIR/$LABEL-parent-final.json" >/dev/null

printf '%s\n' "$CHILD_ID" > "$RESULTS_DIR/$LABEL-child-id"
fabro events "$PARENT_ID" --json --server "$FABRO_SERVER_URL" \
  > "$RESULTS_DIR/$LABEL-parent-events.jsonl"
fabro events "$CHILD_ID" --json --server "$FABRO_SERVER_URL" \
  > "$RESULTS_DIR/$LABEL-child-events.jsonl"

python3 docs/internal/daap4005_observed_metrics.py \
  "$RESULTS_DIR/$LABEL-parent-events.jsonl" \
  > "$RESULTS_DIR/$LABEL-parent-observed.json"
python3 docs/internal/daap4005_observed_metrics.py \
  "$RESULTS_DIR/$LABEL-child-events.jsonl" \
  > "$RESULTS_DIR/$LABEL-child-observed.json"
jq -e '.coverage.sequence_gaps == []' \
  "$RESULTS_DIR/$LABEL-parent-observed.json" >/dev/null
jq -e '.coverage.sequence_gaps == []' \
  "$RESULTS_DIR/$LABEL-child-observed.json" >/dev/null
```

Repeat that positive block with `LABEL=warm`,
`PARENT_ID=$WARM_PARENT_ID`.

For `synthetic-miss`, verify one pending child with the same tagged lifecycle
checks, then have the operator deny that child in the UI. Do not approve it or
answer `R`. Wait for both runs to terminate, preserve their events, and verify
the failure is truthful:

```bash
LABEL=synthetic-miss
PARENT_ID=$SYNTHETIC_MISS_PARENT_ID
api "$FABRO_SERVER_URL/api/v1/runs?parent_id=$PARENT_ID&limit=10" \
  > "$RESULTS_DIR/$LABEL-children-before-denial.json"
test "$(jq '.data | length' \
  "$RESULTS_DIR/$LABEL-children-before-denial.json")" -eq 1
CHILD_ID=$(jq -er '.data[0].id' \
  "$RESULTS_DIR/$LABEL-children-before-denial.json")
api "$FABRO_SERVER_URL/api/v1/runs/$CHILD_ID" \
  > "$RESULTS_DIR/$LABEL-child-pending.json"
jq -e '
  .lifecycle.status.kind == "pending" and
  .lifecycle.status.reason == "approval_required" and
  .lifecycle.approval.state == "pending"
' "$RESULTS_DIR/$LABEL-child-pending.json" >/dev/null

read -r -p "Deny synthetic child $CHILD_ID in the UI, wait for both runs to finish, then type denied: " answer
test "$answer" = denied
api "$FABRO_SERVER_URL/api/v1/runs/$CHILD_ID" \
  > "$RESULTS_DIR/$LABEL-child-final.json"
api "$FABRO_SERVER_URL/api/v1/runs/$PARENT_ID" \
  > "$RESULTS_DIR/$LABEL-parent-final.json"
jq -e '.lifecycle.status.kind == "failed"' \
  "$RESULTS_DIR/$LABEL-child-final.json" >/dev/null
jq -e '.lifecycle.status.kind == "failed"' \
  "$RESULTS_DIR/$LABEL-parent-final.json" >/dev/null
fabro events "$PARENT_ID" --json --server "$FABRO_SERVER_URL" \
  > "$RESULTS_DIR/$LABEL-parent-events.jsonl"
fabro events "$CHILD_ID" --json --server "$FABRO_SERVER_URL" \
  > "$RESULTS_DIR/$LABEL-child-events.jsonl"
```

Review structured event evidence, not prompt text:

- cold: no prepared lookup is expected; registration is expected;
- synthetic miss: lookup of the all-zero ID, registration fallback, exactly
  one pending child, operator denial, and truthful terminal failure are
  expected; this is not normal preparation or a success measurement;
- warm: lookup of `EXPECTED_CHILD_VERSION_ID` is expected, with no clone,
  package, or `fabro_workflow_version_create`;
- cold and warm: one child, operator approval, exact-parent recheck `R`, and a
  terminal gather result are required.

If Factory owns lookup in a native command/hook rather than an agent tool,
retain that stage's output/artifact with the HTTP status and returned ID. Keep
that HTTP result separate from the native-agent-tool availability flag.

## Record model counts and timing

`daap4005_observed_metrics.py` deduplicates by `(run_id, stream_seq)`, counts
actual `LlmRequestStarted` records, uses `item.derived` visit boundaries, and
uses top-level `recorded_at` milliseconds. It does not infer unobserved calls.

Initialize the results file with no fabricated rows:

```bash
printf '%s\n' \
  'cache,parent_run_id,child_run_id,dispatch_model_calls,status_model_calls,dispatch_latency_ms,approved_to_start_ms,child_completion_to_parent_detection_ms' \
  > "$RESULTS_DIR/measurements.csv"
```

Append a row only after that case has complete terminal evidence:

```bash
epoch_ms() {
  python3 -c '
from datetime import datetime
import sys
value=sys.argv[1].replace("Z","+00:00")
print(int(datetime.fromisoformat(value).timestamp() * 1000))
' "$1"
}

measure_case() {
  local label=$1
  local parent_id=$2
  local child_id
  local observed="$RESULTS_DIR/$label-parent-observed.json"
  local child="$RESULTS_DIR/$label-child-final.json"
  local dispatch_started
  local child_created
  local approved
  local child_started
  local child_completed
  local parent_detected

  child_id=$(cat "$RESULTS_DIR/$label-child-id")
  dispatch_started=$(jq -er --arg stage "$DISPATCH_STAGE" '
    [.stage_timings[$stage][].started_at_ms] | min
  ' "$observed")
  child_created=$(jq -er '.timestamps.created_at' "$child")
  approved=$(jq -er '.lifecycle.approval.decided_at' "$child")
  child_started=$(jq -er '.timestamps.started_at' "$child")
  child_completed=$(jq -er '.timestamps.completed_at' "$child")
  parent_detected=$(jq -er '
    [
      .tool_events[][]
      | select(.name | endswith("fabro_run_gather"))
      | select(.phase == "completed" and .is_error == false)
      | .at_ms
    ] | max
  ' "$observed")

  printf '%s,%s,%s,%s,%s,%s,%s,%s\n' \
    "$label" \
    "$parent_id" \
    "$child_id" \
    "$(jq -er --arg stage "$DISPATCH_STAGE" \
      '.stages[$stage].observed_events.LlmRequestStarted // 0' "$observed")" \
    "$(jq -er --arg stage "$STATUS_STAGE" \
      '.stages[$stage].observed_events.LlmRequestStarted // 0' "$observed")" \
    "$(( $(epoch_ms "$child_created") - dispatch_started ))" \
    "$(( $(epoch_ms "$child_started") - $(epoch_ms "$approved") ))" \
    "$(( parent_detected - $(epoch_ms "$child_completed") ))"
}

measure_case cold "$COLD_PARENT_ID" >> "$RESULTS_DIR/measurements.csv"
measure_case warm "$WARM_PARENT_ID" >> "$RESULTS_DIR/measurements.csv"
```

Preserve the raw JSONL, observed summaries, run responses, Factory preparation
evidence, and CSV. Redact opaque bindings and credentials before sharing.
These isolated measurements can validate the mechanism and compare cold and
warm behavior; the synthetic miss is separate negative evidence. They are not
a fleet benchmark and do not replace the mandatory final-head native test
gate.
