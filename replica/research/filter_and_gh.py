import csv, json, re, sys, urllib.request, urllib.parse, time, os
scr = sys.argv[1]
platform = re.compile(r"terraform cloud|hcp terraform|\btfc\b|terraform enterprise|\btfe\b|spacelift|env0|atlantis|scalr|opentofu|open tofu|\btofu\b", re.I)
opinion = re.compile(r"\b(hate|annoy|frustrat|painful|pain|slow|expensive|overpriced|pricing|price|cost|costly|stuck|broken|bug|buggy|lock(ed|ing)?|outage|down|unreliable|clunky|limit(ed|ation)?|wish|missing|lacks?|can'?t|cannot|doesn'?t|don'?t|switch(ed|ing)?|migrat|alternative|moved (to|away)|lock-?in|license|rug|pull|drift|rum\b|per resource|worse|terrible|awful|nightmare|ugly|confusing|complicated|painpoint|issue|problem|sucks?|love|great|recommend|better)\b", re.I)
rows = list(csv.DictReader(open(scr + "/hn_raw.csv", encoding="utf-8")))
keep = [r for r in rows if platform.search(r["text"]) and opinion.search(r["text"])]
print("hn comments naming a platform or alternative:", sum(1 for r in rows if platform.search(r["text"])), "| with an opinion signal:", len(keep))

# GitHub: the most-voted open OpenTofu issues (the project's own request board; reactions are the votes)
def gh(url):
    req = urllib.request.Request(url, headers={"User-Agent": "replica-research/1.0", "Accept": "application/vnd.github+json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)

issues = []
for page in (1, 2):
    q = urllib.parse.urlencode({"q": "repo:opentofu/opentofu is:issue is:open", "sort": "reactions-+1", "order": "desc", "per_page": 50, "page": page})
    try:
        d = gh("https://api.github.com/search/issues?" + q)
    except Exception as e:
        print("github skipped:", e)
        break
    issues += d.get("items", [])
    time.sleep(1)
gh_rows = []
for it in issues:
    votes = it.get("reactions", {}).get("+1", 0)
    body = (it.get("body") or "").strip()
    text = it["title"].strip()
    if body:
        text += "\n\n" + body[:700]
    gh_rows.append({"source": "github-opentofu", "url": it["html_url"], "date": (it.get("created_at") or "")[:10], "rating": "", "text": text, "_votes": votes})
print("github issues:", len(gh_rows), "top votes:", [r["_votes"] for r in gh_rows[:8]])

out = scr + "/reviews_all.csv"
with open(out, "w", newline="", encoding="utf-8") as fh:
    w = csv.DictWriter(fh, fieldnames=["source", "url", "date", "rating", "text"])
    w.writeheader()
    for r in keep + gh_rows:
        w.writerow({k: r[k] for k in ["source", "url", "date", "rating", "text"]})
json.dump({r["url"]: r["_votes"] for r in gh_rows}, open(scr + "/gh_votes.json", "w"))
print("total rows", len(keep) + len(gh_rows))
