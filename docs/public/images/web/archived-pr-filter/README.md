# Archived pull request filter evidence

These fixture previews show the existing Runs workspace `In review` component
with safe, synthetic pull requests. They do not use GitHub credentials or
change repository or pull request state.

- `before.png` renders one pull request from an active repository and one from
  an archived repository. The shared count is 2.
- `after.png` renders the active-repository result returned after GitHub applies
  `archived:false`. The shared count is 1.

Both images are labeled as fixture previews. Repository state is included in
each synthetic pull request title and repository name so the before/after
difference is explicit.

From the repository root, capture both images at a 1200 × 800 viewport:

```bash
bun run docs/public/images/web/archived-pr-filter/capture.ts
```

The capture script bundles the real `InReviewColumn` and `InReviewCard`
components from `apps/fabro-web/app/routes/runs.tsx`, builds the existing
Tailwind stylesheet, and takes screenshots with the worker's preinstalled
headless Chromium. Its temporary bundle is created outside the checkout and
removed when capture finishes.
