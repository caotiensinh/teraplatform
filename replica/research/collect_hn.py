"""Collect public Hacker News comments about HCP Terraform / OpenTofu through the official Algolia API.
Only comments that name the products are kept; text is copied exactly (HTML tags removed, entities decoded)."""
import csv, html, json, re, sys, time, urllib.parse, urllib.request

QUERIES = [
    "terraform cloud", "hcp terraform", "terraform enterprise", "terraform cloud pricing",
    "terraform cloud alternative", "terraform alternative", "opentofu", "opentofu migration",
    "spacelift terraform", "atlantis terraform", "env0 terraform", "scalr terraform",
    "terraform state lock", "terraform plan slow", "terraform drift detection", "terraform remote state",
    "terraform resources under management", "terraform license change", "terraform cloud runs",
    "terragrunt terraform cloud", "terraform workspaces", "terraform cloud outage",
]
MENTION = re.compile(r"terraform cloud|hcp terraform|tfc\b|terraform enterprise|tfe\b|spacelift|env0|atlantis|scalr|opentofu|open tofu|\btofu\b|terraform|terragrunt", re.I)
TAG = re.compile(r"<[^>]+>")


def clean(t):
    t = re.sub(r"<p>", "\n\n", t)
    t = re.sub(r"<br\s*/?>", "\n", t)
    t = TAG.sub("", t)
    return html.unescape(t).strip()


def fetch(q, page):
    u = "https://hn.algolia.com/api/v1/search?" + urllib.parse.urlencode({"query": q, "tags": "comment", "hitsPerPage": 100, "page": page})
    with urllib.request.urlopen(urllib.request.Request(u, headers={"User-Agent": "replica-research/1.0"}), timeout=30) as r:
        return json.load(r)


seen, rows = set(), []
for q in QUERIES:
    for page in range(0, 3):
        try:
            data = fetch(q, page)
        except Exception as e:
            print("skip", q, page, e, file=sys.stderr)
            break
        for h in data.get("hits", []):
            oid = h.get("objectID")
            text = clean(h.get("comment_text") or "")
            if not oid or oid in seen or not (80 <= len(text) <= 2500) or not MENTION.search(text):
                continue
            seen.add(oid)
            rows.append({"source": "hacker-news", "url": "https://news.ycombinator.com/item?id=" + oid,
                         "date": (h.get("created_at") or "")[:10], "rating": "", "text": text})
        if page + 1 >= data.get("nbPages", 1):
            break
        time.sleep(0.3)
    print(q, len(rows), file=sys.stderr)
    time.sleep(0.3)

out = sys.argv[1]
with open(out, "w", newline="", encoding="utf-8") as fh:
    w = csv.DictWriter(fh, fieldnames=["source", "url", "date", "rating", "text"])
    w.writeheader()
    w.writerows(rows)
print("rows", len(rows))
