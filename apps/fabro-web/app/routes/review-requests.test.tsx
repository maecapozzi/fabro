import { describe, expect, test } from "bun:test";
import TestRenderer, { act } from "react-test-renderer";

import type { ReviewRequestListResponse } from "../lib/queries";
import { ReviewerForm, ReviewRequestsView } from "./review-requests";

const response: ReviewRequestListResponse = {
  data: [
    {
      owner: "maecapozzi",
      repository: "fabro",
      number: 418,
      title: "Add a personal review queue",
      author: "octocat",
      html_url: "https://github.com/maecapozzi/fabro/pull/418",
      draft: false,
      request_kind: "direct_and_team",
      teams: ["maecapozzi/platform"],
      updated_at: "2026-11-13T12:00:00Z",
    },
    {
      owner: "acme",
      repository: "payments",
      number: 73,
      title: "Rotate checkout credentials",
      author: "hubot",
      html_url: "https://github.com/acme/payments/pull/73",
      draft: true,
      request_kind: "team",
      teams: ["acme/security"],
      updated_at: "2026-11-12T12:00:00Z",
    },
  ],
  meta: {
    reviewer: "maecapozzi",
    fetched_at: "2026-11-13T12:01:00Z",
    incomplete: true,
    team_coverage: "limited",
  },
};

describe("ReviewRequestsView", () => {
  test("distinguishes actionable, draft, partial, and stale results", () => {
    let renderer: TestRenderer.ReactTestRenderer | undefined;
    act(() => {
      renderer = TestRenderer.create(
        <ReviewRequestsView
          response={response}
          refreshError={new Error("offline")}
          refreshing={false}
          onRefresh={() => {}}
        />,
      );
    });

    const rendered = JSON.stringify(renderer!.toJSON());
    expect(rendered).toContain("Ready for review");
    expect(rendered).toContain("Drafts");
    expect(rendered).toContain("Direct + maecapozzi/platform");
    expect(rendered).toContain("Team · acme/security");
    expect(rendered).toContain("stale results");
    expect(rendered).toContain("exhaustive result set");
    expect(rendered).toContain("Team coverage is limited");
    expect(renderer!.root.findAllByType("a")[0].props.href).toBe(
      "https://github.com/maecapozzi/fabro/pull/418",
    );
  });

  test("renders a trustworthy empty state only for a successful response", () => {
    let renderer: TestRenderer.ReactTestRenderer | undefined;
    act(() => {
      renderer = TestRenderer.create(
        <ReviewRequestsView
          response={{
            data: [],
            meta: {
              ...response.meta,
              incomplete: false,
              team_coverage: "complete",
            },
          }}
          refreshing={false}
          onRefresh={() => {}}
        />,
      );
    });

    expect(JSON.stringify(renderer!.toJSON())).toContain(
      "No pull requests are awaiting your review",
    );
  });

  test("does not claim the queue is empty when coverage is partial", () => {
    let renderer: TestRenderer.ReactTestRenderer | undefined;
    act(() => {
      renderer = TestRenderer.create(
        <ReviewRequestsView
          response={{
            data: [],
            meta: {
              ...response.meta,
              incomplete: true,
              team_coverage: "limited",
            },
          }}
          refreshing={false}
          onRefresh={() => {}}
        />,
      );
    });

    const rendered = JSON.stringify(renderer!.toJSON());
    expect(rendered).toContain("No review requests found in the available results");
    expect(rendered).toContain("does not prove that your full review queue is empty");
    expect(rendered).not.toContain("No pull requests are awaiting your review");
  });
});

describe("ReviewerForm", () => {
  test("shows a validation error and submits a corrected login", () => {
    let submitted = "";
    let renderer: TestRenderer.ReactTestRenderer | undefined;
    act(() => {
      renderer = TestRenderer.create(
        <ReviewerForm
          value="owner/name"
          error="Reviewer must be a valid GitHub login."
          onChange={() => {}}
          onSubmit={(value) => {
            submitted = value;
          }}
        />,
      );
    });

    expect(JSON.stringify(renderer!.toJSON())).toContain(
      "Reviewer must be a valid GitHub login.",
    );
    expect(renderer!.root.findByType("input").props["aria-invalid"]).toBe(true);

    act(() => {
      renderer!.update(
        <ReviewerForm
          value="mae-capozzi"
          onChange={() => {}}
          onSubmit={(value) => {
            submitted = value;
          }}
        />,
      );
    });
    act(() => {
      renderer!.root.findByType("form").props.onSubmit({
        preventDefault() {},
      });
    });

    expect(submitted).toBe("mae-capozzi");
  });
});
