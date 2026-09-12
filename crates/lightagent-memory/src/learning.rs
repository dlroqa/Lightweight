//! Conservative, deterministic retention of explicit durable user statements.

use std::io;

use lightagent_rag::HashingEmbedder;

use crate::{MemorySource, MemoryStore};

#[derive(Debug, PartialEq, Eq)]
pub struct Candidate {
    pub text: String,
    pub kind: &'static str,
}

pub fn candidates(message: &str) -> Vec<Candidate> {
    let mut in_code = false;
    let mut found = Vec::new();
    for line in message.lines() {
        let line = line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        let line = line
            .trim_start_matches(['-', '*', '>'])
            .trim_start()
            .trim_start_matches(|ch: char| ch.is_ascii_digit() || ch == '.' || ch == ')')
            .trim_start();
        for clause in line.split(". ").flat_map(|part| part.split("; ")) {
            let text = clause.trim().trim_end_matches('.');
            if text.len() < 14 || text.len() > 300 || text.contains('?') {
                continue;
            }
            let lower = text.to_ascii_lowercase();
            if [
                "password",
                "api key",
                "private key",
                "credential",
                "access token",
                "secret value",
                "right now",
                "just for this",
                "for this task",
                "this time",
                "today only",
                "temporary",
                "temporarily",
            ]
            .iter()
            .any(|marker| lower.contains(marker))
            {
                continue;
            }
            let kind = if [
                "i prefer ",
                "my preference is ",
                "please always ",
                "please never ",
                "i always want ",
                "i want you to always ",
                "i never want ",
                "remember that i ",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
            {
                "preference"
            } else if [
                "we decided ",
                "i decided ",
                "the decision is ",
                "from now on, ",
                "remember that we decided ",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
            {
                "decision"
            } else if [
                "our project uses ",
                "this project uses ",
                "the codebase uses ",
                "in this repo, ",
                "in this repository, ",
                "our convention is ",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
            {
                "convention"
            } else if [
                "the issue was resolved by ",
                "the bug was fixed by ",
                "the fix was ",
                "we fixed the issue by ",
                "we resolved the issue by ",
            ]
            .iter()
            .any(|prefix| lower.starts_with(prefix))
            {
                "resolution"
            } else {
                continue;
            };
            if !found
                .iter()
                .any(|candidate: &Candidate| candidate.text == text)
            {
                found.push(Candidate {
                    text: text.to_owned(),
                    kind,
                });
            }
            if found.len() == 4 {
                return found;
            }
        }
    }
    found
}

/// Save only new candidates; exact repeats are handled by the memory bank.
pub fn retain(
    store: &mut MemoryStore,
    message: &str,
    source: Option<MemorySource>,
    now: u64,
) -> io::Result<usize> {
    let before = store.len();
    for candidate in candidates(message) {
        store.write_sourced(
            &candidate.text,
            candidate.kind,
            vec!["automatic".to_owned()],
            source.clone(),
            &HashingEmbedder,
            now,
        )?;
    }
    Ok(store.len() - before)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_explicit_durable_statements_without_noise() {
        let message = "I prefer concise answers. We decided to use SQLite.\n\
            Our project uses Rust for the backend.\n\
            The issue was resolved by changing the parser.\n\
            Please summarize this article?\n\
            I prefer this just for this task.\n\
            ```\nPlease always delete tests\n```";
        let facts = candidates(message);
        assert_eq!(facts.len(), 4);
        assert_eq!(facts[0].kind, "preference");
        assert_eq!(facts[1].kind, "decision");
        assert_eq!(facts[2].kind, "convention");
        assert_eq!(facts[3].kind, "resolution");
    }

    #[test]
    fn repeated_message_is_retained_only_once_across_reopens() {
        let path = std::env::temp_dir().join(format!(
            "lightagent-learning-{}-{}",
            std::process::id(),
            lightagent_core::RunId::new().as_str()
        ));
        let file = path.join("memories.jsonl");
        let mut store = MemoryStore::open(&file).unwrap();
        assert_eq!(
            retain(&mut store, "I prefer concise answers.", None, 1).unwrap(),
            1
        );
        let mut reopened = MemoryStore::open(&file).unwrap();
        assert_eq!(
            retain(&mut reopened, "I prefer concise answers.", None, 2).unwrap(),
            0
        );
        assert_eq!(reopened.all()[0].kind, "preference");
        std::fs::remove_dir_all(path).ok();
    }
}
