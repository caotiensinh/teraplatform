# Pricing: Tera Platform

Date: 2026-11-08. Set by Claude on the owner's request ("price it yourself"). **It is a proposal, not a measured price:** there is no cost data, no customer interview and no hosted service yet, so every number below is a judgement from the competitors' public prices and the research. Change it freely; the structure (flat, no per-resource billing) is the part the evidence supports.

## What the others charge (read on 2026-11-08)

| product | model | public prices | source |
| --- | --- | --- | --- |
| HCP Terraform (the original) | per resource per month | Essentials from $0.10, Standard from $0.47, Premium from $0.99; Enterprise (self-managed) custom; a $500 trial credit | <https://www.hashicorp.com/en/pricing?tab=terraform> |
| Spacelift | per plan, limits on users and workers | Free $0 (2 users); Starter from $399 / month (10 users, 2 public workers); Business and Enterprise on quote | <https://spacelift.io/pricing> |
| env zero | per run, free tier | Free: 250 runs / month, 30 active environments, one self-hosted runner; paid plans custom, "no per-resource (RUM) pricing" | <https://www.env0.com/pricing> |
| Scalr | per run | Free: up to 50 runs / month; Business $0.99 per run with volume discounts (100 runs = $99 / month) | <https://scalr.com/pricing/> |

What the original's bill looks like, computed from its public per-resource rates (Essentials / Standard / Premium, per month):

| resources managed | Essentials | Standard | Premium |
| --- | --- | --- | --- |
| 500 | $50 | $235 | $495 |
| 5,000 | $500 | $2,350 | $4,950 |
| 20,000 | $2,000 | $9,400 | $19,800 |
| 100,000 | $10,000 | $47,000 | $99,000 |

The bill grows with the number of things managed, not with how much the product is used. That is the complaint in the research.

## What users said about price (from `feedback.md` and `fixes.md`)

45 of the 712 rows match the price theme, across Hacker News, HashiCorp Discuss and GitHub; by hand-sampling about 5 of 8 are really about price. 20 of the 45 are from 2023, the year of the change to per-resource pricing, and 8 more from 2024: **older evidence, and the per-resource model is still what the page above shows today.** Two of the sharpest, for the owner's reference only (research, never testimonials):

> "10 cents per resource per month my ass. We have around 100k~ resources at an early-stage startup I'm at, our AWS bill is $50~/mo and TFC wants to charge me $10k/mo for that?" - <https://news.ycombinator.com/item?id=39330975>

> "the bill for our team of 5 engineers would have been roughly 20x" - <https://news.ycombinator.com/item?id=39322968>

## The model

**Flat price per organization. Unlimited resources, unlimited workspaces. The bill depends on how many people and how many runs can work at once, never on how much infrastructure is managed.** Monthly, or yearly at ten months' price (two months free). Self-hosted and hosted cost the same; the hosted service does not exist yet (see "What is real today").

Three tiers, named by who they are for:

| | **Solo & small teams** | **Team** | **Business** |
| --- | --- | --- | --- |
| price | **$0** forever | **$79** / month, or $790 / year | **$299** / month, or $2,990 / year |
| for | one person or a small team trying it or running a few stacks | a team that runs infrastructure for a product | several teams and a platform group |
| users | up to 5 | up to 25 | unlimited |
| resources, workspaces | unlimited | unlimited | unlimited |
| concurrent runs | 2 | 10 | 50 |
| state, locking, variables, plan / confirm / apply, API, CLI backend | yes | yes | yes |
| lint in the plan (**built**), pull-request plans and free drift detection (**planned**) | lint | lint; the rest when built | lint; the rest when built |
| audit log | no | 90 days | 1 year |
| runner agents, SSO (**planned**) | no | no | yes |
| support | community | email, 2 business days | email, 1 business day |

Beyond Business (more than 50 concurrent runs, air-gapped, a contract): talk to us, no list price.

### Why these numbers

- **Free tier:** the competitors that charge for the first step lose the people who then recommend a tool; env zero (250 runs) and Scalr (50 runs) give a free tier, Spacelift gives 2 users, HCP gives a trial credit. A flat free tier with unlimited resources answers the research complaint directly for a small team: the person with 100,000 resources and a $50 cloud bill pays $0.
- **Team at $79:** under a fifth of Spacelift Starter ($399 for 10 users) for 25 users, and below what HCP Essentials charges for 800 resources. It is deliberately low: with no cost data the risk of pricing too low is cheap to fix later (a new customer pays a new price), the risk of pricing too high is losing the people the angle is for.
- **Business at $299:** a team of this size on HCP Standard pays that for about 640 resources. The step from Team is the things a platform group needs (agents, SSO, a year of audit log), not volume.
- **Yearly at ten months** is the usual convention; it is not a researched number.

## Billing promises (the complaints, fixed in the product)

The research mostly complains about the *model* (per resource), not about cancelling or refunds (one thin match on billing). These are therefore commitments rather than answers to evidence; **the owner must confirm each before it goes on a page:**

1. The bill never changes because resources were added; only the plan does.
2. Cancel in one click from the billing page; the plan runs to the end of the period, then drops to Solo; state and data stay.
3. A renewal email 14 days before a yearly renewal, with the amount.
4. A price rise gives 90 days' notice and existing customers keep their price for 12 months.
5. Going over a limit (users, concurrent runs) never stops a run; extra runs wait in the queue and the page says which limit and what the next plan costs.

## What is real today (this decides what may be sold)

- **Built:** lint in the plan step (nine rules, never blocks), drift checks on a schedule or on request, accounts and roles, workspaces and settings, state with locks and leases, plan / confirm / apply with a real `tofu` in a sandbox, live logs, force-cancel, the JSON:API, the OpenTofu http backend, archive upload, rate limit.
- **Not built, so not for sale:** pull-request plans (fix P2 in `fixes.md`; P1 lint and P3 drift checks are built, but drift has not been run against a real cloud provider yet), runner agents, SSO, audit log, billing itself, and a hosted service. There is no Stripe code and no seat or run counting in the product. **Until those exist the honest offer is a free self-hosted beta**; the table above becomes true feature by feature, and a tier should not be charged for a row marked planned.
- The users limit and the concurrent-run limit need counting code in the server before they can be enforced.

## Stripe objects to create (the owner creates them, after the code exists)

| product | price | interval |
| --- | --- | --- |
| Tera Platform Team | $79 | monthly |
| Tera Platform Team | $790 | yearly |
| Tera Platform Business | $299 | monthly |
| Tera Platform Business | $2,990 | yearly |

Use Stripe's customer portal for cancel and invoices so that promise 2 needs no code of ours.

## Open questions for the owner

1. Is the product sold hosted (needs infrastructure and a cost model), self-hosted with a licence key, or both? The table assumes the same price for both.
2. Under which licence is the code published? The pricing assumes the free tier is open source and the paid rows are separately licensed or hosted; that is a legal decision.
3. The name has trademark risks (`brand.md`); the Stripe product names above use it.
