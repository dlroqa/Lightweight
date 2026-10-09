//! Where a Jev key saved from the panel is kept: the operating system's own
//! credential store, and nowhere else.
//!
//! * macOS: the login Keychain.
//! * Windows: Credential Manager.
//! * Linux: the Secret Service (GNOME Keyring, KWallet) over D-Bus.
//!
//! There is deliberately no file fallback. A machine whose router cannot reach
//! one of these — a headless server, a service account with no session bus,
//! the Flatpak build (which is given no access to the Secret Service) — keeps
//! the environment variable `api_key_env` names, exactly as before, and the
//! panel says so instead of saving.
//!
//! The environment variable always wins: a router started with
//! `TYPESAFE_API_KEY` set uses it whatever the store holds, so every existing
//! deployment behaves as it did.
//!
//! Every call here blocks (the Secret Service backend runs its own small
//! runtime), so the router calls it before its runtime starts or from
//! `spawn_blocking`, never from a task. No message produced here carries a
//! secret: platform errors are reduced to a fixed sentence per kind, because
//! some of them carry the bytes that failed to decode.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Arc, Mutex};

use crate::domain::Secret;

/// The service name every entry is filed under.
pub const SERVICE: &str = "lightweight-router";

/// The account a Jev key is filed under: one per environment variable name,
/// so a key saved for `TYPESAFE_API_KEY` is the one that variable would have
/// held.
pub fn jev_account(api_key_env: &str) -> String {
    format!("jev/{api_key_env}")
}

/// Why the store could not do what was asked. Never carries a secret.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoreFailure {
    /// There is no credential store this process can use.
    Unavailable(String),
    /// The store is there but refused or failed this operation.
    Failed(String),
}

impl StoreFailure {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "credential_store_unavailable",
            Self::Failed(_) => "credential_store_failed",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Unavailable(message) | Self::Failed(message) => message,
        }
    }
}

impl fmt::Display for StoreFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

/// A place a secret can be kept and read back by this user.
pub trait SecretStore: Send + Sync {
    /// Which store this is, for the panel: never a path or an account.
    fn backend(&self) -> &'static str;
    /// The secret filed under `account`, or `None` when there is none.
    fn get(&self, account: &str) -> Result<Option<Secret>, StoreFailure>;
    fn set(&self, account: &str, value: &str) -> Result<(), StoreFailure>;
    /// Remove the entry. Removing one that is not there succeeds.
    fn delete(&self, account: &str) -> Result<(), StoreFailure>;
}

/// The operating system's credential store, or the reason there is none.
pub fn os_store() -> Arc<dyn SecretStore> {
    // Debug builds only — never a release artifact: the panel render runs a
    // debug router on CI machines with no credential store, and needs one
    // that is there (`memory`) or reliably not (`unavailable`).
    #[cfg(debug_assertions)]
    match std::env::var("LIGHTWEIGHT_ROUTER_TEST_SECRET_STORE").as_deref() {
        Ok("memory") => return Arc::new(MemoryStore::default()),
        Ok("unavailable") => {
            return Arc::new(NoStore {
                reason: "the test credential store is unavailable",
            });
        }
        _ => {}
    }
    if std::env::var_os("FLATPAK_ID").is_some() {
        return Arc::new(NoStore {
            reason: "this Flatpak build has no access to a credential store",
        });
    }
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
    {
        Arc::new(OsStore {
            service: SERVICE.to_owned(),
        })
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows", target_os = "linux")))]
    {
        Arc::new(NoStore {
            reason: "this platform has no supported credential store",
        })
    }
}

/// A store under a service name of the caller's choosing, so a test against
/// the real credential store never touches the router's own entries.
#[doc(hidden)]
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
pub fn os_store_for_service(service: &str) -> Arc<dyn SecretStore> {
    Arc::new(OsStore {
        service: service.to_owned(),
    })
}

/// A machine with no usable store. Every operation says why.
struct NoStore {
    reason: &'static str,
}

impl SecretStore for NoStore {
    fn backend(&self) -> &'static str {
        "none"
    }
    fn get(&self, _: &str) -> Result<Option<Secret>, StoreFailure> {
        Err(StoreFailure::Unavailable(self.reason.to_owned()))
    }
    fn set(&self, _: &str, _: &str) -> Result<(), StoreFailure> {
        Err(StoreFailure::Unavailable(self.reason.to_owned()))
    }
    fn delete(&self, _: &str) -> Result<(), StoreFailure> {
        Err(StoreFailure::Unavailable(self.reason.to_owned()))
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
struct OsStore {
    service: String,
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
impl OsStore {
    /// The platform's store, built per call: on Linux that is a session-bus
    /// connection, which a long-lived router must not hold open for a panel
    /// that is rarely used.
    fn store() -> Result<Arc<keyring_core::CredentialStore>, StoreFailure> {
        #[cfg(target_os = "macos")]
        let built = apple_native_keyring_store::keychain::Store::new()
            .map(|store| store as Arc<keyring_core::CredentialStore>);
        #[cfg(target_os = "windows")]
        let built = windows_native_keyring_store::Store::new()
            .map(|store| store as Arc<keyring_core::CredentialStore>);
        #[cfg(target_os = "linux")]
        let built = zbus_secret_service_keyring_store::Store::new()
            .map(|store| store as Arc<keyring_core::CredentialStore>);
        built.map_err(|err| match describe(&err) {
            StoreFailure::Failed(message) => StoreFailure::Unavailable(message),
            unavailable => unavailable,
        })
    }

    fn entry(&self, account: &str) -> Result<keyring_core::Entry, StoreFailure> {
        Self::store()?
            .build(&self.service, account, None)
            .map_err(|err| describe(&err))
    }
}

#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
impl SecretStore for OsStore {
    fn backend(&self) -> &'static str {
        if cfg!(target_os = "macos") {
            "macos-keychain"
        } else if cfg!(target_os = "windows") {
            "windows-credential-manager"
        } else {
            "secret-service"
        }
    }

    fn get(&self, account: &str) -> Result<Option<Secret>, StoreFailure> {
        match self.entry(account)?.get_password() {
            Ok(value) => {
                let value = value.trim().to_owned();
                Ok((!value.is_empty()).then(|| Secret::new(value)))
            }
            Err(keyring_core::Error::NoEntry) => Ok(None),
            Err(err) => Err(describe(&err)),
        }
    }

    fn set(&self, account: &str, value: &str) -> Result<(), StoreFailure> {
        self.entry(account)?
            .set_password(value)
            .map_err(|err| describe(&err))
    }

    fn delete(&self, account: &str) -> Result<(), StoreFailure> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(err) => Err(describe(&err)),
        }
    }
}

/// A platform error as a sentence that cannot carry a secret: the variants
/// holding bytes (`BadEncoding`, `BadDataFormat`) are named, never printed.
#[cfg(any(target_os = "macos", target_os = "windows", target_os = "linux"))]
fn describe(err: &keyring_core::Error) -> StoreFailure {
    use keyring_core::Error;
    match err {
        Error::NoStorageAccess(_) | Error::NoDefaultStore => StoreFailure::Unavailable(
            "the credential store is not reachable or is locked for this user".to_owned(),
        ),
        Error::PlatformFailure(_) => {
            StoreFailure::Failed("the credential store reported a platform failure".to_owned())
        }
        Error::BadEncoding(_) | Error::BadDataFormat(..) => {
            StoreFailure::Failed("the stored value could not be read back".to_owned())
        }
        Error::Ambiguous(_) => StoreFailure::Failed(
            "the credential store holds more than one matching entry".to_owned(),
        ),
        Error::TooLong(..) | Error::Invalid(..) => {
            StoreFailure::Failed("the credential store refused the value".to_owned())
        }
        _ => StoreFailure::Failed("the credential store failed".to_owned()),
    }
}

/// An in-memory store for tests: the same contract, a process-local map, and a
/// switch to make it unavailable.
#[doc(hidden)]
#[derive(Default)]
pub struct MemoryStore {
    entries: Mutex<HashMap<String, String>>,
    unavailable: std::sync::atomic::AtomicBool,
    failing_writes: std::sync::atomic::AtomicBool,
}

impl MemoryStore {
    pub fn unavailable() -> Self {
        let store = Self::default();
        store.set_unavailable(true);
        store
    }

    pub fn set_unavailable(&self, unavailable: bool) {
        self.unavailable
            .store(unavailable, std::sync::atomic::Ordering::SeqCst);
    }

    /// Make every `set` and `delete` fail while reads still work.
    pub fn set_failing_writes(&self, failing: bool) {
        self.failing_writes
            .store(failing, std::sync::atomic::Ordering::SeqCst);
    }

    /// What is held, for a test's assertions.
    pub fn peek(&self, account: &str) -> Option<String> {
        self.lock().get(account).cloned()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, String>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn check(&self, write: bool) -> Result<(), StoreFailure> {
        if self.unavailable.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(StoreFailure::Unavailable(
                "the test store is unavailable".to_owned(),
            ));
        }
        if write
            && self
                .failing_writes
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(StoreFailure::Failed("the test store failed".to_owned()));
        }
        Ok(())
    }
}

impl SecretStore for MemoryStore {
    fn backend(&self) -> &'static str {
        "memory"
    }
    fn get(&self, account: &str) -> Result<Option<Secret>, StoreFailure> {
        self.check(false)?;
        Ok(self.lock().get(account).cloned().map(Secret::new))
    }
    fn set(&self, account: &str, value: &str) -> Result<(), StoreFailure> {
        self.check(true)?;
        self.lock().insert(account.to_owned(), value.to_owned());
        Ok(())
    }
    fn delete(&self, account: &str) -> Result<(), StoreFailure> {
        self.check(true)?;
        self.lock().remove(account);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_memory_store_keeps_replaces_and_forgets() {
        let store = MemoryStore::default();
        let account = jev_account("TYPESAFE_API_KEY");
        assert_eq!(account, "jev/TYPESAFE_API_KEY");
        assert!(store.get(&account).unwrap().is_none());
        store.set(&account, "first").unwrap();
        store.set(&account, "second").unwrap();
        assert_eq!(store.get(&account).unwrap().unwrap().expose(), "second");
        store.delete(&account).unwrap();
        store.delete(&account).unwrap();
        assert!(store.get(&account).unwrap().is_none());
    }

    #[test]
    fn an_unavailable_store_says_so_without_a_value() {
        let store = MemoryStore::unavailable();
        let failure = store.set("jev/X", "value").unwrap_err();
        assert_eq!(failure.code(), "credential_store_unavailable");
        assert!(!failure.message().contains("value"));
    }
}
