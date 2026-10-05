//! The catalog file: what this machine has, across restarts.
//!
//! Small enough to read and write whole — a few hundred bytes per model — so
//! there is no database here and no incremental format. What matters instead is
//! that a write can never leave the file half-updated, because a truncated
//! `catalog.json` loses every model a user has installed.
//!
//! Blocking I/O on purpose. The rule this workspace learned the hard way is
//! that *CPU-bound and multi-gigabyte* work must leave the async executor;
//! rewriting a few kilobytes on a model install, which happens once per model,
//! is not that.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::alias::{self, ModelSelector};
use crate::error::CatalogError;
use crate::record::InstalledModel;

/// Bumped only when the on-disk shape changes incompatibly.
const FORMAT_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
struct CatalogFile {
    version: u32,
    models: Vec<InstalledModel>,
}

/// Every model this machine has, keyed by catalog id.
#[derive(Debug)]
pub struct CatalogStore {
    path: PathBuf,
    models: BTreeMap<String, InstalledModel>,
}

impl CatalogStore {
    /// Read the catalog, or start an empty one if the file does not exist yet.
    ///
    /// A file that exists and does not parse is an **error**, never an empty
    /// catalog. Starting empty and then saving would overwrite whatever the
    /// user actually had with nothing at all — the one outcome this file exists
    /// to prevent.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, CatalogError> {
        let path = path.into();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self {
                    path,
                    models: BTreeMap::new(),
                });
            }
            Err(err) => {
                return Err(CatalogError::CatalogUnreadable {
                    path,
                    reason: err.to_string(),
                });
            }
        };

        let parsed: CatalogFile =
            serde_json::from_slice(&bytes).map_err(|err| CatalogError::CatalogUnreadable {
                path: path.clone(),
                reason: err.to_string(),
            })?;

        Ok(Self {
            path,
            models: parsed
                .models
                .into_iter()
                .map(|model| (model.id.clone(), model))
                .collect(),
        })
    }

    /// An in-memory catalog with no file behind it, for tests and dry runs.
    pub fn in_memory() -> Self {
        Self {
            path: PathBuf::new(),
            models: BTreeMap::new(),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every model, in id order.
    pub fn models(&self) -> impl Iterator<Item = &InstalledModel> {
        self.models.values()
    }

    pub fn len(&self) -> usize {
        self.models.len()
    }

    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&InstalledModel> {
        self.models.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut InstalledModel> {
        self.models.get_mut(id)
    }

    /// The model a user named, by its alias or by its catalog id.
    ///
    /// The one lookup every "which model?" question goes through, so the CLI,
    /// the control API and anything added later read a name the same way:
    ///
    /// 1. an alias, ignoring case;
    /// 2. a catalog id, exactly.
    ///
    /// `default` and an empty name are not resolved here. They mean "whatever
    /// is loaded", which is a fact about a running gateway, not about the
    /// catalog — see [`ModelSelector`].
    pub fn resolve(&self, name: &str) -> Option<&InstalledModel> {
        let ModelSelector::Named(name) = ModelSelector::parse(Some(name)) else {
            return None;
        };
        self.by_alias(name).or_else(|| self.models.get(name))
    }

    /// The model holding this alias, ignoring case.
    pub fn by_alias(&self, alias: &str) -> Option<&InstalledModel> {
        self.models.values().find(|model| {
            model
                .alias
                .as_deref()
                .is_some_and(|held| alias::same_name(held, alias))
        })
    }

    /// Whether `raw` may become the alias of `for_id` — or, with `None`, of a
    /// model that is about to be added. Returns the alias trimmed.
    ///
    /// The one check every way of naming a model goes through, so the
    /// selector namespace stays unambiguous: `default`, then aliases, then
    /// canonical ids, and **no name in more than one of them**. An alias may
    /// not equal any model's id in any casing — its own included, which would
    /// be a second spelling of one name — nor another model's alias.
    pub fn check_alias(&self, raw: &str, for_id: Option<&str>) -> Result<String, CatalogError> {
        let alias = alias::validate_alias(raw).map_err(|problem| CatalogError::InvalidAlias {
            alias: raw.to_owned(),
            problem,
        })?;
        if let Some(named) = self
            .models
            .values()
            .find(|model| alias::same_name(&model.id, &alias))
        {
            return Err(CatalogError::AliasIsModelId {
                alias,
                id: named.id.clone(),
            });
        }
        if let Some(owner) = self.models.values().find(|other| {
            Some(other.id.as_str()) != for_id
                && other
                    .alias
                    .as_deref()
                    .is_some_and(|held| alias::same_name(held, &alias))
        }) {
            return Err(CatalogError::AliasInUse {
                alias,
                owner: owner.id.clone(),
            });
        }
        Ok(alias)
    }

    /// Refuse a canonical id that some *other* model already uses as its
    /// alias, ignoring case.
    ///
    /// The other direction of [`Self::check_alias`]. An id this catalog
    /// chooses steps around aliases instead (see [`Self::free_id`]); this is
    /// for the ids it cannot choose — a pinned model's manifest id, a link's
    /// file name — which are refused before any bytes are fetched rather than
    /// installed under a name that already means another model.
    pub fn ensure_id_unaliased(&self, id: &str) -> Result<(), CatalogError> {
        match self.models.values().find(|model| {
            model.id != id
                && model
                    .alias
                    .as_deref()
                    .is_some_and(|held| alias::same_name(held, id))
        }) {
            Some(owner) => Err(CatalogError::ModelIdIsAlias {
                id: id.to_owned(),
                alias: owner.alias.clone().unwrap_or_default(),
                owner: owner.id.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Give a model an alias, change it, or clear it with `None`.
    ///
    /// `id` is the catalog id. Refuses rather than adjusts: an alias that is
    /// invalid, reserved, held by another model, or equal to any model's id is
    /// an error naming the problem (see [`Self::check_alias`]), and the user's
    /// choice is never rewritten into something that happens to be free.
    ///
    /// Touches nothing but the alias. Not saved here — callers save, as they
    /// do after every other change to the store.
    pub fn set_alias(
        &mut self,
        id: &str,
        alias: Option<&str>,
    ) -> Result<&InstalledModel, CatalogError> {
        if !self.models.contains_key(id) {
            return Err(CatalogError::UnknownModel { id: id.to_owned() });
        }

        let alias = match alias {
            None => None,
            Some(raw) => Some(self.check_alias(raw, Some(id))?),
        };

        let model = self
            .models
            .get_mut(id)
            .ok_or_else(|| CatalogError::UnknownModel { id: id.to_owned() })?;
        model.alias = alias;
        Ok(model)
    }

    /// Whether `name` already identifies a model, as an id or as an alias,
    /// ignoring case for both.
    fn is_taken(&self, name: &str) -> bool {
        self.models.keys().any(|id| alias::same_name(id, name)) || self.by_alias(name).is_some()
    }

    /// The model with these exact bytes, if the catalog already has it.
    ///
    /// Lets a second import of the same file be recognised as the same model
    /// rather than installed twice under two names.
    pub fn by_digest(&self, sha256: &str) -> Option<&InstalledModel> {
        self.models
            .values()
            .find(|model| model.sha256.eq_ignore_ascii_case(sha256))
    }

    /// Add a model, refusing to shadow one that is already there.
    pub fn insert(&mut self, model: InstalledModel) -> Result<(), CatalogError> {
        if self.models.contains_key(&model.id) {
            return Err(CatalogError::DuplicateModel { id: model.id });
        }
        self.models.insert(model.id.clone(), model);
        Ok(())
    }

    /// Add a model, replacing any record with the same id.
    ///
    /// Used when a model is re-downloaded: the file is new, so the digest and
    /// size are new, and the old record describes something that is gone.
    pub fn replace(&mut self, model: InstalledModel) -> Option<InstalledModel> {
        self.models.insert(model.id.clone(), model)
    }

    pub fn remove(&mut self, id: &str) -> Result<InstalledModel, CatalogError> {
        self.models
            .remove(id)
            .ok_or_else(|| CatalogError::UnknownModel { id: id.to_owned() })
    }

    /// An id that is not taken, derived from `base`.
    ///
    /// Two different files with the same name is an ordinary thing — the same
    /// model at two quantizations, or a re-download alongside the original — so
    /// it gets a suffix rather than a refusal.
    ///
    /// An id is ours to choose, so it also steps around every alias: a new
    /// model whose id equalled someone's alias would make that name mean two
    /// models.
    pub fn free_id(&self, base: &str) -> String {
        if !self.is_taken(base) {
            return base.to_owned();
        }
        (2u32..)
            .map(|n| format!("{base}-{n}"))
            .find(|candidate| !self.is_taken(candidate))
            .unwrap_or_else(|| base.to_owned())
    }

    /// Write the catalog out atomically.
    ///
    /// Temp file plus rename: on every platform this workspace targets, a
    /// rename over an existing file is atomic, so a crash mid-write leaves
    /// either the old catalog or the new one and never a half of each.
    pub fn save(&self) -> Result<(), CatalogError> {
        if self.path.as_os_str().is_empty() {
            return Ok(());
        }
        if let Some(parent) = self.path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|err| CatalogError::io("creating the catalog directory", err))?;
        }

        let file = CatalogFile {
            version: FORMAT_VERSION,
            models: self.models.values().cloned().collect(),
        };
        let mut bytes = serde_json::to_vec_pretty(&file)
            .map_err(|err| CatalogError::io("encoding the catalog", std::io::Error::other(err)))?;
        bytes.push(b'\n');

        let temporary = crate::record::temp_sibling(&self.path);
        std::fs::write(&temporary, &bytes)
            .map_err(|err| CatalogError::io("writing the catalog", err))?;
        std::fs::rename(&temporary, &self.path).map_err(|err| {
            // Leaving the temp file behind would accumulate one per failed
            // save, and none of them is the catalog.
            let _ = std::fs::remove_file(&temporary);
            CatalogError::io("replacing the catalog", err)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Integrity, Source};
    use lightweight_core::Actionable;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            // The clock alone is not unique: on a coarse timer two tests running in
            // parallel are handed the same name. The counter and the pid settle it.
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let sequence = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hermes-catalog-{tag}-{}-{unique}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("temp dir");
            Self(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn model(id: &str, sha: &str) -> InstalledModel {
        InstalledModel {
            id: id.to_owned(),
            alias: None,
            name: id.to_owned(),
            path: PathBuf::from(format!("/models/{id}.gguf")),
            bytes: 10,
            sha256: sha.to_owned(),
            integrity: Integrity::Imported,
            source: Source::Import {
                original_path: PathBuf::from("/elsewhere.gguf"),
            },
            architecture: "llama".into(),
            supported: true,
            param_count: None,
            quantization: None,
            context_length: Some(4096),
            weight_bytes: None,
            added_at: 1,
            last_loaded_at: None,
            last_n_ctx: None,
        }
    }

    #[test]
    fn a_first_run_starts_empty_rather_than_failing() {
        let temp = TempDir::new("first");
        let store = CatalogStore::open(temp.0.join("catalog.json")).expect("open");
        assert!(store.is_empty());
    }

    #[test]
    fn models_survive_a_save_and_reopen() {
        let temp = TempDir::new("roundtrip");
        let path = temp.0.join("catalog.json");

        let mut store = CatalogStore::open(&path).expect("open");
        store.insert(model("qwen3", "aa")).expect("insert");
        store.insert(model("smollm2", "bb")).expect("insert");
        store.save().expect("save");

        let reopened = CatalogStore::open(&path).expect("reopen");
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get("qwen3").map(|m| m.sha256.as_str()), Some("aa"));
        // Ordered by id, so a listing is stable between runs.
        let ids: Vec<&str> = reopened.models().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["qwen3", "smollm2"]);
    }

    #[test]
    fn an_unreadable_catalog_is_an_error_rather_than_an_empty_one() {
        // The failure this prevents: parse fails, we start empty, the next
        // save overwrites the user's real catalog with nothing.
        let temp = TempDir::new("corrupt");
        let path = temp.0.join("catalog.json");
        std::fs::write(&path, b"{ not json").expect("write");

        let err = CatalogStore::open(&path).expect_err("must not be treated as empty");
        assert_eq!(err.code(), "catalog_unreadable");
    }

    #[test]
    fn a_saved_catalog_is_replaced_whole_and_leaves_no_temp_file_behind() {
        let temp = TempDir::new("atomic");
        let path = temp.0.join("catalog.json");

        let mut store = CatalogStore::open(&path).expect("open");
        store.insert(model("a", "aa")).expect("insert");
        store.save().expect("save");
        store.insert(model("b", "bb")).expect("insert");
        store.save().expect("save again");

        let entries: Vec<_> = std::fs::read_dir(&temp.0)
            .expect("read dir")
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(entries, vec!["catalog.json".to_owned()], "{entries:?}");
    }

    #[test]
    fn the_same_id_twice_is_refused_and_a_free_id_is_offered() {
        let mut store = CatalogStore::in_memory();
        store.insert(model("qwen3", "aa")).expect("insert");

        let err = store.insert(model("qwen3", "cc")).expect_err("duplicate");
        assert_eq!(err.code(), "duplicate_model");

        assert_eq!(store.free_id("qwen3"), "qwen3-2");
        assert_eq!(store.free_id("other"), "other");
    }

    #[test]
    fn the_same_file_imported_twice_is_found_by_its_digest() {
        // Otherwise a user who imports the same file from two paths ends up
        // with two catalog entries pointing at identical bytes.
        let mut store = CatalogStore::in_memory();
        store.insert(model("qwen3", "abc123")).expect("insert");
        assert_eq!(
            store.by_digest("ABC123").map(|m| m.id.as_str()),
            Some("qwen3")
        );
        assert!(store.by_digest("nope").is_none());
    }

    #[test]
    fn removing_a_model_that_is_not_there_says_so() {
        let mut store = CatalogStore::in_memory();
        let err = store.remove("ghost").expect_err("unknown");
        assert_eq!(err.code(), "unknown_model");
    }

    fn two_models() -> CatalogStore {
        let mut store = CatalogStore::in_memory();
        store
            .insert(model("qwen3.5-9b-fable-5-v1-q8_0", "aa"))
            .expect("insert");
        store.insert(model("lfm2.5-1.2b", "bb")).expect("insert");
        store
    }

    #[test]
    fn an_alias_resolves_to_its_model_and_the_id_still_does_too() {
        let mut store = two_models();
        store
            .set_alias("qwen3.5-9b-fable-5-v1-q8_0", Some("Coder"))
            .expect("alias");

        let by_alias = store.resolve("Coder").map(|m| m.id.as_str());
        let by_id = store
            .resolve("qwen3.5-9b-fable-5-v1-q8_0")
            .map(|m| m.id.as_str());
        assert_eq!(by_alias, Some("qwen3.5-9b-fable-5-v1-q8_0"));
        assert_eq!(by_alias, by_id);
        // Any casing of the alias, and surrounding whitespace, reach it.
        assert_eq!(
            store.resolve(" coder ").map(|m| m.id.as_str()),
            Some("qwen3.5-9b-fable-5-v1-q8_0")
        );
        // The display casing is the one the user typed.
        assert_eq!(
            store
                .get("qwen3.5-9b-fable-5-v1-q8_0")
                .and_then(|m| m.alias.as_deref()),
            Some("Coder")
        );
        assert!(store.resolve("Programming").is_none());
    }

    #[test]
    fn default_is_never_resolved_against_the_catalog() {
        // It means "whatever is loaded", which only a running gateway knows.
        let store = two_models();
        assert!(store.resolve("default").is_none());
        assert!(store.resolve("").is_none());
    }

    #[test]
    fn an_alias_held_by_another_model_is_refused_in_any_casing() {
        let mut store = two_models();
        store
            .set_alias("qwen3.5-9b-fable-5-v1-q8_0", Some("Coder"))
            .expect("alias");
        for clash in ["Coder", "coder", "CODER", "  Coder  "] {
            let err = store
                .set_alias("lfm2.5-1.2b", Some(clash))
                .expect_err("a second model may not take the same alias");
            assert_eq!(err.code(), "alias_in_use", "{clash}");
        }
        // Refused, not adjusted: nothing was written to the second model.
        assert_eq!(store.get("lfm2.5-1.2b").and_then(|m| m.alias.clone()), None);
    }

    #[test]
    fn a_model_may_restate_or_recase_its_own_alias() {
        let mut store = two_models();
        store.set_alias("lfm2.5-1.2b", Some("Fast")).expect("alias");
        store
            .set_alias("lfm2.5-1.2b", Some("FAST"))
            .expect("its own alias is not a clash with itself");
        assert_eq!(
            store.get("lfm2.5-1.2b").and_then(|m| m.alias.as_deref()),
            Some("FAST")
        );
    }

    #[test]
    fn an_alias_may_not_be_another_models_id() {
        // Otherwise that id would name two models.
        let mut store = two_models();
        let err = store
            .set_alias("lfm2.5-1.2b", Some("QWEN3.5-9B-FABLE-5-V1-Q8_0"))
            .expect_err("another model's id");
        assert_eq!(err.code(), "alias_is_model_id");
        // Nor its own id, in any casing: that would be one name spelled twice.
        let err = store
            .set_alias("lfm2.5-1.2b", Some("LFM2.5-1.2B"))
            .expect_err("its own id");
        assert_eq!(err.code(), "alias_is_model_id");
    }

    #[test]
    fn an_id_that_is_already_an_alias_is_refused_in_any_casing() {
        // Alias first, then an install whose id is fixed (pinned, or a link's
        // file name) and would collide with it.
        let mut store = two_models();
        store
            .set_alias("qwen3.5-9b-fable-5-v1-q8_0", Some("Coder"))
            .expect("alias");
        for clash in ["coder", "Coder", "CODER"] {
            let err = store
                .ensure_id_unaliased(clash)
                .expect_err("shadows an alias");
            assert_eq!(err.code(), "model_id_is_alias", "{clash}");
            let said = err.to_string();
            assert!(
                said.contains("Coder") && said.contains("qwen3.5-9b-fable-5-v1-q8_0"),
                "{said}"
            );
        }
        // A model is never in conflict with its own record, and an unrelated
        // id is free.
        store
            .ensure_id_unaliased("qwen3.5-9b-fable-5-v1-q8_0")
            .expect("its own id");
        store
            .ensure_id_unaliased("research")
            .expect("an unrelated id");
    }

    #[test]
    fn a_generated_id_steps_around_an_alias_in_any_casing() {
        // The import case: the catalog chooses the id, so it chooses another.
        let mut store = two_models();
        store
            .set_alias("qwen3.5-9b-fable-5-v1-q8_0", Some("CODER"))
            .expect("alias");
        assert_eq!(store.free_id("coder"), "coder-2");
        assert_eq!(store.free_id("research"), "research");
    }

    #[test]
    fn a_new_alias_is_checked_against_every_name_already_in_use() {
        let mut store = two_models();
        store.set_alias("lfm2.5-1.2b", Some("Fast")).expect("alias");
        assert_eq!(
            store.check_alias(" Coder ", None).as_deref().ok(),
            Some("Coder")
        );
        assert_eq!(
            store.check_alias("FAST", None).map_err(|e| e.code()),
            Err("alias_in_use")
        );
        assert_eq!(
            store.check_alias("Lfm2.5-1.2B", None).map_err(|e| e.code()),
            Err("alias_is_model_id")
        );
        assert_eq!(
            store.check_alias("default", None).map_err(|e| e.code()),
            Err("invalid_alias")
        );
    }

    #[test]
    fn an_invalid_or_reserved_alias_is_refused_before_anything_changes() {
        let mut store = two_models();
        for bad in ["default", "", "   ", "../model", "foo/bar", "foo\\bar"] {
            let err = store
                .set_alias("lfm2.5-1.2b", Some(bad))
                .expect_err("invalid alias");
            assert_eq!(err.code(), "invalid_alias", "{bad:?}");
        }
        assert_eq!(store.get("lfm2.5-1.2b").and_then(|m| m.alias.clone()), None);
    }

    #[test]
    fn renaming_an_alias_retires_the_old_name() {
        let mut store = two_models();
        let id = "qwen3.5-9b-fable-5-v1-q8_0";
        store.set_alias(id, Some("Coder")).expect("alias");
        store.set_alias(id, Some("Programming")).expect("rename");

        assert_eq!(
            store.resolve("Programming").map(|m| m.id.as_str()),
            Some(id)
        );
        assert!(store.resolve("Coder").is_none(), "no alias history is kept");
        assert_eq!(store.resolve(id).map(|m| m.id.as_str()), Some(id));
        // The old name is free for another model now.
        store
            .set_alias("lfm2.5-1.2b", Some("Coder"))
            .expect("a retired alias can be reused");
    }

    #[test]
    fn clearing_an_alias_leaves_the_model_reachable_by_id() {
        let mut store = two_models();
        store.set_alias("lfm2.5-1.2b", Some("Fast")).expect("alias");
        store.set_alias("lfm2.5-1.2b", None).expect("clear");
        assert!(store.resolve("Fast").is_none());
        assert!(store.resolve("lfm2.5-1.2b").is_some());
    }

    #[test]
    fn an_alias_on_a_model_that_is_not_there_says_so() {
        let mut store = two_models();
        let err = store
            .set_alias("ghost", Some("Coder"))
            .expect_err("unknown");
        assert_eq!(err.code(), "unknown_model");
    }

    #[test]
    fn removing_a_model_takes_its_alias_with_it() {
        let mut store = two_models();
        store.set_alias("lfm2.5-1.2b", Some("Fast")).expect("alias");
        store.remove("lfm2.5-1.2b").expect("remove");
        assert!(store.resolve("Fast").is_none());
        assert!(store.by_alias("Fast").is_none());
    }

    #[test]
    fn a_new_id_steps_around_an_alias_rather_than_shadowing_it() {
        let mut store = two_models();
        store
            .set_alias("lfm2.5-1.2b", Some("Qwen3"))
            .expect("alias");
        assert_eq!(store.free_id("qwen3"), "qwen3-2");
    }

    #[test]
    fn an_alias_survives_a_save_and_reopen() {
        let temp = TempDir::new("alias");
        let path = temp.0.join("catalog.json");

        let mut store = CatalogStore::open(&path).expect("open");
        store.insert(model("qwen3", "aa")).expect("insert");
        store.set_alias("qwen3", Some("Coder")).expect("alias");
        store.save().expect("save");

        let reopened = CatalogStore::open(&path).expect("reopen");
        assert_eq!(
            reopened.resolve("Coder").map(|m| m.id.as_str()),
            Some("qwen3")
        );
        assert_eq!(reopened.get("qwen3").map(|m| m.sha256.as_str()), Some("aa"));
    }

    #[test]
    fn a_catalog_written_before_aliases_loads_and_can_be_given_one() {
        // The shape every existing installation has on disk: no alias field.
        let temp = TempDir::new("legacy");
        let path = temp.0.join("catalog.json");
        std::fs::write(
            &path,
            br#"{"version":1,"models":[{
                "id":"qwen3-8b-q4_k_m","name":"Qwen3 8B","path":"/models/Qwen3-8B-Q4_K_M.gguf",
                "bytes":5000,"sha256":"ab12","integrity":"imported",
                "source":{"kind":"import","original_path":"/models/Qwen3-8B-Q4_K_M.gguf"},
                "architecture":"qwen3","supported":true,"context_length":40960,
                "added_at":1700000000,"last_loaded_at":1700000100,"last_n_ctx":8192}]}"#,
        )
        .expect("write legacy catalog");

        let mut store = CatalogStore::open(&path).expect("a legacy catalog must load");
        let legacy = store
            .get("qwen3-8b-q4_k_m")
            .expect("the model is still there");
        assert_eq!(legacy.alias, None, "no alias is invented for an old record");
        assert_eq!(legacy.last_n_ctx, Some(8192));
        assert!(store.resolve("qwen3-8b-q4_k_m").is_some());

        store
            .set_alias("qwen3-8b-q4_k_m", Some("Coder"))
            .expect("an old record can be given an alias");
        store.save().expect("save");

        let reopened = CatalogStore::open(&path).expect("reopen");
        let model = reopened.resolve("Coder").expect("the alias persisted");
        assert_eq!(model.id, "qwen3-8b-q4_k_m");
        assert_eq!(model.sha256, "ab12");
        assert_eq!(model.context_length, Some(40960));
        assert_eq!(model.last_loaded_at, Some(1_700_000_100));
    }

    #[test]
    fn an_in_memory_catalog_saves_nowhere_rather_than_to_the_working_directory() {
        let store = CatalogStore::in_memory();
        store.save().expect("a save with no path is a no-op");
        assert!(store.path().as_os_str().is_empty());
    }
}
