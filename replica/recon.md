# Recon map: Terraform / OpenTofu platform (working name: TBD, see replica-brand)

Researched 2026-11-07 from public docs only. Clean-room: functionality and UX
patterns, no source code, no private APIs, no HashiCorp or OpenTofu assets,
names or copy. Terraform is a HashiCorp trademark and its code is BSL, so the
clone gets its own name and its own implementation.

## 1. Scope (confirmed by the user)

- **App / platform:** HCP Terraform (web dashboard + REST API) and the
  OpenTofu CLI/engine around it. Desktop CLI + web.
- **Slice:** the core loop `plan -> review -> apply`, with state storage,
  locking and workspaces. Not in scope: provider ecosystem, public registry,
  HCL language engine parity on day one.
- **For whom:** the user's own product. Written in Rust.
- **Order of work (user's call):** 1) rebuild parity, 2) analyse what real
  users need that neither project meets, 3) improve beyond parity.

## 2. Sources

| source | URL | gave us |
| --- | --- | --- |
| HCP Terraform overview | https://developer.hashicorp.com/terraform/cloud-docs | editions, feature areas, doc sidebar as screen list |
| Workspaces | https://developer.hashicorp.com/terraform/cloud-docs/workspaces | workspace contents, projects, tier notes |
| Run states | https://developer.hashicorp.com/terraform/cloud-docs/workspaces/run/states | full run lifecycle |
| Policy enforcement | https://developer.hashicorp.com/terraform/cloud-docs/policy-enforcement | policy frameworks, scopes, overrides |
| API overview | https://developer.hashicorp.com/terraform/cloud-docs/api-docs | auth, token types, JSON:API, rate limit, entitlements |
| API workspaces | https://developer.hashicorp.com/terraform/cloud-docs/api-docs/workspaces | workspace attributes, lock/unlock |
| API runs | https://developer.hashicorp.com/terraform/cloud-docs/api-docs/run | run attributes, operations, actions |
| API state versions | https://developer.hashicorp.com/terraform/cloud-docs/api-docs/state-versions | state version model, retention |
| API variables | https://developer.hashicorp.com/terraform/cloud-docs/api-docs/variables | variable fields |
| OpenTofu docs | https://opentofu.org/docs/ | overview, versions |
| OpenTofu CLI | https://opentofu.org/docs/cli/commands/ | command list |
| OpenTofu state | https://opentofu.org/docs/language/state/ | state purpose, storage |
| Backends | https://opentofu.org/docs/language/settings/backends/configuration/ | backend config, limits |
| State encryption | https://opentofu.org/docs/language/state/encryption/ | encryption model |
| OpenTofu 1.13 | https://opentofu.org/blog/opentofu-1-13-0/ | recent features, limits |
| Linting plans | https://opentofu.org/blog/plans-for-linting/ | roadmap, community demand |

Not read yet (gaps in this recon): variable sets, notifications, teams and
permissions page, Plans and features page, locking page, cloud backend
protocol, configuration-versions API. Rows depending on them are marked
`guess` below.

## 3. Screens (web dashboard)

No UI walkthrough exists in the docs, so screens are inferred from the docs
sidebar and API resources. Confidence: medium.

| ID | screen | how you get there | purpose | key components | states |
| --- | --- | --- | --- | --- | --- |
| S01 | Sign in / org picker | root | auth, choose organization | form, org list | empty, error |
| S02 | Organization overview | org home | workspaces, recent runs | table, filters, status badge | empty, filled, loading |
| S03 | Projects list | org > projects | group workspaces, scope access | list, create modal | empty, filled |
| S04 | Workspace list | org or project | find workspace, see current run state | table, tag filter, search | empty, filled, error |
| S05 | Workspace overview | workspace | resource count, current run, last state | summary cards, run timeline | locked, drifted, errored |
| S06 | Runs list | workspace > runs | run history, filters | table, status filter, search by user/commit | empty, filled |
| S07 | Run detail | run | live plan/apply log, stage progress, actions | log viewer, stage stepper, confirm/discard/cancel buttons | every run state in section 5 |
| S08 | Plan diff view | run detail | create/update/destroy resources | diff list, expand per resource | no changes, large plan |
| S09 | States | workspace > states | state versions, rollback | version list, download, outputs | empty, pending, finalized |
| S10 | Variables | workspace > variables | terraform/env vars, sensitive | table, add modal, sensitive mask | empty, filled |
| S11 | Workspace settings | workspace > settings | execution mode, version, VCS, auto-apply, triggers | form sections, danger zone | saved, error |
| S12 | VCS connection | settings | link repo, branch, triggers | OAuth connect, repo picker | disconnected, connected |
| S13 | Teams and permissions | org > teams | roles per workspace/project | team list, role matrix | free tier limit |
| S14 | Policies | org > policy sets | policy sets, scope, enforcement level | list, editor, override | passed, soft-failed, hard-failed |
| S15 | Private registry (guess) | org > registry | share modules | module list, version | empty |
| S16 | Agents (guess) | org > agent pools | self-hosted runners | pool list, token | no agents |
| S17 | Notifications (guess) | workspace > notifications | webhook/email/Slack on run events | form | none |
| S18 | Tokens | user / team / org settings | API tokens | list, create, revoke | empty |

## 4. User flows

- **F01 Create workspace and first run:** S02 -> S04 -> create -> S11 -> S12 (VCS) -> S10 (vars) -> S07. Edge: name already taken, no VCS entitlement.
- **F02 Plan and apply (VCS-driven):** push to branch -> run auto-created -> S07 (Fetching, Planning) -> Needs Confirmation -> approve -> Applying -> Applied. Edge: plan errored, no changes, policy soft-fail needs override, workspace locked, queue ahead of you.
- **F03 Speculative plan on pull request:** PR opened -> plan-only run -> result posted back. Edge: no apply possible.
- **F04 Review a state change:** S09 -> pick version -> inspect outputs/resources -> rollback. Edge: pending (non-finalized) version, lock held.
- **F05 Lock / unlock / force-unlock:** S05 lock -> reason; admin force-unlock. Edge: unlock blocked while latest state version is still pending.
- **F06 Policy override:** S07 shows Policy Override -> user with override permission approves, or discard.
- **F07 CLI remote operation:** `tofu login` -> `tofu init` (cloud/backend block) -> `tofu plan` streams a remote run -> `tofu apply`.
- **F08 API automation:** create configuration version -> upload files -> run auto-created -> apply action.

Happy path clicks (F02, VCS-driven): 0 to start (push), 1 to confirm apply. That is the number to beat.

## 5. Run lifecycle (core state machine)

Order: pending -> [fetching] -> [pre-plan tasks] -> planning -> needs
confirmation / planned -> [policy check] -> [cost estimating -> cost
estimated] -> [policy check] -> [post-plan tasks] -> confirmed ->
[pre-apply tasks] -> apply queued -> applying -> [post-apply tasks] -> applied.

Final states: applied, planned_and_finished, errored (plan or apply),
discarded, canceled (force_canceled), policy_soft_failed (plan-only).
Actions: apply (202, only if paused), discard, cancel, force-cancel (admin,
needs earlier cancel + cool-off), force-execute. Runs in a workspace process
in queue order. Source: run-states page + API runs page.

Operations: plan_only (speculative), plan_and_apply, save_plan,
refresh_only, destroy, empty_apply, action_only.

## 6. Components

Status badge (one per run state), log viewer (streamed, ANSI colour), stage
stepper, resource diff list, data table with filters and pagination, tag
chips, sensitive-value field (write once, masked), confirm/discard button
pair with comment box, lock banner, role matrix, token create modal (shown
once), toast, empty states.

## 7. Inferred data model

JSON:API style, base `/api/v2`, bearer tokens. Confidence is high for API
fields (read from docs) and medium for relations marked guess.

- **Organization**: name, entitlements by tier. Has many projects, workspaces, teams, agent pools. (high)
- **Project**: name; groups workspaces; scopes permissions. (high)
- **Workspace**: name (unique per org, `[A-Za-z0-9_-]`), execution_mode (remote | local | agent), agent_pool_id, auto_apply, allow_destroy_plan, speculative_enabled, queue_all_runs, terraform_version (exact or constraint), working_directory, trigger_prefixes/patterns, file_triggers_enabled, global_remote_state, description, tags, locked, locked_reason, resource_count, current_run, vcs_repo{identifier, branch, ingress_submodules, tags_regex}. Evidence: API workspaces. (high)
- **ConfigurationVersion**: snapshot of config used by a run; created then uploaded. (medium, own page not read)
- **Run**: status, source (ui | api | configuration-version), plan_only, is_destroy, refresh, refresh_only, save_plan, auto_apply, allow_empty_apply, replace_addrs, target_addrs, variables (override workspace vars), message, debugging_mode, trigger_reason, has_changes, status_timestamps, actions{is_confirmable, is_discardable, is_cancelable, is_force_cancelable}. Relations: workspace, configuration_version, input_state_version, plan, apply, cost_estimate, policy_checks, comments, run_events, created_by. (high)
- **StateVersion**: serial, lineage, format version, status (pending | finalized | discarded), intermediate, upload/download URLs (single-use, time-limited), vcs_commit, terraform_version; async-populated resources/modules/providers/outputs. Relations: run, workspace, outputs. Creation requires the workspace locked by the caller, plus serial and md5. (high)
- **Variable**: key, value, description, category (terraform | env), hcl, sensitive (write once). Run-level variables override. (high)
- **Team / User / Token**: token kinds user, team, organization (cannot plan/apply), audit-trail. (high for tokens, guess for membership model)
- **PolicySet / Policy**: framework (native, Sentinel, OPA), scope (global, project, workspace), enforcement levels (advisory, soft-mandatory, hard-mandatory), override permission. (medium)
- **Notification** (guess): destination, triggers, enabled.
- **AgentPool** (guess): name, tokens, agents.

API rules to replicate: forbidden and missing both return 404 (no existence
leak); 30 requests/second per user; paginate with page[number]/page[size]
(default 20, max 100); `include=` for related resources; feature absent from
the plan returns 404.

State retention (free tier): last 100 states across the org plus latest per
workspace always kept, older kept six months.

## 8. CLI surface (OpenTofu)

init, validate, plan, apply, destroy, console, fmt, force-unlock, get, graph,
import, login, logout, metadata, output, providers, refresh, show, state,
taint, untaint, test, version, workspace; global `-chdir`. Config files `.tf`
and `.tofu`. Backends are built in only (no backend plugins), one backend
block per configuration, backend config may not reference state. State is
JSON; direct edits discouraged; scripts should use `output -json` and
`show -json`. State and plan encryption (PBKDF2, AWS/GCP/Azure/OpenBao KMS,
external) is an OpenTofu extra. Evidence: sources in section 2.

## 9. What cannot be cloned

See `features.csv` rows with `skip`: the provider ecosystem and registry
content, vendor-owned hosted infrastructure and its user network, Sentinel
(proprietary policy language), HashiCorp trademarks and docs text,
enterprise support contracts. A Rust engine that evaluates full HCL and runs
every existing provider is a separate, very large project, handled in
phase 2 by speaking the existing provider plugin protocol instead of
rewriting providers.

## 10. Size

- Screens: 18. Flows: 8. Entities: about 12.
- Hard parts: (1) HCL parser + evaluator and the plan/apply graph engine in Rust, (2) provider plugin protocol compatibility, (3) state locking and consistency under concurrent runs, with encryption.
- Size: **XL for full parity**. Rescope recommended: **L** for phase 1 = state backend + locking + workspaces + run queue + run lifecycle + web UI + API, driving the existing `tofu` binary for the engine; a native Rust engine comes after.
