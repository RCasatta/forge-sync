use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SyncState {
    Complete,
    Partial,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Cursor {
    pub used: Option<DateTime<Utc>>,
    pub next_safe: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RateLimit {
    pub limit: Option<u32>,
    pub remaining: Option<u32>,
    pub reset_epoch: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Counts {
    pub open_issues: usize,
    pub open_reviews: usize,
    pub changed_issues: usize,
    pub changed_reviews: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub schema_version: u32,
    pub program_version: String,
    pub provider: String,
    pub identity: String,
    pub state: SyncState,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub last_complete_at: Option<DateTime<Utc>>,
    pub cursor: Cursor,
    pub reconciliation_in_progress: bool,
    pub last_reconciliation_at: Option<DateTime<Utc>>,
    pub pages_completed: u64,
    pub requests_completed: u64,
    pub rate_limit: Option<RateLimit>,
    pub counts: Counts,
    pub unsupported_resources: Vec<String>,
    pub failure: Option<String>,
}

impl Manifest {
    pub fn started(
        provider: &str,
        identity: &str,
        previous: Option<&Self>,
        now: DateTime<Utc>,
    ) -> Self {
        let last_complete_at = previous.and_then(|m| {
            m.last_complete_at
                .or_else(|| (m.state == SyncState::Complete).then_some(m.started_at))
        });
        let successful_cursor = previous
            .and_then(|m| m.cursor.next_safe)
            .or(last_complete_at);
        let reconcile_due = previous
            .and_then(|m| m.last_reconciliation_at)
            .is_none_or(|time| now.signed_duration_since(time).num_days() >= 7);
        Self {
            schema_version: 1,
            program_version: env!("CARGO_PKG_VERSION").to_owned(),
            provider: provider.to_owned(),
            identity: identity.to_owned(),
            state: SyncState::Partial,
            started_at: now,
            finished_at: None,
            last_complete_at,
            cursor: Cursor {
                used: successful_cursor.map(|time| time - chrono::Duration::minutes(5)),
                next_safe: successful_cursor,
            },
            reconciliation_in_progress: reconcile_due,
            last_reconciliation_at: previous.and_then(|m| m.last_reconciliation_at),
            pages_completed: 0,
            requests_completed: 0,
            rate_limit: None,
            counts: Counts::default(),
            unsupported_resources: Vec::new(),
            failure: None,
        }
    }

    pub fn complete(&mut self, now: DateTime<Utc>) {
        self.state = SyncState::Complete;
        self.finished_at = Some(now);
        self.last_complete_at = Some(now);
        self.failure = None;
    }

    pub fn fail(&mut self, message: &str) {
        self.state = SyncState::Failed;
        self.finished_at = Some(Utc::now());
        self.failure = Some(sanitize_failure(message));
    }

    pub fn partial(&mut self, message: &str) {
        self.state = SyncState::Partial;
        self.finished_at = Some(Utc::now());
        self.failure = Some(sanitize_failure(message));
    }
}

fn sanitize_failure(message: &str) -> String {
    let one_line = message.lines().next().unwrap_or("synchronization failed");
    one_line.chars().take(300).collect()
}
