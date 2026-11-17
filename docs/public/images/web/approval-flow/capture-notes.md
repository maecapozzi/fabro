# Approval flow preview capture

These are synthetic before/after previews. They contain no live Slack messages,
live run IDs, credentials, or private host data.

The Fabro header preview renders the production `RunDetailHeader` and
`buildRunDetailRun` components with a safe pending-approval fixture. The
"before" panel preserves the baseline Approve action while reproducing the old
generic `Pending` label and shortened repository name; the "after" panel uses
the current `Needs your approval` mapping and full repository identity. The
parent-prompt preview renders the production
`RunDockShell` for the synthetic before state and the final production
`InterviewDock` for the after state. The Slack preview mirrors the Block Kit
copy and actions covered by the `fabro-slack` launch-approval tests.

Reproduce from the repository root:

```sh
(cd apps/fabro-web && bun run build)
(cd apps/fabro-web && bun build scripts/approval-flow-preview.tsx \
  --entry-naming preview.js \
  --outdir /tmp/fabro-approval-preview --target browser
)
cp docs/public/images/web/approval-flow/preview.html /tmp/fabro-approval-preview/
cp apps/fabro-web/dist/assets/app-*.css /tmp/fabro-approval-preview/app.css
cp docs/public/images/web/approval-flow/preview.css /tmp/fabro-approval-preview/
/opt/playwright/chromium_headless_shell-1243/chrome-headless-shell-linux-arm64/chrome-headless-shell \
  --headless --no-sandbox --disable-gpu --allow-file-access-from-files --hide-scrollbars \
  --user-data-dir=/tmp/fabro-approval-preview/slack-browser-profile \
  --window-size=1440,760 \
  --screenshot=docs/public/images/web/approval-flow/slack-before-after.png \
  'file:///tmp/fabro-approval-preview/preview.html?kind=slack'
/opt/playwright/chromium_headless_shell-1243/chrome-headless-shell-linux-arm64/chrome-headless-shell \
  --headless --no-sandbox --disable-gpu --allow-file-access-from-files --hide-scrollbars \
  --user-data-dir=/tmp/fabro-approval-preview/fabro-browser-profile \
  --window-size=1440,760 \
  --screenshot=docs/public/images/web/approval-flow/fabro-before-after.png \
  'file:///tmp/fabro-approval-preview/preview.html?kind=fabro'
mkdir -p /tmp/fabro-approval-preview/interview-browser-profile
/opt/playwright/chromium_headless_shell-1243/chrome-headless-shell-linux-arm64/chrome-headless-shell \
  --headless --no-sandbox --disable-gpu --allow-file-access-from-files --hide-scrollbars \
  --user-data-dir=/tmp/fabro-approval-preview/interview-browser-profile \
  --window-size=1440,760 \
  --screenshot=docs/public/images/web/approval-flow/fabro-interview-before-after.png \
  'file:///tmp/fabro-approval-preview/preview.html?kind=interview'
```

The capture wrapper adds `preview.css` after the application stylesheet and
uses a local HTML shell with `<div id="root"></div>`.
