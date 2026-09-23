use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::github::{encode_path_segment, GithubApi};
use crate::gitlab::{encode_project, GitlabApi};
use crate::manifest::{Manifest, SyncState};
use crate::model::{index_item, number, updated_at, IndexItem};
use crate::pagination::collect_pages;
use crate::store::Store;
use crate::{Error, Result};

const GITLAB_ISSUE_FILES: &[&str] = &[
    "issue",
    "discussions",
    "resource-state-events",
    "resource-label-events",
];
const GITLAB_MR_FILES: &[&str] = &[
    "merge-request",
    "approvals",
    "discussions",
    "commits",
    "changes",
    "pipeline",
];

fn log_sync_started(manifest: &Manifest) {
    let mode = if manifest.last_complete_at.is_some() {
        "incremental"
    } else {
        "bootstrap"
    };
    let reconciliation = if manifest.reconciliation_in_progress {
        ", reconciliation due"
    } else {
        ""
    };
    eprintln!(
        "forge-sync: {}: starting {} ({mode}{reconciliation})",
        manifest.provider, manifest.identity
    );
}

fn log_sync_completed(manifest: &Manifest, elapsed: Duration) {
    let review_kind = if manifest.provider == "github" {
        "pulls"
    } else {
        "merge requests"
    };
    let reconciliation = if manifest.reconciliation_in_progress {
        ", reconciliation pending"
    } else {
        ""
    };
    let rate_limit = manifest
        .rate_limit
        .as_ref()
        .and_then(|rate| rate.remaining.zip(rate.limit))
        .map(|(remaining, limit)| format!(", GitHub rate limit {remaining}/{limit}"))
        .unwrap_or_default();
    eprintln!(
        "forge-sync: {}: completed {} in {:.1}s: {} open issues, {} open {review_kind}, {} changed issues, {} changed {review_kind}, {} requests, {} pages{rate_limit}{reconciliation}",
        manifest.provider,
        manifest.identity,
        elapsed.as_secs_f64(),
        manifest.counts.open_issues,
        manifest.counts.open_reviews,
        manifest.counts.changed_issues,
        manifest.counts.changed_reviews,
        manifest.requests_completed,
        manifest.pages_completed,
    );
}

fn prior_manifest(store: &Store, provider: &str, identity: &str) -> Result<Option<Manifest>> {
    let prior: Option<Manifest> = store.read_json("manifest.json")?;
    if let Some(manifest) = &prior {
        if manifest.schema_version != 1
            || manifest.provider != provider
            || manifest.identity != identity
        {
            return Err(Error::Inconsistent(
                "manifest identity or schema does not match command".to_owned(),
            ));
        }
    }
    Ok(prior)
}

fn changed_since(value: &Value, cursor: Option<chrono::DateTime<Utc>>) -> bool {
    let Some(cursor) = cursor else { return true };
    updated_at(value)
        .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
        .is_none_or(|stamp| stamp.with_timezone(&Utc) >= cursor)
}

fn merge_items(target: &mut BTreeMap<u64, Value>, items: impl IntoIterator<Item = Value>) {
    for item in items {
        if let Some(number) = number(&item) {
            target.insert(number, item);
        }
    }
}

fn fetch_pages<A, F>(api: &mut A, endpoint: &str, mut get: F) -> Result<(Vec<Value>, u64)>
where
    F: FnMut(&mut A, &str) -> Result<Value>,
{
    collect_pages(endpoint, |page| get(api, page))
}

fn detail_needs_refresh(
    store: &Store,
    kind_dir: &str,
    item: &Value,
    manifest: &Manifest,
    files: &[&str],
) -> bool {
    let Some(id) = number(item) else { return false };
    (manifest.reconciliation_in_progress && manifest.last_complete_at.is_some())
        || files
            .iter()
            .any(|file| !store.exists(format!("{kind_dir}/{id}/{file}.json")))
        || (manifest.cursor.used.is_some() && changed_since(item, manifest.cursor.used))
}

fn github_collection<A: GithubApi>(
    api: &mut A,
    endpoint: &str,
    manifest: &mut Manifest,
) -> Result<Vec<Value>> {
    let (items, pages) = fetch_pages(api, endpoint, |api, page| api.get(page))?;
    manifest.pages_completed += pages;
    Ok(items)
}

fn github_checks<A: GithubApi>(
    api: &mut A,
    endpoint: &str,
    manifest: &mut Manifest,
) -> Result<Value> {
    let mut checks = Vec::new();
    let mut page = 1_u64;
    loop {
        let value = api.get(&format!("{endpoint}?per_page=100&page={page}"))?;
        manifest.pages_completed += 1;
        let page_checks = value
            .get("check_runs")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Json {
                context: "GitHub check-runs response".to_owned(),
            })?;
        let count = page_checks.len();
        checks.extend(page_checks.iter().cloned());
        if count < 100 {
            break;
        }
        page += 1;
    }
    Ok(Value::Array(checks))
}

#[derive(Debug, Serialize, Deserialize)]
struct GithubBootstrap {
    schema_version: u32,
    repository: String,
    started_at: DateTime<Utc>,
    last_delta_at: DateTime<Utc>,
    next_page: u64,
    reached_end: bool,
    items: BTreeMap<u64, Value>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GithubReconciliation {
    schema_version: u32,
    repository: String,
    started_at: DateTime<Utc>,
    next_page: u64,
    reached_end: bool,
    items: BTreeMap<u64, Value>,
    completed: BTreeSet<u64>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GithubDelta {
    schema_version: u32,
    repository: String,
    started_at: DateTime<Utc>,
    since: DateTime<Utc>,
    next_page: u64,
    reached_end: bool,
    items: BTreeMap<u64, Value>,
}

fn github_bundle_current(store: &Store, item: &Value) -> bool {
    let Some(id) = number(item) else { return false };
    let (directory, files): (&str, &[&str]) = if item.get("pull_request").is_some() {
        (
            "pulls",
            &[
                "pull",
                "issue-comments",
                "review-comments",
                "reviews",
                "commits",
                "files",
                "check-runs",
                "sync-meta",
            ],
        )
    } else {
        ("issues", &["issue", "comments", "events", "sync-meta"])
    };
    if files
        .iter()
        .any(|file| !store.exists(format!("{directory}/{id}/{file}.json")))
    {
        return false;
    }
    store
        .read_json::<Value>(format!("{directory}/{id}/sync-meta.json"))
        .ok()
        .flatten()
        .and_then(|meta| {
            meta.get("updated_at")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        == updated_at(item).map(str::to_owned)
}

fn github_refresh_item<A: GithubApi>(
    repo: &str,
    store: &Store,
    api: &mut A,
    manifest: &mut Manifest,
    item: &Value,
    force: bool,
) -> Result<()> {
    if !force && github_bundle_current(store, item) {
        return Ok(());
    }
    let id = number(item).ok_or_else(|| Error::Json {
        context: "GitHub item number".to_owned(),
    })?;
    if item.get("pull_request").is_none() {
        eprintln!("forge-sync: github: refreshing issue #{id}");
        let base = format!("issues/{id}");
        store.create_dir(&base)?;
        let detail = api.get(&format!("/repos/{repo}/issues/{id}"))?;
        store.atomic_write_json(format!("{base}/issue.json"), &detail)?;
        for (name, endpoint) in [
            ("comments", format!("/repos/{repo}/issues/{id}/comments")),
            ("events", format!("/repos/{repo}/issues/{id}/timeline")),
        ] {
            let values = github_collection(api, &endpoint, manifest)?;
            store.atomic_write_json(format!("{base}/{name}.json"), &values)?;
        }
        store.atomic_write_json(
            format!("{base}/sync-meta.json"),
            &serde_json::json!({"updated_at": updated_at(item)}),
        )?;
        return Ok(());
    }

    eprintln!("forge-sync: github: refreshing pull request #{id}");
    let base = format!("pulls/{id}");
    store.create_dir(&base)?;
    let detail = api.get(&format!("/repos/{repo}/pulls/{id}"))?;
    store.atomic_write_json(format!("{base}/pull.json"), &detail)?;
    for (name, endpoint) in [
        (
            "issue-comments",
            format!("/repos/{repo}/issues/{id}/comments"),
        ),
        (
            "review-comments",
            format!("/repos/{repo}/pulls/{id}/comments"),
        ),
        ("reviews", format!("/repos/{repo}/pulls/{id}/reviews")),
        ("commits", format!("/repos/{repo}/pulls/{id}/commits")),
        ("files", format!("/repos/{repo}/pulls/{id}/files")),
    ] {
        let values = github_collection(api, &endpoint, manifest)?;
        store.atomic_write_json(format!("{base}/{name}.json"), &values)?;
    }
    let sha = detail
        .pointer("/head/sha")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::Json {
            context: "GitHub pull request head SHA".to_owned(),
        })?;
    let checks = github_checks(
        api,
        &format!(
            "/repos/{repo}/commits/{}/check-runs",
            encode_path_segment(sha)
        ),
        manifest,
    )?;
    store.atomic_write_json(format!("{base}/check-runs.json"), &checks)?;
    store.atomic_write_json(
        format!("{base}/sync-meta.json"),
        &serde_json::json!({"updated_at": updated_at(item)}),
    )
}

fn apply_github_changes(open: &mut BTreeMap<u64, Value>, changed: &[Value]) {
    for item in changed {
        let Some(id) = number(item) else { continue };
        if item.get("state").and_then(Value::as_str) == Some("open") {
            open.insert(id, item.clone());
        } else {
            open.remove(&id);
        }
    }
}

fn newest_first(items: impl IntoIterator<Item = Value>) -> Vec<Value> {
    let mut items: Vec<_> = items.into_iter().collect();
    items.sort_by(|left, right| updated_at(right).cmp(&updated_at(left)));
    items
}

fn publish_github(
    store: &Store,
    open: &BTreeMap<u64, Value>,
    changed: &[Value],
    manifest: &mut Manifest,
) -> Result<()> {
    let (open_pulls, open_issues): (Vec<_>, Vec<_>) = open
        .values()
        .cloned()
        .partition(|item| item.get("pull_request").is_some());
    let (changed_pulls, changed_issues): (Vec<_>, Vec<_>) = changed
        .iter()
        .cloned()
        .partition(|item| item.get("pull_request").is_some());
    let mut index = Vec::new();
    let mut indexed = BTreeMap::new();
    merge_items(&mut indexed, open.values().cloned());
    merge_items(&mut indexed, changed.iter().cloned());
    for (id, item) in indexed {
        if item.get("pull_request").is_some() {
            let base = format!("pulls/{id}");
            let source = store
                .read_json::<Value>(format!("{base}/pull.json"))?
                .unwrap_or(item);
            let paths = [
                "pull",
                "issue-comments",
                "review-comments",
                "reviews",
                "commits",
                "files",
                "check-runs",
            ]
            .map(|name| format!("{base}/{name}.json"))
            .to_vec();
            if let Some(value) = index_item("github", "pull", &source, paths) {
                index.push(value);
            }
        } else {
            let base = format!("issues/{id}");
            if let Some(value) = index_item(
                "github",
                "issue",
                &item,
                vec![
                    format!("{base}/issue.json"),
                    format!("{base}/comments.json"),
                    format!("{base}/events.json"),
                ],
            ) {
                index.push(value);
            }
        }
    }
    index.sort_by_key(|item| (item.kind.clone(), item.number));
    manifest.counts.open_issues = open_issues.len();
    manifest.counts.open_reviews = open_pulls.len();
    manifest.counts.changed_issues = changed_issues.len();
    manifest.counts.changed_reviews = changed_pulls.len();
    store.atomic_write_json("open-issues.json", &open_issues)?;
    store.atomic_write_json("open-pulls.json", &open_pulls)?;
    store.atomic_write_json("changed-items.json", changed)?;
    store.atomic_write_json("index.json", &index)
}

pub fn github<A: GithubApi>(repository: &str, output: &Path, api: &mut A) -> Result<()> {
    let started = Instant::now();
    let store = Store::open(output.to_owned())?;
    let _lock = store.lock()?;
    let previous = prior_manifest(&store, "github", repository)?;
    let mut manifest = Manifest::started("github", repository, previous.as_ref(), Utc::now());
    manifest.unsupported_resources.push(
        "GitHub Discussions require authenticated GraphQL and are not synchronized".to_owned(),
    );
    log_sync_started(&manifest);
    store.atomic_write_json("manifest.json", &manifest)?;
    let result = api
        .prime_rate_limit()
        .and_then(|()| github_inner(repository, &store, api, &mut manifest, previous.as_ref()));
    manifest.requests_completed = api.requests();
    manifest.rate_limit = Some(api.rate_limit());
    match result {
        Ok(()) => {
            manifest.complete(Utc::now());
            store.atomic_write_json("manifest.json", &manifest)?;
            log_sync_completed(&manifest, started.elapsed());
            Ok(())
        }
        Err(error) => {
            if matches!(error, Error::RateLimited(_)) {
                manifest.partial(&error.to_string());
            } else {
                manifest.fail(&error.to_string());
            }
            let _ = store.atomic_write_json("manifest.json", &manifest);
            Err(error)
        }
    }
}

fn github_inner<A: GithubApi>(
    repository: &str,
    store: &Store,
    api: &mut A,
    manifest: &mut Manifest,
    previous: Option<&Manifest>,
) -> Result<()> {
    let repo = repository
        .split('/')
        .map(encode_path_segment)
        .collect::<Vec<_>>()
        .join("/");
    if previous.is_some_and(|prior| prior.last_complete_at.is_some()) {
        return github_incremental(repository, &repo, store, api, manifest);
    }
    github_bootstrap(repository, &repo, store, api, manifest)
}

fn github_incremental<A: GithubApi>(
    repository: &str,
    repo: &str,
    store: &Store,
    api: &mut A,
    manifest: &mut Manifest,
) -> Result<()> {
    let cursor = manifest.cursor.used.ok_or_else(|| {
        Error::Inconsistent("completed GitHub manifest has no incremental cursor".to_owned())
    })?;
    let (changed, delta_started) =
        github_incremental_delta(repository, repo, store, api, manifest, cursor)?;
    let changed_pulls = changed
        .iter()
        .filter(|item| item.get("pull_request").is_some())
        .count();
    eprintln!(
        "forge-sync: github: {repository}: found {} changed issues and {changed_pulls} changed pull requests",
        changed.len() - changed_pulls
    );

    let mut open = BTreeMap::new();
    let issues: Vec<Value> = store
        .read_json("open-issues.json")?
        .ok_or_else(|| Error::Inconsistent("open-issues.json is missing".to_owned()))?;
    let pulls: Vec<Value> = store
        .read_json("open-pulls.json")?
        .ok_or_else(|| Error::Inconsistent("open-pulls.json is missing".to_owned()))?;
    merge_items(&mut open, issues);
    merge_items(&mut open, pulls);
    apply_github_changes(&mut open, &changed);
    publish_github(store, &open, &changed, manifest)?;
    if manifest.reconciliation_in_progress {
        match github_reconcile(repository, repo, store, api, manifest, &changed) {
            Ok(()) | Err(Error::RateLimited(_)) => {}
            Err(error) => return Err(error),
        }
    }
    // A multi-window delta may have been moving while its pages were traversed.
    // Advancing only to its initial start makes the next run catch that movement.
    manifest.cursor.next_safe = Some(delta_started);
    Ok(())
}

fn github_incremental_delta<A: GithubApi>(
    repository: &str,
    repo: &str,
    store: &Store,
    api: &mut A,
    manifest: &mut Manifest,
    since: DateTime<Utc>,
) -> Result<(Vec<Value>, DateTime<Utc>)> {
    eprintln!(
        "forge-sync: github: {repository}: scanning changes since {}",
        since.to_rfc3339_opts(SecondsFormat::Secs, true)
    );
    let prior: Option<GithubDelta> = store.read_json("github-delta.json")?;
    let can_resume = prior.as_ref().is_some_and(|progress| {
        progress.schema_version == 1 && progress.repository == repository && progress.since == since
    });
    let mut progress = if can_resume {
        prior.expect("checked above")
    } else {
        GithubDelta {
            schema_version: 1,
            repository: repository.to_owned(),
            started_at: manifest.started_at,
            since,
            next_page: 1,
            reached_end: false,
            items: BTreeMap::new(),
        }
    };
    let endpoint = |page| {
        format!(
            "/repos/{repo}/issues?state=all&sort=updated&direction=desc&since={}&per_page=100&page={page}",
            since.to_rfc3339_opts(SecondsFormat::Secs, true)
        )
    };
    // Page numbers can shift while a delta spans rate windows. Re-reading the
    // newest page is cheap and ensures newly active items are handled first.
    if progress.next_page > 1 {
        let value = api.get(&endpoint(1))?;
        manifest.pages_completed += 1;
        let page = value.as_array().ok_or_else(|| Error::Json {
            context: "GitHub incremental newest page".to_owned(),
        })?;
        merge_items(&mut progress.items, page.iter().cloned());
        eprintln!(
            "forge-sync: github: {repository}: resumed change scan found {} items on newest page ({} total)",
            page.len(),
            progress.items.len()
        );
        store.atomic_write_json("github-delta.json", &progress)?;
        for item in newest_first(page.iter().cloned()) {
            github_refresh_item(repo, store, api, manifest, &item, false)?;
        }
    }
    for item in newest_first(progress.items.values().cloned()) {
        github_refresh_item(repo, store, api, manifest, &item, false)?;
    }
    while !progress.reached_end {
        let value = api.get(&endpoint(progress.next_page))?;
        manifest.pages_completed += 1;
        let page = value.as_array().ok_or_else(|| Error::Json {
            context: "GitHub incremental changed-item page".to_owned(),
        })?;
        merge_items(&mut progress.items, page.iter().cloned());
        progress.next_page += 1;
        progress.reached_end = page.len() < 100;
        store.atomic_write_json("github-delta.json", &progress)?;
        for item in newest_first(page.iter().cloned()) {
            github_refresh_item(repo, store, api, manifest, &item, false)?;
        }
    }
    Ok((
        newest_first(progress.items.into_values()),
        progress.started_at,
    ))
}

fn github_reconcile<A: GithubApi>(
    repository: &str,
    repo: &str,
    store: &Store,
    api: &mut A,
    manifest: &mut Manifest,
    changed: &[Value],
) -> Result<()> {
    let prior_progress: Option<GithubReconciliation> =
        store.read_json("github-reconciliation.json")?;
    let can_resume = prior_progress.as_ref().is_some_and(|progress| {
        progress.schema_version == 1
            && progress.repository == repository
            && manifest
                .last_reconciliation_at
                .is_none_or(|completed| progress.started_at > completed)
    });
    let mut progress = if can_resume {
        prior_progress.expect("checked above")
    } else {
        GithubReconciliation {
            schema_version: 1,
            repository: repository.to_owned(),
            started_at: manifest.started_at,
            next_page: 1,
            reached_end: false,
            items: BTreeMap::new(),
            completed: BTreeSet::new(),
        }
    };
    eprintln!(
        "forge-sync: github: {repository}: reconciliation has {} open items discovered, {} completed",
        progress.items.len(),
        progress.completed.len()
    );
    // Preserve items that move across page boundaries while reconciliation runs,
    // and remove state transitions observed by the successful incremental pass.
    apply_github_changes(&mut progress.items, changed);
    store.atomic_write_json("github-reconciliation.json", &progress)?;

    for item in newest_first(progress.items.values().cloned()) {
        let Some(id) = number(&item) else { continue };
        if !progress.completed.contains(&id) {
            github_refresh_item(repo, store, api, manifest, &item, true)?;
            progress.completed.insert(id);
            store.atomic_write_json("github-reconciliation.json", &progress)?;
        }
    }
    while !progress.reached_end {
        let endpoint = format!(
            "/repos/{repo}/issues?state=open&sort=updated&direction=desc&per_page=100&page={}",
            progress.next_page
        );
        let value = api.get(&endpoint)?;
        manifest.pages_completed += 1;
        let page = value.as_array().ok_or_else(|| Error::Json {
            context: "GitHub reconciliation open-item page".to_owned(),
        })?;
        merge_items(&mut progress.items, page.iter().cloned());
        eprintln!(
            "forge-sync: github: {repository}: reconciliation page {} found {} items ({} total)",
            progress.next_page,
            page.len(),
            progress.items.len()
        );
        progress.next_page += 1;
        progress.reached_end = page.len() < 100;
        store.atomic_write_json("github-reconciliation.json", &progress)?;
        for item in newest_first(page.iter().cloned()) {
            let Some(id) = number(&item) else { continue };
            github_refresh_item(repo, store, api, manifest, &item, true)?;
            progress.completed.insert(id);
            store.atomic_write_json("github-reconciliation.json", &progress)?;
        }
    }
    publish_github(store, &progress.items, changed, manifest)?;
    manifest.last_reconciliation_at = Some(Utc::now());
    manifest.reconciliation_in_progress = false;
    Ok(())
}

fn github_bootstrap<A: GithubApi>(
    repository: &str,
    repo: &str,
    store: &Store,
    api: &mut A,
    manifest: &mut Manifest,
) -> Result<()> {
    let run_started = manifest.started_at;
    let mut progress: GithubBootstrap = match store.read_json("github-bootstrap.json")? {
        Some(progress) => progress,
        None => GithubBootstrap {
            schema_version: 1,
            repository: repository.to_owned(),
            started_at: run_started,
            last_delta_at: run_started,
            next_page: 1,
            reached_end: false,
            items: BTreeMap::new(),
        },
    };
    if progress.schema_version != 1 || progress.repository != repository {
        return Err(Error::Inconsistent(
            "GitHub bootstrap state does not match command".to_owned(),
        ));
    }
    eprintln!(
        "forge-sync: github: {repository}: bootstrap at page {}, {} open items discovered",
        progress.next_page,
        progress.items.len()
    );

    // On resumed runs, catch up recent activity before spending requests on old pages.
    if progress.next_page > 1 || progress.reached_end {
        let since = progress.last_delta_at - chrono::Duration::minutes(5);
        let changed = github_collection(
            api,
            &format!(
                "/repos/{repo}/issues?state=all&sort=updated&direction=desc&since={}",
                since.to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            manifest,
        )?;
        apply_github_changes(&mut progress.items, &changed);
        store.atomic_write_json("github-bootstrap.json", &progress)?;
        for item in newest_first(changed) {
            github_refresh_item(repo, store, api, manifest, &item, false)?;
        }
        progress.last_delta_at = run_started;
        store.atomic_write_json("github-bootstrap.json", &progress)?;
    }

    // Finish any bundles belonging to already enumerated pages, newest first.
    for item in newest_first(progress.items.values().cloned()) {
        github_refresh_item(repo, store, api, manifest, &item, false)?;
    }

    while !progress.reached_end {
        let endpoint = format!(
            "/repos/{repo}/issues?state=open&sort=updated&direction=desc&per_page=100&page={}",
            progress.next_page
        );
        let value = api.get(&endpoint)?;
        manifest.pages_completed += 1;
        let page = value.as_array().ok_or_else(|| Error::Json {
            context: "GitHub bootstrap open-item page".to_owned(),
        })?;
        merge_items(&mut progress.items, page.iter().cloned());
        eprintln!(
            "forge-sync: github: {repository}: bootstrap page {} found {} items ({} total)",
            progress.next_page,
            page.len(),
            progress.items.len()
        );
        progress.next_page += 1;
        progress.reached_end = page.len() < 100;
        store.atomic_write_json("github-bootstrap.json", &progress)?;
        for item in newest_first(page.iter().cloned()) {
            github_refresh_item(repo, store, api, manifest, &item, false)?;
        }
    }

    // Close the race between the beginning of the traversal and its final page.
    let since = progress.last_delta_at - chrono::Duration::minutes(5);
    let changed = github_collection(
        api,
        &format!(
            "/repos/{repo}/issues?state=all&sort=updated&direction=desc&since={}",
            since.to_rfc3339_opts(SecondsFormat::Secs, true)
        ),
        manifest,
    )?;
    apply_github_changes(&mut progress.items, &changed);
    store.atomic_write_json("github-bootstrap.json", &progress)?;
    for item in newest_first(changed.iter().cloned()) {
        github_refresh_item(repo, store, api, manifest, &item, false)?;
    }
    progress.last_delta_at = run_started;
    store.atomic_write_json("github-bootstrap.json", &progress)?;
    publish_github(store, &progress.items, &changed, manifest)?;
    manifest.last_reconciliation_at = Some(Utc::now());
    manifest.reconciliation_in_progress = false;
    manifest.cursor.next_safe = Some(progress.started_at);
    Ok(())
}

fn gitlab_collection<A: GitlabApi>(
    api: &mut A,
    endpoint: &str,
    manifest: &mut Manifest,
) -> Result<Vec<Value>> {
    let (items, pages) = fetch_pages(api, endpoint, |api, page| api.get(page))?;
    manifest.pages_completed += pages;
    Ok(items)
}

fn gitlab_pipeline_result(result: Result<Value>, pipeline_id: u64) -> Result<Value> {
    match result {
        Ok(value) => Ok(value),
        Err(Error::RemoteNotFound) => Ok(serde_json::json!({
            "id": pipeline_id,
            "unavailable": "not_found"
        })),
        Err(error) => Err(error),
    }
}

pub fn gitlab<A: GitlabApi>(host: &str, project: &str, output: &Path, api: &mut A) -> Result<()> {
    let started = Instant::now();
    let identity = format!("{host}/{project}");
    let store = Store::open(output.to_owned())?;
    let _lock = store.lock()?;
    let previous = prior_manifest(&store, "gitlab", &identity)?;
    let mut manifest = Manifest::started("gitlab", &identity, previous.as_ref(), Utc::now());
    log_sync_started(&manifest);
    store.atomic_write_json("manifest.json", &manifest)?;
    let result = gitlab_inner(project, &store, api, &mut manifest);
    manifest.requests_completed = api.requests();
    match result {
        Ok(()) => {
            if manifest.reconciliation_in_progress {
                manifest.last_reconciliation_at = Some(Utc::now());
                manifest.reconciliation_in_progress = false;
            }
            manifest.complete(Utc::now());
            store.atomic_write_json("manifest.json", &manifest)?;
            log_sync_completed(&manifest, started.elapsed());
            Ok(())
        }
        Err(error) => {
            manifest.fail(&error.to_string());
            let _ = store.atomic_write_json("manifest.json", &manifest);
            Err(error)
        }
    }
}

fn gitlab_inner<A: GitlabApi>(
    project: &str,
    store: &Store,
    api: &mut A,
    manifest: &mut Manifest,
) -> Result<()> {
    let project_path = encode_project(project);
    let prefix = format!("projects/{project_path}");
    let metadata = api.get(&prefix)?;
    let project_id = metadata
        .get("id")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::Json {
            context: "GitLab project metadata".to_owned(),
        })?;
    store.atomic_write_json("project.json", &metadata)?;
    let open_issues = gitlab_collection(
        api,
        &format!("{prefix}/issues?state=opened&order_by=updated_at&sort=desc"),
        manifest,
    )?;
    let open_mrs = gitlab_collection(
        api,
        &format!("{prefix}/merge_requests?state=opened&order_by=updated_at&sort=desc"),
        manifest,
    )?;
    let cursor_query = manifest
        .cursor
        .used
        .map(|cursor| {
            format!(
                "&updated_after={}",
                cursor.to_rfc3339_opts(SecondsFormat::Secs, true)
            )
        })
        .unwrap_or_default();
    let changed_issues = gitlab_collection(
        api,
        &format!("{prefix}/issues?scope=all&order_by=updated_at&sort=asc{cursor_query}"),
        manifest,
    )?;
    let changed_mrs = gitlab_collection(
        api,
        &format!("{prefix}/merge_requests?scope=all&order_by=updated_at&sort=asc{cursor_query}"),
        manifest,
    )?;
    let todos = gitlab_collection(api, "todos?state=pending", manifest)?;
    let todos: Vec<_> = todos
        .into_iter()
        .filter(|todo| todo.pointer("/project/id").and_then(Value::as_u64) == Some(project_id))
        .collect();
    eprintln!(
        "forge-sync: gitlab: {project}: scan found {} open issues, {} open merge requests, {} changed issues, {} changed merge requests, {} pending todos",
        open_issues.len(),
        open_mrs.len(),
        changed_issues.len(),
        changed_mrs.len(),
        todos.len()
    );

    let mut issues = BTreeMap::new();
    merge_items(&mut issues, open_issues.iter().cloned());
    merge_items(&mut issues, changed_issues.iter().cloned());
    let mut mrs = BTreeMap::new();
    merge_items(&mut mrs, open_mrs.iter().cloned());
    merge_items(&mut mrs, changed_mrs.iter().cloned());
    let issue_refresh_total = issues
        .values()
        .filter(|item| detail_needs_refresh(store, "issues", item, manifest, GITLAB_ISSUE_FILES))
        .count();
    let mr_refresh_total = mrs
        .values()
        .filter(|item| {
            detail_needs_refresh(store, "merge-requests", item, manifest, GITLAB_MR_FILES)
        })
        .count();
    eprintln!(
        "forge-sync: gitlab: {project}: refreshing {issue_refresh_total}/{} issue bundles and {mr_refresh_total}/{} merge-request bundles",
        issues.len(),
        mrs.len()
    );
    let mut issues_refreshed = 0_usize;
    let mut mrs_refreshed = 0_usize;
    let mut index: Vec<IndexItem> = Vec::new();
    for (id, item) in &issues {
        let base = format!("issues/{id}");
        if detail_needs_refresh(store, "issues", item, manifest, GITLAB_ISSUE_FILES) {
            store.create_dir(&base)?;
            let detail = api.get(&format!("{prefix}/issues/{id}"))?;
            store.atomic_write_json(format!("{base}/issue.json"), &detail)?;
            for (name, endpoint) in [
                ("discussions", format!("{prefix}/issues/{id}/discussions")),
                (
                    "resource-state-events",
                    format!("{prefix}/issues/{id}/resource_state_events"),
                ),
                (
                    "resource-label-events",
                    format!("{prefix}/issues/{id}/resource_label_events"),
                ),
            ] {
                let values = gitlab_collection(api, &endpoint, manifest)?;
                store.atomic_write_json(format!("{base}/{name}.json"), &values)?;
            }
            issues_refreshed += 1;
            if issues_refreshed.is_multiple_of(25) || issues_refreshed == issue_refresh_total {
                eprintln!(
                    "forge-sync: gitlab: {project}: refreshed {issues_refreshed}/{issue_refresh_total} issue bundles (latest IID {id})"
                );
            }
        }
        let paths = GITLAB_ISSUE_FILES
            .iter()
            .map(|name| format!("{base}/{name}.json"))
            .collect();
        if let Some(item) = index_item("gitlab", "issue", item, paths) {
            index.push(item);
        }
    }
    for (id, item) in &mrs {
        let base = format!("merge-requests/{id}");
        let mut source = item.clone();
        if detail_needs_refresh(store, "merge-requests", item, manifest, GITLAB_MR_FILES) {
            store.create_dir(&base)?;
            let detail = api.get(&format!("{prefix}/merge_requests/{id}"))?;
            source = detail.clone();
            store.atomic_write_json(format!("{base}/merge-request.json"), &detail)?;
            let approvals = api.get(&format!("{prefix}/merge_requests/{id}/approvals"))?;
            store.atomic_write_json(format!("{base}/approvals.json"), &approvals)?;
            for (name, endpoint) in [
                (
                    "discussions",
                    format!("{prefix}/merge_requests/{id}/discussions"),
                ),
                ("commits", format!("{prefix}/merge_requests/{id}/commits")),
            ] {
                let values = gitlab_collection(api, &endpoint, manifest)?;
                store.atomic_write_json(format!("{base}/{name}.json"), &values)?;
            }
            let changes = api.get(&format!("{prefix}/merge_requests/{id}/changes"))?;
            store.atomic_write_json(format!("{base}/changes.json"), &changes)?;
            let pipeline = if let Some(pipeline_id) =
                detail.pointer("/head_pipeline/id").and_then(Value::as_u64)
            {
                gitlab_pipeline_result(
                    api.get(&format!("{prefix}/pipelines/{pipeline_id}")),
                    pipeline_id,
                )?
            } else {
                Value::Null
            };
            store.atomic_write_json(format!("{base}/pipeline.json"), &pipeline)?;
            mrs_refreshed += 1;
            if mrs_refreshed.is_multiple_of(10) || mrs_refreshed == mr_refresh_total {
                eprintln!(
                    "forge-sync: gitlab: {project}: refreshed {mrs_refreshed}/{mr_refresh_total} merge-request bundles (latest IID {id})"
                );
            }
        } else if let Some(detail) =
            store.read_json::<Value>(format!("{base}/merge-request.json"))?
        {
            source = detail;
        }
        let paths = GITLAB_MR_FILES
            .iter()
            .map(|name| format!("{base}/{name}.json"))
            .collect();
        if let Some(item) = index_item("gitlab", "merge-request", &source, paths) {
            index.push(item);
        }
    }
    index.sort_by_key(|item| (item.kind.clone(), item.number));
    manifest.counts.open_issues = open_issues.len();
    manifest.counts.open_reviews = open_mrs.len();
    manifest.counts.changed_issues = changed_issues.len();
    manifest.counts.changed_reviews = changed_mrs.len();
    store.atomic_write_json("open-issues.json", &open_issues)?;
    store.atomic_write_json("open-merge-requests.json", &open_mrs)?;
    store.atomic_write_json("todos.json", &todos)?;
    store.atomic_write_json(
        "changed-items.json",
        &[changed_issues, changed_mrs].concat(),
    )?;
    store.atomic_write_json("index.json", &index)?;
    manifest.cursor.next_safe = Some(manifest.started_at);
    Ok(())
}

pub fn status(output: &Path) -> Result<String> {
    let store = Store::open_existing(output.to_owned())?;
    let manifest: Manifest = store
        .read_json("manifest.json")?
        .ok_or_else(|| Error::Inconsistent("manifest.json is missing".to_owned()))?;
    let required = match manifest.provider.as_str() {
        "github" => ["open-issues.json", "open-pulls.json", "index.json"].as_slice(),
        "gitlab" => [
            "open-issues.json",
            "open-merge-requests.json",
            "todos.json",
            "index.json",
        ]
        .as_slice(),
        _ => {
            return Err(Error::Inconsistent(
                "unknown provider in manifest".to_owned(),
            ))
        }
    };
    let missing: Vec<_> = required.iter().filter(|path| !store.exists(path)).collect();
    if !missing.is_empty() {
        return Err(Error::Inconsistent(format!(
            "complete snapshot files are missing: {missing:?}"
        )));
    }
    if manifest.state != SyncState::Complete || manifest.finished_at.is_none() {
        return Err(Error::Inconsistent(format!(
            "latest synchronization state is {:?}",
            manifest.state
        )));
    }
    let completed = manifest
        .last_complete_at
        .ok_or_else(|| Error::Inconsistent("last complete timestamp is missing".to_owned()))?;
    Ok(format!(
        "{} {}: complete at {}; {} open issues, {} open reviews; cursor {}; reconciliation {}",
        manifest.provider,
        manifest.identity,
        completed.to_rfc3339_opts(SecondsFormat::Secs, true),
        manifest.counts.open_issues,
        manifest.counts.open_reviews,
        manifest
            .cursor
            .next_safe
            .map(|time| time.to_rfc3339_opts(SecondsFormat::Secs, true))
            .unwrap_or_else(|| "none".to_owned()),
        if manifest.reconciliation_in_progress {
            "in progress"
        } else {
            "complete"
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::RateLimit;
    use std::collections::VecDeque;

    fn private_tempdir() -> tempfile::TempDir {
        let temp = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        temp
    }

    struct FakeGithub {
        responses: VecDeque<Value>,
        requests: u64,
        endpoints: Vec<String>,
    }
    impl GithubApi for FakeGithub {
        fn get(&mut self, endpoint: &str) -> Result<Value> {
            self.requests += 1;
            self.endpoints.push(endpoint.to_owned());
            self.responses
                .pop_front()
                .ok_or_else(|| Error::Remote("unexpected request".to_owned()))
        }
        fn rate_limit(&self) -> RateLimit {
            RateLimit {
                limit: Some(60),
                remaining: Some(40),
                reset_epoch: None,
            }
        }
        fn requests(&self) -> u64 {
            self.requests
        }
    }

    #[test]
    fn github_splits_issues_and_pulls_and_status_is_local() {
        let issue = serde_json::json!({"id": 1, "number": 1, "title":"i", "state":"open", "updated_at":"2025-01-01T00:00:00Z"});
        let pull = serde_json::json!({"id": 2, "number": 2, "title":"p", "state":"open", "updated_at":"2025-01-01T00:00:00Z", "pull_request":{}});
        let pull_detail =
            serde_json::json!({"number":2,"title":"p","state":"open","head":{"sha":"abc"}});
        let mut responses = VecDeque::from(vec![
            Value::Array(vec![issue.clone(), pull.clone()]),
            issue.clone(),
            Value::Array(vec![]),
            Value::Array(vec![]),
            pull_detail,
            Value::Array(vec![]),
            Value::Array(vec![]),
            Value::Array(vec![]),
            Value::Array(vec![]),
            Value::Array(vec![]),
            serde_json::json!({"check_runs":[]}),
            Value::Array(vec![]),
        ]);
        let temp = private_tempdir();
        let mut api = FakeGithub {
            responses: std::mem::take(&mut responses),
            requests: 0,
            endpoints: Vec::new(),
        };
        github("owner/repo", temp.path(), &mut api).unwrap();
        let store = Store::open_existing(temp.path().to_owned()).unwrap();
        assert_eq!(
            store
                .read_json::<Vec<Value>>("open-issues.json")
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .read_json::<Vec<Value>>("open-pulls.json")
                .unwrap()
                .unwrap()
                .len(),
            1
        );
        assert!(status(temp.path()).unwrap().contains("complete"));
    }

    #[test]
    fn failed_run_does_not_advance_cursor() {
        let temp = private_tempdir();
        let mut api = FakeGithub {
            responses: VecDeque::new(),
            requests: 0,
            endpoints: Vec::new(),
        };
        assert!(github("owner/repo", temp.path(), &mut api).is_err());
        let store = Store::open_existing(temp.path().to_owned()).unwrap();
        let manifest: Manifest = store.read_json("manifest.json").unwrap().unwrap();
        assert_eq!(manifest.state, SyncState::Failed);
        assert!(manifest.cursor.next_safe.is_none());
        assert!(status(temp.path()).is_err());
    }

    #[test]
    fn malformed_update_keeps_previous_complete_snapshot() {
        let issue = serde_json::json!({"id": 1, "number": 1, "title":"old", "state":"open", "updated_at":"2025-01-01T00:00:00Z"});
        let mut first = FakeGithub {
            responses: VecDeque::from(vec![
                Value::Array(vec![issue.clone()]),
                issue.clone(),
                Value::Array(vec![]),
                Value::Array(vec![]),
                Value::Array(vec![]),
            ]),
            requests: 0,
            endpoints: Vec::new(),
        };
        let temp = private_tempdir();
        github("owner/repo", temp.path(), &mut first).unwrap();
        let store = Store::open_existing(temp.path().to_owned()).unwrap();
        let old_snapshot: Vec<Value> = store.read_json("open-issues.json").unwrap().unwrap();

        let newer = serde_json::json!({"id": 1, "number": 1, "title":"new", "state":"open", "updated_at":"2099-01-01T00:00:00Z"});
        let mut second = FakeGithub {
            responses: VecDeque::from(vec![
                Value::Array(vec![newer]),
                serde_json::json!({"malformed":true}),
            ]),
            requests: 0,
            endpoints: Vec::new(),
        };
        assert!(github("owner/repo", temp.path(), &mut second).is_err());
        let snapshot: Vec<Value> = store.read_json("open-issues.json").unwrap().unwrap();
        assert_eq!(snapshot, old_snapshot);
        let manifest: Manifest = store.read_json("manifest.json").unwrap().unwrap();
        assert_eq!(manifest.state, SyncState::Failed);
        assert!(manifest.last_complete_at.is_some());
    }

    #[test]
    fn reconciliation_forces_complete_bundle_refresh() {
        let temp = private_tempdir();
        let store = Store::open(temp.path().join("cache")).unwrap();
        for file in ["issue", "comments", "events"] {
            store
                .atomic_write_json(format!("issues/7/{file}.json"), &Value::Null)
                .unwrap();
        }
        let now = Utc::now();
        let mut manifest = Manifest::started("github", "o/r", None, now);
        manifest.reconciliation_in_progress = true;
        let item = serde_json::json!({"number":7,"updated_at":"2020-01-01T00:00:00Z"});
        assert!(!detail_needs_refresh(
            &store,
            "issues",
            &item,
            &manifest,
            &["issue", "comments", "events"]
        ));
        manifest.last_complete_at = Some(now - chrono::Duration::days(8));
        assert!(detail_needs_refresh(
            &store,
            "issues",
            &item,
            &manifest,
            &["issue", "comments", "events"]
        ));
    }

    #[test]
    fn completed_cache_syncs_changes_first_without_open_enumeration() {
        let issue = serde_json::json!({"id": 1, "number": 1, "title":"i", "state":"open", "updated_at":"2025-01-01T00:00:00Z"});
        let temp = private_tempdir();
        let mut initial = FakeGithub {
            responses: VecDeque::from(vec![
                Value::Array(vec![issue.clone()]),
                issue,
                Value::Array(vec![]),
                Value::Array(vec![]),
                Value::Array(vec![]),
            ]),
            requests: 0,
            endpoints: Vec::new(),
        };
        github("owner/repo", temp.path(), &mut initial).unwrap();

        let mut incremental = FakeGithub {
            responses: VecDeque::from(vec![Value::Array(vec![])]),
            requests: 0,
            endpoints: Vec::new(),
        };
        github("owner/repo", temp.path(), &mut incremental).unwrap();
        assert_eq!(incremental.endpoints.len(), 1);
        assert!(incremental.endpoints[0].contains("state=all"));
        assert!(incremental.endpoints[0].contains("since="));
        assert!(!incremental.endpoints[0].contains("state=open"));
    }

    struct RateScript {
        first_page: Option<Value>,
        endpoints: Vec<String>,
        requests: u64,
    }

    impl GithubApi for RateScript {
        fn get(&mut self, endpoint: &str) -> Result<Value> {
            self.requests += 1;
            self.endpoints.push(endpoint.to_owned());
            self.first_page.take().ok_or(Error::RateLimited(2))
        }

        fn rate_limit(&self) -> RateLimit {
            RateLimit {
                limit: Some(60),
                remaining: Some(2),
                reset_epoch: None,
            }
        }

        fn requests(&self) -> u64 {
            self.requests
        }
    }

    #[test]
    fn rate_stop_persists_bootstrap_page_and_resume_checks_delta_first() {
        let items: Vec<_> = (1..=100)
            .map(|id| {
                serde_json::json!({
                    "id": id, "number": id, "title": format!("issue {id}"),
                    "state": "open", "updated_at": "2025-01-01T00:00:00Z"
                })
            })
            .collect();
        let temp = private_tempdir();
        let mut first = RateScript {
            first_page: Some(Value::Array(items)),
            endpoints: Vec::new(),
            requests: 0,
        };
        assert!(matches!(
            github("owner/repo", temp.path(), &mut first),
            Err(Error::RateLimited(2))
        ));
        let store = Store::open_existing(temp.path().to_owned()).unwrap();
        let progress: GithubBootstrap = store.read_json("github-bootstrap.json").unwrap().unwrap();
        assert_eq!(progress.next_page, 2);
        assert_eq!(progress.items.len(), 100);
        let manifest: Manifest = store.read_json("manifest.json").unwrap().unwrap();
        assert_eq!(manifest.state, SyncState::Partial);

        let mut resumed = RateScript {
            first_page: Some(Value::Array(vec![])),
            endpoints: Vec::new(),
            requests: 0,
        };
        assert!(matches!(
            github("owner/repo", temp.path(), &mut resumed),
            Err(Error::RateLimited(2))
        ));
        assert!(resumed.endpoints[0].contains("state=all"));
        assert!(resumed.endpoints[0].contains("since="));
        assert!(resumed.endpoints[1].contains("/issues/"));
        assert!(!resumed
            .endpoints
            .iter()
            .any(|endpoint| endpoint.contains("page=1") && endpoint.contains("state=open")));
    }

    #[test]
    fn rate_limited_reconciliation_does_not_invalidate_fresh_delta() {
        let issue = serde_json::json!({
            "id": 1, "number": 1, "title": "i", "state": "open",
            "updated_at": "2025-01-01T00:00:00Z"
        });
        let temp = private_tempdir();
        let mut initial = FakeGithub {
            responses: VecDeque::from(vec![
                Value::Array(vec![issue.clone()]),
                issue,
                Value::Array(vec![]),
                Value::Array(vec![]),
                Value::Array(vec![]),
            ]),
            requests: 0,
            endpoints: Vec::new(),
        };
        github("owner/repo", temp.path(), &mut initial).unwrap();
        let store = Store::open_existing(temp.path().to_owned()).unwrap();
        let mut old: Manifest = store.read_json("manifest.json").unwrap().unwrap();
        old.last_reconciliation_at = Some(Utc::now() - chrono::Duration::days(8));
        store.atomic_write_json("manifest.json", &old).unwrap();

        let mut limited = RateScript {
            first_page: Some(Value::Array(vec![])),
            endpoints: Vec::new(),
            requests: 0,
        };
        github("owner/repo", temp.path(), &mut limited).unwrap();
        let current: Manifest = store.read_json("manifest.json").unwrap().unwrap();
        assert_eq!(current.state, SyncState::Complete);
        assert!(current.reconciliation_in_progress);
        assert!(status(temp.path())
            .unwrap()
            .contains("reconciliation in progress"));
    }

    #[test]
    fn missing_gitlab_pipeline_becomes_an_explicit_marker() {
        let marker = gitlab_pipeline_result(Err(Error::RemoteNotFound), 491_567).unwrap();
        assert_eq!(
            marker,
            serde_json::json!({"id": 491567, "unavailable": "not_found"})
        );
        assert!(matches!(
            gitlab_pipeline_result(Err(Error::Remote("failure".to_owned())), 1),
            Err(Error::Remote(_))
        ));
    }
}
