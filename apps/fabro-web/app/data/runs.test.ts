import { describe, expect, test } from "bun:test";
import type { Run, RunStatus as ApiRunStatus } from "@qltysh/fabro-api-client";
import {
  columnForStatus,
  columnStatusDisplay,
  isRunStatus,
  mapRunListItem,
  mapRunToRunItem,
  runStatusDisplay,
} from "./runs";
import { TEST_PRINCIPAL, makeUsage } from "../lib/test-fixtures";

function makeRun(overrides: Partial<Run> = {}): Run {
  return {
    id:               "01ABC",
    goal:             "Fix the build",
    title:            "Fix the build",
    workflow:         { slug: "fix_build", name: "Fix Build", graph_name: "FixBuild", node_count: 0, edge_count: 0 },
    automation:       null,
    repository:       { name: "myrepo", origin_url: null, provider: "unknown" },
    created_by:       TEST_PRINCIPAL,
    origin:           { kind: "api" },
    labels:           {},
    lifecycle:        {
      status:          { kind: "running" },
      approval:        null,
      pending_control: null,
      queue_position:  null,
      error:           null,
      archived:        false,
      archived_at:     null,
    },
    sandbox:          null,
    models:           [],
    source_directory: "/home/user/myrepo",
    timestamps:       {
      created_at:     "2026-04-08T12:00:00Z",
      started_at:     "2026-04-08T12:00:00Z",
      last_event_at:  null,
      completed_at:   null,
    },
    timing:           {
      wall_time_ms:      65000,
      inference_time_ms: 0,
      tool_time_ms:      0,
      active_time_ms:    0,
    },
    usage:            makeUsage({}, 500000),
    size:             "XS",
    diff:             null,
    pull_request:     null,
    current_question: null,
    superseded_by:    null,
    retried_from:     null,
    links:            { web: null },
    ...overrides,
  };
}

function withStatus(status: ApiRunStatus): Pick<Run, "lifecycle"> {
  return {
    lifecycle: {
      status,
      approval: null,
      pending_control: null,
      queue_position:  null,
      error:           null,
      archived:        false,
      archived_at:     null,
    },
  };
}

describe("mapRunListItem", () => {
  test("trusts shared server fields for board items", () => {
    const summary = makeRun({
      title:         "Server supplied title",
      ...withStatus({ kind: "paused", prior_block: null }),
      pull_request: {
        owner: "fabro-sh",
        repo: "fabro",
        number: 123,
        html_url: "https://github.com/fabro-sh/fabro/pull/123",
      },
    });
    summary.lifecycle.pending_control = "cancel";
    const item = mapRunListItem(summary);
    expect(item.id).toBe("01ABC");
    expect(item.title).toBe("Server supplied title");
    expect(item.workflow).toBe("Fix Build");
    expect(item.repo).toBe("myrepo");
    expect(item.sourceDirectory).toBe("/home/user/myrepo");
    expect(item.elapsed).toBeDefined();
    expect(item.column).toBe("running");
    expect(item.lifecycleStatus).toBe("paused");
    expect(item.number).toBe(123);
    expect(item.pullRequestUrl).toBe("https://github.com/fabro-sh/fabro/pull/123");
    expect(item.pendingControl).toBe("cancel");
  });

  test("uses a fallback title when the server title is blank", () => {
    const summary = makeRun({ id: "01EMPTY", goal: "", title: "" });

    expect(mapRunListItem(summary).title).toBe("Untitled run");
  });

  test("keeps the repository owner for an unambiguous approval identity", () => {
    const summary = makeRun({
      repository: { name: "maecapozzi/fabro", origin_url: null, provider: "github" },
    });

    expect(mapRunListItem(summary).repo).toBe("maecapozzi/fabro");
  });

  test("carries the billed total so the size chip can show it on hover", () => {
    expect(mapRunListItem(makeRun()).totalUsdMicros).toBe(500000);
  });

  test("leaves the cost undefined for runs whose usage carries none", () => {
    expect(mapRunListItem(makeRun({ usage: makeUsage() })).totalUsdMicros).toBeUndefined();
    expect(
      mapRunListItem(makeRun({ usage: makeUsage({ input: 12 }) })).totalUsdMicros,
    ).toBeUndefined();
  });

  test("distinguishes approval, queue, running, and denied native states", () => {
    const approvalRun = makeRun({
      ...withStatus({ kind: "pending", reason: "approval_required" }),
    });
    approvalRun.lifecycle.approval = {
      state: "pending",
      requested_at: "2026-04-08T12:00:00Z",
      decided_at: null,
      denial_reason: null,
    };
    const approval = mapRunListItem(approvalRun);
    const queued = mapRunListItem(makeRun({
      ...withStatus({ kind: "runnable" }),
    }));
    const running = mapRunListItem(makeRun({
      ...withStatus({ kind: "running" }),
    }));
    const denied = mapRunListItem(makeRun({
      ...withStatus({ kind: "failed", reason: "approval_denied" }),
    }));
    const archivedDeniedRun = makeRun({
      ...withStatus({ kind: "failed", reason: "approval_denied" }),
    });
    archivedDeniedRun.lifecycle.archived = true;
    archivedDeniedRun.lifecycle.archived_at = "2026-04-08T13:00:00Z";
    const archivedDenied = mapRunListItem(archivedDeniedRun);

    expect(approval.lifecycleStatusLabel).toBe("Needs your approval");
    expect(approval.pendingApproval).toBe(true);
    expect(queued.lifecycleStatusLabel).toBe("Queued");
    expect(running.lifecycleStatusLabel).toBe("Running");
    expect(denied.lifecycleStatusLabel).toBe("Denied");
    expect(denied.pendingApproval).toBe(false);
    expect(archivedDenied.lifecycleStatusLabel).toBe("Archived");
    expect(archivedDenied.pendingApproval).toBe(false);
  });
});

describe("mapRunToRunItem", () => {
  test("maps canonical run summary to RunItem", () => {
    const summary = makeRun({
      pull_request: {
        owner: "fabro-sh",
        repo: "fabro",
        number: 456,
        html_url: "https://github.com/fabro-sh/fabro/pull/456",
      },
    });
    const item = mapRunToRunItem(summary);
    expect(item.id).toBe("01ABC");
    expect(item.title).toBe("Fix the build");
    expect(item.workflow).toBe("Fix Build");
    expect(item.repo).toBe("myrepo");
    expect(item.sourceDirectory).toBe("/home/user/myrepo");
    expect(item.elapsed).toBeDefined();
    expect(item.lifecycleStatus).toBe("running");
    expect(item.number).toBe(456);
    expect(item.pullRequestUrl).toBe("https://github.com/fabro-sh/fabro/pull/456");
  });

  test("handles missing optional fields", () => {
    const summary = makeRun({
      id:               "01DEF",
      goal:             "",
      title:            "",
      workflow:         { slug: null, name: null, graph_name: null, node_count: 0, edge_count: 0 },
      source_directory: null,
      repository:       { name: "unknown", origin_url: null, provider: "unknown" },
      ...withStatus({ kind: "submitted" }),
      timestamps:       {
        created_at:     "2026-04-08T12:00:00Z",
        started_at:     null,
        last_event_at:  null,
        completed_at:   null,
      },
      timing:           null,
      usage:          makeUsage(),
    });
    const item = mapRunToRunItem(summary);
    expect(item.id).toBe("01DEF");
    expect(item.title).toBe("Untitled run");
    expect(item.workflow).toBe("unknown");
    expect(item.repo).toBe("unknown");
    expect(item.sourceDirectory).toBeUndefined();
  });

  test("falls back to graph name and slug for workflow labels", () => {
    const graphFallback = mapRunToRunItem(
       makeRun({ workflow: { slug: "fix_build", name: null, graph_name: "FixBuild", node_count: 0, edge_count: 0 } }),
    );
    const slugFallback = mapRunToRunItem(
       makeRun({ workflow: { slug: "fix_build", name: null, graph_name: null, node_count: 0, edge_count: 0 } }),
    );

    expect(graphFallback.workflow).toBe("FixBuild");
    expect(slugFallback.workflow).toBe("fix_build");
  });

  test("recognizes canonical blocked, pending, and runnable run statuses", () => {
    expect(isRunStatus("pending")).toBe(true);
    expect(isRunStatus("runnable")).toBe(true);
    expect(isRunStatus("blocked")).toBe(true);
    expect(runStatusDisplay).toHaveProperty("pending");
    expect(runStatusDisplay).toHaveProperty("runnable");
    expect(runStatusDisplay).toHaveProperty("blocked");
  });

  test("recognizes archived as a terminal run status", () => {
    expect(isRunStatus("archived")).toBe(true);
    expect(runStatusDisplay).toHaveProperty("archived");
  });

  test("uses blocked board column instead of waiting", () => {
    expect(columnStatusDisplay).toHaveProperty("blocked");
    expect(columnStatusDisplay).not.toHaveProperty("waiting");
  });
});

describe("columnForStatus", () => {
  test("returns null for lifecycle states that do not map to a board column", () => {
    expect(columnForStatus("removing")).toBeNull();
  });
});
