//! `SchemaRegistry` — two-layer (L1 built-in / L2 project-local) schema store.
//!
//! # Lookup precedence (Crux invariant)
//!
//! L2 (project-local) schemas always shadow L1 (built-in) schemas for the
//! same registry key. The two layers are kept in separate `HashMap`s; they
//! are **never merged** into a single unordered map.
//!
//! # Zero-config guarantee (Crux invariant)
//!
//! Built-in schemas (`ytk-canonical-v1`, `madr-v1`, `minimal-v1`) are
//! compiled into the binary via `rust-embed`. [`SchemaRegistry::new`] never
//! reads from the filesystem and always succeeds when the binary is correctly
//! built.

use std::collections::HashMap;
use std::path::Path;

use rust_embed::RustEmbed;
use thiserror::Error;

use crate::schema::{ChapterSchema, SchemaError};

// ---------------------------------------------------------------------------
// Compile-time embedded assets
// ---------------------------------------------------------------------------

/// All YAML files under `crates/journal/embed/` are compiled into the binary.
///
/// The folder path is relative to the crate's `Cargo.toml` directory (K-48).
#[derive(RustEmbed)]
#[folder = "embed/"]
struct EmbeddedSchemas;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors returned by [`SchemaRegistry`] constructors.
#[derive(Debug, Error)]
pub enum RegistryError {
    /// A built-in schema file is absent from the embedded binary.
    ///
    /// This should never happen in a correctly built binary; it indicates a
    /// broken `rust-embed` asset configuration.
    #[error("built-in schema '{name}' is missing from the embedded binary")]
    BuiltinMissing { name: String },

    /// A built-in schema embedded at compile time contains invalid UTF-8.
    ///
    /// This should never happen in a correctly built binary; it indicates a
    /// corrupted embedded asset.
    #[error("built-in schema '{name}' contains invalid UTF-8")]
    BuiltinInvalidUtf8 { name: String },

    /// A built-in schema embedded at compile time failed to parse.
    ///
    /// This should never happen in a correctly built binary; it surfaces
    /// YAML typos during development.
    #[error("built-in schema load error: {0}")]
    BuiltinLoad(#[from] SchemaError),

    /// A project-local schema file failed to parse.
    ///
    /// In normal operation this is logged and skipped (silent-skip policy).
    /// The variant is retained for future strict-mode use.
    #[error("project-local schema load error at {path}: {source}")]
    ProjectLocalLoad {
        path: String,
        #[source]
        source: SchemaError,
    },

    /// An I/O error occurred while reading the project-local schema directory.
    ///
    /// In normal operation this is logged and treated as an empty L2 layer.
    #[error("io error reading {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },
}

// ---------------------------------------------------------------------------
// Public type
// ---------------------------------------------------------------------------

/// Two-layer schema registry.
///
/// # Lookup order
///
/// [`get`](SchemaRegistry::get) checks **L2 first**, then falls back to L1.
/// This ensures project-local schemas override built-ins for the same
/// `schema_id` + `version` key.
///
/// # Registry key format
///
/// Keys follow the `"<schema_id>-v<version>"` pattern
/// (e.g. `"ytk-canonical-v1"`).  This is distinct from the YAML
/// `schema_id` field value (e.g. `"ytk-canonical"`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaRegistry {
    /// L1: built-in schemas embedded at compile time.
    l1: HashMap<String, ChapterSchema>,
    /// L2: project-local schemas loaded at runtime from `.journal/schemas/`.
    l2: HashMap<String, ChapterSchema>,
}

impl SchemaRegistry {
    /// Construct a registry with only the built-in schemas (L1).
    ///
    /// This constructor never touches the filesystem and always succeeds for
    /// a correctly built binary.
    pub fn new() -> Result<Self, RegistryError> {
        let l1 = Self::load_builtins()?;
        Ok(SchemaRegistry {
            l1,
            l2: HashMap::new(),
        })
    }

    /// Construct a registry with both built-in (L1) and project-local (L2)
    /// schemas.
    ///
    /// Project-local schemas are loaded from
    /// `<project_root>/.journal/schemas/*.yaml`.  If the directory is absent
    /// or a file fails to parse, a warning is logged and the file is skipped
    /// (the registry remains operational with L1 schemas only).
    pub fn with_project_local(project_root: &Path) -> Result<Self, RegistryError> {
        let l1 = Self::load_builtins()?;
        let l2 = Self::load_project_local(&project_root.join(".journal").join("schemas"));
        Ok(SchemaRegistry { l1, l2 })
    }

    /// Look up a schema by registry key (e.g. `"ytk-canonical-v1"`).
    ///
    /// L2 (project-local) takes precedence over L1 (built-in) for the same
    /// key. Returns `None` if neither layer contains the key.
    pub fn get(&self, schema_id: &str) -> Option<&ChapterSchema> {
        self.l2.get(schema_id).or_else(|| self.l1.get(schema_id))
    }

    /// List all known registry keys (L1 ∪ L2, de-duplicated, L2 wins).
    pub fn list(&self) -> Vec<&str> {
        let mut keys: HashMap<&str, ()> = self.l1.keys().map(|k| (k.as_str(), ())).collect();
        for k in self.l2.keys() {
            keys.insert(k.as_str(), ());
        }
        keys.into_keys().collect()
    }

    /// Load a [`ChapterSchema`] from a YAML literal string into the L2 layer.
    ///
    /// Parses the YAML, derives the registry key (`"<schema_id>-v<version>"`),
    /// and inserts the schema into L2.  If a schema with the same key already
    /// exists in L2 it is **overwritten** (idempotent — same YAML produces the
    /// same key and value, so repeated calls are safe).
    ///
    /// # Arguments
    ///
    /// * `yaml` — a YAML string conforming to the `ChapterSchema` format (see
    ///   `docs/design.md §5`).
    ///
    /// # Returns
    ///
    /// The registry key that was inserted (e.g. `"ytk-canonical-v1"`).
    ///
    /// # Errors
    ///
    /// Returns [`RegistryError::ProjectLocalLoad`] (with `path = "<runtime>"`)
    /// when the YAML cannot be parsed as a valid `ChapterSchema`.
    pub fn load_from_yaml_str(&mut self, yaml: &str) -> Result<String, RegistryError> {
        let schema = ChapterSchema::parse_str(yaml).map_err(|e| {
            tracing::warn!(error = ?e, "load_from_yaml_str: parse failed");
            RegistryError::ProjectLocalLoad {
                path: "<runtime>".to_string(),
                source: e,
            }
        })?;
        let key = format!("{}-v{}", schema.schema_id(), schema.version());
        let overwritten = self.l2.insert(key.clone(), schema).is_some();
        if overwritten {
            tracing::info!(key = %key, "load_from_yaml_str: existing key overwritten (idempotent)");
        }
        Ok(key)
    }

    // -----------------------------------------------------------------------
    // Private helpers
    // -----------------------------------------------------------------------

    /// Load the three built-in schemas from the compile-time embedded bytes.
    fn load_builtins() -> Result<HashMap<String, ChapterSchema>, RegistryError> {
        const BUILTINS: &[&str] = &["ytk_canonical_v1.yaml", "madr_v1.yaml", "minimal_v1.yaml"];

        let mut map = HashMap::new();
        for file_name in BUILTINS {
            let file = match EmbeddedSchemas::get(file_name) {
                Some(f) => f,
                None => {
                    tracing::error!(
                        "built-in schema '{file_name}' is missing from the embedded binary"
                    );
                    return Err(RegistryError::BuiltinMissing {
                        name: file_name.to_string(),
                    });
                }
            };
            let yaml = match std::str::from_utf8(&file.data) {
                Ok(s) => s,
                Err(e) => {
                    tracing::error!("built-in schema '{file_name}' is not valid UTF-8: {e}");
                    return Err(RegistryError::BuiltinInvalidUtf8 {
                        name: file_name.to_string(),
                    });
                }
            };
            let schema = ChapterSchema::parse_str(yaml).map_err(|e| {
                tracing::error!("built-in schema {file_name} parse failed: {e}");
                RegistryError::BuiltinLoad(e)
            })?;
            let key = format!("{}-v{}", schema.schema_id(), schema.version());
            map.insert(key, schema);
        }
        Ok(map)
    }

    /// Load project-local schemas from `<schemas_dir>/*.yaml`.
    ///
    /// Directory absence and per-file parse failures are logged and skipped
    /// (silent-skip policy, Phase 3 design choice 3-A).
    fn load_project_local(schemas_dir: &Path) -> HashMap<String, ChapterSchema> {
        let mut map = HashMap::new();

        if !schemas_dir.exists() {
            tracing::warn!("schema directory absent: {}", schemas_dir.display());
            return map;
        }

        let entries = match std::fs::read_dir(schemas_dir) {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!(
                    "failed to read schema directory {}: {}",
                    schemas_dir.display(),
                    err
                );
                return map;
            }
        };

        for entry_result in entries {
            let entry = match entry_result {
                Ok(e) => e,
                Err(err) => {
                    tracing::warn!("error iterating schema directory: {}", err);
                    continue;
                }
            };

            let path = entry.path();

            // Only process *.yaml files that are regular files (or symlinks to files).
            let is_yaml = path.extension().map(|ext| ext == "yaml").unwrap_or(false);
            let is_file = entry
                .file_type()
                .map(|ft| ft.is_file() || ft.is_symlink())
                .unwrap_or(false);

            if !is_yaml || !is_file {
                continue;
            }

            let yaml = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(err) => {
                    tracing::warn!(
                        "project-local schema read failed at {}: {}",
                        path.display(),
                        err
                    );
                    continue;
                }
            };

            match ChapterSchema::parse_str(&yaml) {
                Ok(schema) => {
                    let key = format!("{}-v{}", schema.schema_id(), schema.version());
                    map.insert(key, schema);
                }
                Err(err) => {
                    tracing::warn!(
                        "project-local schema parse failed at {}: {}",
                        path.display(),
                        err
                    );
                }
            }
        }

        map
    }
}
