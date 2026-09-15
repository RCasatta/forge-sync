use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct IndexItem {
    pub provider: String,
    pub kind: String,
    pub number: u64,
    pub state: String,
    pub title: String,
    pub author: Option<String>,
    pub draft: Option<bool>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub url: Option<String>,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
    pub reviewers: Vec<String>,
    pub head_sha: Option<String>,
    pub raw_paths: Vec<String>,
}

fn login(value: &Value) -> Option<String> {
    value
        .get("login")
        .or_else(|| value.get("username"))?
        .as_str()
        .map(str::to_owned)
}

fn people(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(login)
        .collect()
}

fn labels(value: &Value) -> Vec<String> {
    value
        .get("labels")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|label| {
            label
                .as_str()
                .map(str::to_owned)
                .or_else(|| label.get("name")?.as_str().map(str::to_owned))
        })
        .collect()
}

pub fn index_item(
    provider: &str,
    kind: &str,
    value: &Value,
    raw_paths: Vec<String>,
) -> Option<IndexItem> {
    let number = value.get("number").or_else(|| value.get("iid"))?.as_u64()?;
    Some(IndexItem {
        provider: provider.to_owned(),
        kind: kind.to_owned(),
        number,
        state: value
            .get("state")
            .and_then(Value::as_str)
            .unwrap_or("unknown")
            .to_owned(),
        title: value
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        author: value
            .get("user")
            .or_else(|| value.get("author"))
            .and_then(login),
        draft: value
            .get("draft")
            .or_else(|| value.get("work_in_progress"))
            .and_then(Value::as_bool),
        created_at: value
            .get("created_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        updated_at: value
            .get("updated_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        url: value
            .get("html_url")
            .or_else(|| value.get("web_url"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        labels: labels(value),
        assignees: people(value.get("assignees")),
        reviewers: people(
            value
                .get("reviewers")
                .or_else(|| value.get("requested_reviewers")),
        ),
        head_sha: value
            .pointer("/head/sha")
            .or_else(|| value.get("sha"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        raw_paths,
    })
}

pub fn number(value: &Value) -> Option<u64> {
    value.get("number").or_else(|| value.get("iid"))?.as_u64()
}

pub fn updated_at(value: &Value) -> Option<&str> {
    value.get("updated_at")?.as_str()
}
