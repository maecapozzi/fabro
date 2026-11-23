# Pull-request review feedback counts

Fabro exposes two independent counts in `ReviewReadiness`:

- `unresolved_thread_count` is GitHub's count of unresolved review threads. Fabro
  never changes a thread's resolution state while collecting it.
- `feedback_awaiting_response_count` counts unresolved review threads and
  top-level reviewer items that have not been handled by either a response or
  GitHub thread resolution. A response does not resolve a thread, and thread
  resolution is never treated as a response.

A meaningful response is a GitHub comment authored by either the pull request
author or the authenticated Factory GitHub identity and marked with
`<!-- factory-feedback:KIND:REST_NUMERIC_ID -->`. The supported kinds are
`comment` for a top-level pull-request conversation comment, `inline` for a
review-thread comment, and `review` for a submitted review summary. The ID is
the target item's positive numeric GitHub REST/database ID; GraphQL node IDs,
unknown kinds, malformed markers, and markers from other actors are ignored.
Fabro resolves the Factory identity from the GitHub credential used to collect
the pull request, so it may be distinct from the pull request author.

Fabro selects the latest genuine external inline comment in each thread after
loading every page of that thread's history. A valid marker handles only the
targeted comment revision it follows: when the reviewer edits that same REST ID
after the response, the feedback reopens until a newer marker follows the
edit. GitHub resolution clears the prior thread obligation, but a later genuine
reviewer follow-up after a recognized Factory response reopens it even if
GitHub leaves the thread marked resolved.

Top-level PR conversation comments and non-empty reviewer review summaries are
also feedback, and each is a separate item. An unrelated PR-author comment
cannot clear one: only a valid response marker with the matching kind,
database ID, and later response timestamp can. Empty review wrappers,
including inline-only `COMMENTED` reviews and empty approvals, do not create
feedback. Comments by the PR author or Factory identity are self activity.
Routine external status chatter is not an obligation, but a substantive
finding from a distinct automated code-review bot is reviewer feedback.
Deleted or anonymous authors are ignored because their identity cannot be
classified safely. GitHub's own resolution state applies only to review
threads.

The classification is deterministic and does not depend on a bot's name. A
non-empty, visible comment from an external human is feedback. A bot comment is
feedback only when it has a concrete concern (such as authorization, injection,
a regression, or unsafe behavior) or a specific request to change code.
Behavioral findings such as a missing check or accepting input without
validation also count. Leading HTML metadata and URLs are ignored, so metadata
cannot hide a visible finding and a status-dashboard URL cannot invent one.
Known no-findings, completed-review, and deployment status messages are routine
noise. This keeps actual automated review findings visible while avoiding
obligations from status bots.

The count is `null`, never zero, when GitHub cannot provide complete evidence.
The collector paginates review threads, thread comments, top-level comments, and
submitted reviews before returning a count. Any partial GraphQL response,
missing requested field, pagination failure, or transport failure makes both
counts unavailable for that pull request rather than publishing partial zeros.
Factory should reply with the typed marker or explicitly resolve the GitHub
thread as appropriate; neither action implies the other.
