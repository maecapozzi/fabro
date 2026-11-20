# Pull-request review feedback counts

Fabro exposes two independent counts in `ReviewReadiness`:

- `unresolved_thread_count` is GitHub's count of unresolved review threads. Fabro
  never changes a thread's resolution state while collecting it.
- `feedback_awaiting_response_count` counts reviewer feedback whose latest
  meaningful comment is still from a reviewer. A reply does not resolve a
  thread, and resolving a thread does not reply to a reviewer.

A meaningful response is a GitHub comment authored by the PR author and marked
with `<!-- factory-feedback:KIND:ID -->`, where `ID` is the exact GitHub node
ID of the feedback item. For a Factory-created PR, native Fabro treats the PR
author as the Factory identity. This is intentionally narrower than a Factory
configuration that recognizes a distinct actor: deployments using that model
must make Factory create the PR as that actor until Fabro carries the configured
actor identity in this API. Other bot comments are ignored as noise.

An inline review thread is awaiting a response when it has an outstanding
reviewer comment not referenced by a Factory marker. A later reviewer comment
has a new ID and therefore reopens the item until its own marker is present.

Top-level PR conversation comments and non-empty reviewer review summaries are
also feedback, and each is a separate item. An unrelated PR-author comment
cannot clear it: only a Factory marker that names that exact comment or review
ID can. Empty approval summaries do not create feedback; unrelated bot chatter
is ignored. GitHub's own resolution state applies only to review threads.

The count is `null`, never zero, when GitHub cannot provide complete evidence.
The collector paginates review threads, thread comments, and top-level comments
before returning a count. Factory should reply as the PR author or resolve the
GitHub thread explicitly as appropriate; neither action implies the other.
