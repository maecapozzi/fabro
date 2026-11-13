import { useState } from "react";
import {
  ArrowPathIcon,
  InboxArrowDownIcon,
} from "@heroicons/react/24/outline";

import { EmptyState, ErrorState, LoadingState, Spinner } from "../components/state";
import { INPUT_CLASS, SECONDARY_BUTTON_CLASS } from "../components/ui";
import { ApiError } from "../lib/api-client";
import { formatAbsoluteTs, formatRelativeTime } from "../lib/format";
import {
  type ReviewRequest,
  type ReviewRequestListResponse,
  useAuthMe,
  useReviewRequests,
} from "../lib/queries";

function requestLabel(request: ReviewRequest): string {
  if (request.request_kind === "direct_and_team") {
    return `Direct + ${request.teams.join(", ")}`;
  }
  if (request.request_kind === "team") {
    return request.teams.length > 0 ? `Team · ${request.teams.join(", ")}` : "Team";
  }
  return "Direct";
}

function ReviewRequestRow({ request }: { request: ReviewRequest }) {
  return (
    <li className="grid gap-3 px-4 py-4 sm:grid-cols-[minmax(0,1fr)_auto] sm:px-6">
      <div className="min-w-0">
        <div className="flex flex-wrap items-center gap-2">
          <a
            href={request.html_url}
            target="_blank"
            rel="noreferrer"
            className="truncate text-sm font-medium text-fg hover:text-teal-300 hover:underline"
          >
            {request.title}
          </a>
          {request.draft ? (
            <span className="rounded-full bg-overlay px-2 py-0.5 text-xs font-medium text-fg-3">
              Draft
            </span>
          ) : null}
        </div>
        <p className="mt-1 text-sm text-fg-3">
          {request.owner}/{request.repository} #{request.number} opened by{" "}
          <span className="text-fg-2">@{request.author}</span>
        </p>
      </div>
      <div className="flex items-center gap-3 text-xs text-fg-muted sm:justify-end">
        <span className="rounded-full border border-line px-2 py-1 text-fg-3">
          {requestLabel(request)}
        </span>
        <time dateTime={request.updated_at} title={formatAbsoluteTs(request.updated_at)}>
          {formatRelativeTime(request.updated_at)}
        </time>
      </div>
    </li>
  );
}

function Notice({ children, tone = "neutral" }: { children: React.ReactNode; tone?: "neutral" | "warning" }) {
  return (
    <p
      role={tone === "warning" ? "status" : undefined}
      className={
        tone === "warning"
          ? "rounded-md border border-amber-400/30 bg-amber-400/10 px-4 py-3 text-sm text-fg-2"
          : "rounded-md border border-line bg-panel px-4 py-3 text-sm text-fg-3"
      }
    >
      {children}
    </p>
  );
}

export function ReviewRequestsView({
  response,
  refreshError,
  refreshing,
  onRefresh,
}: {
  response: ReviewRequestListResponse;
  refreshError?: unknown;
  refreshing: boolean;
  onRefresh: () => void;
}) {
  const actionable = response.data.filter((request) => !request.draft);
  const drafts = response.data.filter((request) => request.draft);

  return (
    <div className="space-y-5">
      <div className="flex flex-wrap items-start justify-between gap-3">
        <div>
          <p className="text-sm text-fg-3">
            Open pull requests currently requesting <span className="text-fg">@{response.meta.reviewer}</span>.
          </p>
          <p className="mt-1 text-xs text-fg-muted">
            Updated <time dateTime={response.meta.fetched_at}>{formatRelativeTime(response.meta.fetched_at)}</time>
          </p>
        </div>
        <button
          type="button"
          onClick={onRefresh}
          disabled={refreshing}
          className={SECONDARY_BUTTON_CLASS}
        >
          {refreshing ? <Spinner className="size-4" /> : <ArrowPathIcon className="size-4" aria-hidden="true" />}
          Refresh
        </button>
      </div>

      {refreshError ? (
        <Notice tone="warning">
          Refresh failed. These are stale results from the last successful update.
        </Notice>
      ) : null}
      {response.meta.incomplete ? (
        <Notice tone="warning">
          GitHub could not return an exhaustive result set. Some review requests may be missing.
        </Notice>
      ) : null}
      {response.meta.team_coverage === "limited" ? (
        <Notice>
          Team coverage is limited. The configured GitHub credential cannot establish all of your team memberships, so team requests may be missing.
        </Notice>
      ) : null}

      {response.data.length === 0 ? (
        <EmptyState
          icon={InboxArrowDownIcon}
          title={
            response.meta.incomplete || response.meta.team_coverage === "limited"
              ? "No review requests found in the available results"
              : "No pull requests are awaiting your review"
          }
          description={
            response.meta.incomplete || response.meta.team_coverage === "limited"
              ? "Coverage is partial, so this result does not prove that your full review queue is empty."
              : "This queue only clears a request when GitHub removes or fulfills your direct or team review request."
          }
        />
      ) : (
        <>
          <section aria-labelledby="actionable-review-requests">
            <h2 id="actionable-review-requests" className="mb-2 text-sm font-medium text-fg">
              Ready for review <span className="text-fg-muted">({actionable.length})</span>
            </h2>
            <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line bg-panel">
              {actionable.map((request) => (
                <ReviewRequestRow key={`${request.owner}/${request.repository}#${request.number}`} request={request} />
              ))}
            </ul>
          </section>
          {drafts.length > 0 ? (
            <section aria-labelledby="draft-review-requests">
              <h2 id="draft-review-requests" className="mb-2 text-sm font-medium text-fg">
                Drafts <span className="text-fg-muted">({drafts.length})</span>
              </h2>
              <p className="mb-2 text-xs text-fg-muted">
                Retained for visibility; drafts are not actionable until marked ready.
              </p>
              <ul className="divide-y divide-line overflow-hidden rounded-lg border border-line bg-panel">
                {drafts.map((request) => (
                  <ReviewRequestRow key={`${request.owner}/${request.repository}#${request.number}`} request={request} />
                ))}
              </ul>
            </section>
          ) : null}
        </>
      )}
    </div>
  );
}

export function ReviewerForm({
  value,
  error,
  onChange,
  onSubmit,
}: {
  value: string;
  error?: string;
  onChange: (value: string) => void;
  onSubmit: (value: string) => void;
}) {
  return (
    <form
      className="mx-auto max-w-md space-y-4 rounded-lg border border-line bg-panel p-6"
      onSubmit={(event) => {
        event.preventDefault();
        const reviewer = value.trim();
        if (reviewer) onSubmit(reviewer);
      }}
    >
      <div>
        <label htmlFor="reviewer" className="text-sm font-medium text-fg">
          GitHub reviewer
        </label>
        <p className="mt-1 text-sm text-fg-3">
          This sign-in does not include a GitHub identity. Enter your GitHub login explicitly.
        </p>
      </div>
      {error ? (
        <p id="reviewer-error" role="alert" className="text-sm text-red-300">
          {error}
        </p>
      ) : null}
      <input
        id="reviewer"
        name="reviewer"
        value={value}
        onChange={(event) => onChange(event.target.value)}
        className={INPUT_CLASS}
        autoComplete="username"
        aria-invalid={error ? true : undefined}
        aria-describedby={error ? "reviewer-error" : undefined}
        required
      />
      <button type="submit" className={SECONDARY_BUTTON_CLASS}>Show review requests</button>
    </form>
  );
}

export default function ReviewRequestsRoute() {
  const auth = useAuthMe();
  const needsReviewer = auth.data?.provider !== "github";
  const [reviewerInput, setReviewerInput] = useState("");
  const [reviewer, setReviewer] = useState<string | undefined>();
  const queue = useReviewRequests(
    needsReviewer ? reviewer : undefined,
    !needsReviewer || Boolean(reviewer),
  );
  const reviewerError =
    needsReviewer &&
    reviewer &&
    !queue.data &&
    queue.error instanceof ApiError &&
    queue.error.status === 400
      ? queue.error.message
      : undefined;

  if (needsReviewer && (!reviewer || reviewerError)) {
    return (
      <ReviewerForm
        value={reviewerInput}
        error={reviewerError}
        onChange={setReviewerInput}
        onSubmit={(value) => {
          if (value === reviewer) {
            void queue.mutate();
          } else {
            setReviewer(value);
          }
        }}
      />
    );
  }

  if (queue.isLoading && !queue.data) {
    return <LoadingState label="Loading review requests…" />;
  }
  if (queue.error && !queue.data) {
    const unavailable = queue.error instanceof ApiError && queue.error.status === 503;
    const rateLimited = queue.error instanceof ApiError && queue.error.status === 429;
    return (
      <ErrorState
        title={
          unavailable
            ? "GitHub connection unavailable"
            : rateLimited
              ? "GitHub rate limit reached"
              : "Couldn't load review requests"
        }
        description={
          queue.error instanceof ApiError
            ? queue.error.message
            : "GitHub review requests could not be loaded. This is not an empty queue."
        }
        onRetry={() => void queue.mutate()}
      />
    );
  }
  if (!queue.data) return null;

  return (
    <ReviewRequestsView
      response={queue.data}
      refreshError={queue.error}
      refreshing={queue.isValidating}
      onRefresh={() => void queue.mutate()}
    />
  );
}
