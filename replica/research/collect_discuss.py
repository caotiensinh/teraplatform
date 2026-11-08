"""Opening posts of public HashiCorp Discuss topics about HCP Terraform / Terraform Cloud (Discourse public search API).
Only the topic starters' first posts are kept (the people describing a problem or asking for something), not staff replies."""
import csv, json, re, sys, time, urllib.parse, urllib.request

QUERIES = ["HCP Terraform", "terraform cloud run stuck", "terraform cloud pricing", "terraform cloud workspace lock", "terraform cloud state",
           "terraform cloud slow", "terraform cloud feature request", "terraform cloud agent", "terraform cloud API", "HCP Terraform resources under management",
           "terraform cloud drift", "terraform cloud queue", "terraform cloud error", "terraform cloud plan", "terraform cloud apply failed", "terraform cloud cost estimation",
           "terraform cloud variable sets", "terraform cloud private registry", "terraform cloud VCS", "terraform cloud run task"]

def get(u):
    with urllib.request.urlopen(urllib.request.Request(u, headers={"User-Agent": "replica-research/1.0"}), timeout=30) as r:
        return json.load(r)

staff = set()
posts = {}
slugs = {}
for q in QUERIES:
    for page in (1, 2):
        try:
            d = get("https://discuss.hashicorp.com/search.json?" + urllib.parse.urlencode({"q": q, "page": page}))
        except Exception as e:
            print("skip", q, page, e, file=sys.stderr)
            break
        for u in d.get("users", []):
            if u.get("moderator") or u.get("admin"):
                staff.add(u["username"])
        for t in d.get("topics", []):
            slugs[t["id"]] = t["slug"]
        for p in d.get("posts", []):
            if p.get("post_number") == 1:
                posts[p["id"]] = p
        time.sleep(0.4)
    print(q, len(posts), file=sys.stderr)

rows = []
for pid, p in posts.items():
    if p.get("username") in staff:
        continue
    try:
        full = get(f"https://discuss.hashicorp.com/posts/{pid}.json")
    except Exception as e:
        print("skip post", pid, e, file=sys.stderr)
        continue
    raw = (full.get("raw") or "").strip()
    raw = re.sub(r"```.*?```", "", raw, flags=re.S).strip()   # log dumps are not opinions
    if len(raw) < 80:
        continue
    raw = raw[:1500]
    rows.append({"source": "hashicorp-discuss", "url": f"https://discuss.hashicorp.com/t/{slugs.get(p['topic_id'], 'topic')}/{p['topic_id']}",
                 "date": (p.get("created_at") or "")[:10], "rating": "", "text": raw})
    time.sleep(0.25)
with open(sys.argv[1], "w", newline="", encoding="utf-8") as fh:
    w = csv.DictWriter(fh, fieldnames=["source", "url", "date", "rating", "text"])
    w.writeheader()
    w.writerows(rows)
print("rows", len(rows))
