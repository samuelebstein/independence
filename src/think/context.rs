//! Deterministic, privacy-safe preparation of one Ollama request context.
//!
//! The current tokenizer is intentionally conservative rather than exact: it
//! estimates no fewer than one token for every UTF-8 byte plus message
//! envelopes. This makes trimming earlier than a model-specific tokenizer
//! would, but avoids claiming word or character counts are exact token counts.

use thiserror::Error;
use tracing::{info, info_span};

use super::{ChatMessage, ConversationTurn};

/// Conservative explicit fallback when the model/runtime does not provide a
/// verified active context setting. It remains below many model theoretical
/// capacities while allowing the existing multi-photo interaction to fit.
pub(crate) const DEFAULT_RUNTIME_CONTEXT_TOKENS: usize = 8_192;
pub(crate) const DEFAULT_RESERVED_OUTPUT_TOKENS: usize = 512;
pub(crate) const DEFAULT_SAFETY_MARGIN_TOKENS: usize = 256;
pub(crate) const DEFAULT_IMAGE_TOKEN_ALLOWANCE: usize = 1_024;

/// An explicit runtime context budget. The application sends this selected
/// capacity to Ollama rather than confusing theoretical model capacity with
/// the actual request limit it manages.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ContextBudget {
    pub runtime_context_tokens: usize,
    pub reserved_output_tokens: usize,
    pub safety_margin_tokens: usize,
    pub image_token_allowance: usize,
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self {
            runtime_context_tokens: DEFAULT_RUNTIME_CONTEXT_TOKENS,
            reserved_output_tokens: DEFAULT_RESERVED_OUTPUT_TOKENS,
            safety_margin_tokens: DEFAULT_SAFETY_MARGIN_TOKENS,
            image_token_allowance: DEFAULT_IMAGE_TOKEN_ALLOWANCE,
        }
    }
}

impl ContextBudget {
    pub(crate) fn input_budget(self) -> Result<usize, ContextError> {
        self.runtime_context_tokens
            .checked_sub(self.reserved_output_tokens)
            .and_then(|budget| budget.checked_sub(self.safety_margin_tokens))
            .filter(|budget| *budget > 0)
            .ok_or(ContextError::InvalidBudget {
                context: self.runtime_context_tokens,
                reserved_output: self.reserved_output_tokens,
                safety_margin: self.safety_margin_tokens,
            })
    }
}

/// A history snapshot ready for one model request. The original controller
/// history is never mutated by trimming.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PreparedContext {
    pub history: Vec<ConversationTurn>,
    pub input_budget_tokens: usize,
    pub estimated_tokens_before_trim: usize,
    pub estimated_tokens_after_trim: usize,
    pub removed_exchanges: usize,
    pub removed_messages: usize,
    pub included_messages: usize,
    pub tool_definition_tokens: usize,
}

/// An explicit reason that a context could not be prepared safely.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub(crate) enum ContextError {
    #[error(
        "context configuration is invalid: context={context}, reserved_output={reserved_output}, safety_margin={safety_margin}"
    )]
    InvalidBudget {
        context: usize,
        reserved_output: usize,
        safety_margin: usize,
    },

    #[error(
        "the required current request needs about {required_tokens} tokens, but the available input budget is {input_budget_tokens}; reduce the latest request, tool result, or configured output reserve"
    )]
    RequiredContextTooLarge {
        required_tokens: usize,
        input_budget_tokens: usize,
    },
}

/// Trims only oldest complete prior exchanges until a request fits.
///
/// `system` and `active_messages` are required for the next request and are
/// never removed. A `ConversationTurn` includes all of its tool rounds, so a
/// tool call can never be separated from its result by this policy.
pub(crate) fn prepare(
    system: &ChatMessage,
    history: &[ConversationTurn],
    active_messages: &[ChatMessage],
    tool_definition_tokens: usize,
    budget: ContextBudget,
) -> Result<PreparedContext, ContextError> {
    let input_budget_tokens = budget.input_budget()?;
    let span = info_span!(
        "independence.context.prepare",
        runtime_context_tokens = budget.runtime_context_tokens,
        reserved_output_tokens = budget.reserved_output_tokens,
        input_budget_tokens,
        history_exchanges = history.len(),
    );
    let _entered = span.enter();

    let required_tokens = estimate_message(system, budget)
        .saturating_add(estimate_messages(active_messages, budget))
        .saturating_add(tool_definition_tokens);
    if required_tokens > input_budget_tokens {
        return Err(ContextError::RequiredContextTooLarge {
            required_tokens,
            input_budget_tokens,
        });
    }

    let per_exchange = history
        .iter()
        .map(|turn| estimate_turn(turn, budget))
        .collect::<Vec<_>>();
    let estimated_tokens_before_trim = required_tokens.saturating_add(
        per_exchange
            .iter()
            .copied()
            .fold(0usize, usize::saturating_add),
    );
    let mut retained_start = 0usize;
    let mut estimated_tokens_after_trim = estimated_tokens_before_trim;
    while estimated_tokens_after_trim > input_budget_tokens && retained_start < history.len() {
        estimated_tokens_after_trim =
            estimated_tokens_after_trim.saturating_sub(per_exchange[retained_start]);
        retained_start += 1;
    }

    let removed_exchanges = retained_start;
    let removed_messages = history[..retained_start]
        .iter()
        .map(conversation_turn_message_count)
        .sum();
    let retained_history = history[retained_start..].to_vec();
    let included_messages = 1usize.saturating_add(active_messages.len()).saturating_add(
        retained_history
            .iter()
            .map(conversation_turn_message_count)
            .sum::<usize>(),
    );
    info!(
        target: "context",
        event = "prepared",
        estimated_tokens_before_trim,
        estimated_tokens_after_trim,
        removed_exchanges,
        removed_messages,
        included_messages,
        estimator = "conservative_utf8_byte_estimate",
        "context prepared"
    );
    Ok(PreparedContext {
        history: retained_history,
        input_budget_tokens,
        estimated_tokens_before_trim,
        estimated_tokens_after_trim,
        removed_exchanges,
        removed_messages,
        included_messages,
        tool_definition_tokens,
    })
}

pub(crate) fn estimate_tool_definitions(serialized_json: &[u8]) -> usize {
    // The schema is supplied as JSON in the prompt. Counting bytes is a
    // deliberately conservative approximation when the selected model's
    // exact tokenizer is unavailable.
    serialized_json.len().saturating_add(8)
}

fn estimate_messages(messages: &[ChatMessage], budget: ContextBudget) -> usize {
    messages
        .iter()
        .map(|message| estimate_message(message, budget))
        .fold(0usize, usize::saturating_add)
}

fn estimate_message(message: &ChatMessage, budget: ContextBudget) -> usize {
    message.conservative_token_estimate(budget.image_token_allowance)
}

fn estimate_turn(turn: &ConversationTurn, budget: ContextBudget) -> usize {
    let user = ChatMessage::user(&turn.user, &[]);
    let assistant = ChatMessage::assistant(&turn.assistant);
    let tool_rounds = turn.tool_rounds.iter().fold(0usize, |total, round| {
        let assistant = ChatMessage::assistant_with_tools(&round.assistant_content, &round.calls);
        total
            .saturating_add(estimate_message(&assistant, budget))
            .saturating_add(
                round
                    .results
                    .iter()
                    .map(|result| {
                        estimate_message(
                            &ChatMessage::tool(&result.tool_name, &result.content),
                            budget,
                        )
                    })
                    .fold(0usize, usize::saturating_add),
            )
    });
    estimate_message(&user, budget)
        .saturating_add(tool_rounds)
        .saturating_add(estimate_message(&assistant, budget))
}

pub(crate) fn conversation_turn_message_count(turn: &ConversationTurn) -> usize {
    2usize.saturating_add(
        turn.tool_rounds
            .iter()
            .map(|round| 1usize.saturating_add(round.results.len()))
            .sum::<usize>(),
    )
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{ChatMessage, ContextBudget, ContextError, prepare};
    use crate::think::{ConversationTurn, ToolCall, ToolResult, ToolRound};

    fn turn(user: &str, assistant: &str) -> ConversationTurn {
        ConversationTurn {
            user: user.into(),
            tool_rounds: Vec::new(),
            assistant: assistant.into(),
        }
    }

    #[test]
    fn retains_all_history_when_it_fits() {
        let system = ChatMessage::system("system");
        let active = vec![ChatMessage::user("latest", &[])];
        let history = vec![turn("one", "two")];
        let prepared = prepare(
            &system,
            &history,
            &active,
            0,
            ContextBudget {
                runtime_context_tokens: 200,
                reserved_output_tokens: 20,
                safety_margin_tokens: 10,
                image_token_allowance: 10,
            },
        )
        .unwrap();
        assert_eq!(prepared.history, history);
        assert_eq!(prepared.removed_exchanges, 0);
    }

    #[test]
    fn trims_oldest_complete_exchange_first() {
        let system = ChatMessage::system("system");
        let active = vec![ChatMessage::user("latest request", &[])];
        let history = vec![
            turn("aaaaaaaaaaaaaaaaaaaaaaaa", "bbbbbbbbbbbbbbbbbbbbbbbb"),
            turn("recent", "answer"),
        ];
        let prepared = prepare(
            &system,
            &history,
            &active,
            0,
            ContextBudget {
                runtime_context_tokens: 160,
                reserved_output_tokens: 20,
                safety_margin_tokens: 20,
                image_token_allowance: 10,
            },
        )
        .unwrap();
        assert_eq!(prepared.removed_exchanges, 1);
        assert_eq!(prepared.history, vec![history[1].clone()]);
        assert!(prepared.estimated_tokens_after_trim <= prepared.input_budget_tokens);
    }

    #[test]
    fn never_separates_tool_calls_from_results() {
        let system = ChatMessage::system("system");
        let active = vec![ChatMessage::user("latest", &[])];
        let history = vec![ConversationTurn {
            user: "old request".into(),
            tool_rounds: vec![ToolRound {
                assistant_content: String::new(),
                calls: vec![ToolCall::from_value(
                    Some(0),
                    "recall".into(),
                    serde_json::json!({"query": "old"}),
                )],
                results: vec![ToolResult {
                    tool_name: "recall".into(),
                    content: "result".into(),
                }],
            }],
            assistant: "old answer".into(),
        }];
        let prepared = prepare(
            &system,
            &history,
            &active,
            0,
            ContextBudget {
                runtime_context_tokens: 80,
                reserved_output_tokens: 8,
                safety_margin_tokens: 4,
                image_token_allowance: 8,
            },
        )
        .unwrap();
        assert!(prepared.history.is_empty());
        assert_eq!(prepared.removed_messages, 4);
    }

    #[test]
    fn rejects_required_current_context_instead_of_dropping_it() {
        let system = ChatMessage::system("system");
        let active = vec![ChatMessage::user(&"x".repeat(100), &[])];
        let result = prepare(
            &system,
            &[],
            &active,
            0,
            ContextBudget {
                runtime_context_tokens: 32,
                reserved_output_tokens: 8,
                safety_margin_tokens: 4,
                image_token_allowance: 8,
            },
        );
        assert!(matches!(
            result,
            Err(ContextError::RequiredContextTooLarge { .. })
        ));
    }

    #[test]
    #[ignore = "benchmark: run explicitly with --ignored --nocapture"]
    fn benchmarks_large_history_assembly_and_chronological_trimming() {
        let history = (0..2_000)
            .map(|index| {
                turn(
                    &format!("user exchange {index}: {}", "x".repeat(240)),
                    &format!("assistant exchange {index}: {}", "y".repeat(240)),
                )
            })
            .collect::<Vec<_>>();
        let system = ChatMessage::system("system instructions");
        let active = vec![ChatMessage::user("most recent request", &[])];
        let budget = ContextBudget {
            runtime_context_tokens: 8_192,
            reserved_output_tokens: 512,
            safety_margin_tokens: 256,
            image_token_allowance: 1_024,
        };

        let started = Instant::now();
        let mut removed = 0usize;
        for _ in 0..100 {
            let prepared = prepare(&system, &history, &active, 1_500, budget)
                .expect("benchmark context should fit after trimming");
            removed = prepared.removed_exchanges;
        }
        let elapsed = started.elapsed();
        println!(
            "history_exchanges={} runs=100 total_ms={} average_us={} removed_exchanges={removed}",
            history.len(),
            elapsed.as_millis(),
            elapsed.as_micros() / 100,
        );
    }
}
