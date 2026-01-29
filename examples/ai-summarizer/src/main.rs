//! # AI Summarizer Example
//!
//! Shows how to call the Anthropic API directly in Seesaw effects.
//! No special adapter - just reqwest + serde.

use anyhow::{bail, Result};
use async_trait::async_trait;
use seesaw_core::{
    Command, Effect, EffectContext, Engine, EngineBuilder, Event, Machine,
};
use serde::{Deserialize, Serialize};
use std::env;
use uuid::Uuid;

// ============================================================================
// Events (Facts)
// ============================================================================

#[derive(Debug, Clone)]
enum SummaryEvent {
    /// User requested text to be summarized
    SummarizeRequested {
        task_id: Uuid,
        text: String,
    },

    /// Summary generated
    Summarized {
        task_id: Uuid,
        summary: String,
        tokens_used: u32,
    },

    /// Summary failed
    SummaryFailed {
        task_id: Uuid,
        reason: String,
    },
}

// Event is auto-implemented via blanket impl for Clone + Send + Sync + 'static

// ============================================================================
// Commands (Intent)
// ============================================================================

#[derive(Debug, Clone)]
enum SummaryCommand {
    Summarize {
        task_id: Uuid,
        text: String,
    },
}

impl Command for SummaryCommand {}

// ============================================================================
// Machine (Decision Logic)
// ============================================================================

struct SummaryMachine;

impl Machine for SummaryMachine {
    type Event = SummaryEvent;
    type Command = SummaryCommand;

    fn decide(&mut self, event: &SummaryEvent) -> Option<SummaryCommand> {
        match event {
            SummaryEvent::SummarizeRequested { task_id, text } => {
                Some(SummaryCommand::Summarize {
                    task_id: *task_id,
                    text: text.clone(),
                })
            }
            _ => None,
        }
    }
}

// ============================================================================
// Effect (Execution - Calls Anthropic API directly)
// ============================================================================

struct SummarizeEffect;

#[async_trait]
impl Effect<SummaryCommand, Deps> for SummarizeEffect {
    type Event = SummaryEvent;

    async fn execute(
        &self,
        cmd: SummaryCommand,
        ctx: EffectContext<Deps>
    ) -> Result<SummaryEvent> {
        let SummaryCommand::Summarize { task_id, text } = cmd;

        // Call Anthropic API directly using reqwest
        let request = AnthropicRequest {
            model: "claude-3-5-sonnet-20241022".to_string(),
            max_tokens: 1024,
            messages: vec![
                Message {
                    role: "user".to_string(),
                    content: format!("Summarize this text in 2-3 sentences:\n\n{}", text),
                }
            ],
        };

        match call_anthropic(&ctx.deps().http_client, &ctx.deps().api_key, request).await {
            Ok(response) => {
                let summary = response.content
                    .first()
                    .and_then(|c| c.text.clone())
                    .unwrap_or_default();

                Ok(SummaryEvent::Summarized {
                    task_id,
                    summary,
                    tokens_used: response.usage.input_tokens + response.usage.output_tokens,
                })
            }
            Err(e) => {
                Ok(SummaryEvent::SummaryFailed {
                    task_id,
                    reason: e.to_string(),
                })
            }
        }
    }
}

// ============================================================================
// Anthropic API Types (Just plain structs)
// ============================================================================

#[derive(Debug, Serialize)]
struct AnthropicRequest {
    model: String,
    max_tokens: u32,
    messages: Vec<Message>,
}

#[derive(Debug, Serialize, Deserialize)]
struct Message {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<ContentBlock>,
    usage: Usage,
}

#[derive(Debug, Deserialize)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    input_tokens: u32,
    output_tokens: u32,
}

/// Call Anthropic API - just a plain function
async fn call_anthropic(
    client: &reqwest::Client,
    api_key: &str,
    request: AnthropicRequest,
) -> Result<AnthropicResponse> {
    let response = client
        .post("https://api.anthropic.com/v1/messages")
        .header("x-api-key", api_key)
        .header("anthropic-version", "2023-06-01")
        .header("content-type", "application/json")
        .json(&request)
        .send()
        .await?;

    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await?;
        bail!("API error {}: {}", status, body);
    }

    Ok(response.json().await?)
}

// ============================================================================
// Dependencies
// ============================================================================

struct Deps {
    http_client: reqwest::Client,
    api_key: String,
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<()> {
    let api_key = env::var("ANTHROPIC_API_KEY")
        .expect("ANTHROPIC_API_KEY environment variable required");

    let deps = Deps {
        http_client: reqwest::Client::new(),
        api_key,
    };

    let engine = EngineBuilder::new(deps)
        .with_machine(SummaryMachine)
        .with_effect::<SummaryCommand, _>(SummarizeEffect)
        .build();

    let handle = engine.start();

    // Summarize some text
    let text = r#"
        Rust is a multi-paradigm, general-purpose programming language that emphasizes
        performance, type safety, and concurrency. It enforces memory safety—meaning that
        all references point to valid memory—without a garbage collector. To simultaneously
        enforce memory safety and prevent data races, its "borrow checker" tracks the object
        lifetime of all references in a program during compilation.
    "#;

    let task_id = Uuid::new_v4();
    println!("Summarizing text...");

    handle.emit_and_await(SummaryEvent::SummarizeRequested {
        task_id,
        text: text.to_string(),
    }).await?;

    println!("Summary complete!");

    Ok(())
}
