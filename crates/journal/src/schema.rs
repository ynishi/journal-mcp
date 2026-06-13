//! ChapterSchema — declarative YAML spec for chapter state machines.
//!
//! A `ChapterSchema` describes a chapter's states, transitions, and section
//! policies. Built-in schemas are embedded at compile time via `SchemaRegistry`.

use std::collections::HashMap;

use serde::Deserialize;
use thiserror::Error;

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

/// Errors that can occur while parsing a [`ChapterSchema`] from YAML.
#[derive(Debug, Error)]
pub enum SchemaError {
    /// YAML syntax or structure error.
    #[error("yaml parse error: {0}")]
    Parse(#[from] serde_yaml::Error),

    /// A transition's `requires.sections_present` or `sections_non_empty`
    /// references a section name that is not declared in `sections`.
    #[error("unknown section '{section}' in schema '{schema_id}'")]
    UnknownSection { schema_id: String, section: String },

    /// A required top-level field (`schema_id`, `version`, `states`,
    /// `transitions`, `sections`) is absent or empty.
    #[error("missing required field '{field}' in schema '{schema_id}'")]
    MissingField { schema_id: String, field: String },

    /// The `append_policy` value in a section is not one of the four
    /// recognised values.
    #[error("invalid append_policy '{value}' in section '{section}'")]
    InvalidAppendPolicy { section: String, value: String },
}

// ---------------------------------------------------------------------------
// Leaf types
// ---------------------------------------------------------------------------

/// Allowed values for `sections.<name>.append_policy`.
///
/// Serialised in YAML as kebab-case (e.g. `append-only-chain`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppendPolicy {
    /// Multiple rows allowed; rows are linked by `previous_id` for corrections.
    AppendOnlyChain,
    /// Multiple rows allowed; chronological order, no correction chain.
    AppendOnlyLog,
    /// Only one row allowed; a second append is an error.
    AppendOnce,
    /// One row exists; later replace attempts are blocked.
    ReplaceForbidden,
}

/// Per-section policy declaration parsed from a schema YAML.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct SectionSpec {
    /// Whether this section must be present before `close_chapter`.
    #[serde(default)]
    pub required: bool,

    /// Append policy for rows in this section.
    pub append_policy: Option<AppendPolicy>,

    /// Whether a evidence reference (file:line / URL / hash) is required.
    #[serde(default)]
    pub evidence_required: bool,

    /// Human-readable description of the section's purpose.
    pub description: Option<String>,
}

/// A single state in the chapter state machine.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StateSpec {
    /// Unique state identifier (e.g. `"open"`, `"appending"`, `"closed"`).
    pub id: String,

    /// Whether this is the initial state of a new chapter.
    #[serde(default)]
    pub initial: bool,

    /// Whether this is a terminal (accepting) state.
    #[serde(default)]
    pub terminal: bool,
}

/// The optional `requires` block on a transition.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Default)]
pub struct TransitionRequires {
    /// Section names that must have at least one row appended.
    #[serde(default)]
    pub sections_present: Vec<String>,

    /// Section names that must have at least one non-empty row.
    #[serde(default)]
    pub sections_non_empty: Vec<String>,
}

/// A state-machine transition.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct TransitionSpec {
    /// Source state id.
    pub from: String,
    /// Target state id.
    pub to: String,
    /// Event name that triggers this transition (e.g. `"close_chapter"`).
    pub on: String,
    /// Optional preconditions that must hold before the transition fires.
    #[serde(default)]
    pub requires: Option<TransitionRequires>,
}

// ---------------------------------------------------------------------------
// Raw deserialise intermediary for render block
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, Default)]
struct RawFileProjection {
    #[serde(default)]
    section_order: Vec<String>,
}

#[derive(Debug, Deserialize, Default)]
struct RawRender {
    file_projection: Option<RawFileProjection>,
}

// ---------------------------------------------------------------------------
// Raw intermediate for the whole schema (serde layer)
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawSchema {
    schema_id: Option<String>,
    version: Option<u32>,
    #[serde(default)]
    states: Vec<StateSpec>,
    #[serde(default)]
    transitions: Vec<TransitionSpec>,
    #[serde(default)]
    sections: HashMap<String, SectionSpec>,
    render: Option<RawRender>,
}

// ---------------------------------------------------------------------------
// Public type
// ---------------------------------------------------------------------------

/// Parsed, validated chapter schema.
///
/// # Registry key vs `schema_id`
///
/// The `SchemaRegistry` stores schemas under a composite key of the form
/// `"<schema_id>-v<version>"` (e.g. `"ytk-canonical-v1"`).  The accessor
/// [`ChapterSchema::schema_id`] returns only the YAML literal value
/// (e.g. `"ytk-canonical"`), **not** the registry key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterSchema {
    schema_id: String,
    version: u32,
    states: Vec<StateSpec>,
    transitions: Vec<TransitionSpec>,
    sections: HashMap<String, SectionSpec>,
    section_order: Vec<String>,
}

impl ChapterSchema {
    /// Parse a `ChapterSchema` from a YAML string.
    ///
    /// Returns `Err(SchemaError)` if the YAML is malformed, required fields
    /// are absent, or a transition references an unknown section name.
    pub fn parse_str(yaml: &str) -> Result<Self, SchemaError> {
        let raw: RawSchema = serde_yaml::from_str(yaml)?;

        // --- validate required top-level fields ---
        let schema_id =
            raw.schema_id
                .filter(|s| !s.is_empty())
                .ok_or_else(|| SchemaError::MissingField {
                    schema_id: String::new(),
                    field: "schema_id".into(),
                })?;

        let version = raw.version.ok_or_else(|| SchemaError::MissingField {
            schema_id: schema_id.clone(),
            field: "version".into(),
        })?;

        // states and transitions are allowed to be empty for minimal schemas,
        // but sections must be present (even as an empty mapping).
        let states = raw.states;
        let transitions = raw.transitions;
        let sections = raw.sections;

        // --- validate transition requires against declared sections ---
        for t in &transitions {
            if let Some(req) = &t.requires {
                for name in req
                    .sections_present
                    .iter()
                    .chain(req.sections_non_empty.iter())
                {
                    if !sections.contains_key(name.as_str()) {
                        return Err(SchemaError::UnknownSection {
                            schema_id,
                            section: name.clone(),
                        });
                    }
                }
            }
        }

        // --- extract render.file_projection.section_order ---
        let section_order = raw
            .render
            .and_then(|r| r.file_projection)
            .map(|fp| fp.section_order)
            .unwrap_or_default();

        Ok(ChapterSchema {
            schema_id,
            version,
            states,
            transitions,
            sections,
            section_order,
        })
    }

    /// The YAML literal `schema_id` value (e.g. `"ytk-canonical"`).
    ///
    /// This is **not** the registry lookup key. Use `"ytk-canonical-v1"` to
    /// look up in [`SchemaRegistry`](crate::SchemaRegistry).
    pub fn schema_id(&self) -> &str {
        &self.schema_id
    }

    /// The schema version number.
    pub fn version(&self) -> u32 {
        self.version
    }

    /// The declared states.
    pub fn states(&self) -> &[StateSpec] {
        &self.states
    }

    /// The declared transitions.
    pub fn transitions(&self) -> &[TransitionSpec] {
        &self.transitions
    }

    /// The declared sections, keyed by section name.
    pub fn sections(&self) -> &HashMap<String, SectionSpec> {
        &self.sections
    }

    /// Section render order from `render.file_projection.section_order`.
    ///
    /// Returns an empty slice for schemas that do not declare a render block.
    pub fn section_order(&self) -> &[String] {
        &self.section_order
    }

    /// The id of the initial state, if one is declared.
    pub fn initial_state(&self) -> Option<&str> {
        self.states
            .iter()
            .find(|s| s.initial)
            .map(|s| s.id.as_str())
    }
}
