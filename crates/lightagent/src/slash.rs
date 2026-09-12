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
    /// `/skills` — list the skills loaded for this session, extensions' included.
    Skills,
    /// `/reload` — reload installed extensions and tool settings between turns.
    Reload,
    /// `/extensions` — list installed extensions.
    Extensions,
    /// `/extensions install <directory>` — install a local bundle.
    ExtensionInstall(String),
    /// `/extensions uninstall <name>` — remove a global bundle.
    ExtensionUninstall(String),
    /// `/onboard <file.md>` — install dropped Markdown as profile guidance.
    Onboard(String),
    /// `/onboard remove` — withdraw profile guidance.
    OnboardRemove,
    /// `/new` — start a fresh run in the same profile.
    New,
    /// `/approve` — approve the pending tool call.
    Approve,
    /// `/reject` — reject the pending tool call.
    Reject,
    /// `/stop` — cancel the current run.
    Stop,
    /// `/continue` — pick up a run that paused on its time budget.
    Continue,
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
    let rest = trimmed.strip_prefix('/')?.trim_start();
    let word = rest.split_whitespace().next().unwrap_or("");
    let args = rest.get(word.len()..).unwrap_or("").trim();
    let command = match word {
        "help" | "h" | "?" => Slash::Help,
        "tools" => Slash::Tools,
        "skills" => Slash::Skills,
        "reload" => Slash::Reload,
        "extensions" => {
            if args.is_empty() || args == "list" {
                Slash::Extensions
            } else if args == "install" {
                Slash::ExtensionInstall(String::new())
            } else if let Some(source) = args.strip_prefix("install ") {
                Slash::ExtensionInstall(source.trim().to_owned())
            } else if args == "uninstall" {
                Slash::ExtensionUninstall(String::new())
            } else if let Some(name) = args.strip_prefix("uninstall ") {
                Slash::ExtensionUninstall(name.trim().to_owned())
            } else {
                Slash::Unknown("extensions".to_owned())
            }
        }
        "onboard" => {
            if args == "remove" {
                Slash::OnboardRemove
            } else {
                Slash::Onboard(args.to_owned())
            }
        }
        "new" => Slash::New,
        "approve" | "y" | "yes" => Slash::Approve,
        "reject" | "n" | "no" => Slash::Reject,
        "stop" => Slash::Stop,
        "continue" | "resume" => Slash::Continue,
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
        assert_eq!(parse("/skills"), Some(Slash::Skills));
        assert_eq!(parse("/reload"), Some(Slash::Reload));
        assert_eq!(parse("/extensions"), Some(Slash::Extensions));
        assert_eq!(parse("/extensions list"), Some(Slash::Extensions));
        assert_eq!(
            parse("/extensions install ./my tool"),
            Some(Slash::ExtensionInstall("./my tool".into()))
        );
        assert_eq!(
            parse("/extensions uninstall my-tool"),
            Some(Slash::ExtensionUninstall("my-tool".into()))
        );
        assert_eq!(
            parse("/onboard '/tmp/My Notes.md'"),
            Some(Slash::Onboard("'/tmp/My Notes.md'".into()))
        );
        assert_eq!(parse("/onboard remove"), Some(Slash::OnboardRemove));
        assert_eq!(parse("/exit"), Some(Slash::Exit));
        assert_eq!(parse("/q"), Some(Slash::Exit));
        assert_eq!(parse("/approve now"), Some(Slash::Approve));
        assert_eq!(parse("/continue"), Some(Slash::Continue));
        assert_eq!(parse("/resume"), Some(Slash::Continue));
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
