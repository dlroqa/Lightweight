//! Reconstructing tool calls from a streamed model turn.
//!
//! A tool call arrives in fragments across many deltas: the first delta of an
//! index carries its `id` and `name`, and every delta contributes a slice of
//! the argument string, in order. The client's job is the mirror of the
//! gateway's emitter (`lightweight-api::stream`): keep the `id` and `name` from
//! the first delta of each index and ignore any later repeat, and concatenate
//! the arguments. A provider that repeats an `id` at a known index must not
//! open a second call.

use std::collections::BTreeMap;

use crate::invoker::ToolCall;

/// Accumulates streamed tool-call deltas into whole [`ToolCall`]s.
#[derive(Clone, Debug, Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<u32, Partial>,
    /// Index order as first seen, so reconstructed calls keep their arrival
    /// order rather than the numeric order of a map.
    order: Vec<u32>,
}

#[derive(Clone, Debug, Default)]
struct Partial {
    id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl ToolCallAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Fold one delta into the accumulator.
    ///
    /// `id` and `name` are taken from the first delta that carries them and any
    /// later value at the same index is ignored; `arguments` are appended.
    pub fn push(
        &mut self,
        index: u32,
        id: Option<String>,
        name: Option<String>,
        arguments: Option<String>,
    ) {
        if !self.calls.contains_key(&index) {
            self.order.push(index);
        }
        let entry = self.calls.entry(index).or_default();
        if entry.id.is_none()
            && let Some(id) = id
        {
            entry.id = Some(id);
        }
        if entry.name.is_none()
            && let Some(name) = name
        {
            entry.name = Some(name);
        }
        if let Some(arguments) = arguments {
            entry.arguments.push_str(&arguments);
        }
    }

    /// Whether any tool call has been seen.
    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    /// Reconstruct the whole tool calls, in arrival order.
    ///
    /// A call whose `id` never arrived is given a deterministic one derived
    /// from its index, so the matching tool result can still be addressed.
    pub fn into_calls(self) -> Vec<ToolCall> {
        let Self { mut calls, order } = self;
        order
            .into_iter()
            .filter_map(|index| {
                calls.remove(&index).map(|partial| ToolCall {
                    id: partial.id.unwrap_or_else(|| format!("call_{index}")),
                    name: partial.name.unwrap_or_default(),
                    arguments: partial.arguments,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accumulator_reconstructs_split_arguments() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(
            0,
            Some("call_1".into()),
            Some("read_file".into()),
            Some(String::new()),
        );
        acc.push(0, None, None, Some("{\"path\":".into()));
        acc.push(0, None, None, Some("\"a.txt\"}".into()));

        let calls = acc.into_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, "{\"path\":\"a.txt\"}");
    }

    #[test]
    fn accumulator_ignores_repeated_id_and_name() {
        // A provider that repeats itself must not open a second call or change
        // the name mid-stream.
        let mut acc = ToolCallAccumulator::new();
        acc.push(
            0,
            Some("call_1".into()),
            Some("first".into()),
            Some("a".into()),
        );
        acc.push(
            0,
            Some("call_2".into()),
            Some("second".into()),
            Some("b".into()),
        );

        let calls = acc.into_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[0].name, "first");
        assert_eq!(calls[0].arguments, "ab");
    }

    #[test]
    fn two_indexes_keep_their_arrival_order() {
        let mut acc = ToolCallAccumulator::new();
        acc.push(
            0,
            Some("call_a".into()),
            Some("a".into()),
            Some("{}".into()),
        );
        acc.push(
            1,
            Some("call_b".into()),
            Some("b".into()),
            Some("{}".into()),
        );
        let calls = acc.into_calls();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_a");
        assert_eq!(calls[1].id, "call_b");
    }
}
