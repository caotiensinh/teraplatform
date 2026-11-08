# Data sources and provenance

This folder holds a dataset of **public discussion** about HCP Terraform, OpenTofu and tools around them, collected on **2026-11-08** as market research for this project (see `fixes.md` for what was concluded from it). This page says where every row came from, how it was collected and filtered, what was changed, and what rights apply. If something here is wrong or you want a row removed, see the last section.

## The files

| file | what | rows |
| --- | --- | --- |
| `reviews.csv` | the dataset used for `feedback.md` and `fixes.md` | 713 (712 after removing one duplicate when ranked) |
| `reviews-dropped-jobads.csv` | rows removed before ranking because they were hiring adverts, kept so the filter can be checked | 73 |
| `themes.json` | the keyword themes used to rank | |
| `research/collect_hn.py`, `research/collect_discuss.py`, `research/filter_and_gh.py` | the scripts that collected the rows | |

## Per-row provenance

Every row of both CSV files carries its own source:

| column | meaning |
| --- | --- |
| `source` | `hacker-news`, `hashicorp-discuss` or `github-opentofu` |
| `url` | the permalink of the comment, topic or issue. **Open it to see the author, the date and the surrounding thread.** Author names were not collected |
| `date` | when the text was posted (UTC date, as the source reports it) |
| `rating` | empty: none of these sources has star ratings |
| `text` | the text, copied exactly except as listed under "What was changed" |
| `retrieved_on` | `2026-11-08` for every row |
| `retrieved_via` | the API call that returned it |
| `votes` | GitHub issues only: the number of thumbs-up reactions on 2026-11-08 |
| `redacted` | `1` when an e-mail address was removed from the text |

## The three sources

| source | rows | posted between | how it was collected | terms |
| --- | --- | --- | --- | --- |
| **Hacker News** comments (<https://news.ycombinator.com/>) | 386 | 2014-07-29 and 2026-10-29 | the Algolia HN Search API, <https://hn.algolia.com/api>, `tags=comment`, 22 search phrases (`research/collect_hn.py`), up to 3 pages of 100 each | the site's terms: <https://www.ycombinator.com/legal/>; the data comes from the public HN API, <https://github.com/HackerNews/API> |
| **HashiCorp Discuss** opening posts (<https://discuss.hashicorp.com/>) | 227 | 2019-06-28 and 2026-11-02 | the forum's public Discourse API: `/search.json` for 20 search phrases, then `/posts/{id}.json` for the first post of each topic (`research/collect_discuss.py`); API reference <https://docs.discourse.org/> | the forum's terms: <https://discuss.hashicorp.com/tos> |
| **OpenTofu issues** (<https://github.com/opentofu/opentofu/issues>) | 100 | 2023-08-26 and 2026-09-18 | the GitHub REST API, `GET /search/issues` with `q=repo:opentofu/opentofu is:issue is:open`, sorted by reactions, the first 100 (`research/filter_and_gh.py`); API reference <https://docs.github.com/en/rest/search/search#search-issues-and-pull-requests> | GitHub's terms: <https://docs.github.com/en/site-policy/github-terms/github-terms-of-service> |

Requests were made once, identified as `replica-research/1.0`, at a low rate, with no login, and only the public endpoints above. Nothing was scraped from a page. One Discuss query was refused by the forum for sending too many requests (HTTP 429) and its results are missing from the dataset.

### The search phrases

- **Hacker News (22):** terraform cloud; hcp terraform; terraform enterprise; terraform cloud pricing; terraform cloud alternative; terraform alternative; opentofu; opentofu migration; spacelift terraform; atlantis terraform; env0 terraform; scalr terraform; terraform state lock; terraform plan slow; terraform drift detection; terraform remote state; terraform resources under management; terraform license change; terraform cloud runs; terragrunt terraform cloud; terraform workspaces; terraform cloud outage.
- **HashiCorp Discuss (20):** HCP Terraform; terraform cloud run stuck / pricing / workspace lock / state / slow / feature request / agent / API / drift / queue / error / plan / apply failed / cost estimation / variable sets / private registry / VCS / run task; HCP Terraform resources under management.

## How rows were chosen

1. **Hacker News:** the search phrases returned 2,568 distinct comments of 80 to 2,500 characters that mention Terraform or a related tool. Of these, 671 named a platform or an alternative. A comment was kept only if it was 80 to 2,500 characters long, **named a platform or alternative** (terraform cloud, hcp terraform, tfc, terraform enterprise, spacelift, env0, atlantis, scalr, opentofu) **and** contained at least one word that signals an opinion (for example expensive, slow, stuck, lock, wish, switch, migrate, license, drift). That left 459 (386 after the hiring adverts of step 4 were removed). The exact patterns are in `research/filter_and_gh.py`. This is a keyword filter: it admits comments that are not opinions and misses opinions in other words.
2. **HashiCorp Discuss:** only the **first post of each topic**, never staff replies (accounts flagged moderator or admin were skipped), with fenced code blocks removed and the text cut at 1,500 characters.
3. **OpenTofu issues:** the issue title, then the first 700 characters of the body.
4. **Hiring adverts** (73 rows) were removed by pattern (for example "we're hiring", "| Remote |") and kept in `reviews-dropped-jobads.csv`. A first, too broad version of this filter wrongly removed 71 real comments (a price such as "$10k" matched a salary pattern); it was caught when a quote failed its check and fixed. The numbers above are after the fix.

## What was changed

- **E-mail addresses were removed** from six rows (`redacted = 1`), replaced by `[email removed]`. They were personal contact details in comments that were not about the product. One address of a cloud service account that appears in a technical post was kept. Nothing else was edited: no spelling, no wording.
- Text is cut at the lengths above, so a row can end in the middle of a longer original; open the `url` for the whole thing.
- Some HN rows show odd characters (for example `don�ft` for an apostrophe). That is how the text came back from the API in this collection, not an edit.
- HTML was turned into plain text for Hacker News (paragraph tags became blank lines, other tags dropped, entities decoded).

## What this dataset cannot say

- It is discussion by people who write on Hacker News, ask for help on a vendor forum, or vote on an open-source project's issues. It is **not a sample of customers** and has no star ratings.
- Counts in `feedback.md` are keyword matches; by hand-reading samples, about half to two thirds were really about the theme (`fixes.md` says how many per theme). A vote count on an OpenTofu issue is the exception: each is a person.
- 118 of the 386 Hacker News comments date from 2023, the year HashiCorp changed its pricing and licence; the pricing complaints are mostly from that period.

## Rights

The text of each comment, post and issue belongs to **its author**. It is reproduced here, with its permalink, **only so that the research in `fixes.md` can be checked**, and is not offered under this project's licence. The collection scripts are this project's own code. Because the sites above allow public reading but do not release their users' words for reuse, **do not treat this dataset as freely redistributable**: before copying it elsewhere, read the terms linked in the table, and do not use it to contact or profile the authors.

## How to cite

For a quotation in a document, cite the `url` of its row and the `date`: *[source], [date], <url>*. For the dataset as a whole: *Tera Platform research data, collected 2026-11-08 from the sources listed in `replica/DATA-SOURCES.md`.*

## Removal

If you wrote one of these texts and want it removed, or the text contains personal data that should not be here, open an issue on this repository with the row's `url`, and the row will be deleted from the file (and the history can be rewritten if that is needed).
