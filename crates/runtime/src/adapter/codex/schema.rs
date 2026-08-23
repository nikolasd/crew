//! Schema-compatibility manifest and comparison logic for the Codex
//! `app-server` JSON-RPC protocol.
//!
//! `codex app-server generate-json-schema --out <dir> --experimental` is a
//! real, no-model-call codegen command the installed Codex CLI (0.145.0)
//! exposes: it dumps every JSON-RPC method's request/response/
//! notification shape as JSON Schema files under `<dir>` (a `v1`/`v2`
//! subdirectory per schema-versioned method, plus flat files for
//! unversioned ones). `fixtures/adapters/codex/schema-version.json` is a
//! committed manifest of the subset of that surface this adapter depends
//! on -- captured once against the installed binary, per the plan's Task
//! 4 Step 1. [`verify_against_installed_binary`] regenerates the live
//! schema and asserts the manifest's required methods/fields are still
//! present; it is never a byte-identical diff (which would break on every
//! upstream casing/formatting/doc-comment change).

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodRequirement {
    pub method: String,
    pub family: MethodFamily,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum MethodFamily {
    ClientRequest,
    ServerNotification,
    ServerRequest,
}

impl MethodFamily {
    /// The generated schema file (at the output directory's root) whose
    /// top-level `oneOf` enumerates every method name in this family.
    fn schema_file(self) -> &'static str {
        match self {
            Self::ClientRequest => "ClientRequest.json",
            Self::ServerNotification => "ServerNotification.json",
            Self::ServerRequest => "ServerRequest.json",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequiredFieldSet {
    /// Path, relative to the generated output directory, of the schema
    /// file backing this params/response shape (e.g. `v2/TurnStartParams.json`
    /// or the flat `ExecCommandApprovalParams.json`).
    pub schema_file: String,
    /// Field names this adapter depends on existing in that schema's
    /// `properties` (whether or not the vendor currently marks them
    /// `required` on the wire).
    pub fields: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SchemaManifest {
    pub codex_cli_version: String,
    pub generated_via: String,
    pub required_methods: Vec<MethodRequirement>,
    pub required_field_sets: Vec<RequiredFieldSet>,
}

impl SchemaManifest {
    /// Loads the committed manifest from `path`.
    ///
    /// # Errors
    /// Returns an error if `path` cannot be read or does not parse as a
    /// [`SchemaManifest`].
    pub fn load(path: &Path) -> Result<Self, SchemaCompatibilityError> {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| SchemaCompatibilityError::Read(path.display().to_string(), e))?;
        serde_json::from_str(&raw)
            .map_err(|e| SchemaCompatibilityError::Parse(path.display().to_string(), e))
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SchemaCompatibilityError {
    #[error("failed to run `{0} app-server generate-json-schema`: {1}")]
    Generate(String, std::io::Error),
    #[error("`{0} app-server generate-json-schema` exited with status {1}")]
    GenerateExit(String, std::process::ExitStatus),
    #[error("failed to read manifest file {0}: {1}")]
    Read(String, std::io::Error),
    #[error("failed to parse manifest/schema file {0}: {1}")]
    Parse(String, serde_json::Error),
    #[error("method {method:?} (family {family:?}) is missing from the freshly generated {file}")]
    MissingMethod {
        method: String,
        family: MethodFamily,
        file: String,
    },
    #[error("generated schema file {file} is missing required field {field:?}")]
    MissingField { file: String, field: String },
}

/// Regenerates `codex_bin`'s app-server JSON Schema into a fresh temp
/// directory and checks every method/field `manifest` depends on is still
/// present.
///
/// # Errors
/// Returns [`SchemaCompatibilityError`] if the generator cannot be run,
/// exits non-zero, or the generated schema no longer covers everything
/// `manifest` requires.
pub fn verify_against_installed_binary(
    manifest: &SchemaManifest,
    codex_bin: &str,
) -> Result<(), SchemaCompatibilityError> {
    let dir = std::env::temp_dir().join(format!("crew-codex-schema-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir)
        .map_err(|e| SchemaCompatibilityError::Generate(codex_bin.to_string(), e))?;

    let status = Command::new(codex_bin)
        .args(["app-server", "generate-json-schema", "--out"])
        .arg(&dir)
        .arg("--experimental")
        .status()
        .map_err(|e| SchemaCompatibilityError::Generate(codex_bin.to_string(), e))?;
    if !status.success() {
        return Err(SchemaCompatibilityError::GenerateExit(
            codex_bin.to_string(),
            status,
        ));
    }

    for requirement in &manifest.required_methods {
        let file = requirement.family.schema_file();
        let methods = read_method_enum(&dir.join(file))?;
        if !methods.contains(&requirement.method) {
            return Err(SchemaCompatibilityError::MissingMethod {
                method: requirement.method.clone(),
                family: requirement.family,
                file: file.to_string(),
            });
        }
    }

    for field_set in &manifest.required_field_sets {
        let properties = read_properties(&dir.join(&field_set.schema_file))?;
        for field in &field_set.fields {
            if !properties.contains(field) {
                return Err(SchemaCompatibilityError::MissingField {
                    file: field_set.schema_file.clone(),
                    field: field.clone(),
                });
            }
        }
    }

    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

/// Reads a `ClientRequest.json`/`ServerNotification.json`/`ServerRequest.json`-
/// shaped file (a top-level `oneOf` of `{method: {enum: [name]}, ...}`
/// entries) and collects every method name it enumerates.
fn read_method_enum(path: &Path) -> Result<HashSet<String>, SchemaCompatibilityError> {
    let doc = read_json(path)?;
    let mut methods = HashSet::new();
    if let Some(entries) = doc.get("oneOf").and_then(Value::as_array) {
        for entry in entries {
            if let Some(name) = entry
                .get("properties")
                .and_then(|p| p.get("method"))
                .and_then(|m| m.get("enum"))
                .and_then(Value::as_array)
                .and_then(|a| a.first())
                .and_then(Value::as_str)
            {
                methods.insert(name.to_string());
            }
        }
    }
    Ok(methods)
}

/// Reads a plain object-shaped schema file's top-level `properties` key
/// names.
fn read_properties(path: &Path) -> Result<HashSet<String>, SchemaCompatibilityError> {
    let doc = read_json(path)?;
    Ok(doc
        .get("properties")
        .and_then(Value::as_object)
        .map(|m| m.keys().cloned().collect())
        .unwrap_or_default())
}

fn read_json(path: &Path) -> Result<Value, SchemaCompatibilityError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| SchemaCompatibilityError::Read(path.display().to_string(), e))?;
    serde_json::from_str(&raw)
        .map_err(|e| SchemaCompatibilityError::Parse(path.display().to_string(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_family_maps_to_the_right_generated_file() {
        assert_eq!(
            MethodFamily::ClientRequest.schema_file(),
            "ClientRequest.json"
        );
        assert_eq!(
            MethodFamily::ServerNotification.schema_file(),
            "ServerNotification.json"
        );
        assert_eq!(
            MethodFamily::ServerRequest.schema_file(),
            "ServerRequest.json"
        );
    }
}
