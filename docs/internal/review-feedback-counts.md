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

An unresolved inline review thread is awaiting a response when it has an
outstanding reviewer comment not referenced by a valid `inline` marker. A later
reviewer follow-up has a new database ID and makes the thread await a response
again until that follow-up receives its own marker. A resolved thread does not
contribute to this count even without a marker; it still remains independent
from whether a response was posted.

Top-level PR conversation comments and non-empty reviewer review summaries are
also feedback, and each is a separate item. An unrelated PR-author comment
cannot clear one: only a valid response marker with the matching kind and
database ID can. Empty review wrappers, including inline-only `COMMENTED`
reviews and empty approvals, do not create feedback. Comments by the PR author
or Factory identity, unrelated bot chatter, deleted/anonymous authors, and
other bot-authored comments are ignored as feedback noise. GitHub's own
resolution state applies only to review threads.

The count is `null`, never zero, when GitHub cannot provide complete evidence.
The collector paginates review threads, thread comments, top-level comments, and
submitted reviews before returning a count. Any partial GraphQL response,
missing requested field, pagination failure, or transport failure makes both
counts unavailable for that pull request rather than publishing partial zeros.
Factory should reply with the typed marker or explicitly resolve the GitHub
thread as appropriate; neither action implies the other.
