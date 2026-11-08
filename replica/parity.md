# Parity report

Date: 2026-11-08. Original: HCP Terraform and the OpenTofu CLI, studied from
their public documentation only (`recon.md`). Clone: the Rust run platform in
`tfclone/`.

| | |
| --- | --- |
| **Overall** | **54.3 / 100** (34.6 at first; 36.4 after the OpenTofu http backend; 40.1 after the real `tofu` driver and its sandbox; 43.8 after live log streaming and force-cancel; 50.6 after the JSON:API, pagination, archive upload and rate limit; 54.3 after lock leases, workspace settings and state transfer URLs. The lists below the table are as of the 34.6 run; `features.csv` is current) |
| Feature score | 54.3 (34 features counted, must 3 / should 2 / could 1, partial counts half) |
| Must-haves done | 7 of 17 fully (live log, run actions, REST API, workspaces and settings, locking); 9 more are partial; 1 not started |
| Layout score | **not measured**: see below |
| Open bugs | none at S1 or S2 (`bugs.md`, 13 found; B13 fixed except for the Windows process mode limit) |
| **Verdict** | **Not shippable**: 10 of 17 must-have features are not done |

The score says "54.3" because that is where the clone is. The part that exists
(accounts, tenant isolation, queue, state and secrets handling, plan/confirm/
apply) is solid and tested; the part that makes it a replacement for the
original (a real engine, the CLI backend protocol, VCS, policy) is not built.

## Layout diff: not done, and why

`imgdiff.py` compares screenshots of the original and the clone at the same
viewport and state. There are no reference screenshots: the recon was done
from documentation, and the original's dashboard sits behind an account. The
skill's rule is public pages or the user's own account. Taking screenshots of
HCP Terraform needs the user's own sign-in and their consent, so I did not
fabricate or approximate any, and the layout score is left blank rather than
guessed. If you want it: sign in to your own HCP Terraform account, save
screenshots of the workspace list, run detail and states pages (1440x900 and
390x844) into `replica/screens/` as `S04.png`, `S07.png`, `S09.png`, and run
`imgdiff.py` against `replica/clone-screens/`. The clone's side can be
captured the same way from the running server. Colour is ignored by design;
do not chase the original's look.

## Feature parity (from parity.py)

features 54.3  (34 counted, must-haves 7 of 17 done)

Not shippable yet: 10 must-have features are not done.

### By area, weakest first
- vcs                            0.0  (1 features)
- org                            0.0  (1 features)
- policy                         0.0  (2 features)
- ops                            0.0  (3 features)
- cli                            0.0  (1 features)
- registry                       0.0  (1 features)
- engine                        16.7  (3 features)
- api                           21.4  (3 features)
- config                        30.0  (2 features)
- state                         37.5  (3 features)
- core                          50.0  (1 features)
- auth                          50.0  (2 features)
- security                      50.0  (3 features)
- runs                          61.9  (8 features)

### Missing, in build order
- [must] cli: CLI login and remote backend protocol, no  (page not read; needs spike)
- [must] engine: Engine: dependency graph plan/apply, no  (phase 2 native)
- [must] engine: Engine: provider plugin protocol, no  (reuse existing providers)
- [must] api: REST API JSON:API /api/v2, partial  (JSON envelope, status codes, 404 across organizations; not full JSON:API (no relationships, include, pagination))
- [must] auth: Tokens (user/team/org), partial  (user and organization tokens with expiry and revoke; organization token cannot plan or apply; no team tokens)
- [must] config: Variables (terraform/env/sensitive/hcl), partial  (input and environment variables, sensitive write-only, masked in logs; hcl flag not interpreted; no variable sets)
- [must] core: Workspaces (create/list/settings), partial  (create, list, lock, variables, state list; no settings screen; one organization per user in the UI)
- [must] engine: Engine: HCL parse + evaluate, partial  (simulator reads resource blocks only; tofu driver written, never run against a real binary, not sandboxed)
- [must] runs: Configuration versions + upload, partial  (text configuration only (1 MB); no archive upload)
- [must] runs: Confirm / discard / cancel run actions, partial  (confirm and discard when paused, cancel before applying; no force-cancel or force-execute; applying cannot be canceled by design)
- [must] runs: Live plan/apply log streaming, partial  (ordered, secrets masked; page reloads every 2 s while a run works; no SSE, no ANSI colour)
- [must] runs: Run lifecycle state machine, partial  (10 of about 24 states; no policy, cost, task or fetch states)
- [must] security: State encryption at rest, partial  (AES-256-GCM, key per workspace, master key from environment; no KMS key providers, no rotation)
- [must] state: Remote state storage with versions, partial  (versions with unique serials in PostgreSQL, encrypted, survive restart; no upload API, no download URLs, no retention policy)
- [must] state: State locking (lock/unlock/force-unlock), partial  (manual and run locks; force-unlock ends the run holding the lock; no lease or heartbeat; no pending-state rule (state is finalized atomically))
- [should] api: Pagination and include, no  (default 20 max 100)
- [should] api: Rate limiting 30 req/s per user, no
- [should] config: Run-level variable overrides, no
- [should] ops: Agents / self-hosted runners, no  (guess on details)
- [should] ops: Drift detection / continuous validation, no  (paid tier in original)
- [should] org: Projects (grouping and scoped access), no  (paid tier in original)
- [should] policy: Policy as code (own format), no  (advisory / soft / hard levels with override)
- [should] security: Audit logging, no  (paid tier in original)
- [should] state: State rollback, no  (duplicates old version as current)
- [should] vcs: VCS-driven runs (webhook to run), no  (GitHub first)
- [should] auth: Teams and role permissions, partial  (owner/admin/member through one can() function; no teams, no invite screen)
- [should] runs: Plan diff view per resource, partial  (resource list with action; no attribute-level diff)
- [could] ops: Notifications (webhook/email/Slack), no  (guess; page not read)
- [could] policy: Cost estimation, no
- [could] registry: Private module/provider registry, no
- [could] runs: Run tasks (external hooks pre/post), no

### Left out on purpose (not scored)
- Sentinel policy language: proprietary; use own format
- Public provider registry content: owned by third parties
- HashiCorp branding / docs text: trademark and copyright
- Hosted run infrastructure and user network: not clonable

## Behaviour differences

Original behaviour is taken from its documentation, not from using the product.
"Keep" means the difference is deliberate.

| flow | original does | clone does | fix or keep |
| --- | --- | --- | --- |
| F02 starting a run | a push to the linked branch starts a run (0 clicks); API and CLI can also start one | paste a configuration in a form, or call the API: 3 to 4 steps | fix: VCS-driven runs (M3) |
| F02 confirming | run pauses at "Needs Confirmation"; an authorised user approves or rejects | same; the action bar also states how many resources will be destroyed | keep (better) |
| F02 run stages | pre/post-plan and apply tasks, cost estimate, policy checks, VCS fetch | queue, plan, confirm, apply, done | fix later: policy first |
| F02 cancel | cancel, force-cancel (admin, after a first cancel and a cool-off), force-execute | cancel before apply starts; an applying run cannot be canceled | fix: force-cancel with a safety rule |
| F02 log | live stream while the run works | the whole log, page reloads every 2 s | fix: SSE streaming |
| F05 unlock | unlocking waits while the latest state version is still pending; force-unlock is admin only | no pending-state concept; force-unlock (admin) also ends the run that holds the lock | keep, revisit when state upload exists |
| F05 stuck locks | not described in the pages read | runs interrupted by a restart are recovered at startup; API and CLI locks lapse by themselves, API locks can be renewed with a heartbeat | fixed 2026-11-08 (G12) |
| F08 errors | forbidden and missing both 404 | 404 across organizations, 403 inside your organization when your role is too low | keep (a known member learns nothing new) |
| F08 API shape | JSON:API with relationships, `include`, pagination (20 default, 100 max), 30 requests/s per user | same shape for the resources the platform has; fixed one-second rate window (a burst across a window edge can reach twice the limit) | fixed 2026-11-08 |
| tokens | user, team, organization and audit tokens | user and organization tokens; organization tokens cannot plan or apply (same rule) | fix: team and audit tokens |
| state | single-use signed download and upload URLs; free-tier retention of recent versions | single-use 5-minute download and upload URLs held in server memory (not valid across instances or a restart); the API never returns state inline; every version kept | URLs fixed 2026-11-08; retention still open |
| variables | terraform and environment kinds, sensitive flag, `hcl` flag, variable sets, run-level overrides | kinds and sensitive flag; no hcl, sets or overrides | fix |
| secrets in logs | not described | sensitive values are masked in every stored log line and never shown again after saving | keep |
| encryption | not described in the pages read (OpenTofu encrypts state with its own key providers) | state, saved plans and sensitive variables sealed with AES-256-GCM, per-workspace keys | keep; add KMS providers |

## Top five to build next

Ordered by what unlocks the most must-haves.

1. **CLI login and the remote backend protocol.** Without it `tofu` cannot use the platform, so nothing is usable end to end with real infrastructure. First read the `cloud` backend and locking pages (not read in the recon), then build the smallest backend that `tofu init` and `tofu plan` accept. Must-have.
2. **Run the `tofu` driver against a real binary, then sandbox it.** The driver has never run for real and runs with the server's privileges. Until this is done the engine rows stay partial and untrusted configuration is unsafe. Gates the engine must-haves.
3. ~~**Log streaming (SSE), the missing run states and force-cancel.**~~ **Done 2026-11-08.** Live log and run actions are complete; the lifecycle stays partial (policy, cost, run-task and fetch states need infrastructure that does not exist yet).
4. ~~**Real JSON:API and configuration upload.**~~ **Done 2026-11-08.** Resource objects with relationships and links, `include`, paged lists (20 default, 100 max), JSON:API request documents, two-step archive upload and a 30 requests/s limit. The configuration-version row stays partial: only `.tf` files at the top of the archive (no local modules, no `.tf.json`). Clients written for the original's API still need its other resource types.
5. ~~**Lock lease with heartbeat, workspace settings screen, state download/upload URLs.**~~ **Done 2026-11-08.** Locks taken through the API or the CLI lapse on their own (1 h with a heartbeat, 12 h for the CLI, which cannot renew); workspaces can be renamed, described, given a default auto-apply and deleted (typed-name confirmation); state moves through single-use 5-minute URLs and is never inline in an API answer. Locking and workspaces are complete; state storage stays partial (no retention policy, URLs are per-process). The stuck-lock part of G12 now has a mitigation; whether users actually complain about it is still unchecked.

After these, run `/replica-entrepreneur` to confirm which of the unverified gaps
(G8 to G12) real users complain about, then build the fixes (M4): linting in
the plan step, partial planning for unknown values, free drift detection, and
the native engine behind the existing `Engine` trait.

## Honest limits of this report

- The original was studied from documentation, so behaviour differences are what the documents say, not what the product does.
- Feature status comes from `features.csv`, which I maintain. Each "yes" or "partial" is backed by a passing test; the notes column says what is missing.
- No layout score, so there is no visual verdict on the dashboard.
