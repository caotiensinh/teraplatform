# Brand: Tera Platform

Date: 2026-11-08. The name was chosen by the owner. This file records what was checked, what was not, and the risks found. It is a screening, not legal clearance.

## The name and the risks found

**Tera Platform.** Short, easy to say, and it says "infrastructure at scale". Three risks, in order of weight:

1. **Closeness to the original's name.** The product competes with HCP Terraform and is described next to it. "Tera" is the first four letters of "Terraform", which HashiCorp owns as a trademark. The test a trademark office uses is whether the names are confusingly similar in sound, look or meaning. "Tera" alone is not "Terraform", and "tera" is an ordinary prefix (as in terabyte), but a buyer who sees "Tera Platform" in a list of Terraform platforms may read it as related. **This is the point to put in front of a trademark lawyer before money is spent on the name.** A safer variant is a name with no shared letters.
2. **A well-known software project already called Tera.** On crates.io, `tera` is the most used template engine for Rust (43 million downloads, checked 2026-11-08). This product is written in Rust, so searches, package names and conversations will collide with it.
3. **Other users of "Tera"** (a game, a data-warehouse company, many small firms). Not searched here; the trademark databases below will show them.

## Checks

| check | result | date |
| --- | --- | --- |
| US trademark (tmsearch.uspto.gov, classes 9 and 42) | **to run** | |
| EU trademark (EUIPO eSearch / TMview) | **to run** | |
| Canada (ISED) | **to run** | |
| WIPO Global Brand Database | **to run** | |
| domain `teraplatform.com` | has DNS records: taken or parked, treat as unavailable | 2026-11-08 |
| domain `teraplatform.io`, `teraplatform.dev`, `tera-platform.com` | no DNS answer; **not a registrar check**, availability still to confirm | 2026-11-08 |
| domain `tera.dev`, `tera.io` | have DNS records: taken | 2026-11-08 |
| GitHub `teraplatform` | free (404) | 2026-11-08 |
| GitHub `tera-platform` | **taken** (an account exists) | 2026-11-08 |
| crates.io `tera-platform`, npm `tera-platform` | free (404) | 2026-11-08 |
| crates.io `tera` | taken, very popular (see risk 2) | 2026-11-08 |
| App Store / Google Play | not applicable (a web product) | |
| X, Instagram, TikTok handles | **to run** | |
| web search "tera platform" + category | **to run** | |

## Palette

Unchanged from the design step: a teal accent (`--color-accent: #0b6b63`, dark mode `#4fd1c0`), chosen in `design/tokens.css` with the contrast checker at the time. It shares no family with the original's purple. The original's purples (`#7B42BC`, `#844FBA`, `#5C4EE5`) are in `brand.json` so the sweep catches them; none was found.

## Logo brief

- **Idea:** a wordmark "Tera" with a small mark made of stacked layers (state, plan, apply), reading as a platform that holds infrastructure steady.
- **Type:** symbol plus wordmark. Must work at 16 px (favicon) and as a 1024 px app icon.
- **Deliverables:** SVG, 1024x1024 icon with no transparency, favicon set, 1200x630 social image.
- **Must not resemble the original's mark** (from memory, not checked against the current logo: a purple mark built from angled blocks): no block-built letter, no purple, no stacked angled blocks. Put the drafts next to the original's logo and check.

## Voice

Three words: **direct** (not blunt), **calm** (not casual), **exact** (not technical for its own sake).

| do | don't |
| --- | --- |
| "Plan finished: 2 to add, 0 to change." | "Awesome! Your plan is ready!" |
| "Stopping this run can leave resources that exist but are not recorded." | "Are you sure?" |
| "The workspace is locked by a run." | "Oops, something went wrong." |
| "Type the workspace name to delete it." | "This action is dangerous." |
| "Nothing to apply." | "All good!" |

The existing screens already follow this voice (the force-cancel and delete warnings, the lock banners). The strings that carried the placeholder name now say "Tera Platform" (page titles, the top bar, the sign-in page).

## Sweep

`python3 sweep.py . --config replica/brand.json` on 2026-11-08: clean. It found one hit on the way, a code comment naming the original's API; reworded.

Not covered by the sweep and still to do by eye: favicon, OG image, email templates (the development mailer prints plain text), app icon. None exists yet.

## What is left to rename

Crate and binary names still say `tf-*` and `tfclone` (internal; `tf-server` is the binary users run). Renaming them is mechanical but touches every test and script, so it was not done. Say if you want it.
