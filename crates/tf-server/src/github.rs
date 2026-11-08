//! Pull-request runs: a repository (GitHub) tells the platform about pull requests and pushes
//! through a signed webhook; the platform fetches the commit, plans it as a speculative run and
//! comments the result on the pull request. A push to the tracked branch queues a run that applies
//! (after confirmation, unless the workspace auto-applies).
//!
//! What is trusted: only a request signed with the workspace's webhook secret (HMAC-SHA256 over the
//! exact body). What is not run: pull requests from forks (their code would run with this
//! workspace's secrets and variables), and anything for another repository.
//!
//! Nothing here has been tried against github.com itself; the tests play GitHub with a local
//! server that answers the two calls the platform makes (download a tarball, post a comment).

use crate::{queue_run_on, AppState};
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;
use tf_core::*;
use tf_store::Store;

/// Most the platform will download for one commit.
const MAX_TARBALL: usize = 25_000_000;
/// Most changes and findings listed in a comment.
const COMMENT_LISTED: usize = 25;

/// What a webhook delivery asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Decision {
    Ping,
    /// Not for us, with the reason (answered 202 so GitHub does not retry or flag it).
    Ignore(String),
    /// Plan this pull request's head commit.
    Plan { pr: u64, sha: String },
    /// A push to the tracked branch: plan it for applying.
    Apply { sha: String },
}

fn is_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit()) && s.chars().any(|c| c != '0')
}

/// Decides what to do with one delivery. Pure: no store, no network.
pub fn decide(event: &str, payload: &Value, link: &VcsLink) -> Decision {
    let repo_of = |v: &Value| v.get("full_name").and_then(Value::as_str).map(str::to_ascii_lowercase);
    let ours = link.repo.to_ascii_lowercase();
    match event {
        "ping" => Decision::Ping,
        "pull_request" => {
            if repo_of(&payload["repository"]).as_deref() != Some(ours.as_str()) {
                return Decision::Ignore("this event is for another repository".into());
            }
            let action = payload["action"].as_str().unwrap_or("");
            if !matches!(action, "opened" | "reopened" | "synchronize") {
                return Decision::Ignore(format!("pull request action \"{action}\" does not need a plan"));
            }
            let pr = &payload["pull_request"];
            let head_repo = repo_of(&pr["head"]["repo"]);
            if head_repo.as_deref() != Some(ours.as_str()) {
                return Decision::Ignore("pull requests from forks are not planned: their code would run with this workspace's secrets".into());
            }
            let (Some(number), Some(sha)) = (pr["number"].as_u64(), pr["head"]["sha"].as_str()) else {
                return Decision::Ignore("the pull request has no number or commit".into());
            };
            if !is_sha(sha) {
                return Decision::Ignore("the commit is not a 40-character hash".into());
            }
            Decision::Plan { pr: number, sha: sha.to_ascii_lowercase() }
        }
        "push" => {
            if repo_of(&payload["repository"]).as_deref() != Some(ours.as_str()) {
                return Decision::Ignore("this event is for another repository".into());
            }
            if payload["ref"].as_str() != Some(format!("refs/heads/{}", link.branch).as_str()) {
                return Decision::Ignore("the push is not to the tracked branch".into());
            }
            if payload["deleted"].as_bool() == Some(true) {
                return Decision::Ignore("the branch was deleted".into());
            }
            match payload["after"].as_str() {
                Some(sha) if is_sha(sha) => Decision::Apply { sha: sha.to_ascii_lowercase() },
                _ => Decision::Ignore("the push has no commit".into()),
            }
        }
        other => Decision::Ignore(format!("event \"{other}\" is not used")),
    }
}

/// `sha256=<hex>` → `<hex>`.
fn signature_hex(headers: &HeaderMap) -> Option<&str> {
    headers.get("x-hub-signature-256")?.to_str().ok()?.strip_prefix("sha256=")
}

fn reply(status: StatusCode, what: &str, detail: &str) -> Response {
    (status, [(axum::http::header::CONTENT_TYPE, "application/json")], json!({ "status": what, "detail": detail }).to_string()).into_response()
}

/// `POST /webhooks/github/{workspace}`
pub async fn webhook(State(st): State<AppState>, Path(ws): Path<String>, headers: HeaderMap, body: Bytes) -> Response {
    let creds = match st.store.vcs_credentials(&ws).await {
        Ok(Some(c)) => c,
        Ok(None) => return reply(StatusCode::NOT_FOUND, "not_connected", "this workspace is not connected to a repository"),
        Err(_) => return reply(StatusCode::NOT_FOUND, "not_connected", "this workspace is not connected to a repository"),
    };
    // Before anything else is read: only the holder of the secret may talk to this endpoint.
    let signed = signature_hex(&headers).map(|sig| tf_crypto::hmac_sha256_matches(creds.webhook_secret.as_bytes(), &body, sig)).unwrap_or(false);
    if !signed {
        return reply(StatusCode::UNAUTHORIZED, "bad_signature", "the signature does not match");
    }
    let event = headers.get("x-github-event").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let Ok(payload) = serde_json::from_slice::<Value>(&body) else {
        return reply(StatusCode::BAD_REQUEST, "bad_payload", "the body is not JSON");
    };
    match decide(&event, &payload, &creds.link) {
        Decision::Ping => reply(StatusCode::OK, "pong", "connected"),
        Decision::Ignore(why) => reply(StatusCode::ACCEPTED, "ignored", &why),
        Decision::Plan { pr, sha } => start(st, creds, Some(pr), sha, &payload).await,
        Decision::Apply { sha } => start(st, creds, None, sha, &payload).await,
    }
}

async fn start(st: AppState, creds: VcsCredentials, pr: Option<u64>, sha: String, payload: &Value) -> Response {
    let ws = creds.link.workspace_id.clone();
    // GitHub redelivers; the same commit of the same pull request is planned once.
    if let Ok(runs) = st.store.list_runs(Some(&ws)).await {
        if runs.iter().any(|r| r.vcs.as_ref().map(|v| v.sha == sha && v.pr == pr).unwrap_or(false)) {
            return reply(StatusCode::OK, "duplicate", "this commit was already queued");
        }
    }
    let title: String = payload["pull_request"]["title"].as_str().unwrap_or("").chars().filter(|c| !c.is_control()).take(120).collect();
    let message = match pr {
        Some(n) if title.is_empty() => format!("PR #{n}"),
        Some(n) => format!("PR #{n}: {title}"),
        None => format!("push {}", &sha[..7]),
    };
    tokio::spawn(async move {
        if let Err(e) = queue_from_commit(&st, &creds, pr, &sha, &message).await {
            eprintln!("github: workspace {}: {e}", creds.link.workspace_id);
            if let (Some(n), Some(token)) = (pr, creds.token.as_deref()) {
                let body = format!("**Tera Platform could not plan commit `{}`.**\n\n{}", &sha[..7], sanitize(&e));
                let _ = post_comment(&st.cfg.github_api, token, &creds.link.repo, n, &body).await;
            }
        }
    });
    reply(StatusCode::ACCEPTED, "accepted", "the commit is being fetched and planned")
}

async fn queue_from_commit(st: &AppState, creds: &VcsCredentials, pr: Option<u64>, sha: &str, message: &str) -> Result<(), String> {
    let link = &creds.link;
    let url = format!("{}/repos/{}/tarball/{}", st.cfg.github_api.trim_end_matches('/'), link.repo, sha);
    let tarball = http_get(url, creds.token.clone(), MAX_TARBALL).await?;
    let opts = crate::archive::ExtractOptions { strip_top: true, subdir: link.working_dir.clone() };
    let text = crate::archive::extract_config_with(&tarball, crate::MAX_CONFIG_BYTES, &opts)?;
    let ws = st.store.get_workspace(&link.workspace_id).await.map_err(|e| e.to_string())?;
    let cv = st.store.create_config_version(&ws.id, &text, &format!("github:{}", &sha[..7])).await.map_err(|e| e.to_string())?;
    let (op, auto_apply) = if pr.is_some() { (Operation::PlanOnly, false) } else { (Operation::PlanAndApply, ws.auto_apply) };
    let origin = VcsOrigin { repo: link.repo.clone(), sha: sha.to_string(), pr };
    let run = queue_run_on(&st.store, &ws.id, &cv.id, message, op, auto_apply, "github", RunOptions { vcs: Some(origin), ..Default::default() }).await.map_err(|e| e.to_string())?;
    let _ = st.store.append_log(&run.id, &format!("Fetched {}@{}{}.", link.repo, &sha[..7], if link.working_dir.is_empty() { String::new() } else { format!(" ({})", link.working_dir) })).await;
    Ok(())
}

// ---------- talking to GitHub ----------

async fn http_get(url: String, token: Option<String>, max: usize) -> Result<Vec<u8>, String> {
    tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).redirects(5).user_agent("tera-platform").build();
        let mut req = agent.get(&url).set("Accept", "application/vnd.github+json");
        if let Some(t) = &token {
            req = req.set("Authorization", &format!("Bearer {t}"));
        }
        match req.call() {
            Ok(resp) => {
                let mut buf = Vec::new();
                resp.into_reader().take(max as u64 + 1).read_to_end(&mut buf).map_err(|e| format!("reading the download failed: {e}"))?;
                if buf.len() > max {
                    return Err(format!("the repository download is larger than {} MB", max / 1_000_000));
                }
                Ok(buf)
            }
            Err(ureq::Error::Status(401 | 403, _)) => Err("GitHub refused the access token (401 or 403); check that it can read this repository".into()),
            Err(ureq::Error::Status(404, _)) => Err("GitHub could not find that commit or repository (404); for a private repository the access token must be set".into()),
            Err(ureq::Error::Status(code, _)) => Err(format!("GitHub answered {code}")),
            Err(_) => Err("could not reach GitHub".into()),
        }
    })
    .await
    .map_err(|_| "the download task failed".to_string())?
}

/// Posts a comment on a pull request. Errors carry no token.
pub async fn post_comment(api: &str, token: &str, repo: &str, pr: u64, body: &str) -> Result<(), String> {
    let url = format!("{}/repos/{}/issues/{}/comments", api.trim_end_matches('/'), repo, pr);
    let (token, payload) = (token.to_string(), json!({ "body": body }).to_string());
    tokio::task::spawn_blocking(move || {
        let agent = ureq::AgentBuilder::new().timeout(Duration::from_secs(30)).user_agent("tera-platform").build();
        match agent
            .post(&url)
            .set("Accept", "application/vnd.github+json")
            .set("Authorization", &format!("Bearer {token}"))
            .set("Content-Type", "application/json")
            .send_string(&payload)
        {
            Ok(_) => Ok(()),
            Err(ureq::Error::Status(code, _)) => Err(format!("GitHub answered {code} when posting the comment")),
            Err(_) => Err("could not reach GitHub to post the comment".into()),
        }
    })
    .await
    .map_err(|_| "the comment task failed".to_string())?
}

// ---------- the comment ----------

/// Keeps text from a repository (names, messages) from doing anything in a comment: no code spans
/// broken open, no `@mentions` that would notify someone, no control characters, bounded length.
pub fn sanitize(s: &str) -> String {
    let clean: String = s.chars().filter(|c| !c.is_control() || *c == ' ').map(|c| if c == '`' { '\'' } else { c }).collect();
    let clean = clean.replace('@', "@\u{200b}");
    if clean.chars().count() > 200 {
        format!("{}...", clean.chars().take(200).collect::<String>())
    } else {
        clean
    }
}

/// The markdown posted on a pull request when its plan has finished (`None`: the plan failed).
pub fn comment_for(run: &Run, plan: Option<&PlanSummary>, base_url: &str) -> String {
    let sha = run.vcs.as_ref().map(|v| v.sha.as_str()).unwrap_or("");
    let short = &sha[..sha.len().min(7)];
    let mut out = format!("### Tera Platform plan for `{short}`\n\n");
    match plan {
        None => out.push_str("**The plan failed.** Open the run for the log.\n"),
        Some(s) if !s.has_changes() => out.push_str("**No changes.** The infrastructure matches this configuration.\n"),
        Some(s) => out.push_str(&format!("**Plan: {} to add, {} to change, {} to destroy.**\n", s.add, s.change, s.destroy)),
    }
    if !run.changes.is_empty() {
        out.push('\n');
        for c in run.changes.iter().take(COMMENT_LISTED) {
            let sym = match c.action {
                ChangeAction::Create => "+",
                ChangeAction::Update => "~",
                ChangeAction::Delete => "-",
                ChangeAction::Replace => "-/+",
            };
            out.push_str(&format!("- `{sym} {}`\n", sanitize(&c.addr)));
        }
        if run.changes.len() > COMMENT_LISTED {
            out.push_str(&format!("- ... and {} more\n", run.changes.len() - COMMENT_LISTED));
        }
    }
    if !run.lint.is_empty() {
        out.push_str(&format!("\n**Lint: {} finding(s)**\n\n", run.lint.len()));
        for f in run.lint.iter().take(COMMENT_LISTED) {
            let at = match (&f.file, f.line) {
                (Some(file), Some(l)) => format!(" `{}:{l}`", sanitize(file)),
                (None, Some(l)) => format!(" `line {l}`"),
                _ => String::new(),
            };
            out.push_str(&format!("- {}: {}{at}\n", f.severity, sanitize(&f.message)));
        }
        if run.lint.len() > COMMENT_LISTED {
            out.push_str(&format!("- ... and {} more\n", run.lint.len() - COMMENT_LISTED));
        }
    }
    out.push_str(&format!("\n[Open the run]({}/runs/{})\n", base_url.trim_end_matches('/'), run.id));
    out
}

/// Where the runner reports finished pull-request plans.
#[derive(Clone)]
pub struct Reporter {
    pub base_url: String,
    pub github_api: String,
}

impl Reporter {
    /// Posts the result of a finished plan on its pull request, if it has one and the workspace
    /// has an access token. What happens is written to the run's log.
    pub async fn plan_finished(&self, store: &Arc<dyn Store>, run: &Run, plan: Option<&PlanSummary>) {
        let Some(origin) = &run.vcs else { return };
        let Some(pr) = origin.pr else { return };
        let say = |line: String| async move {
            let _ = store.append_log(&run.id, &line).await;
        };
        let creds = match store.vcs_credentials(&run.workspace_id).await {
            Ok(Some(c)) if c.link.repo.eq_ignore_ascii_case(&origin.repo) => c,
            _ => return,
        };
        let Some(token) = creds.token.as_deref() else {
            say("No access token is set for this repository, so the result was not posted on the pull request.".into()).await;
            return;
        };
        // the run as it is now: the changes and findings were stored just before this is called
        let current = store.get_run(&run.id).await.unwrap_or_else(|_| run.clone());
        let body = comment_for(&current, plan, &self.base_url);
        match post_comment(&self.github_api, token, &origin.repo, pr, &body).await {
            Ok(()) => say(format!("Posted the result on pull request #{pr}.")).await,
            Err(e) => say(format!("Could not post the result on pull request #{pr}: {e}")).await,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link() -> VcsLink {
        VcsLink { workspace_id: "w".into(), repo: "Acme/Infra".into(), branch: "main".into(), working_dir: String::new(), has_token: true, created_at: 0 }
    }
    const SHA: &str = "0123456789abcdef0123456789abcdef01234567";

    fn pr(action: &str, head_repo: &str) -> Value {
        json!({ "action": action, "repository": { "full_name": "acme/infra" },
            "pull_request": { "number": 7, "title": "Add a bucket", "head": { "sha": SHA, "repo": { "full_name": head_repo } } } })
    }

    #[test]
    fn a_pull_request_from_the_same_repository_is_planned() {
        for a in ["opened", "reopened", "synchronize"] {
            assert_eq!(decide("pull_request", &pr(a, "acme/infra"), &link()), Decision::Plan { pr: 7, sha: SHA.into() }, "{a}");
        }
        assert_eq!(decide("pull_request", &pr("opened", "ACME/INFRA"), &link()), Decision::Plan { pr: 7, sha: SHA.into() }, "repository names compare without case");
    }

    #[test]
    fn forks_other_actions_and_other_repositories_are_ignored_with_a_reason() {
        let fork = decide("pull_request", &pr("opened", "mallory/infra"), &link());
        assert!(matches!(&fork, Decision::Ignore(w) if w.contains("forks")), "{fork:?}");
        for a in ["closed", "edited", "labeled", ""] {
            assert!(matches!(decide("pull_request", &pr(a, "acme/infra"), &link()), Decision::Ignore(_)), "{a}");
        }
        let mut other = pr("opened", "acme/infra");
        other["repository"]["full_name"] = json!("acme/other");
        assert!(matches!(decide("pull_request", &other, &link()), Decision::Ignore(w) if w.contains("another repository")));
        let mut no_head_repo = pr("opened", "acme/infra");
        no_head_repo["pull_request"]["head"]["repo"] = Value::Null;
        assert!(matches!(decide("pull_request", &no_head_repo, &link()), Decision::Ignore(_)), "a deleted fork has no repository");
        let mut bad_sha = pr("opened", "acme/infra");
        bad_sha["pull_request"]["head"]["sha"] = json!("../../etc/passwd");
        assert!(matches!(decide("pull_request", &bad_sha, &link()), Decision::Ignore(_)), "a commit is 40 hex digits and nothing else");
        bad_sha["pull_request"]["head"]["sha"] = json!("0".repeat(40));
        assert!(matches!(decide("pull_request", &bad_sha, &link()), Decision::Ignore(_)), "the all-zero hash means nothing");
        assert!(matches!(decide("issues", &json!({}), &link()), Decision::Ignore(_)));
        assert_eq!(decide("ping", &json!({}), &link()), Decision::Ping);
    }

    #[test]
    fn a_push_counts_only_for_the_tracked_branch() {
        let push = |r: &str, after: &str, deleted: bool| json!({ "ref": r, "after": after, "deleted": deleted, "repository": { "full_name": "acme/infra" } });
        assert_eq!(decide("push", &push("refs/heads/main", SHA, false), &link()), Decision::Apply { sha: SHA.into() });
        assert!(matches!(decide("push", &push("refs/heads/feature", SHA, false), &link()), Decision::Ignore(_)));
        assert!(matches!(decide("push", &push("refs/heads/main", &"0".repeat(40), true), &link()), Decision::Ignore(_)), "a deleted branch");
        assert!(matches!(decide("push", &push("refs/tags/main", SHA, false), &link()), Decision::Ignore(_)), "a tag is not the branch");
        assert!(matches!(decide("push", &push("refs/heads/main", "nope", false), &link()), Decision::Ignore(_)));
    }

    /// Against the real github.com (set TF_TEST_GITHUB_REAL=1): the download follows GitHub's redirect
    /// and its tarball has the one top folder this code strips. The repository is a public one that holds
    /// no Terraform, so a read archive ends in "no .tf files" and not in an error about the archive.
    #[tokio::test]
    async fn the_real_github_tarball_has_the_layout_this_code_expects() {
        if std::env::var("TF_TEST_GITHUB_REAL").ok().filter(|v| v == "1").is_none() {
            eprintln!("SKIPPED: TF_TEST_GITHUB_REAL not set");
            return;
        }
        let sha = "7fd1a60b01f91b314f59955a4e4d4e80d8edf11d";
        let bytes = http_get(format!("https://api.github.com/repos/octocat/Hello-World/tarball/{sha}"), None, MAX_TARBALL).await.expect("download");
        let opts = crate::archive::ExtractOptions { strip_top: true, subdir: String::new() };
        let err = crate::archive::extract_config_with(&bytes, 1_000_000, &opts).unwrap_err();
        assert!(err.contains("no .tf files"), "the archive was read, and holds no Terraform: {err}");
        let flat = crate::archive::extract_config_with(&bytes, 1_000_000, &crate::archive::ExtractOptions::default());
        assert!(flat.is_err());
        // a commit that does not exist is a clear 404 message
        let missing = http_get("https://api.github.com/repos/octocat/Hello-World/tarball/0000000000000000000000000000000000000001".into(), None, MAX_TARBALL).await.unwrap_err();
        assert!(missing.contains("404"), "{missing}");
    }

    #[test]
    fn text_from_a_repository_cannot_break_out_of_a_comment() {
        let s = sanitize("aws_s3_bucket.`evil` ping @octocat\u{7}\nnext");
        assert!(!s.contains('`') && !s.contains('\u{7}') && !s.contains('\n'), "{s:?}");
        assert!(!s.contains("@octocat"), "a mention is broken so nobody is notified: {s:?}");
        assert!(sanitize(&"x".repeat(500)).chars().count() <= 203);
    }

    fn run_with(changes: Vec<ResourceChange>, lint: Vec<LintFinding>) -> Run {
        Run {
            id: "run-1".into(), workspace_id: "w".into(), config_version_id: "c".into(), status: RunStatus::PlannedAndFinished, operation: Operation::PlanOnly,
            source: "github".into(), message: String::new(), auto_apply: false, summary: None, changes, input_state_serial: None, status_timestamps: Default::default(),
            lint, skip_refresh: false, refresh_only: false, vcs: Some(VcsOrigin { repo: "acme/infra".into(), sha: SHA.into(), pr: Some(7) }), created_at: 0, updated_at: 0,
        }
    }

    #[test]
    fn the_comment_says_what_the_plan_does_lists_changes_and_findings_and_links_the_run() {
        let r = run_with(
            vec![ResourceChange { addr: "aws_s3_bucket.a".into(), action: ChangeAction::Create }, ResourceChange { addr: "aws_x.b".into(), action: ChangeAction::Replace }],
            vec![LintFinding { rule: "unused-variable".into(), severity: "warning".into(), message: "variable \"v\" is declared but never used".into(), file: Some("main.tf".into()), line: Some(3) }],
        );
        let c = comment_for(&r, Some(&PlanSummary { add: 2, change: 0, destroy: 1, outputs: 0 }), "https://tera.example.com/");
        assert!(c.contains("plan for `0123456`") && c.contains("Plan: 2 to add, 0 to change, 1 to destroy."), "{c}");
        assert!(c.contains("- `+ aws_s3_bucket.a`") && c.contains("- `-/+ aws_x.b`"));
        assert!(c.contains("Lint: 1 finding(s)") && c.contains("`main.tf:3`"));
        assert!(c.contains("[Open the run](https://tera.example.com/runs/run-1)"), "no double slash: {c}");
        assert!(comment_for(&r, Some(&PlanSummary::default()), "http://x").contains("No changes."));
        assert!(comment_for(&r, None, "http://x").contains("The plan failed."));
        let many = run_with((0..40).map(|i| ResourceChange { addr: format!("a.r{i}"), action: ChangeAction::Create }).collect(), vec![]);
        let c = comment_for(&many, Some(&PlanSummary { add: 40, ..Default::default() }), "http://x");
        assert!(c.contains("... and 15 more") && !c.contains("a.r39"), "the list is bounded");
    }
}
