//! Integration tests for `SchemaRegistry` and `ChapterSchema`.
//!
//! Three properties are validated:
//!
//! - **T1** (`test_parse_three_builtins_and_fetch`): `SchemaRegistry::new()`
//!   returns all three built-in schemas without touching the filesystem.
//! - **T2** (`test_unknown_section_rejected`): `ChapterSchema::parse_str` rejects
//!   a YAML whose transition requires a section not declared in `sections`.
//! - **T3** (`test_project_local_overrides_builtin`): A project-local schema
//!   with the same `schema_id` + `version` as a built-in schema shadows the
//!   built-in (L2 overrides L1).

use journal::{ChapterSchema, SchemaError, SchemaRegistry};

// ---------------------------------------------------------------------------
// T1: parse three built-ins and fetch
// ---------------------------------------------------------------------------

/// `SchemaRegistry::new()` must return all three built-in schemas even when no
/// `.journal/schemas/` directory exists anywhere (compile-time embed guarantee).
#[test]
fn test_parse_three_builtins_and_fetch() {
    let registry = SchemaRegistry::new().expect("new should succeed");

    // ytk-canonical-v1
    let ytk = registry
        .get("ytk-canonical-v1")
        .expect("ytk-canonical-v1 must exist");
    assert_eq!(ytk.schema_id(), "ytk-canonical");
    assert_eq!(ytk.version(), 1);
    assert!(ytk.sections().contains_key("Verified"));
    assert!(ytk.sections().contains_key("Not Done"));
    assert!(ytk.sections().contains_key("Issues touched"));
    assert!(ytk.sections().contains_key("Done"));
    assert!(ytk.sections().contains_key("Decided"));
    // section_order must be non-empty for ytk-canonical
    assert!(!ytk.section_order().is_empty());
    assert_eq!(ytk.section_order()[0], "Verified");
    // initial state
    assert_eq!(ytk.initial_state(), Some("open"));

    // madr-v1
    let madr = registry.get("madr-v1").expect("madr-v1 must exist");
    assert_eq!(madr.schema_id(), "madr");
    assert_eq!(madr.version(), 1);
    assert!(madr.sections().contains_key("Context"));
    assert!(madr.sections().contains_key("Decision"));
    // madr has no render block → section_order is empty
    assert!(madr.section_order().is_empty());

    // minimal-v1
    let minimal = registry.get("minimal-v1").expect("minimal-v1 must exist");
    assert_eq!(minimal.schema_id(), "minimal");
    assert_eq!(minimal.version(), 1);
    assert!(minimal.sections().is_empty());
    assert!(minimal.section_order().is_empty());

    // list() must cover all three keys
    let mut ids: Vec<&str> = registry.list();
    ids.sort();
    assert_eq!(ids, vec!["madr-v1", "minimal-v1", "ytk-canonical-v1"]);
}

// ---------------------------------------------------------------------------
// T2: unknown section rejected
// ---------------------------------------------------------------------------

/// A YAML transition that references a section not declared in `sections` must
/// cause `ChapterSchema::parse_str` to return `SchemaError::UnknownSection`.
#[test]
fn test_unknown_section_rejected() {
    let bad_yaml = r#"
schema_id: bad-schema
version: 1
states:
  - id: open
    initial: true
  - id: closed
    terminal: true
transitions:
  - from: open
    to: closed
    on: close_chapter
    requires:
      sections_present: [NonExistentSection]
sections:
  ExistingSection:
    required: true
"#;

    let result = ChapterSchema::parse_str(bad_yaml);
    match result {
        Err(SchemaError::UnknownSection { schema_id, section }) => {
            assert_eq!(schema_id, "bad-schema");
            assert_eq!(section, "NonExistentSection");
        }
        other => panic!("expected UnknownSection error, got: {other:?}"),
    }
}

/// A YAML transition referencing an unknown section in `sections_non_empty`
/// is also rejected.
#[test]
fn test_unknown_section_in_non_empty_rejected() {
    let bad_yaml = r#"
schema_id: bad-schema
version: 1
states:
  - id: open
    initial: true
  - id: closed
    terminal: true
transitions:
  - from: open
    to: closed
    on: close_chapter
    requires:
      sections_non_empty: [GhostSection]
sections:
  RealSection:
    required: true
"#;

    let result = ChapterSchema::parse_str(bad_yaml);
    assert!(
        matches!(result, Err(SchemaError::UnknownSection { .. })),
        "expected UnknownSection, got: {result:?}"
    );
}

// ---------------------------------------------------------------------------
// T3: L2 project-local overrides L1 built-in
// ---------------------------------------------------------------------------

/// A project-local schema file with the same `schema_id` + `version` as a
/// built-in must shadow the built-in (Crux invariant: L2 overrides L1).
///
/// We write a modified `ytk-canonical` schema (description changed) into a
/// tempdir and confirm the registry returns the L2 version, while an
/// unrelated schema (`madr-v1`) is still served from L1.
#[test]
fn test_project_local_overrides_builtin() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::TempDir::new()?;
    let schemas_dir = dir.path().join(".journal").join("schemas");
    std::fs::create_dir_all(&schemas_dir)?;

    // Write an override for ytk-canonical with a changed description.
    let override_yaml = r#"
schema_id: ytk-canonical
version: 1
description: project-local override description
states:
  - id: open
    initial: true
  - id: appending
  - id: closed
    terminal: true
transitions:
  - from: open
    to: appending
    on: append_section
  - from: appending
    to: closed
    on: close_chapter
    requires:
      sections_present: [Verified, Done]
      sections_non_empty: [Verified]
sections:
  Verified:
    required: true
    append_policy: append-only-chain
  Done:
    required: true
    append_policy: append-only-chain
"#;

    std::fs::write(schemas_dir.join("ytk_canonical_v1.yaml"), override_yaml)?;

    let registry = SchemaRegistry::with_project_local(dir.path())?;

    // The L2 schema must be returned for ytk-canonical-v1.
    let ytk = registry
        .get("ytk-canonical-v1")
        .expect("ytk-canonical-v1 must still exist");
    assert_eq!(ytk.schema_id(), "ytk-canonical");
    assert_eq!(ytk.version(), 1);
    // The L2 override has only two sections; the original built-in has nine.
    // If the built-in were returned instead, this assertion would fail.
    assert_eq!(ytk.sections().len(), 2, "L2 schema must have 2 sections");
    assert!(ytk.sections().contains_key("Verified"));
    assert!(ytk.sections().contains_key("Done"));
    assert!(
        !ytk.sections().contains_key("Notes"),
        "L1 'Notes' section must not appear when L2 overrides"
    );

    // madr-v1 is unaffected and still served from L1.
    let madr = registry.get("madr-v1").expect("madr-v1 must still exist");
    assert_eq!(madr.schema_id(), "madr");
    assert!(madr.sections().contains_key("Context"));

    // dir must stay alive until here so the tempdir is not dropped early.
    drop(dir);
    Ok(())
}

/// When the `.journal/schemas/` directory is absent, `with_project_local`
/// still returns all built-in schemas without error.
#[test]
fn test_no_schemas_dir_still_returns_builtins() -> Result<(), Box<dyn std::error::Error>> {
    let dir = tempfile::TempDir::new()?;
    // No .journal/schemas/ created in dir.

    let registry = SchemaRegistry::with_project_local(dir.path())?;

    assert!(registry.get("ytk-canonical-v1").is_some());
    assert!(registry.get("madr-v1").is_some());
    assert!(registry.get("minimal-v1").is_some());

    drop(dir);
    Ok(())
}
