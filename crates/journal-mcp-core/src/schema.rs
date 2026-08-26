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

/// Errors that can occur while parsing or querying a [`ChapterSchema`].
#[derive(Debug, Error)]
pub enum SchemaError {
    /// YAML syntax or structure error.
    #[error("yaml parse error: {0}")]
    Parse(#[from] serde_yaml::Error),

    /// A transition's `requires.sections_present` or `sections_non_empty`
    /// references a section name that is not declared in `sections`.
    /// Used exclusively during `parse_str` validation.
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

    /// No transition exists from `current` state on event `event` at runtime.
    #[error("no transition from state '{current}' on event '{event}'")]
    NoTransition { current: String, event: String },

    /// The named section does not exist in this schema at runtime.
    #[error("section '{section}' not found in schema '{schema_id}'")]
    SectionNotFound { schema_id: String, section: String },
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

// ---------------------------------------------------------------------------
// Hook types
// ---------------------------------------------------------------------------

/// A hook action that can be attached to section append events.
///
/// Currently only `KeywordDetect` is implemented; other variants are reserved
/// for future extensions (ST3 spec: literal `body.contains` only).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HookAction {
    /// Scan the appended body for literal substring matches.
    ///
    /// # Fields
    ///
    /// * `patterns` — list of literal substrings to search for.
    /// * `response` — warning kind string (e.g. `"warn_carryover"`).
    KeywordDetect {
        /// Literal substrings to match against the appended body.
        patterns: Vec<String>,
        /// Warning kind emitted when any pattern matches.
        response: String,
    },
}

/// A hook declaration on a section: fires when a section is appended.
///
/// # Fields
///
/// * `on_append` — the action to execute on each append.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct HookSpec {
    /// Action executed when a section row is appended.
    pub on_append: HookAction,
}

/// A warning emitted by a hook when its condition is satisfied.
///
/// # Fields
///
/// * `kind` — warning category (e.g. `"warn_carryover"`).
/// * `section` — section that triggered the hook.
/// * `hint` — matched patterns joined by `", "`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookWarning {
    /// Warning kind, taken from [`HookAction::KeywordDetect`]'s `response` field.
    pub kind: String,
    /// Name of the section that triggered this warning.
    pub section: String,
    /// Comma-separated list of matched patterns.
    pub hint: String,
}

// ---------------------------------------------------------------------------
// Section spec
// ---------------------------------------------------------------------------

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

    /// Hooks fired when a row is appended to this section.
    ///
    /// Defaults to an empty list for sections that declare no hooks, which
    /// ensures backwards-compatible deserialisation of existing YAML files.
    #[serde(default)]
    pub hooks: Vec<HookSpec>,
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
    #[serde(default)]
    chapter_header: Option<String>,
    #[serde(default)]
    section_header: Option<String>,
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
/// `"<schema_id>-v<version>"` (e.g. `"journal-mcp-canonical-v1"`).  The accessor
/// [`ChapterSchema::schema_id`] returns only the YAML literal value
/// (e.g. `"journal-mcp-canonical"`), **not** the registry key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChapterSchema {
    schema_id: String,
    version: u32,
    states: Vec<StateSpec>,
    transitions: Vec<TransitionSpec>,
    sections: HashMap<String, SectionSpec>,
    section_order: Vec<String>,
    chapter_header: Option<String>,
    section_header: Option<String>,
}

impl ChapterSchema {
    /// Whether any transition requires `section` to be non-empty.
    ///
    /// Used to reject an empty body at append time rather than only at close
    /// time: the close-time check still stands, but discovering "this section
    /// may not be empty" only when closing means the caller learns about it
    /// long after the write that caused it.
    pub fn section_requires_non_empty(&self, section: &str) -> bool {
        self.transitions.iter().any(|t| {
            t.requires
                .as_ref()
                .is_some_and(|r| r.sections_non_empty.iter().any(|s| s == section))
        })
    }

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

        // --- extract render.file_projection fields ---
        let (section_order, chapter_header, section_header) = raw
            .render
            .and_then(|r| r.file_projection)
            .map(|fp| (fp.section_order, fp.chapter_header, fp.section_header))
            .unwrap_or_default();

        Ok(ChapterSchema {
            schema_id,
            version,
            states,
            transitions,
            sections,
            section_order,
            chapter_header,
            section_header,
        })
    }

    /// The YAML literal `schema_id` value (e.g. `"journal-mcp-canonical"`).
    ///
    /// This is **not** the registry lookup key. Use `"journal-mcp-canonical-v1"` to
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

    /// Chapter-level header template from `render.file_projection.chapter_header`.
    ///
    /// Returns `None` for schemas that do not declare a `chapter_header` in
    /// their render block.  The template may contain `{date}` and `{name}`
    /// placeholders.
    pub fn chapter_header(&self) -> Option<&str> {
        self.chapter_header.as_deref()
    }

    /// Section-level header template from `render.file_projection.section_header`.
    ///
    /// Returns `None` for schemas that do not declare a `section_header` in
    /// their render block.  The template may contain a `{section_name}`
    /// placeholder.
    pub fn section_header(&self) -> Option<&str> {
        self.section_header.as_deref()
    }

    /// The id of the initial state, if one is declared.
    pub fn initial_state(&self) -> Option<&str> {
        self.states
            .iter()
            .find(|s| s.initial)
            .map(|s| s.id.as_str())
    }

    // -----------------------------------------------------------------------
    // Runtime helpers (ST3 additions)
    // -----------------------------------------------------------------------

    /// Look up the first matching state-machine transition for `(current, event)`.
    ///
    /// # Arguments
    ///
    /// * `current` — the chapter's current state id.
    /// * `event` — the event name triggering the transition (e.g. `"append_section"`).
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError::NoTransition`] when no transition matches
    /// `from == current && on == event`.
    pub fn transition(&self, current: &str, event: &str) -> Result<&TransitionSpec, SchemaError> {
        self.transitions
            .iter()
            .find(|t| t.from == current && t.on == event)
            .ok_or_else(|| {
                tracing::warn!(
                    target: "journal::schema",
                    current,
                    event,
                    schema_id = %self.schema_id,
                    "transition: no matching transition found"
                );
                SchemaError::NoTransition {
                    current: current.to_owned(),
                    event: event.to_owned(),
                }
            })
    }

    /// Look up a section spec by name.
    ///
    /// # Arguments
    ///
    /// * `name` — section name (e.g. `"Verified"`).
    ///
    /// # Errors
    ///
    /// Returns [`SchemaError::SectionNotFound`] when the section is absent.
    ///
    /// See also [`ChapterSchema::section_requires_non_empty`].
    pub fn section(&self, name: &str) -> Result<&SectionSpec, SchemaError> {
        self.sections.get(name).ok_or_else(|| {
            tracing::warn!(
                target: "journal::schema",
                section = name,
                schema_id = %self.schema_id,
                "section: section not found"
            );
            SchemaError::SectionNotFound {
                schema_id: self.schema_id.clone(),
                section: name.to_owned(),
            }
        })
    }

    /// Execute all hooks declared on a section for an appended body.
    ///
    /// Only [`HookAction::KeywordDetect`] is implemented; it performs literal
    /// substring matching (`body.contains(pattern)`).  Unknown hook variants
    /// and sections without hooks return an empty list.
    ///
    /// # Arguments
    ///
    /// * `section_name` — name of the section being appended.
    /// * `body` — the text content being appended.
    ///
    /// # Returns
    ///
    /// A (possibly empty) list of [`HookWarning`]s, one per hook whose
    /// condition matched.
    pub fn run_hooks(&self, section_name: &str, body: &str) -> Vec<HookWarning> {
        let spec = match self.sections.get(section_name) {
            Some(s) => s,
            None => return Vec::new(),
        };

        let mut warnings = Vec::new();
        for hook in &spec.hooks {
            match &hook.on_append {
                HookAction::KeywordDetect { patterns, response } => {
                    let matched: Vec<&str> = patterns
                        .iter()
                        .filter(|p| body.contains(p.as_str()))
                        .map(String::as_str)
                        .collect();
                    if !matched.is_empty() {
                        warnings.push(HookWarning {
                            kind: response.clone(),
                            section: section_name.to_owned(),
                            hint: matched.join(", "),
                        });
                    }
                }
            }
        }
        warnings
    }
}
