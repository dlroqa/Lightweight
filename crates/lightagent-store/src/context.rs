//! Bounded model context derived from the durable, unabridged session.

use std::collections::HashSet;

use lightagent_core::provider::ProviderMessage;

use crate::session::{Session, StoredMessage};

/// Conservative estimate used for packing, not provider billing.
fn tokens(text: &str) -> usize {
    text.len().div_ceil(3).saturating_add(5)
}

fn excerpt(text: &str, budget: usize) -> String {
    let chars = budget.saturating_mul(3);
    if text.chars().count() <= chars {
        text.to_owned()
    } else {
        format!(
            "{}…",
            text.chars()
                .take(chars.saturating_sub(1))
                .collect::<String>()
        )
    }
}

fn terms(text: &str) -> Vec<String> {
    text.split(|ch: char| !ch.is_alphanumeric())
        .filter(|word| word.len() > 2)
        .map(str::to_lowercase)
        .collect()
}

fn relevant(text: &str, query: &[String]) -> usize {
    let haystack = terms(text);
    query.iter().filter(|word| haystack.contains(word)).count()
}

fn recap(messages: &[StoredMessage], budget: usize) -> String {
    if messages.is_empty() || budget < 30 {
        return String::new();
    }
    let important: Vec<usize> = (0..messages.len())
        .filter(|&index| {
            let message = &messages[index];
            let text = message.content.to_lowercase();
            message.role == "user"
                && [
                    "prefer", "remember", "must", "decided", "need", "don't", "do not",
                ]
                .iter()
                .any(|word| text.contains(word))
        })
        .collect();
    let mut candidates: Vec<usize> = important.into_iter().rev().collect();
    candidates.extend((messages.len().saturating_sub(4)..messages.len()).rev());
    let mut seen = HashSet::new();
    candidates.retain(|index| seen.insert(*index));
    let mut lines = Vec::new();
    let mut spent = tokens(
        "Earlier session excerpts (the full transcript remains saved; excerpts may omit detail):",
    );
    for index in candidates {
        let message = &messages[index];
        if !matches!(message.role.as_str(), "user" | "assistant") {
            continue;
        }
        let line = format!(
            "[message {} {}] {}",
            index + 1,
            message.role,
            excerpt(&message.content.replace('\n', " "), 48)
        );
        let cost = tokens(&line);
        if spent + cost <= budget {
            spent += cost;
            lines.push((index, line));
        }
    }
    lines.sort_by_key(|(index, _)| *index);
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "Earlier session excerpts (the full transcript remains saved; excerpts may omit detail):\n{}",
            lines
                .into_iter()
                .map(|(_, line)| line)
                .collect::<Vec<_>>()
                .join("\n")
        )
    }
}

fn evidence(session: &Session, query: &str, budget: usize) -> String {
    if budget < 30 {
        return String::new();
    }
    let query = terms(query);
    let mut candidates = Vec::new();
    for (run_index, run) in session.runs.iter().enumerate() {
        for tool in &run.tools {
            if tool.result_excerpt.is_empty() {
                continue;
            }
            let searchable = format!(
                "{} {} {} {}",
                tool.tool,
                tool.arguments_preview,
                tool.source.as_deref().unwrap_or(""),
                tool.result_excerpt
            );
            let score = relevant(&searchable, &query);
            if score > 0 {
                candidates.push((score, run_index, tool));
            }
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut lines = Vec::new();
    let mut spent = tokens("Relevant tool evidence from this session:");
    for (_, _, tool) in candidates.into_iter().take(5) {
        let line = format!(
            "[tool {} {}{}] {}{}",
            tool.id,
            tool.tool,
            tool.source
                .as_ref()
                .map(|s| format!(" source={s}"))
                .unwrap_or_default(),
            excerpt(&tool.result_excerpt.replace('\n', " "), 75),
            if tool.truncated { " [excerpt]" } else { "" }
        );
        let cost = tokens(&line);
        if spent + cost <= budget {
            spent += cost;
            lines.push(line);
        }
    }
    if lines.is_empty() {
        String::new()
    } else {
        format!(
            "Relevant tool evidence from this session:\n{}",
            lines.join("\n")
        )
    }
}

/// Keep recent turns verbatim and compact older material within a share of the
/// model's context. The provider's system prompt, tools, current user message,
/// and answer still need room; the source transcript is never modified.
pub fn model_history(
    session: &Session,
    current: &str,
    context_limit: usize,
) -> Vec<ProviderMessage> {
    let budget = context_limit
        .saturating_mul(35)
        .saturating_div(100)
        .min(16_000)
        .saturating_sub(tokens(current));
    if budget == 0 {
        return Vec::new();
    }
    let recent_budget = budget.saturating_mul(65) / 100;
    let mut recent = Vec::new();
    let mut recent_start = session.messages.len();
    let mut spent = 0;
    for (index, message) in session.messages.iter().enumerate().rev() {
        if !matches!(message.role.as_str(), "user" | "assistant") {
            continue;
        }
        let cost = tokens(&message.content);
        if spent + cost > recent_budget {
            break;
        }
        spent += cost;
        recent_start = index;
        recent.push(match message.role.as_str() {
            "user" => ProviderMessage::user(message.content.clone()),
            _ => ProviderMessage::assistant(message.content.clone()),
        });
    }
    recent.reverse();
    let mut history = Vec::new();
    let summary = recap(
        &session.messages[..recent_start],
        budget.saturating_mul(18) / 100,
    );
    if !summary.is_empty() {
        history.push(ProviderMessage::system(summary));
    }
    let evidence = evidence(session, current, budget.saturating_mul(17) / 100);
    if !evidence.is_empty() {
        history.push(ProviderMessage::system(evidence));
    }
    history.extend(recent);
    history
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{RunRecord, ToolHistoryEntry};
    use std::time::SystemTime;

    #[test]
    fn long_session_keeps_recent_turns_and_recoverable_evidence() {
        let mut session = Session::new("default", "chat");
        session.push_message(StoredMessage::new("user", "I prefer concise Rust answers"));
        for index in 0..20 {
            session.push_message(StoredMessage::new(
                "assistant",
                format!("filler {index} ").repeat(20),
            ));
        }
        session.push_message(StoredMessage::new("user", "where is the deploy script?"));
        session.push_run(RunRecord {
            run_id: "run-1".into(),
            started_at: SystemTime::now(),
            ended_at: None,
            stop_reason: None,
            tools: vec![ToolHistoryEntry {
                id: "call-1".into(),
                tool: "fs.read".into(),
                arguments_preview: "ops/deploy.sh".into(),
                result_excerpt: "deploy steps are documented here".into(),
                source: Some("ops/deploy.sh".into()),
                truncated: false,
                outcome: "ok".into(),
                duration_ms: None,
            }],
        });
        let history = model_history(&session, "where is deploy?", 4_000);
        let text = history
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("ops/deploy.sh"));
        assert!(text.contains("where is the deploy script?"));
        assert!(text.contains("I prefer concise Rust answers"));
        assert!(
            history
                .iter()
                .map(|message| tokens(&message.content))
                .sum::<usize>()
                < 1_400
        );
        assert_eq!(session.messages.len(), 22);
    }
}
