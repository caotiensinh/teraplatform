# Fixes: what users of the original hate, want and are not served by

Date: 2026-11-08. Evidence: `reviews.csv` (712 rows after removing one duplicate), ranked in `feedback.md` with `themes.json`. Every quote below is an exact substring of a row of `reviews.csv` (checked by the script that wrote this file) and carries its link.

## What this evidence is, and is not

- **There are no star reviews.** HCP Terraform is a web service; it has no app-store listing. The sources are public *discussion*, not product reviews: **386 Hacker News comments**, **227 opening posts on HashiCorp Discuss** (people asking for help), **100 GitHub issues** of OpenTofu (the project's own request board; the vote counts are the issue's thumbs-up reactions on 2026-11-08). 712 rows, 3 sources.
- **Not collected:** G2, Capterra and Trustpilot (their terms and bot checks forbid scraping and reading them row by row by hand was not done), Reddit (needs OAuth under its API terms; not set up), Google Play and App Store (none exist for this product). The sample is therefore tilted toward people who write on Hacker News and ask for help on a forum. It says nothing about the silent majority of customers.
- **Theme counts are keyword matches, an upper bound.** I read a random sample of eight matches (all of them when fewer) for the main themes and counted by hand how many were really about the theme: *pricing* about 5 of 8 (the rest mention a free tier or a price of something else); *licence and lock-in* about 5 of 8 (the rest use the word "fork" or "lock" for something else); *drift* about 3 of 8 were complaints or requests, the rest explanations; *slow plan* 1 of 7 (the rest are timeouts, error dumps, unrelated); *stuck locks* 3 of 6 were about lock errors or lock requests, 1 was a run stuck in a queue; *vcs/PR flow* and *self-hosting* about 5 of 8 and 4 of 8. So read every count below as roughly half to two thirds real. The themes with a vote count from the OpenTofu board are the exception: a vote is a person.
- **73 rows were removed before ranking** as hiring posts (`reviews-dropped-jobads.csv`): they named the products but were adverts, not opinions.
- **Dates:** the pricing complaints are mostly from August 2023 to March 2024 (20 of 45 in 2023, 8 in 2024, 6 in 2025 or 2026), right after HashiCorp's pricing change. **Check the original's current pricing before building anything on that complaint.**
- The OpenTofu issues are about the *CLI and language*, not the hosted platform. They are used for what they are: the votes of people who run this tool.

## 1. What they hate

| # | problem | matches | sources | weight of evidence |
| --- | --- | --- | --- | --- |
| H1 | Price, and being billed per resource under management | 45 | HN, Discuss | strong quotes, but mostly 2023 |
| H2 | The licence change and lock-in to one vendor | 78 | HN, Discuss, GitHub | many, but about the CLI licence; OpenTofu already answers it |
| H3 | Tools get stuck, slow or unreliable (outages, runs not starting, slow refresh) | 13 + 7 + 6 | HN, Discuss, GitHub | weak by keywords; the strongest evidence is the vote count on refresh issues (below) |
| H4 | Drift handling is a paid feature | 13 | HN, Discuss | thin: one clear statement |

**H1 Price per resource.**

> "10 cents per resource per month" - hacker-news, 2024-02, <https://news.ycombinator.com/item?id=39330975>

> "the bill for our team of 5 engineers would have been roughly 20x" - hacker-news, 2024-02, <https://news.ycombinator.com/item?id=39322968>

**H2 Licence and lock-in.**

> "The license changes they made were the icing on that cake." - hacker-news, 2023-11, <https://news.ycombinator.com/item?id=38335530>

> "the license changes made me cautious to integrate anymore." - hacker-news, 2024-04, <https://news.ycombinator.com/item?id=40149550>

**H3 Slow, stuck, unreliable.** Weak by keyword counts. What is solid is the votes below (refresh) and one report of runs that never started:

> "any time I attempt to queue a plan in any of my organizations' terraform workspaces, the plan is stuck in the "Plan Queued" phase." - hashicorp-discuss, 2021-01, <https://discuss.hashicorp.com/t/all-terraform-workspaces-in-organization-get-stuck-in-plan-queued-state-when-trying-to-plan-a-run/20362>

> "Option to skip refreshing resource instances whose configuration hasn't changed since the last apply" - github-opentofu, 2024-06, 131 votes, <https://github.com/opentofu/opentofu/issues/1703>

> "Refreshing takes a long time for moderate-to-large configurations" - github-opentofu, 2026-07, 31 votes, <https://github.com/opentofu/opentofu/issues/4372>

**H4 Drift is paid.**

> "automated drift detection, enterprise SSO, and stack management, to be delivered as an open, self-hostable tool." - hacker-news, 2025-09, <https://news.ycombinator.com/item?id=45361875>

## 2. What is missing (asked for by name)

| # | request | evidence | sources |
| --- | --- | --- | --- |
| M1 | First-party linting in the run | 211 votes on the most-voted OpenTofu request; 17 keyword matches | GitHub, HN, Discuss |
| M2 | Pull-request driven runs (Atlantis-style) | 57 matches, Atlantis named again and again | HN, Discuss |
| M3 | Orchestrating several workspaces together ("stacks") | 202 votes | GitHub |
| M4 | Skip refreshing what has not changed; faster refresh | 131 + 31 votes | GitHub |
| M5 | A shared module and provider cache | 62 votes | GitHub |
| M6 | Self-hostable alternative to HCP Terraform, with agents | 69 matches (many off topic), a few direct statements | HN, Discuss, GitHub |
| M7 | Backends as plugins | 61 votes | GitHub |
| M8 | Lock that expires (TTL) | 8 votes on one issue; this platform already has leases | GitHub |

> "Add first party linting support i.e. `tofu lint`" - github-opentofu, 2024-11, 211 votes, <https://github.com/opentofu/opentofu/issues/2213>

> "Implement the stack concept" - github-opentofu, 2023-11, 202 votes, <https://github.com/opentofu/opentofu/issues/931>

> "Implement TF_MODULE_CACHE_DIR" - github-opentofu, 2024-01, 62 votes, <https://github.com/opentofu/opentofu/issues/1199>

> "Backends as plugins" - github-opentofu, 2023-09, 61 votes, <https://github.com/opentofu/opentofu/issues/382>

> "Leverage DynamoDB TTLs in State Locks" - github-opentofu, 2024-02, 8 votes, <https://github.com/opentofu/opentofu/issues/1222>

> "From experience terragrunt + atlantis has been much better experience than hashicorp's own terraform enterprise service." - hacker-news, 2022-08, <https://news.ycombinator.com/item?id=32659605>

> "I haven't explored what self-hosted alternatives there are to Terraform Cloud." - hacker-news, 2023-03, <https://news.ycombinator.com/item?id=35260902>

## 3. What is unsolved (jobs and groups the original ignores)

- **Small teams priced out.** The quote with `$10k/mo` for a `$50/mo` AWS bill (H1) is a group the per-resource model does not serve: few engineers, many small resources. Evidence: H1 only; thin on its own.
- **Teams that want one tool instead of Terraform Cloud plus Atlantis plus tflint plus a drift script.** Several comments describe stacking them; none is a complaint in so many words. This is an inference from M1, M2 and H4 together, not a quoted need. It is marked as such.
- **No source in this sample describes a group that the original ignores outright.** I looked for "not built for X" statements and found none worth quoting.

## 4. What the evidence says about the gaps G8 to G12

| gap | verdict | why |
| --- | --- | --- |
| G8 slow plan / refresh on large states | **supported** | 131 and 31 votes on refresh requests; keyword matches alone were weak |
| G9 provider download size and cold start | **weakly supported** | 62 votes for a shared module cache; 4 keyword matches on provider size |
| G10 drift is paid and detect-only | **thin** | 13 mentions, one clear statement (H4) |
| G11 HCL error messages and debugging | **not supported** | zero keyword matches; no vote-backed request found |
| G12 stuck locks | **weak** | 6 matches (one HCP run stuck in "Plan Queued", one lock-TTL request with 8 votes) |
| G2 unknown values at plan time (earlier cited) | **weak** | 3 matches, one issue with 25 votes |
| G1 linting (earlier cited) | **confirmed** | 211 votes |

## 5. The fix plan

Chosen by evidence times how cheaply it can be built here. Size: S (days), M (about a week), L (weeks). Each is a row in `features.csv` with `original = no`.

| # | build | size | skill | evidence |
| --- | --- | --- | --- | --- |
| P1 | **Lint in the plan step** (**done 2026-11-08**, see `backend.md`): parse the configuration with an HCL parser and run a small rule set (unused variables, missing provider version pins, deprecated syntax, `tofu validate` first) before planning; show findings on the run page | M | replica-build | M1: 211 votes |
| P2 | **Pull-request runs**: a GitHub webhook (and a pull-request comment) starts a speculative plan and posts the result; merge applies | L | replica-backend | M2: 57 matches; also the missing VCS must-have |
| P3 | **Refresh strategy and free drift detection** (**done 2026-11-08**, partly: see `backend.md`): scheduled refresh-only runs report drift; normal plans can skip the refresh of resources whose configuration did not change | M | replica-backend | M4: 131 + 31 votes; H4 |
| P4 | **Shared provider and module cache** between runs and sandboxes (a mounted cache directory) | S | replica-backend | M5: 62 votes; G9 |
| P5 | **Run triggers across workspaces** (a simple stack: apply B after A) | L | replica-build | M3: 202 votes |
| P6 | **Self-hosted runner agent** that pulls work from the server (one small binary) | L | replica-backend | M6 |
| P7 | Flat, public pricing and no per-resource billing | S (a decision) | replica-launch | H1 |

Not planned: a native HCL evaluator for error messages (G11, no evidence); backends as plugins (the platform keeps state itself; the votes are for the CLI). Lock leases are done (M8).

## 6. The angle

Three options, each grounded in a theme. The recommendation is at the end.

**A. Price.**
For platform teams who found their bill multiplied when HCP Terraform charged per resource, {product} runs Terraform and OpenTofu on your own servers for a flat price.
Evidence: H1, 45 matches across 3 sources, but 20 of 45 are from 2023. **Verify the original's current pricing first.**

**B. One tool for the whole pull-request loop.**
For teams that run Atlantis, a lint script and a drift cron next to their Terraform platform, {product} plans on the pull request, lints, and reports drift in one open, self-hosted binary.
Evidence: M2 57 matches (2 sources), M1 211 votes, H4 one statement. Builds on P1, P2 and P3, which do not exist yet.

**C. Open and yours.**
For teams that left Terraform's licence behind, {product} is an open, self-hostable run platform for OpenTofu that you can read and fork.
Evidence: H2, 78 matches across 3 sources, but OpenTofu already answers the licence part for the CLI.

**Recommendation: B, with A as the pricing page.** B is the only one that is a product difference rather than a price or a licence, and its evidence is two unrelated kinds (a vote count of 211 and a crowd of people naming Atlantis). A rests on the oldest evidence and can be undone by a price cut; C is shared with every OpenTofu-based tool. B is not buildable today: P1 to P3 come first. Do not put the original's name in the product name, ads or store listing.
