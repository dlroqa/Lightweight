//! Structural protection for user-authored text.
//!
//! Spec section 26 requires that prompts are not logged by default, and that a
//! "Privacy Mode" can disable prompt logging completely. Both are easy to state
//! and easy to violate: one `tracing::info!(?request)` on a struct containing a
//! prompt is all it takes, and nobody notices until the logs are shipped.
//!
//! So the guarantee here is structural rather than procedural. User text is
//! wrapped in [`Private`], which:
//!
//! * renders as `<redacted, 412 chars>` from `Display` **and** `Debug`, so
//!   `{}`, `{:?}` and `tracing`'s field capture are all safe;
//! * does not implement `Serialize`, so it cannot be swept into a JSON log line
//!   by a `#[derive(Serialize)]` on some enclosing struct;
//! * only yields its contents through [`Private::reveal`], which is a single
//!   greppable token that a reviewer can audit every use of.
//!
//! The default is redaction. Revealing requires someone to have called
//! [`set_privacy_mode`] with [`PrivacyMode::PromptsLogged`], and
//! [`PrivacyMode::Strict`] makes even that impossible for the rest of the run.

use std::fmt;
use std::sync::atomic::{AtomicU8, Ordering};

/// How much user text this process is permitted to write to logs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PrivacyMode {
    /// The default. Prompts and completions are redacted in logs. Metadata —
    /// token counts, timings, model ids — is still recorded.
    Standard,
    /// Prompt logging deliberately enabled for debugging. Never the default,
    /// and the gateway logs a warning at startup when it is on.
    PromptsLogged,
    /// Privacy Mode. Redaction is locked on: any later attempt to move to
    /// [`PrivacyMode::PromptsLogged`] is refused for the lifetime of the
    /// process.
    Strict,
}

impl PrivacyMode {
    const STANDARD: u8 = 0;
    const PROMPTS_LOGGED: u8 = 1;
    const STRICT: u8 = 2;

    const fn as_u8(self) -> u8 {
        match self {
            Self::Standard => Self::STANDARD,
            Self::PromptsLogged => Self::PROMPTS_LOGGED,
            Self::Strict => Self::STRICT,
        }
    }

    const fn from_u8(v: u8) -> Self {
        match v {
            Self::PROMPTS_LOGGED => Self::PromptsLogged,
            Self::STRICT => Self::Strict,
            // Anything unexpected falls back to the safe mode rather than
            // failing open.
            _ => Self::Standard,
        }
    }
}

/// Starts at `Standard`: redaction is on before any configuration is read, so
/// there is no window during startup where a prompt could leak.
static PRIVACY_MODE: AtomicU8 = AtomicU8::new(PrivacyMode::STANDARD);

/// Set the process-wide privacy mode.
///
/// Returns the mode actually in force afterwards. Once [`PrivacyMode::Strict`]
/// has been set it cannot be relaxed, so the return value will still be
/// `Strict` if a caller tries — check it rather than assuming the request was
/// honoured.
pub fn set_privacy_mode(mode: PrivacyMode) -> PrivacyMode {
    // Strict is a one-way latch. `fetch_update` gives us compare-and-swap
    // semantics so two threads racing at startup cannot combine to unlock it.
    let result = PRIVACY_MODE.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |current| {
        if PrivacyMode::from_u8(current) == PrivacyMode::Strict {
            None
        } else {
            Some(mode.as_u8())
        }
    });

    match result {
        Ok(_) => mode,
        Err(_) => PrivacyMode::Strict,
    }
}

/// The privacy mode currently in force.
pub fn privacy_mode() -> PrivacyMode {
    PrivacyMode::from_u8(PRIVACY_MODE.load(Ordering::Relaxed))
}

/// Whether user text may currently appear in logs.
pub fn prompt_logging_enabled() -> bool {
    privacy_mode() == PrivacyMode::PromptsLogged
}

/// Something that can describe its size and shape without revealing content.
///
/// Implemented for the shapes we actually wrap. Requiring it — rather than
/// blanket-implementing over `Display` — means adding a new kind of sensitive
/// payload is a deliberate act.
pub trait Sensitive {
    /// A summary safe to print in any context. Must not include any content.
    fn redacted_summary(&self) -> String;
}

impl Sensitive for str {
    fn redacted_summary(&self) -> String {
        format!("<redacted, {} chars>", self.chars().count())
    }
}

impl Sensitive for String {
    fn redacted_summary(&self) -> String {
        self.as_str().redacted_summary()
    }
}

impl<T> Sensitive for Vec<T> {
    fn redacted_summary(&self) -> String {
        format!("<redacted, {} items>", self.len())
    }
}

impl<T: Sensitive> Sensitive for Option<T> {
    fn redacted_summary(&self) -> String {
        match self {
            Some(inner) => inner.redacted_summary(),
            None => "<none>".to_owned(),
        }
    }
}

impl<T: Sensitive + ?Sized> Sensitive for &T {
    fn redacted_summary(&self) -> String {
        (**self).redacted_summary()
    }
}

/// A wrapper for user-authored text that cannot be logged by accident.
///
/// Note the deliberate omissions: no `Serialize`, no `Deref`, no
/// `impl AsRef<str>`. Each of those would let the contents escape into a log
/// line without anyone writing [`Private::reveal`].
#[derive(Clone, PartialEq, Eq)]
pub struct Private<T>(T);

impl<T> Private<T> {
    pub const fn new(value: T) -> Self {
        Self(value)
    }

    /// Yield the contents.
    ///
    /// Every call site is an intentional decision to handle user text. Audit
    /// them with `rg 'reveal\(\)'` — a call inside a logging macro is a bug.
    pub fn reveal(&self) -> &T {
        &self.0
    }

    /// Consume the wrapper and yield the contents. Same rules as
    /// [`Private::reveal`].
    pub fn into_revealed(self) -> T {
        self.0
    }

    /// Apply a function to the contents, keeping the result wrapped.
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Private<U> {
        Private(f(self.0))
    }
}

impl<T: Sensitive> Private<T> {
    /// The redacted description, regardless of the current privacy mode.
    /// Use this when a summary is wanted even in a debug build.
    pub fn summary(&self) -> String {
        self.0.redacted_summary()
    }
}

impl<T: Sensitive + fmt::Display> fmt::Display for Private<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if prompt_logging_enabled() {
            fmt::Display::fmt(&self.0, f)
        } else {
            f.write_str(&self.0.redacted_summary())
        }
    }
}

/// `Debug` redacts exactly as `Display` does. `tracing`'s `?field` capture uses
/// `Debug`, and that is the most likely way for a prompt to reach a log.
impl<T: Sensitive + fmt::Debug> fmt::Debug for Private<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if prompt_logging_enabled() {
            fmt::Debug::fmt(&self.0, f)
        } else {
            f.write_str(&self.0.redacted_summary())
        }
    }
}

impl<T> From<T> for Private<T> {
    fn from(value: T) -> Self {
        Self(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The privacy mode is process-global, so these tests must not run
    /// concurrently with each other. They are serialized through this mutex
    /// and each one restores the default before returning.
    static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn with_mode<R>(mode: PrivacyMode, f: impl FnOnce() -> R) -> R {
        let _lock = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        PRIVACY_MODE.store(mode.as_u8(), Ordering::SeqCst);
        let result = f();
        PRIVACY_MODE.store(PrivacyMode::STANDARD, Ordering::SeqCst);
        result
    }

    #[test]
    fn redacts_by_default() {
        with_mode(PrivacyMode::Standard, || {
            let prompt = Private::new(String::from("my secret medical question"));
            assert_eq!(format!("{prompt}"), "<redacted, 26 chars>");
            assert_eq!(format!("{prompt:?}"), "<redacted, 26 chars>");
        });
    }

    #[test]
    fn debug_redacts_too_because_tracing_uses_it() {
        with_mode(PrivacyMode::Standard, || {
            let prompt = Private::new(String::from("hello"));
            // `tracing::info!(?prompt)` formats with Debug. If Debug leaked,
            // the default field-capture syntax would be a data leak.
            assert!(!format!("{prompt:?}").contains("hello"));
        });
    }

    #[test]
    fn reveals_only_when_prompt_logging_is_on() {
        with_mode(PrivacyMode::PromptsLogged, || {
            let prompt = Private::new(String::from("hello"));
            assert_eq!(format!("{prompt}"), "hello");
        });
    }

    #[test]
    fn reveal_works_regardless_of_mode() {
        with_mode(PrivacyMode::Standard, || {
            let prompt = Private::new(String::from("hello"));
            // The engine still needs the actual prompt; redaction governs
            // logging, not function.
            assert_eq!(prompt.reveal(), "hello");
        });
    }

    #[test]
    fn strict_mode_cannot_be_relaxed() {
        with_mode(PrivacyMode::Standard, || {
            assert_eq!(set_privacy_mode(PrivacyMode::Strict), PrivacyMode::Strict);

            // The whole point of Privacy Mode: a later call, from anywhere,
            // cannot turn prompt logging back on.
            let after = set_privacy_mode(PrivacyMode::PromptsLogged);
            assert_eq!(after, PrivacyMode::Strict);
            assert_eq!(privacy_mode(), PrivacyMode::Strict);
            assert!(!prompt_logging_enabled());
        });
    }

    #[test]
    fn standard_mode_can_still_be_switched() {
        with_mode(PrivacyMode::Standard, || {
            assert_eq!(
                set_privacy_mode(PrivacyMode::PromptsLogged),
                PrivacyMode::PromptsLogged
            );
            assert!(prompt_logging_enabled());
            assert_eq!(
                set_privacy_mode(PrivacyMode::Standard),
                PrivacyMode::Standard
            );
            assert!(!prompt_logging_enabled());
        });
    }

    #[test]
    fn summary_describes_shape_not_content() {
        with_mode(PrivacyMode::Standard, || {
            let messages = Private::new(vec!["a", "b", "c"]);
            assert_eq!(messages.summary(), "<redacted, 3 items>");
        });
    }

    #[test]
    fn char_count_is_not_byte_count() {
        with_mode(PrivacyMode::Standard, || {
            // Four codepoints, twelve bytes. Reporting bytes would leak a
            // little about the script being used.
            let prompt = Private::new(String::from("日本語だ"));
            assert_eq!(prompt.summary(), "<redacted, 4 chars>");
        });
    }
}
