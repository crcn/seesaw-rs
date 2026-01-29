//! # HTTP Fetcher Example
//!
//! Shows how to use `reqwest` directly in Seesaw effects.
//! No adapters, no ceremony - just standard library usage.

use anyhow::Result;
use async_trait::async_trait;
use seesaw_core::{
    Command, Effect, EffectContext, Engine, EngineBuilder, Event, Machine,
};
use std::collections::HashMap;
use uuid::Uuid;

// ============================================================================
// Events (Facts)
// ============================================================================

#[derive(Debug, Clone)]
enum FetchEvent {
    /// User requested a URL to be fetched
    FetchRequested {
        fetch_id: Uuid,
        url: String,
    },

    /// Fetch succeeded
    Fetched {
        fetch_id: Uuid,
        url: String,
        content: String,
        status: u16,
    },

    /// Fetch failed
    FetchFailed {
        fetch_id: Uuid,
        url: String,
        reason: String,
    },
}

// Event is auto-implemented via blanket impl for Clone + Send + Sync + 'static

// ============================================================================
// Commands (Intent)
// ============================================================================

#[derive(Debug, Clone)]
enum FetchCommand {
    /// Fetch a URL
    Fetch {
        fetch_id: Uuid,
        url: String,
    },
}

impl Command for FetchCommand {}

// ============================================================================
// Machine (Decision Logic)
// ============================================================================

struct FetchMachine;

impl Machine for FetchMachine {
    type Event = FetchEvent;
    type Command = FetchCommand;

    fn decide(&mut self, event: &FetchEvent) -> Option<FetchCommand> {
        match event {
            FetchEvent::FetchRequested { fetch_id, url } => {
                Some(FetchCommand::Fetch {
                    fetch_id: *fetch_id,
                    url: url.clone(),
                })
            }
            _ => None,
        }
    }
}

// ============================================================================
// Effect (Execution - Uses reqwest directly)
// ============================================================================

struct FetchEffect;

#[async_trait]
impl Effect<FetchCommand, Deps> for FetchEffect {
    type Event = FetchEvent;

    async fn execute(
        &self,
        cmd: FetchCommand,
        ctx: EffectContext<Deps>
    ) -> Result<FetchEvent> {
        let FetchCommand::Fetch { fetch_id, url } = cmd;

        // Use reqwest directly - no adapter needed!
        match ctx.deps().http_client.get(&url).send().await {
            Ok(response) => {
                let status = response.status().as_u16();

                if response.status().is_success() {
                    let content = response.text().await?;

                    Ok(FetchEvent::Fetched {
                        fetch_id,
                        url,
                        content,
                        status,
                    })
                } else {
                    Ok(FetchEvent::FetchFailed {
                        fetch_id,
                        url,
                        reason: format!("HTTP {}", status),
                    })
                }
            }
            Err(e) => {
                Ok(FetchEvent::FetchFailed {
                    fetch_id,
                    url,
                    reason: e.to_string(),
                })
            }
        }
    }
}

// ============================================================================
// Dependencies
// ============================================================================

struct Deps {
    http_client: reqwest::Client,
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let deps = Deps {
        http_client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()?,
    };

    let engine = EngineBuilder::new(deps)
        .with_machine(FetchMachine)
        .with_effect::<FetchCommand, _>(FetchEffect)
        .build();

    let handle = engine.start();

    // Fetch some URLs
    let urls = vec![
        "https://example.com",
        "https://httpbin.org/status/200",
        "https://httpbin.org/status/404",
    ];

    for url in urls {
        let fetch_id = Uuid::new_v4();
        println!("Fetching: {}", url);

        handle.emit_and_await(FetchEvent::FetchRequested {
            fetch_id,
            url: url.to_string(),
        }).await?;
    }

    println!("All fetches complete!");

    Ok(())
}
