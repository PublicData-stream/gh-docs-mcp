use std::{collections::HashSet, path::Path, time::Duration};

use docs_mcp::{
    control::{self, Keys, Link, Reason, SyncRequested},
    health, health_listener,
};
use docs_mcp_core::{Result, required};
use docs_mcp_sync::observation::{self, Approval};
use tokio::time::Instant;
use uuid::Uuid;

struct Poll {
    approval: Approval,
    repository: Uuid,
    accepted: Option<String>,
    pending: Option<SyncRequested>,
    due: Instant,
}

impl Poll {
    async fn run(
        &mut self,
        sender: &mut control::Client,
        fixture: Option<&docs_mcp_sync::Fixture>,
    ) {
        let Ok(now) = control::unix_seconds() else {
            return;
        };
        if self
            .pending
            .as_ref()
            .is_some_and(|event| now.saturating_sub(event.observed_at) >= 86_400)
        {
            self.pending = None;
        }
        if self.pending.is_none() {
            let Ok(digest) = observation::poll(&self.approval, fixture).await else {
                eprintln!("feed observation failed");
                return;
            };
            if self.accepted.as_ref() == Some(&digest) {
                return;
            }
            let Ok(observed_at) = control::unix_seconds() else {
                return;
            };
            self.pending = Some(SyncRequested {
                event_id: Uuid::now_v7(),
                repository_id: self.repository,
                observation_digest: digest,
                observed_at,
                reason: Reason::FeedRefs,
            });
        }
        if let Some(event) = &self.pending {
            if sender.request_sync(event).await.is_ok() {
                self.accepted = self.pending.take().map(|event| event.observation_digest);
            } else {
                self.due = Instant::now() + control::jitter(Duration::from_secs(30));
                eprintln!("feed hint acknowledgment pending");
            }
        }
    }
}

async fn run() -> Result<()> {
    let mut approvals = match std::env::var("DOCS_MCP_FEED_ROSTER_FILE") {
        Ok(path) if !path.is_empty() => {
            control::config::<Vec<Approval>>(Path::new(&path), 1024 * 1024)?
        }
        Ok(_) | Err(std::env::VarError::NotPresent) => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    if approvals.len() > 128 {
        return Err("too many feed repositories".into());
    }
    let initial = Instant::now();
    let mut repositories = HashSet::new();
    let mut polls = Vec::new();
    for approval in approvals.drain(..) {
        let (repository, _, _) = approval.validate()?;
        if !repositories.insert(repository) {
            return Err("duplicate feed repository".into());
        }
        polls.push(Poll {
            approval,
            repository: repository.uuid(),
            accepted: None,
            pending: None,
            due: initial,
        });
    }
    let fixture = control::fixture()?;
    let mut sender = control::Client::new(
        control::address(&required("DOCS_MCP_FEED_SYNC_ADDR")?)?,
        Link::FeedSync,
        Keys::load(Link::FeedSync)?,
    );
    let interval = Duration::from_secs(900);
    loop {
        let Some(poll) = polls.iter_mut().min_by_key(|poll| poll.due) else {
            std::future::pending::<()>().await;
            break;
        };
        tokio::time::sleep_until(poll.due).await;
        poll.due = Instant::now() + control::jitter(interval);
        poll.run(&mut sender, fixture.as_ref()).await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let health = health(health_listener().await?);
    let enabled = control::enabled()?;
    tokio::select! {
        outcome = health => outcome?,
        outcome = control::shutdown() => outcome?,
        outcome = run(), if enabled => outcome?,
    }
    Ok(())
}
