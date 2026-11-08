# Bugs

Found by `/replica-test` on 2026-11-08 (B01 to B09), by running the real `tofu` driver the same day (B10 to B12), and by testing force-cancel against a real apply (B13). Every bug below was reproduced, then
given a failing test, then fixed, then the test went green. Nothing is
committed yet: "fixed" means fixed in the working tree with its test passing.
Open S1: none. Open S2: none.

| ID | sev | title | status |
| --- | --- | --- | --- |
| B01 | S1 | Force-unlock leaves the run that held the lock still active; on PostgreSQL the queue of every workspace stalls | fixed, tested |
| B02 | S2 | A run interrupted by a server restart stays "planning" forever and keeps its workspace locked | fixed, tested |
| B03 | S2 | Under 960 px the main navigation is hidden, so Runs, Tokens and Account are unreachable | fixed, measured |
| B04 | S3 | At 390 px every page scrolls sideways and "Sign out" is off screen | fixed, measured |
| B05 | S3 | Double-clicking "Confirm & apply" lands on a 409 error page | fixed, tested |
| B06 | S3 | No server-side limit on run message or configuration size | fixed, tested |
| B07 | S3 | Sign-out and sign-in forms can be forged from another website | fixed, tested |
| B08 | S4 | Malformed API JSON gets a plain-text error, not the JSON error envelope | fixed, tested |
| B09 | S4 | Sign-in, sign-up and reset pages have no `<h1>` | fixed, tested |
| B10 | S1 | The `tofu` child inherited the server's environment: a provisioner could print `TF_MASTER_KEY` and `DATABASE_URL` | fixed, tested with the real binary |
| B11 | S2 | A `backend` block in a run's configuration was honoured, so state could be sent to a server the author chose | fixed, tested with the real binary |
| B12 | S3 | A plan that only changed outputs was reported as "no changes" and never applied | fixed, tested with the real binary |
| B13 | S2 | Killing `tofu` mid-apply leaves an empty state file: every resource it had finished is unrecorded | fixed in containers and on Unix; Windows process mode cannot be fixed, documented |

---

## B01 (S1) Force-unlock poisons the run queue

**Steps.** Queue a run on workspace A and let it reach "Needs confirmation" (it holds A's lock). Force-unlock A. Queue a second run on A.

**Expected.** The run that held the lock ends (discarded, or canceled if it had not finished planning). The second run starts. Other workspaces are unaffected.

**Actual.** The first run stayed active with no lock. On PostgreSQL the second run could not start because the database refuses two lock-holding runs on one workspace; `claim_next` failed with `that already exists` on every poll (every 500 ms), always on the same oldest run, so **no workspace's queued run was ever started again**. In the in-memory store the same sequence ran two lock-holding runs at once.

**Evidence.** Real server on PostgreSQL: `runner: claim failed: that already exists` six times in a few seconds; status of the new run stayed `pending` after force-unlock. Tests that failed before the fix: `force_unlock_ends_the_run_that_held_the_lock` (both stores), `one_poisoned_workspace_cannot_starve_the_queue` (PostgreSQL: `claiming must not fail because of another workspace: Conflict("that already exists")`), `f05_e1_force_unlock_ends_the_run_that_held_the_lock`.

**Fix.** Force-unlock now ends the run holding the lock (discards it while it waits for confirmation, cancels it before it applies) and refuses while a run is applying. `claim_next` also skips any workspace that still has an active lock-holding run, so one inconsistent workspace can never block the others.

## B02 (S2) Interrupted runs never finish

**Steps.** Queue a run with a slow engine. Kill the server while it is planning. Start it again.

**Expected.** The run is marked failed ("interrupted"), the lock is released, new runs start.

**Actual.** The run stayed `planning` forever, the workspace stayed locked, a newly queued run stayed `pending`, and force-unlock did not unstick it (see B01).

**Evidence.** Real server: after restart `run status = planning | workspace lock = {owner: run:...}`; a new run `pending`. Test `interrupted_runs_are_recovered` (written against the new `recover_interrupted`; the live reproduction above is the "before").

**Fix.** `Store::recover_interrupted`, called at startup before the runner: runs left `planning` or `applying` become `errored` with a log line, locks are released. Runs waiting for a person (`needs_confirmation`) and runs not yet started are left alone. **Limit:** it assumes one server instance; with several, a starting instance would error another's live runs (needs heartbeats first).

## B03 (S2) No navigation on small screens

**Steps.** Open any page below 960 px wide.

**Expected.** The same links (Workspaces, Runs, API tokens, Account) are reachable.

**Actual.** The sidebar was `display: none` and nothing replaced it.

**Evidence.** Measured in the browser: `nav display = none` at 390 px and 800 px. Guard test `mobile_e1_the_stylesheet_never_hides_navigation_on_small_screens`.

**Fix.** Under 960 px the sidebar becomes a scrollable strip under the top bar.

## B04 (S3) Pages overflow at phone width

**Steps.** Open any signed-in page at 390 px.

**Actual.** `scrollWidth` 528 px on most pages and 784 px on API tokens; the "Sign out" button's right edge was at 512 px (off screen). Cause: the top bar did not wrap, grid tracks stretched to fit wide content, the tokens toolbar did not wrap, tables could not scroll.

**Fix.** Top bar wraps and hides the email and engine chips under 640 px, content may shrink (`min-width: 0`), toolbars wrap, tables scroll inside their frame.

**Evidence after.** Browser measurement of 8 signed-in pages at 390, 800 and 1440 px: 0 of 24 overflow, all four navigation links and "Sign out" in view.

## B05 (S3) Double click on Confirm shows an error

**Steps.** Click "Confirm & apply" twice quickly (or refresh a stale tab after applying).

**Actual.** The second request got a 409 "That action is not possible right now" page.

**Expected.** Land on the run page; the action already happened.

**Fix.** Web run actions are idempotent when the run is already in the requested state. The JSON API stays strict (409), so scripts still learn the truth. Test `f02_e1_double_clicking_confirm_is_not_an_error_page`.

## B06 (S3) Unbounded message and configuration

**Steps.** POST a run with a 5,000-character message.

**Actual.** Accepted and stored; shown in full in lists. **Expected** a 422.

**Fix.** Message at most 200 characters, configuration at most 1 MB, enforced for pages and API. Oversized request bodies become the JSON error envelope. Test `f02_e4_oversized_message_and_config_are_refused_cleanly`.

## B07 (S3) Login and logout CSRF

**Steps.** From another origin, POST to `/logout` or `/login` with the victim's browser.

**Actual.** `/logout` signed the victim out; `/login` accepted a form from any site (an attacker could sign a victim in as the attacker).

**Fix.** Every state-changing request that names an `Origin` must name this host; foreign and opaque (`null`) origins get 403, before any other handling. Requests without an `Origin` (scripts using bearer tokens) are unaffected. Test `auth_e4_cross_site_posts_cannot_log_you_in_or_out`.

## B08 (S4) API errors in two formats

**Steps.** POST `{not json` to `/api/v2/workspaces`.

**Actual.** Plain text `Failed to parse the request body as JSON...`. **Expected** `{"errors":[{"status":"422","title":...}]}` like every other API error.

**Fix.** `ApiJson` extractor. Test `api_e1_malformed_json_gets_the_json_error_envelope` (malformed JSON, wrong type, missing content type, wrong content type).

## B09 (S4) Public pages without a heading

**Actual.** Found by a script-based accessibility scan of 11 pages in the browser: `/login`, `/signup`, `/forgot` had no `<h1>` (the title was a `div`).

**Fix.** Card titles on public pages are `<h1>`. Test `a11y_e1_every_page_has_exactly_one_h1_and_labelled_controls` (15 pages, also checks the language attribute and that every control has a label).

## B10 (S1) The server's secrets reach whoever writes a configuration

**Steps.** With `TF_ENGINE=tofu`, queue a run whose configuration has `provisioner "local-exec" { command = "echo %TF_MASTER_KEY% %DATABASE_URL%" }`.

**Expected.** The server's secrets are not visible to code a run executes.

**Actual.** The log printed the master key, the database URL and an unrelated canary variable: `tofu` and everything it starts inherited the server's whole environment.

**Evidence.** `provisioners_cannot_read_the_servers_secrets` against OpenTofu 1.13.9: failed with `seen=canary-8f3a91 key=canary-master-77c2 db=postgres://canary-db-5d10`.

**Fix.** The child environment is built from nothing (path and shell variables, the run's own variables, and sandbox-owned `HOME`, temp and CLI-config paths). In container mode only variables passed by name enter the container. Also tested end to end through the platform (`real_tofu_e1_...`).

## B11 (S2) A configuration could redirect the run's state

**Steps.** Queue a run whose configuration contains `terraform { backend "http" { address = "..." } }`.

**Actual.** `tofu init` used that backend (the run failed against an unreachable address; against a reachable one the state, which can hold secrets, would be read from and written to the author's server instead of the platform's store).

**Fix.** Every run gets a `platform_override.tf` that pins a local backend; the platform alone decides where state lives. Test `a_backend_block_in_the_configuration_cannot_redirect_state`.

## B12 (S3) Output-only changes were never applied

**Steps.** Apply a configuration, then add a new `output` block and queue a run.

**Actual.** The plan counted resources only, so it was reported "no changes" and finished without apply: the output never existed in state.

**Fix.** `PlanSummary.outputs` counts changed outputs; a plan with only output changes pauses for confirmation like any other. Shown on the run page. Test `a_change_that_only_touches_outputs_is_still_a_change`, and the same case through the platform.

## B13 (S2) A hard kill throws away what the apply had done

**Steps.** Apply a configuration with two resources, one instant and one with a long `local-exec`. Kill `tofu` after four seconds, once the first resource says `Creation complete`.

**Expected.** The record of the finished resource survives.

**Actual.** `terraform.tfstate` is **0 bytes**. tofu keeps state in memory and writes it out at the end or when asked to stop. A force-cancel that just kills the process would leave the platform with no record of resources that really exist, and the next plan would create them a second time.

**Evidence.** Real OpenTofu 1.13.9 on Windows (state size 0 after the kill). The same run in a container with `docker kill --signal=INT` instead: tofu exits in about one second, kills the provisioner, and the file holds serial 3 with the finished resource and the interrupted one marked `tainted`.

**Fix.** On a stop the engine first asks tofu to stop (Ctrl-C equivalent, container or Unix) and waits `cancel_grace` (15 s) for it to write its state, and only then kills it. The runner stores whatever state came back before the lock is released. Tests: `canceling_asks_tofu_to_stop_so_the_finished_resources_are_kept` (real tofu in Docker), `real_container_e6_...` (through the platform), `force_cancel_answers_only_after_the_state_is_recorded_and_before_the_lock_is_released`. **Not fixable here:** Windows without a container cannot send Ctrl-C to a hidden child process, so a force-cancel there still loses the record (the log says so, and the test pins that behaviour). The Unix process-mode interrupt is untested.

---

## To check (not reproduced or deliberately not changed)

- **Lockout by design.** Ten wrong passwords lock *that email* out for 15 minutes, so someone can lock a known address out. Common trade-off; a per-IP limit plus a softer per-account limit would help, but IP is not yet available behind the dev server.
- **Verification link is a GET that consumes its token.** Mail scanners that pre-fetch links can burn it; a confirm button on the page would fix it.
- **Very short sensitive values** (one or two characters) are masked everywhere in logs, which can make logs unreadable. Not changed: masking errs on the safe side.
- **Growth.** `/runs` lists every run, run logs are unlimited, and the workspace list issues one query per workspace. Fine at today's size, not measured at scale.
- **Resolved on 2026-11-08:** a canceled or force-unlocked run's engine step used to keep running; cancel, force-cancel and force-unlock now stop it (tested with the simulator, real tofu on Windows and real tofu in a container).
- **Stopping needs the runner.** A force-cancel waits up to 45 s for the runner to finish with the stopped step; if the runner were stuck, the request would take that long before ending the run.
- **Several server instances** are unsupported: startup recovery (B02), the in-memory rate limiter and the polling runner all assume one.
- **`Secure` cookie over real HTTPS** and **TLS to PostgreSQL** were never exercised.
