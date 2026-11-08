# Gap analysis: what Terraform / OpenTofu do not meet today

Step 2 of the user's plan (after parity). Each gap is labelled:
**cited** = stated on a public page we read; **hypothesis** = widely
discussed but not verified here, to confirm with `/replica-entrepreneur`
(real reviews, Reddit, Hacker News) before we build on it.

| # | gap | label | evidence | improvement idea in Rust |
| --- | --- | --- | --- | --- |
| G1 | No built-in linting; most-voted OpenTofu issue. Teams bolt on tflint, OPA, Infracost. | cited | opentofu.org/blog/plans-for-linting | one binary: lint + policy + cost in the plan step |
| G2 | Unknown values at plan time block `for_each`, `count`, `enabled`, and blind policy checks on plan JSON. | cited | opentofu.org/blog/opentofu-1-13-0 | partial/graph-aware planning that reports what is unknown and why |
| G3 | Backend credentials and secrets can land in plain text in `.terraform/` and plan files. | cited | opentofu.org backend configuration page | secrets never written to disk; credentials by reference only |
| G4 | OpenTofu has no built-in collaboration layer (docs point to third-party TACOS). HCP Terraform is closed and tier-gated (projects, drift detection, audit logs on paid tiers). | cited | opentofu.org state page; HCP workspaces page | open, self-hosted run platform in one Rust binary |
| G5 | Encryption does not protect against replay or data loss, and cannot protect values from the operator running the command. | cited | opentofu.org state encryption page | signed, versioned state with rollback checks and per-run credentials |
| G6 | Backends are built in only; no backend plugins. | cited | opentofu.org backend page | pluggable storage (S3, Postgres, filesystem) behind one trait |
| G7 | Permission failures and missing resources both return 404 in HCP API (good for secrecy, hard to debug). | cited | API overview | 404 externally, precise reason in audit log and an opt-in debug header |
| G8 | Slow plan/refresh on very large states. | **supported** (2026-11-08) | 131 + 31 votes on OpenTofu issues 1703, 4372 | Rust parallel graph walk, incremental refresh, state sharding |
| G9 | Provider download size and cold-start time in CI. | **weakly supported** | 62 votes for a shared module cache (issue 1199); 4 keyword matches | shared provider cache with content addressing |
| G10 | Drift handling is detect-only and paid in HCP. | **thin** | tier gating cited; 13 mentions, one clear statement in the sample | free drift detection with scheduled refresh-only runs and a fix proposal |
| G11 | HCL error messages and debugging of module trees. | **not supported** | no keyword matches, no vote-backed request in the sample; drop from the plan | span-accurate diagnostics, explain mode |
| G12 | Concurrency: one run per workspace, lock stuck after crash. | partly cited | `force-unlock` exists; locking page not read | lease-based locks with heartbeat, auto-expire, owner visible. **Built 2026-11-08** (weak evidence that users complain: 6 matches, one lock-TTL request with 8 votes) |

## Next verification step

Run `/replica-entrepreneur` after parity: collect 100+ real reviews and
threads, rank themes, and keep only the gaps users actually complain about.
G8 to G12 stay hypotheses until then.

**Done 2026-11-08** (`fixes.md`): 712 public rows from Hacker News, HashiCorp Discuss and the OpenTofu issue board. No star-rated reviews exist for this product. Verdicts are in the table above.
