# Component specs

Own copy, own icons (Lucide, ISC licence), own colours. The accent is a
neutral placeholder until `replica-brand`. Render target: askama templates +
htmx, so every component is semantic HTML plus the classes in `tokens.css`.
No Tailwind. Tokens only, never raw hex.

## Button
  variants  primary, secondary, ghost, danger
  sizes     sm 28px, md 32px, lg 40px
  states    default, hover, active, focus-visible (2px accent ring), disabled, loading (aria-busy, label kept)
  tokens    primary: bg accent, text on-accent; radius md; font 14/600
  a11y      real <button> or <a>; disabled uses aria-disabled when it must stay focusable
  used on   S02, S05, S07, S09, S10, S11

## Form field (input, select, textarea, checkbox, switch)
  parts     label, control, hint, error text
  sizes     control 32px high
  states    default, hover, focus-visible, disabled, error, readonly
  tokens    border border-input (3.84:1 UI contrast), error border danger
  a11y      label for/id, hint linked with aria-describedby, error with role="alert"
  used on   S01, S10, S11, S12, S14, S18

## Sensitive value field
  Write-once variable value. Shows "Sensitive, write only" after save; editing
  replaces, never reveals. Copy button absent by design.
  states    empty, set (masked), replacing
  a11y      masked text announced as "hidden value"
  used on   S10

## Status badge (run, state version, policy)
  One badge per run state. Colour is never the only signal: icon + text.
  map       pending/queued = neutral; planning/applying/fetching/cost = info (spinner icon);
            needs_confirmation/policy_override = warning (hand icon);
            planned_and_finished/applied/policy_checked = success (check);
            errored/hard_failed = danger (x); discarded/canceled = neutral (slash)
  tokens    *-bg tint with same-hue text, pill radius
  used on   S02, S04, S05, S06, S07, S09, S14

## Run stage stepper
  Horizontal list: Pending, Plan, Policy, Cost, Confirm, Apply, Done. Optional
  stages (policy, cost, tasks) are hidden when not enabled.
  states    upcoming, current (aria-current="step"), done, failed, skipped
  a11y      ordered list, text status for each step
  used on   S07

## Log viewer
  Streams plan/apply output over SSE. Monospace 13/20, dark in both themes.
  features  ANSI colours mapped to tokens, timestamps muted, auto-scroll that stops when the user scrolls up, "Jump to latest", resume with Last-Event-ID, copy line, search.
  states    waiting, streaming, complete, truncated (retention), error
  a11y      role="log", aria-live="off" (screen readers read on demand), keyboard scroll
  used on   S07, S08

## Plan diff list
  Rows per resource: create (+, success), update (~, warning), replace (-/+, danger), destroy (-, danger), read (info). Expand shows attribute diff; sensitive values shown as "(sensitive)".
  states    collapsed, expanded, no changes, very large (virtualised)
  used on   S08

## Action bar (confirm / discard / cancel)
  Primary Confirm and secondary Discard with a comment textarea. Visible only
  when the run's `actions` allow it. Confirm asks for a second click when the
  plan contains destroys ("Apply 3 destroys").
  states    enabled, waiting on policy, no permission (hidden, not disabled)
  used on   S07

## Lock banner
  Warning banner on the workspace: who locked it, reason, since when, lease
  end. Unlock button; force-unlock needs admin and a confirm dialog.
  used on   S05, S07, S09

## Data table
  Sticky header, sortable columns, row hover, keyboard row focus, tag filter
  chips, search, cursor pagination (20 default, 100 max).
  states    loading skeleton, empty (with next action), error with retry, filled
  used on   S02, S04, S06, S09, S10, S13, S18

## Tag chip
  Pill, neutral-bg, removable in edit mode. used on S04, S11.

## Modal / confirm dialog
  Native <dialog>, focus trapped by the browser, Esc closes, destructive
  dialogs name the thing ("Delete workspace prod-network") and require typing
  the name for irreversible actions. used on S03, S05, S11, S18.

## Token reveal
  Shown once after creating an API token; copy button and a clear warning; on
  close the secret is gone. used on S18.

## Role matrix
  Rows teams, columns read / plan / write / admin; radio per row; inherited
  grants shown as read-only. used on S13.

## Policy result row
  Outcome badge (passed, advisory failed, soft failed, hard failed,
  overridden), policy set name, message, Override button for users with the
  permission. used on S07, S14.

## Toast
  Bottom right, role="status", 5 s, pause on hover, error toasts stay.

## Empty state
  Short original line, one primary action. Written fresh per screen.

## Direction (v2): dev-tool, dense, minimal
  Body 14px, table rows 36px, controls 32px (sm 28, lg 40), radius 6px, flat
  surfaces separated by 1px borders instead of shadows. IDs, addresses,
  versions and commit hashes are always monospace. Status colour is never the
  only signal (icon + text). Working brand colour is teal `#0b6b63` (dark
  theme `#4fd1c0`), still a placeholder for `replica-brand`.

## App shell
  Top bar 48px: brand, org switcher, global search (Ctrl K), theme.
  Sidebar 224px: Workspaces, Runs (badge "N need you"), Projects, Policies;
  group Organization: Teams, Tokens, Agents, Audit log. Collapses under 960px.
  Content max 1440px, padding 24px. Breadcrumb + page title + primary action
  in the page header.

## Run detail page (the screen users live in)
  Order, top to bottom, so the decision is visible without scrolling:
  1. Title: run ID (mono) + status badge; actions "View config", "Copy run ID".
  2. Lock banner when the run holds the workspace lock: queue depth, lease end.
  3. Stage stepper with per-stage duration (Fetch, Plan, Policy, Cost, Confirm, Apply, Done).
  4. Stat strip: Add / Change / Destroy counts, policy summary, plan time.
  5. Left column: resource changes (colour-tinted rows with +, ~, - and the
     action), then the plan log. Right column: details (trigger, author,
     engine version, mode, config version, input state serial) and policy results.
  6. Sticky action bar: warns about destroys ("Apply 1 destroy?" with the
     resource names), comment field, Discard, Confirm & apply.

## Workspaces list page
  Page header with "New workspace"; toolbar with filter field and active
  filter chips; dense table: workspace (mono link), current run badge,
  resource count, last change, tags. Rows needing attention sort first.

## Layout
  Spacing scale 2/4/8/12/16/24/32/48. Breakpoints 640, 960, 1280. Reduced
  motion respected. Preview: `preview.html` (built from `preview-src.html` +
  `tokens.css`), checked at mobile and desktop width, light and dark.

## Theme
  Light and dark share role names; values live in `tokens.json` and
  `tokens-dark.json`; the user can force either with `data-theme`.

## Contrast report
  Light 22 pairs, dark 22 pairs, 0 AA failures (`contrast.py`).
