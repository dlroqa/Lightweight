//! Names a person chose for their models, and how a requested name is read.
//!
//! A model's catalog id is ours: a slug derived from its file name, stable so
//! that importing the same file twice is recognisably the same model. It is
//! also long and technical — `qwen3.5-9b-fable-5-v1-q8_0` — which is the wrong
//! thing to make every API client type and every model picker show.
//!
//! An **alias** is the other name: chosen by the user, never derived, and free
//! to look nothing like the file (`Coder`, `Fast`, `Jarvis`). It is a routing
//! and display identity layered over the id and nothing more. The file, the
//! id, the digest, the provenance and every engine parameter are untouched by
//! it, so giving a model an alias, changing it or clearing it never needs a
//! reload.
//!
//! The rules that keep one name meaning one model:
//!
//! * **At most one alias per model, and one model per alias.** Two models may
//!   not share an alias, and nothing here ever renames a user's choice to make
//!   it fit — a clash is refused, never resolved with a `-2`.
//! * **Unique ignoring case.** `Coder` and `coder` are one name, because a
//!   person who typed either meant the same thing. The casing they chose is
//!   kept for display.
//! * **Never another model's id.** Otherwise a request naming that id would
//!   reach a different model depending on which rule was checked first.
//! * **Never a reserved selector.** `default` already means "whatever is
//!   loaded"; a model called `default` would make that ambiguous.

use std::fmt;

/// The longest alias accepted, in characters.
///
/// Room for a descriptive name, short of a sentence: a model picker has to
/// fit it on one line.
pub const MAX_ALIAS_CHARS: usize = 64;

/// Names that already mean something as a `model` value and so cannot be given
/// to a model.
///
/// `default` is the selector for "the model that is loaded right now", used by
/// clients that do not want to know which one that is.
pub const RESERVED_SELECTORS: &[&str] = &["default"];

/// What a client's `model` field asked for.
///
/// One parse, shared by every place that reads a model name, so the OpenAI
/// surface and the control API cannot disagree about what `default` or an
/// empty string means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ModelSelector<'a> {
    /// No model named, or a reserved selector: whatever is loaded.
    Default,
    /// A name — an alias or a catalog id — trimmed of surrounding whitespace.
    Named(&'a str),
}

impl<'a> ModelSelector<'a> {
    pub fn parse(requested: Option<&'a str>) -> Self {
        let trimmed = requested.unwrap_or_default().trim();
        if trimmed.is_empty() || is_reserved(trimmed) {
            Self::Default
        } else {
            Self::Named(trimmed)
        }
    }
}

/// Whether `name` is one of the [`RESERVED_SELECTORS`], ignoring case.
pub fn is_reserved(name: &str) -> bool {
    let name = name.trim();
    RESERVED_SELECTORS
        .iter()
        .any(|reserved| reserved.eq_ignore_ascii_case(name))
}

/// Whether two names are the same alias.
///
/// Trimmed and compared case-insensitively, including outside ASCII, so
/// `Écrivain` and `écrivain` are one name just as `Coder` and `coder` are.
pub fn same_name(a: &str, b: &str) -> bool {
    lookup_key(a) == lookup_key(b)
}

/// The form an alias is compared in. Never stored and never shown.
pub fn lookup_key(name: &str) -> String {
    name.trim().to_lowercase()
}

/// Why an alias was refused before any other model was consulted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AliasProblem {
    Empty,
    TooLong,
    Reserved,
    /// A character that would make the alias unusable somewhere it has to go.
    Character(char),
    /// `.` or `..`, which a URL path silently resolves away.
    DotSegment,
}

impl fmt::Display for AliasProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("an alias cannot be empty"),
            Self::TooLong => write!(f, "an alias can be at most {MAX_ALIAS_CHARS} characters"),
            Self::Reserved => f.write_str(
                "that name is reserved: `default` already means whichever model is loaded",
            ),
            Self::Character('/' | '\\') => {
                f.write_str("an alias cannot contain `/` or `\\`, because it is used in URL paths")
            }
            Self::Character('@') => f.write_str(
                "an alias cannot contain `@`, which model ids use for their context suffix",
            ),
            Self::Character(_) => f.write_str("an alias cannot contain control characters"),
            Self::DotSegment => f.write_str("an alias cannot be `.` or `..`"),
        }
    }
}

/// Check an alias on its own, and return it trimmed.
///
/// Deliberately permissive about what a name may look like — letters in any
/// script, digits, spaces, `-`, `_`, `.` — and strict only where a character
/// would break something: a URL path, a context suffix, a terminal. Whether
/// the alias is free is a question about the other models, and is answered by
/// [`crate::CatalogStore::set_alias`].
pub fn validate_alias(raw: &str) -> Result<String, AliasProblem> {
    let alias = raw.trim();
    if alias.is_empty() {
        return Err(AliasProblem::Empty);
    }
    if alias.chars().count() > MAX_ALIAS_CHARS {
        return Err(AliasProblem::TooLong);
    }
    if is_reserved(alias) {
        return Err(AliasProblem::Reserved);
    }
    if alias == "." || alias == ".." {
        return Err(AliasProblem::DotSegment);
    }
    if let Some(bad) = alias
        .chars()
        .find(|c| matches!(c, '/' | '\\' | '@') || c.is_control())
    {
        return Err(AliasProblem::Character(bad));
    }
    Ok(alias.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_people_actually_choose_are_accepted() {
        for alias in [
            "Coder",
            "Research",
            "Primary",
            "Fast",
            "Reasoning",
            "Jarvis",
            "qwen-main",
            "Qwen-Coder",
            "assistant-1",
            "general_chat",
            "Local-70B",
            "qwen3.5",
            "Local 70B",
            "Écrivain",
        ] {
            assert_eq!(validate_alias(alias).as_deref(), Ok(alias), "{alias}");
        }
    }

    #[test]
    fn surrounding_whitespace_is_trimmed_rather_than_refused() {
        assert_eq!(validate_alias("  Coder \t").as_deref(), Ok("Coder"));
    }

    #[test]
    fn a_name_that_would_break_something_is_refused() {
        assert_eq!(validate_alias(""), Err(AliasProblem::Empty));
        assert_eq!(validate_alias("   "), Err(AliasProblem::Empty));
        assert_eq!(validate_alias("default"), Err(AliasProblem::Reserved));
        assert_eq!(validate_alias(" DEFAULT "), Err(AliasProblem::Reserved));
        assert_eq!(
            validate_alias("../model"),
            Err(AliasProblem::Character('/'))
        );
        assert_eq!(validate_alias("foo/bar"), Err(AliasProblem::Character('/')));
        assert_eq!(
            validate_alias("foo\\bar"),
            Err(AliasProblem::Character('\\'))
        );
        assert_eq!(
            validate_alias("coder@8k"),
            Err(AliasProblem::Character('@'))
        );
        assert_eq!(validate_alias("a\nb"), Err(AliasProblem::Character('\n')));
        assert_eq!(
            validate_alias("a\u{7}b"),
            Err(AliasProblem::Character('\u{7}'))
        );
        assert_eq!(validate_alias(".."), Err(AliasProblem::DotSegment));
        assert_eq!(validate_alias("."), Err(AliasProblem::DotSegment));
    }

    #[test]
    fn the_length_limit_counts_characters_not_bytes() {
        // 64 two-byte characters is 128 bytes and still a 64-character name.
        assert!(validate_alias(&"é".repeat(MAX_ALIAS_CHARS)).is_ok());
        assert_eq!(
            validate_alias(&"a".repeat(MAX_ALIAS_CHARS + 1)),
            Err(AliasProblem::TooLong)
        );
    }

    #[test]
    fn every_refusal_says_why_in_words() {
        for problem in [
            AliasProblem::Empty,
            AliasProblem::TooLong,
            AliasProblem::Reserved,
            AliasProblem::Character('/'),
            AliasProblem::Character('@'),
            AliasProblem::Character('\0'),
            AliasProblem::DotSegment,
        ] {
            let said = problem.to_string();
            assert!(said.contains("alias") || said.contains("default"), "{said}");
        }
    }

    #[test]
    fn names_compare_ignoring_case_in_any_script() {
        assert!(same_name("Coder", "coder"));
        assert!(same_name("CODER", " coder "));
        assert!(same_name("Écrivain", "écrivain"));
        assert!(!same_name("Coder", "Coder2"));
    }

    #[test]
    fn default_and_nothing_both_mean_whatever_is_loaded() {
        assert_eq!(ModelSelector::parse(None), ModelSelector::Default);
        assert_eq!(ModelSelector::parse(Some("")), ModelSelector::Default);
        assert_eq!(ModelSelector::parse(Some("  ")), ModelSelector::Default);
        assert_eq!(
            ModelSelector::parse(Some("default")),
            ModelSelector::Default
        );
        assert_eq!(
            ModelSelector::parse(Some(" default ")),
            ModelSelector::Default
        );
        assert_eq!(
            ModelSelector::parse(Some(" Coder ")),
            ModelSelector::Named("Coder")
        );
    }
}
