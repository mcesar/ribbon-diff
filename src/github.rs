//! Pull requests through the `gh` command: `ribbon pr [NUMBER]` reads one, the status line
//! shows its checks, and `P` posts the notes as a review.

use crate::notes::Note;
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::io::Write;
use std::process::{Command, Stdio};

#[derive(Debug, Clone)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    pub url: String,
    /// `owner/name` of the repository the pull request is in.
    pub repo: String,
    pub base: String,
    pub head: String,
    pub base_ref: String,
}

fn gh(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("gh").args(args).output().context("cannot run gh (install the GitHub CLI: https://cli.github.com)")?;
    if !out.status.success() {
        let error = String::from_utf8_lossy(&out.stderr);
        bail!("gh {}: {}", args.first().copied().unwrap_or(""), error.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git(args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git").args(args).stderr(Stdio::piped()).output()?;
    if !out.status.success() {
        bail!("git {}: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// `owner/name` from a pull request URL: `https://github.com/owner/name/pull/12`.
fn repo_of(url: &str) -> Option<String> {
    let path = url.split("://").nth(1)?.split_once('/')?.1;
    let parts: Vec<&str> = path.split('/').collect();
    (parts.len() >= 2).then(|| format!("{}/{}", parts[0], parts[1]))
}

/// A pull request by number or URL, or the one for the current branch.
pub fn view(which: Option<&str>) -> anyhow::Result<PullRequest> {
    let mut args = vec!["pr", "view"];
    args.extend(which);
    args.extend(["--json", "number,title,url,baseRefOid,headRefOid,baseRefName"]);
    let value: Value = serde_json::from_str(&gh(&args)?).context("gh pr view did not send JSON")?;
    let text = |key: &str| value[key].as_str().map(str::to_owned).with_context(|| format!("gh pr view sent no {key}"));
    let url = text("url")?;
    Ok(PullRequest {
        number: value["number"].as_u64().context("gh pr view sent no number")?,
        title: text("title")?,
        repo: repo_of(&url).with_context(|| format!("cannot read the repository from {url}"))?,
        url,
        base: text("baseRefOid")?,
        head: text("headRefOid")?,
        base_ref: text("baseRefName")?,
    })
}

fn have(commit: &str) -> bool {
    git(&["cat-file", "-e", &format!("{commit}^{{commit}}")]).is_ok()
}

/// The remote for a repository: the one whose URL names it, or else `origin`.
fn remote_for(repo: &str) -> String {
    let remotes = git(&["remote", "-v"]).unwrap_or_default();
    let wanted = repo.to_lowercase();
    remotes
        .lines()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((words.next()?, words.next()?))
        })
        .find(|(_, url)| {
            let url = url.to_lowercase();
            let url = url.strip_suffix(".git").unwrap_or(&url);
            url.ends_with(&format!("/{wanted}")) || url.ends_with(&format!(":{wanted}"))
        })
        .map_or_else(|| "origin".to_owned(), |(name, _)| name.to_owned())
}

/// Fetch both commits of a pull request when this clone does not have them. Only
/// FETCH_HEAD changes: no branch is made or moved.
pub fn fetch(pr: &PullRequest) -> anyhow::Result<()> {
    if have(&pr.base) && have(&pr.head) {
        return Ok(());
    }
    let remote = remote_for(&pr.repo);
    let head = format!("refs/pull/{}/head", pr.number);
    git(&["fetch", "--quiet", "--no-tags", &remote, &head, &pr.base_ref]).with_context(|| format!("cannot fetch pull request #{} from {remote}", pr.number))?;
    if !have(&pr.base) || !have(&pr.head) {
        bail!("fetched {remote}, but it does not have both commits of pull request #{}", pr.number);
    }
    Ok(())
}

/// The `git diff` arguments for what the pull request page shows: the head against the
/// point where it left the base.
pub fn diff_args(pr: &PullRequest) -> Vec<String> {
    vec![format!("{}...{}", pr.base, pr.head)]
}

/// The checks in one line, such as `CI ✗ 2 failed (lint, test) · 9 passed`.
pub fn checks_line(json_text: &str) -> Option<String> {
    let checks: Vec<Value> = serde_json::from_str(json_text).ok()?;
    if checks.is_empty() {
        return Some("CI: no checks".into());
    }
    let mut counts: HashMap<&str, usize> = HashMap::new();
    let mut failed = Vec::new();
    for check in &checks {
        let bucket = check["bucket"].as_str().unwrap_or("pending");
        *counts.entry(bucket).or_default() += 1;
        if bucket == "fail" || bucket == "cancel" {
            failed.push(check["name"].as_str().unwrap_or("?").to_owned());
        }
    }
    let count = |bucket: &str| counts.get(bucket).copied().unwrap_or(0);
    let (fail, pending, pass) = (count("fail") + count("cancel"), count("pending"), count("pass"));
    let mut parts = Vec::new();
    if fail > 0 {
        failed.dedup();
        let mut names = failed.iter().take(3).cloned().collect::<Vec<_>>().join(", ");
        if failed.len() > 3 {
            names.push_str(", …");
        }
        parts.push(format!("{fail} failed ({names})"));
    }
    if pending > 0 {
        parts.push(format!("{pending} running"));
    }
    parts.push(format!("{pass} passed"));
    let mark = if fail > 0 { "✗" } else if pending > 0 { "…" } else { "✓" };
    Some(format!("CI {mark} {}", parts.join(" · ")))
}

/// The checks now. `gh pr checks` exits with an error while checks fail or run, but its
/// JSON is still good.
pub fn checks(pr: &PullRequest) -> Option<String> {
    let number = pr.number.to_string();
    let out = Command::new("gh").args(["pr", "checks", &number, "--repo", &pr.repo, "--json", "name,bucket"]).output().ok()?;
    checks_line(&String::from_utf8_lossy(&out.stdout))
}

/// Lines GitHub accepts comments on: the lines in the diff's hunks, one-based, by path, for
/// the old side and the new side.
pub type Commentable = HashMap<String, (BTreeSet<usize>, BTreeSet<usize>)>;

pub fn commentable(pr: &PullRequest) -> anyhow::Result<Commentable> {
    let patch = git(&["-c", "core.quotepath=false", "diff", "-U3", "--no-color", "--no-ext-diff", "-M", &format!("{}...{}", pr.base, pr.head)])?;
    Ok(crate::patch::parse(&patch)
        .into_iter()
        .map(|file| {
            let lines = |side: &[(usize, String)]| side.iter().map(|(n, _)| n + 1).collect::<BTreeSet<usize>>();
            (file.new_name.clone(), (lines(&file.old_lines), lines(&file.new_lines)))
        })
        .collect())
}

fn body(note: &Note) -> String {
    match &note.rationale {
        Some(rationale) => format!("{}\n\n{rationale}", note.summary),
        None => note.summary.clone(),
    }
}

/// Review comments for notes, on the lines the diff shows. A note whose lines are not in the
/// diff goes into the review's text, with its path and line.
pub fn review(notes: &[&Note], lines: &Commentable) -> (Vec<Value>, Vec<String>) {
    let (mut comments, mut elsewhere) = (Vec::new(), Vec::new());
    for note in notes {
        let shown = lines.get(&note.path).map(|(old, new)| if note.old_side { old } else { new });
        let side = if note.old_side { "LEFT" } else { "RIGHT" };
        let (first, last) = note.range;
        let Some(shown) = shown else {
            elsewhere.push(format!("**{}:{}** {}", note.path, last, body(note)));
            continue;
        };
        if (first..=last).all(|line| shown.contains(&line)) {
            let mut comment = json!({ "path": note.path, "line": last, "side": side, "body": body(note) });
            if first < last {
                comment["start_line"] = first.into();
                comment["start_side"] = side.into();
            }
            comments.push(comment);
        } else if let Some(&line) = shown.range(first..=last).next_back() {
            comments.push(json!({ "path": note.path, "line": line, "side": side, "body": body(note) }));
        } else {
            elsewhere.push(format!("**{}:{}** {}", note.path, last, body(note)));
        }
    }
    (comments, elsewhere)
}

/// The open pull requests, newest first: number, and a line to pick it by.
pub fn list_open() -> anyhow::Result<Vec<(u64, String)>> {
    let text = gh(&["pr", "list", "--state", "open", "--limit", "200", "--json", "number,title,author,headRefName,isDraft"])?;
    let list: Vec<Value> = serde_json::from_str(&text).context("gh pr list did not send JSON")?;
    Ok(list
        .iter()
        .filter_map(|pr| {
            let number = pr["number"].as_u64()?;
            let draft = if pr["isDraft"] == true { " (draft)" } else { "" };
            let line = format!(
                "#{number}  {}{draft}  ·  {}  ·  {}",
                pr["title"].as_str().unwrap_or(""),
                pr["author"]["login"].as_str().unwrap_or("?"),
                pr["headRefName"].as_str().unwrap_or("")
            );
            Some((number, line))
        })
        .collect())
}

/// How a review ends: a comment, an approval, or a request for changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Comment,
    Approve,
    RequestChanges,
}

impl Verdict {
    fn event(self) -> &'static str {
        match self {
            Verdict::Comment => "COMMENT",
            Verdict::Approve => "APPROVE",
            Verdict::RequestChanges => "REQUEST_CHANGES",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Verdict::Comment => "comment",
            Verdict::Approve => "approve",
            Verdict::RequestChanges => "request changes",
        }
    }
}

/// Post a review with comments on the head commit. The review's text is the summary, then the
/// notes that have no line in the diff. Returns the review's URL.
pub fn post_review(pr: &PullRequest, verdict: Verdict, summary: &str, comments: Vec<Value>, elsewhere: &[String]) -> anyhow::Result<String> {
    let text: Vec<&str> = std::iter::once(summary.trim()).filter(|s| !s.is_empty()).chain(elsewhere.iter().map(String::as_str)).collect();
    let request = json!({
        "commit_id": pr.head,
        "event": verdict.event(),
        "body": text.join("\n\n"),
        "comments": comments,
    });
    let reply = api_post(&format!("repos/{}/pulls/{}/reviews", pr.repo, pr.number), &request, "the review")?;
    Ok(reply["html_url"].as_str().unwrap_or(&pr.url).to_owned())
}

/// Post one comment on its own, not in a review. Returns the comment's URL.
pub fn post_comment(pr: &PullRequest, mut comment: Value) -> anyhow::Result<String> {
    comment["commit_id"] = pr.head.clone().into();
    let reply = api_post(&format!("repos/{}/pulls/{}/comments", pr.repo, pr.number), &comment, "the comment")?;
    Ok(reply["html_url"].as_str().unwrap_or(&pr.url).to_owned())
}

/// POST a JSON request with `gh api`, and read the reply.
fn api_post(path: &str, request: &Value, what: &str) -> anyhow::Result<Value> {
    let mut child = Command::new("gh")
        .args(["api", "--method", "POST", path, "--input", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("cannot run gh")?;
    child.stdin.take().context("no stdin for gh")?.write_all(request.to_string().as_bytes())?;
    let out = child.wait_with_output()?;
    let reply: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
    if !out.status.success() {
        let message = reply["message"].as_str().map(str::to_owned).unwrap_or_else(|| String::from_utf8_lossy(&out.stderr).trim().to_owned());
        let details = reply["errors"].as_array().map(|e| e.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default();
        bail!("GitHub refused {what}: {message} {details}");
    }
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(path: &str, old_side: bool, range: (usize, usize)) -> Note {
        Note {
            id: Some("n".into()),
            path: path.into(),
            old_side,
            range,
            anchor: None,
            summary: "s".into(),
            rationale: None,
            author: Some("you".into()),
            status: Some("open".into()),
            reply: None,
            kind: None,
            editable: true,
        }
    }

    #[test]
    fn summarizes_checks() {
        let json = r#"[{"name":"lint","bucket":"fail"},{"name":"test","bucket":"pass"},{"name":"e2e","bucket":"pending"},{"name":"x","bucket":"skipping"}]"#;
        assert_eq!(checks_line(json).unwrap(), "CI ✗ 1 failed (lint) · 1 running · 1 passed");
        assert_eq!(checks_line(r#"[{"name":"a","bucket":"pass"}]"#).unwrap(), "CI ✓ 1 passed");
        assert_eq!(checks_line("[]").unwrap(), "CI: no checks");
        assert_eq!(checks_line("no checks reported"), None);
        assert_eq!(repo_of("https://github.com/mcesar/ribbon-diff/pull/3").as_deref(), Some("mcesar/ribbon-diff"));
    }

    #[test]
    fn puts_notes_on_lines_the_diff_shows() {
        let mut lines = Commentable::new();
        lines.insert("a.rs".into(), (BTreeSet::from([9, 10]), BTreeSet::from([10, 11, 12, 20])));
        let notes = [note("a.rs", false, (10, 12)), note("a.rs", false, (12, 15)), note("a.rs", false, (30, 31)), note("a.rs", true, (9, 9)), note("b.rs", false, (1, 1))];
        let (comments, elsewhere) = review(&notes.iter().collect::<Vec<_>>(), &lines);
        assert_eq!(comments.len(), 3);
        assert_eq!((comments[0]["start_line"].as_u64(), comments[0]["line"].as_u64()), (Some(10), Some(12)));
        // Only line 12 of 12-15 is in the diff.
        assert_eq!((comments[1].get("start_line"), comments[1]["line"].as_u64()), (None, Some(12)));
        assert_eq!(comments[2]["side"], "LEFT");
        assert_eq!(elsewhere, ["**a.rs:31** s", "**b.rs:1** s"]);
    }
}
