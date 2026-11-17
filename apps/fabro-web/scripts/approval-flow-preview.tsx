import React from "react";
import { createRoot } from "react-dom/client";
import { MemoryRouter } from "react-router";
import { SWRConfig } from "swr";

import { InterviewDock } from "../app/components/interview-dock";
import {
  DOCK_CHOICE_BUTTON,
  RunDockShell,
} from "../app/components/run-dock";
import { buildRunDetailRun } from "../app/routes/run-detail/model";
import { RunDetailHeader } from "../app/routes/run-detail/header";

const pendingRun = {
  id: "synthetic-approval",
  goal: "Improve approval actions",
  title: "Improve approval actions",
  workflow: {
    slug: "change",
    name: "Change",
    graph_name: "Change",
    node_count: 4,
    edge_count: 3,
  },
  automation: null,
  repository: {
    name: "maecapozzi/fabro",
    origin_url: "https://github.com/maecapozzi/fabro",
    provider: "github",
  },
  created_by: {
    kind: "user",
    identity: { issuer: "https://github.com", subject: "synthetic-user" },
    login: "mae",
    auth_method: "github",
  },
  origin: { kind: "api" },
  labels: {},
  lifecycle: {
    status: { kind: "pending", reason: "approval_required" },
    approval: {
      state: "pending",
      requested_at: "2026-11-17T15:00:00Z",
      decided_at: null,
      denial_reason: null,
    },
    pending_control: null,
    queue_position: null,
    error: null,
    archived: false,
    archived_at: null,
  },
  sandbox: null,
  models: [],
  source_directory: null,
  timestamps: {
    created_at: "2026-11-17T15:00:00Z",
    started_at: null,
    last_event_at: "2026-11-17T15:00:00Z",
    completed_at: null,
  },
  timing: null,
  usage: {
    input_tokens: 0,
    output_tokens: 0,
    cache_read_tokens: 0,
    cache_creation_tokens: 0,
    reasoning_tokens: 0,
    total_tokens: 0,
    model: {},
    cost: null,
  },
  size: "XS",
  diff: null,
  pull_request: null,
  current_question: null,
  superseded_by: null,
  retried_from: null,
  links: { web: "https://fabro.example/runs/synthetic-approval" },
} as any;

const menu = {
  runId: pendingRun.id,
  groups: { operations: [], lifecycle: [], destructive: [] },
};

const launchNavigationQuestion = {
  id: "synthetic-parent-gate",
  text:
    "CHILD LAUNCH APPROVAL — Open the linked child from this run's Children tab and approve or deny its launch there.",
  stage: "approval_wait@synthetic",
  question_type: "multiple_choice",
  options: [
    {
      key: "recheck_child",
      label: "Recheck whether the child was approved and continue orchestration",
    },
    {
      key: "stop_parent",
      label: "Stop the parent orchestrator",
    },
  ],
  allow_freeform: false,
  timeout_seconds: null,
  context_display: JSON.stringify({
    preferred_next_label: "Recheck child launch approval",
    child_run_id: "synthetic-child",
    child_run_url: "https://fabro.example/runs/synthetic-child",
    child_status: "pending_approval",
  }),
  review_target: null,
} as any;

function FabroPanel({ after }: { after: boolean }) {
  const mapped = buildRunDetailRun(pendingRun);
  const run = after
    ? mapped
    : {
        ...mapped,
        repo: "fabro",
        statusLabel: "Pending",
        statusDot: "bg-fg-muted",
        statusText: "text-fg-muted",
      };
  return (
    <section className="preview-panel">
      <div className="preview-label">Synthetic {after ? "AFTER" : "BEFORE"}</div>
      <RunDetailHeader
        runId={pendingRun.id}
        run={run}
        summary={pendingRun}
        fullHeight={false}
        now={Date.parse("2026-11-17T15:02:00Z")}
        actions={{
          approval: { visible: true, pending: false, onApprove: () => {} },
          menu,
          askTrigger: null,
        }}
      />
      {!after && (
        <p className="preview-note">
          The existing Approve action was present, but the generic label and shortened repository
          obscured why the run was waiting.
        </p>
      )}
      {after && (
        <p className="preview-note">
          The same action now sits beside the explicit native state and full repository identity.
        </p>
      )}
    </section>
  );
}

function SlackPanel({ after }: { after: boolean }) {
  return (
    <section className="preview-panel slack-shell">
      <div className="preview-label">Synthetic {after ? "AFTER" : "BEFORE"}</div>
      <div className="slack-message">
        <div className="slack-avatar">F</div>
        <div>
          <div className="slack-author">Fabro <span>APP · 3:00 PM</span></div>
          {after ? (
            <>
              <p><strong>Fabro needs your approval to start this task.</strong></p>
              <p><strong>Task:</strong> Improve approval actions</p>
              <p><strong>Repository:</strong> maecapozzi/fabro</p>
              <p>Approve to let Fabro queue the task for execution, or deny to stop it.</p>
              <a href="#">Open in Fabro</a>
              <div className="slack-actions">
                <button className="approve">Approve</button>
                <button className="deny">Deny</button>
              </div>
            </>
          ) : (
            <>
              <p><strong>CHILD LAUNCH APPROVAL</strong> — Open the linked child and approve it.</p>
              <pre>{`{"child_run_id":"01J…","child_status":"pending_approval"}`}</pre>
              <button>Recheck child launch approval</button>
            </>
          )}
        </div>
      </div>
    </section>
  );
}

function InterviewPanel({ after }: { after: boolean }) {
  return (
    <section className="preview-panel interview-preview">
      <div className="preview-label">Synthetic {after ? "AFTER" : "BEFORE"}</div>
      {after ? (
        <InterviewDock
          runId="synthetic-parent"
          questions={[launchNavigationQuestion]}
        />
      ) : (
        <RunDockShell
          label="Interview question"
          tone="waiting"
          status="Awaiting input"
          stage="approval_wait@synthetic"
          peek={launchNavigationQuestion.text}
          collapsed={false}
          onCollapsedChange={() => {}}
          body={
            <>
              <p className="text-base/6 font-medium text-fg">
                {launchNavigationQuestion.text}
              </p>
              <pre className="legacy-routing-json">
                {launchNavigationQuestion.context_display}
              </pre>
            </>
          }
          actions={
            <>
              <button type="button" className={DOCK_CHOICE_BUTTON}>
                Recheck whether the child was approved and continue orchestration
              </button>
              <button type="button" className={DOCK_CHOICE_BUTTON}>
                Stop the parent orchestrator
              </button>
            </>
          }
        />
      )}
      <p className="preview-note">
        {after
          ? "The live InterviewDock keeps Check again as navigation and hides machine routing data."
          : "The old prompt looked like an approval gate and exposed machine routing data."}
      </p>
    </section>
  );
}

function Preview() {
  const kind = new URLSearchParams(window.location.search).get("kind");
  const title =
    kind === "slack"
      ? "Slack approval message"
      : kind === "interview"
        ? "Fabro parent navigation prompt"
        : "Fabro run approval";
  return (
    <MemoryRouter>
      <SWRConfig value={{ provider: () => new Map(), dedupingInterval: 0 }}>
        <main>
          <h1>{title}</h1>
          <div className="preview-grid">
            {kind === "slack" ? (
              <>
                <SlackPanel after={false} />
                <SlackPanel after />
              </>
            ) : kind === "interview" ? (
              <>
                <InterviewPanel after={false} />
                <InterviewPanel after />
              </>
            ) : (
              <>
                <FabroPanel after={false} />
                <FabroPanel after />
              </>
            )}
          </div>
        </main>
      </SWRConfig>
    </MemoryRouter>
  );
}

createRoot(document.getElementById("root")!).render(<Preview />);
