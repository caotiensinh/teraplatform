# Test plan

Run 2026-11-08 against the clone only (never against Terraform Cloud or
OpenTofu servers). Flow ids come from `recon.md`; account and security flows
(A01 to A06) were added by `replica-backend`.

How cases are run:

- **auto** = a Rust test in `crates/*/tests` or `src`, run on the in-memory store and, where marked **pg**, also on PostgreSQL 16.
- **browser** = driven or measured in the built-in browser against the local server.
- **n/a** = the feature is not built yet (so there is nothing to test).

Playwright and axe were not used: Playwright would download Chromium, and the
test code is Rust. The accessibility checks below are script-based (headings,
language, labels, landmarks, names) plus the WCAG contrast checker on the
design tokens. They are not a substitute for a screen-reader pass.

## Results at a glance

| | |
| --- | --- |
| automated tests | 191 pass at the last full run (core 13, crypto 9, engine unit 16, real tofu 15, container 11, server unit 14, end to end 62, store 25 in memory and 26 on PostgreSQL; the end-to-end suite also runs on PostgreSQL). The sections below were added as features landed |
| browser checks | keyboard-only sign-in; accessibility scan of 11 pages; layout of 8 pages at 390, 800 and 1440 px (24 combinations); real-server restart and force-unlock runs |
| bugs found | 9 (S1 1, S2 2, S3 4, S4 2), all fixed with tests, see `bugs.md` |
| open S1 / S2 | 0 / 0 |
| not testable yet | VCS runs, policy override, CLI remote backend, state rollback, notifications |

## F01 Create a workspace and queue the first run

| id | case | how | result |
| --- | --- | --- | --- |
| F01-H1 | create workspace in the form, queue a run, see plan, confirm, applied | auto (ui_forms_...) + browser | pass |
| F01-E1 | eight workspaces queued at once all finish, one state serial each, no lock left | auto pg | pass |
| F01-E2 | workspace name invalid or duplicate shows an inline error, not a crash | auto | pass |
| F01-E3 | same workspace name in two organizations is allowed | auto pg | pass |
| F01-E4 | second user's data is invisible (every id route, both directions) | auto pg | pass |
| F01-N1 | member role cannot create a workspace | auto | pass |

## F02 Plan, review, apply

| id | case | how | result |
| --- | --- | --- | --- |
| F02-H1 | plan shows adds/changes/destroys, confirm applies, state serial 1 | auto pg | pass |
| F02-H2 | edit one resource, remove one, add one: plan is 1/1/1; discard leaves state alone | auto pg | pass |
| F02-H3 | auto-apply skips the confirmation; plan-only never applies | auto pg | pass |
| F02-H4 | unchanged config plans "no changes" | auto pg | pass |
| F02-E1 | double click on Confirm lands on the run (**B05**) | auto | pass after fix |
| F02-E2 | markup in resource names, messages and config is escaped on every page | auto | pass |
| F02-E3 | unicode and emoji in message and config survive | auto | pass |
| F02-E4 | 5,000-character message and 1.5 MB config refused cleanly (**B06**) | auto | pass after fix |
| F02-E5 | Windows line endings do not create phantom changes | auto | pass |
| F02-E6 | a pending run can be canceled and the next one starts | auto | pass |
| F02-E7 | two runs on one workspace run in order; the second waits | auto pg | pass |
| F02-E8 | two tabs: stale tab confirming a finished run | auto (with F02-E1) | pass after fix |
| F02-E9 | refresh mid-run: the page refreshes itself every 2 s while a run works, and never while it waits for a person | auto | pass |
| F02-N1 | engine fails during plan / during apply: run errored, log says why, lock released, no state written | auto pg | pass |
| F02-N2 | invalid configuration is an errored plan, not a crash | auto | pass |
| F02-N3 | org token and member roles: queue and confirm rights as specified | auto | pass |
| F02-N4 | confirming a run that is not paused, canceling one that is waiting: 409 on the API | auto | pass |

## F04 State

| id | case | how | result |
| --- | --- | --- | --- |
| F04-H1 | state versions listed newest first, serial increases | auto pg | pass |
| F04-H2 | state, saved plans and sensitive variables are stored sealed (raw rows inspected) | auto pg + real server | pass |
| F04-E1 | sealed data moved to another workspace or key does not open | auto | pass |
| F04-E2 | state survives a server restart and decrypts with the same key | real server | pass |
| F04-H3 | rollback to a previous version | n/a | not built |

## F05 Locking

| id | case | how | result |
| --- | --- | --- | --- |
| F05-H1 | manual lock blocks runs; unlock releases them | auto pg | pass |
| F05-E1 | force-unlock ends the run that held the lock and the queue moves (**B01**) | auto pg | pass after fix |
| F05-E2 | force-unlock refuses while a run is applying | auto pg | pass |
| F05-E3 | lock reason with markup and unicode is escaped | auto | pass |
| F05-E4 | one inconsistent workspace cannot stall other workspaces (**B01**) | auto pg | pass after fix |
| F05-E5 | server killed mid-plan: on restart the run errors, the lock is released (**B02**) | auto pg + real server | pass after fix |
| F05-N1 | members can lock and unlock, only admins can force-unlock | auto | pass |

## F08 API

| id | case | how | result |
| --- | --- | --- | --- |
| F08-H1 | token acts as its user without a cookie or Origin | auto | pass |
| F08-N1 | no token or a wrong token: 401 with `WWW-Authenticate`, even with a valid cookie next to a wrong token | auto | pass |
| F08-N2 | another organization's ids are 404, not 403 | auto pg | pass |
| F08-N3 | malformed JSON, wrong type, wrong content type: JSON error envelope (**B08**) | auto | pass after fix |
| F08-N4 | tokens cannot mint tokens; org tokens cannot plan or apply; revoked and expired tokens stop working | auto pg | pass |

## A01 to A06 Accounts and security

| id | case | how | result |
| --- | --- | --- | --- |
| A01-H1 | sign up, sign in (case-insensitive email), sign out, sign out everywhere | auto pg | pass |
| A01-H2 | sign in with the keyboard only (type, Tab, type, Enter) | browser | pass |
| A01-E1 | bad email, short or 201-character password, non-ASCII organization name, duplicate email, organization clash rolls the account back | auto | pass |
| A01-E2 | unicode password signs in | auto | pass |
| A01-E3 | forged, garbage and expired session cookies are rejected | auto | pass |
| A01-E4 | hostile email and display name escaped on every page | auto | pass |
| A01-N1 | wrong password and unknown email give the same message and status | auto | pass |
| A01-N2 | twelve bad passwords: 429, even the right password refused while limited | auto | pass |
| A01-N3 | foreign Origin cannot sign in or out, or change anything (**B07**) | auto | pass after fix |
| A02-H1 | email verification gates sign-in when required; link is single use | auto | pass |
| A02-H2 | password reset: same answer for unknown accounts, single use, signs out everything | auto | pass |
| A03-H1 | token created once (secret shown once), listed without secret, revoked | auto | pass |
| A04-H1 | sensitive variable: never echoed, listed or logged; reaches the engine; rotating changes the plan | auto | pass |
| A04-E1 | variable key and category validated, form shows errors; the value field is a password field with autocomplete off | auto | pass |
| A05-H1 | delete account: wrong password refused, right password deletes the user and its sole-owner organization and data | auto pg | pass |
| A06-H1 | stored secrets are ciphertext in PostgreSQL; the database refuses plaintext sensitive variables | auto pg | pass |
| A06-E1 | breaking the organization check, CSRF check, log masking or member role each turns a test red (mutation checks) | auto | pass |

## Accessibility, layout, mobile

| id | case | how | result |
| --- | --- | --- | --- |
| UX-A1 | one `<h1>`, language attribute, labelled controls on 15 pages (**B09**) | auto | pass after fix |
| UX-A2 | headings, `main`, labelled `nav`, control names, duplicate ids, table headers, `aria-live` on logs, 11 pages | browser script | pass |
| UX-A3 | text and control contrast on both themes | `contrast.py`, 22 pairs each | pass |
| UX-M1 | navigation reachable below 960 px (**B03**) | browser measure + auto guard | pass after fix |
| UX-M2 | no horizontal scrolling and "Sign out" in view at 390, 800, 1440 px, 8 pages (**B04**) | browser measure | pass after fix (0 of 24 fail) |
| UX-M3 | keyboard focus ring visible, tab order, screen reader | not done | to check manually |

## Not covered

Email delivery, OAuth and payments (not built or out of scope), real
HTTPS, TLS to the database, load beyond eight parallel workspaces, the
`tofu` driver against a real binary, visual regressions (screenshots were
unreliable in this environment, so layout was measured instead), and
assistive technology.

## F07 OpenTofu CLI against the backend (added 2026-11-08)

| id | case | how | result |
| --- | --- | --- | --- |
| F07-H1 | lock, push, pull (byte for byte), versions listed, unlock | auto (cli_e1) | pass |
| F07-H2 | the same flow with a single address and POST/DELETE spellings; state survives a refused purge | auto (cli_e2) | pass |
| F07-H3 | real `tofu` 1.13.9: init, apply, plan (no changes), edit and apply, output, state list, destroy | real client | pass |
| F07-E1 | push without a lock, with someone else's id, bad MD5, invalid JSON, missing serial/lineage | auto | pass |
| F07-E2 | older serial, same serial with other content, other lineage: 409; same state twice: 200 | auto | pass |
| F07-E3 | 5 MB state; empty, non-JSON, id-less, wrong-type and oversized lock bodies | auto (cli_e5) | pass |
| F07-E4 | CLI lock holds platform runs back and releases them; a run's lock refuses the CLI and its pushes | auto (cli_e3) | pass |
| F07-E5 | crashed CLI: lock cleared from the dashboard; real second client refused and shown the holder | auto + real client | pass |
| F07-E6 | `tofu force-unlock` for a CLI lock, the dashboard lock and a run's lock (rights differ) | auto (cli_e7) + real client | pass |
| F07-N1 | no credentials, wrong token, bad Basic, bearer, any username: 401 with a Basic challenge, or accepted | auto (cli_e4) | pass |
| F07-N2 | organization token refused on state; member allowed; other organization 404 | auto (cli_e4) | pass |
| F07-N3 | the workspace page shows the backend block and no secret | auto (cli_e6) | pass |
| F07-N4 | mutation checks: no lock needed, no MD5 check, no lineage check, old serial accepted, org token allowed, wrong Basic parsing: each turns a test red | auto | pass |

## E01 The real tofu driver and its sandbox (added 2026-11-08)

All cases run against OpenTofu 1.13.9 with the built-in `terraform_data` resource. "container" cases need Docker and a Linux binary.

| id | case | how | result |
| --- | --- | --- | --- |
| E01-H1 | plan, apply, no-change plan, input change, destroy; state is tofu's own document | real binary | pass |
| E01-H2 | the same cycle in a container | container | pass |
| E01-H3 | the platform drives it: confirm, apply, secrets masked, second run no changes, output-only change applies, destroy | real binary, through HTTP | pass |
| E01-E1 | provisioner cannot read server secrets (**B10**) | real binary, container | pass after fix |
| E01-E2 | backend block in the configuration is overridden (**B11**) | real binary | pass after fix |
| E01-E3 | output-only change is a change (**B12**) | real binary | pass after fix |
| E01-E4 | runaway apply stopped at the timeout, grandchild killed, queue moves on | real binary + platform | pass |
| E01-E5 | background process left by a provisioner dies with the run | real binary | pass |
| E01-E6 | output flood and an overlong line are capped, the run still finishes | real binary | pass |
| E01-E7 | memory ceiling (Windows job object; container `--memory`), with a control that runs without one | real binary, container | pass |
| E01-E8 | process ceiling (job object; container `--pids-limit`), with a control under the ceiling | real binary, container | pass |
| E01-E9 | run cannot move HOME; scratch directories are removed; stale ones and containers are swept | real binary, container | pass |
| E01-E10 | container: own read-only filesystem, uid 65534, no capabilities, no host directories | container | pass |
| E01-E11 | container: no network by default; the same request reaches the host on `bridge` (control) | container | pass |
| E01-E12 | container: `DOCKER_HOST` and similar names are reserved and reported; server environment absent | container | pass |
| E01-E13 | missing binary, unusable docker or image: clear errors | real binary, container | pass |
| E01-E14 | killed docker client does not leave a container running | container | pass |

## F02b Live log, cancel and force-cancel (added 2026-11-08)

| id | case | how | result |
| --- | --- | --- | --- |
| F02b-H1 | the stream carries the stored log line for line, in order, `id` = position, `status` events, ends with `done` | auto | pass |
| F02b-H2 | it resumes from `?from=`, from `Last-Event-ID` (header wins), nonsense means the start, beyond the end still says `done` | auto + unit | pass |
| F02b-H3 | lines arrive while the engine works; the stream stays open while a person decides and continues to the end; no line lost or repeated | auto | pass |
| F02b-H4 | in a real browser: lines appended without reloading, one reload per status change, final page correct | built-in browser | pass |
| F02b-E1 | another organization gets 404 and no events; anonymous gets none; a sensitive value is never streamed | auto | pass |
| F02b-E2 | `done` waits for the runner's last line after a cancel | auto (slow-stop engine) | pass |
| F02b-E3 | the run page streams while working with a `noscript` reload fallback, and neither while a person decides or after the end | auto | pass |
| F02b-C1 | cancel of a planning run stops the engine step (not only the status); the queue moves on; the web form does the same | auto | pass |
| F02b-C2 | plain cancel of an applying run is refused (409) | auto | pass |
| F02b-C3 | force-cancel of an applying run needs `acknowledge_risk: true` (missing, false, a string, no body: 422) | auto | pass |
| F02b-C4 | force-cancel keeps the finished resources; the next plan only does the rest; a force-canceled planning run invents no state | auto | pass |
| F02b-C5 | force-cancel answers only after the state is recorded, and the lock is free by then | auto (slow-stop engine) | pass |
| F02b-C6 | members and org tokens: 403; another organization: 404; the page offers the control to admins only | auto | pass |
| F02b-C7 | web form: needs the box ticked, survives a double click, banner and badge | auto + built-in browser | pass |
| F02b-C8 | force-unlock of a planning workspace stops the engine step | auto | pass |
| F02b-C9 | pending and waiting-for-person runs cannot be force-canceled; a finished run: 409 | auto | pass |
| F02b-R1 | real tofu, Windows process mode: a canceled apply and its children die, no scratch directory is left, no state survives (documented limit) | real binary | pass |
| F02b-R2 | real tofu in Docker: asked to stop, exits cleanly, finished resource kept, interrupted one tainted, next plan replaces it, container removed | container | pass |
| F02b-R3 | a tofu that does not stop within the grace is killed and its container removed | container | pass |
| F02b-R4 | the same through the platform: force-cancel in under 30 s, state serial 1 with the tainted resource, lock free, no container left | real tofu + Docker | pass |
| F02b-M1 | 13 mutation checks (see `backend.md`) each turn at least one test red | mutation | pass |
| F02b-U1 | Unix process mode `kill -INT` | not run (no Unix host here) | untested |

## F08 JSON:API, pagination, archive upload, rate limit (parity item 4)

| id | what | how | result |
| --- | --- | --- | --- |
| F08-J01 | resources have type, kebab attributes, relationships, links; errors share the media type | e2e j01 | pass |
| F08-J02 | pages: default 20, cap 100, links followable, past the end empty, bad numbers 422 | e2e j02 + unit | pass |
| F08-J03 | include current-run / workspace, once, unsupported include 422 | e2e j03 | pass |
| F08-J04 | request documents, wrong type 409, malformed 422, kebab attributes read | e2e j04 + unit | pass |
| F08-J05 | run, state-version and variable lists paged, newest first, no state documents, other organization 404 | e2e j05 | pass |
| F08-J06 | create version, run before upload refused, upload, second upload 409, run plans both files | e2e j06 | pass |
| F08-J07 | bad archives refused with a reason and retryable; too large when unpacked | e2e j07 + unit | pass |
| F08-J08 | other organization 404, organization token 403, user token ok, foreign workspace's version 404 | e2e j08 | pass |
| F08-J09 | 429 with Retry-After over the limit, others unaffected, pages not counted, recovers | e2e j09 | pass |
| F08-J10 | limit 0 turns it off | e2e j10 | pass |
| F08-S1 | real HTTP: upload with curl, include, paging, 90-call burst gives 30 x 200 and 60 x 429 | smoke script | pass |
| F08-M1 | 12 mutation checks | mutation | 11 red, 1 not caught (gzip stream cap) |
| F08-PG | store contract and e2e on PostgreSQL, migration 0004 | PostgreSQL container | pass |

Total: 211 tests (was 191).

## F05b / F09 lease, settings, state transfer (parity item 5)

| id | what | how | result |
| --- | --- | --- | --- |
| F05b-S1 | a lease lapses, frees the workspace, starts a queued run; claiming notices by itself | store contract | pass (both stores) |
| F05b-S2 | takeover of a lapsed lock; the old holder's unlock no longer works; a lock without a lease never lapses | store contract | pass |
| F05b-S3 | heartbeat renews only the holder's leased lock; none for unlocked, foreign, leaseless and run locks | store contract | pass |
| F05b-K01 | API lock default hour, 0 = none, bad values 422, a short lease lapses and a run starts | e2e k01 | pass |
| F05b-K02 | heartbeat keeps a lock alive, default renewal, 0 refused, leaseless 409, other organization 404 | e2e k02 | pass |
| F05b-K03 | a CLI lock lapses; the old CLI cannot push or unlock; the new one can | e2e k03 | pass |
| F05b-K04 | default CLI lease at least 11 h; the dashboard lock has none; the page says when a lease lapses | e2e k04 | pass |
| F09-S4 | settings validation, rename uniqueness per organization, delete cascade, refused while locked or working | store contract | pass |
| F09-K05 | PATCH in both body forms, refusals change nothing, member 403, other organization 404 | e2e k05 | pass |
| F09-K06 | workspace auto-apply default, run can override, form starts ticked | e2e k06 | pass |
| F09-K07 | DELETE refused while locked or a run waits; then 204, runs gone, name free | e2e k07 | pass |
| F09-K08 | settings page: member sees no buttons, values escaped, errors shown, delete needs the typed name | e2e k08 | pass |
| F10-G | grants: single use, expiry, token shape | unit | pass |
| F10-K09 | download URL works once without a login, no state inline, listings carry none, other organization 404 | e2e k09 | pass |
| F10-K10 | organization token gets no state URLs; the URL token is not an API token | e2e k10 | pass |
| F10-K11 | upload URL: stored, single use, md5 / serial / lineage / older refused and consumed, CLI pulls the same bytes | e2e k11 | pass |
| F10-K12 | no upload while a run or the CLI holds the lock (at request and at upload time); members may write | e2e k12 | pass |
| F10-S5 | real HTTP: download, upload, CLI pull, lease and heartbeat, settings, delete | smoke script | pass |
| F10-M1 | 17 mutation checks | mutation | all red |
| F10-PG | contract and e2e on PostgreSQL, migration 0005, with real tofu and Docker tests | PostgreSQL + tofu + Docker | pass |

Total: 238 tests (was 211).

## F11 lint in the plan step (fix P1)

| id | what | how | result |
| --- | --- | --- | --- |
| F11-U | each rule finds what it should and nothing on a tidy configuration; empty, odd and unparseable input; file and line for joined archives; stable order | 13 unit tests | pass |
| F11-S | findings kept in order with file and line, replaced not appended, survive state changes, unknown run is NotFound | store contract | pass (both stores) |
| F11-L01 | findings in the record, the log (before the plan), no secret value anywhere, the run still plans and applies | e2e l01 | pass |
| F11-L02 | a tidy configuration: no findings, "no findings" in the log, no box on the page | e2e l02 | pass |
| F11-L03 | archive: file name and line within the file | e2e l03 | pass |
| F11-L04 | the page lists findings, says they do not stop the run, no secret; listings carry them | e2e l04 | pass |
| F11-L05 | unparseable text: one `parse-error` and the run goes on | e2e l05 | pass |
| F11-L06 | speculative plans are linted; another organization cannot see them | e2e l06 | pass |
| F11-M | 12 mutation checks: 11 turn a test red (two needed a stronger test first: one configuration had to use two different resource types of the same provider); the twelfth, accepting a mixed template as a lone interpolation, is an equivalent mutant because the check for a closing brace already rejects it | mutation | see backend.md |

## F12 skip the refresh, drift checks (fix P3)

| id | what | how | result |
| --- | --- | --- | --- |
| F12-E | simulator: refresh-only reports drifted entries only; skip is said in the log; both together refused; `resource_drift` parsing | engine unit tests | pass |
| F12-R | a real `tofu`: `-refresh-only` and `-refresh=false` accepted, no drift on a built-in resource, a configuration change still planned when the refresh is skipped | real_tofu (Windows) | pass |
| F12-S1 | options kept with the run; a drift check must be plan-only and cannot skip | store contract | pass (both stores) |
| F12-S2 | due checks: interval, state needed, one waiting check holds the next, recording restarts the interval, off, bad values | store contract | pass |
| F12-S3 | the latest configuration is one that has its content | store contract | pass |
| F12-M01 | skip the refresh through the API and the form | e2e m01 | pass |
| F12-M02 | check now: refused without config or state, clean, drifted (found by the simulator convention), no lock, cannot be applied, real runs go on, other organization 404 | e2e m02 | pass |
| F12-M03 | scheduled check, then again after the interval | e2e m03 | pass |
| F12-M04 | interval validation, members refused, null turns it off, leaving it out keeps it | e2e m04 | pass |
| F12-M05 | never more than one check waiting behind a slow engine | e2e m05 | pass |
| F12-M06 | a failed check is recorded with its time | e2e m06 | pass |
| F12-M07 | settings page field and the check-now button | e2e m07 | pass |
| F12-M08 | refresh_only through the API is plan-only, cannot skip | e2e m08 | pass |
| F12-X | 14 mutation checks, all turn a test red (one, queuing a check without a state, needed a stronger test first) | mutation | see backend.md |

Total: 277 tests (was 259), run on PostgreSQL with the real tofu and Docker cases, none skipped.

## F13 pull-request runs (fix P2)

| id | what | how | result |
| --- | --- | --- | --- |
| F13-C | HMAC-SHA256 against the RFC 4231 vectors; a claimed signature is compared whole, in constant time, never panics | unit | pass |
| F13-A | a GitHub-style archive (one top folder, optional folder) is read; nested module folders still refused | unit | pass |
| F13-D | decisions: same-repository pull requests planned; forks, other repositories, other actions, bad hashes, other branches, deleted branches ignored | unit | pass |
| F13-T | comment text: plan line, changes, lint, link; bounded; hostile text defanged | unit | pass |
| F13-S | link secrets sealed, rotated, token kept or replaced, input validation, removal with the workspace; a run keeps its origin | store contract + memory | pass (both stores) |
| F13-N01 | connect through the API: secret once, no secret on read, validation, rights, other organization | e2e n01 | pass |
| F13-N02 | unsigned, wrongly signed, re-signed and tampered deliveries refused; ping; unknown workspace; the old secret after rotation | e2e n02 | pass |
| F13-N03 | a pull request is planned (speculative, no lock, lint) and commented with the token | e2e n03 | pass |
| F13-N04 | redelivery planned once; new commit and another pull request planned again | e2e n04 | pass |
| F13-N05 | forks, other repository, closed, labelled, other branch, other event: nothing fetched or queued | e2e n05 | pass |
| F13-N06/07 | push: only the configured folder, waits for confirmation, no comment; auto-apply applies | e2e n06, n07 | pass |
| F13-N08 | a commit that cannot be fetched or has no `.tf` files is reported on the pull request | e2e n08 | pass |
| F13-N09 | no token: downloaded anonymously, nothing posted, the log says why | e2e n09 | pass |
| F13-N10 | a failed plan is reported; `@octocat` in a resource name notifies nobody | e2e n10 | pass |
| F13-N11 | settings page: connect, secret shown once, disconnect | e2e n11 | pass |
| F13-R | a real tarball from github.com: redirect followed, top folder as expected, 404 message | opt-in unit test | pass (once) |
| F13-X | 17 mutation checks, all turn a test red | mutation | see backend.md |

Total: 302 tests (was 277), run on PostgreSQL with the real tofu and Docker cases; the opt-in real-GitHub download test was run once separately.
