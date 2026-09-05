//! The slash-command grammar shared by the interactive chat.
//!
//! A line that begins with `/` is a command to the harness rather than a message
//! to the model. Parsing is separated from handling so it can be unit-tested
//! without a running session.

/// A parsed slash command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Slash {
    /// `/help` — list the commands.
    Help,
    /// `/tools` — list the enabled tools.
    Tools,
    /// `/new` — start a fresh run in the same profile.
    New,
    /// `/approve` — approve the pending tool call.
    Approve,
    /// `/reject` — reject the pending tool call.
    Reject,
    /// `/stop` — cancel the current run.
    Stop,
    /// `/exit` or `/quit` — leave the session.
    Exit,
    /// A `/word` that is not a known command; carries the word.
    Unknown(String),
}

/// Parse `line` as a slash command, or `None` when it is an ordinary message.
///
/// Leading whitespace is tolerated; a bare `/` is an unknown command, not a
/// message, so a mistyped slash is reported rather than sent to the model.
pub fn parse(line: &str) -> Option<Slash> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix('/')?;
    let word = rest.split_whitespace().next().unwrap_or("");
    let command = match word {
        "help" | "h" | "?" => Slash::Help,
        "tools" => Slash::Tools,
        "new" => Slash::New,
        "approve" | "y" | "yes" => Slash::Approve,
        "reject" | "n" | "no" => Slash::Reject,
        "stop" => Slash::Stop,
        "exit" | "quit" | "q" => Slash::Exit,
        other => Slash::Unknown(other.to_string()),
    };
    Some(command)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_message_is_not_a_command() {
        assert_eq!(parse("what time is it?"), None);
        assert_eq!(parse("  hello /not a command"), None);
    }

    #[test]
    fn known_commands_parse() {
        assert_eq!(parse("/help"), Some(Slash::Help));
        assert_eq!(parse("  /tools  "), Some(Slash::Tools));
        assert_eq!(parse("/exit"), Some(Slash::Exit));
        assert_eq!(parse("/q"), Some(Slash::Exit));
        assert_eq!(parse("/approve now"), Some(Slash::Approve));
    }

    #[test]
    fn an_unknown_slash_is_reported_not_sent() {
        assert_eq!(
            parse("/frobnicate"),
            Some(Slash::Unknown("frobnicate".into()))
        );
        assert_eq!(parse("/"), Some(Slash::Unknown(String::new())));
    }
}
