//! Organization-configurable redaction rules: a bounded set of compiled
//! [`regex::Regex`] values, parsed from the `security.patterns` array in an
//! org-level configuration document, plus an optional human-readable `id`
//! extracted from an inline `# comment` after each pattern string.
//!
//! These rules are applied alongside the built-in redaction rules in
//! [`crate::security::redaction::Redactor`]. They are compiled once at
//! startup and reused for every subsequent redaction call.

use regex::Regex;

/// A single org-configured redaction rule: a compiled regex pattern with an
/// optional human-readable identifier.
#[derive(Debug, Clone)]
pub struct OrgRedactionRule {
    /// The rule's human-readable identifier, extracted from an inline `#
    /// comment` after the pattern string (if present), or generated from
    /// the pattern index.
    pub id: String,
    /// The compiled regex pattern.
    pattern: Regex,
}

impl OrgRedactionRule {
    /// Compiles a new [`OrgRedactionRule`] from a pattern string and an
    /// optional identifier.
    ///
    /// # Errors
    ///
    /// Returns an error if the pattern string is not a valid regex.
    pub fn new(id: String, pattern: &str) -> Result<Self, String> {
        let compiled =
            Regex::new(pattern).map_err(|e| format!("invalid regex '{pattern}': {e}"))?;
        Ok(Self {
            id,
            pattern: compiled,
        })
    }

    /// Returns the rule's human-readable identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the compiled regex pattern.
    #[must_use]
    pub fn pattern(&self) -> &Regex {
        &self.pattern
    }

    /// Applies this rule to the given text, returning the redacted text.
    pub fn apply(&self, text: &str) -> String {
        self.pattern
            .replace_all(text, format!("[REDACTED:{}]", self.id).as_str())
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_org_rule_applies_correctly() {
        let rule = OrgRedactionRule::new("test".to_string(), r"AKIA[0-9A-Z]{16}").unwrap();
        let text = "my key is AKIA1234567890ABCDEF end";
        let redacted = rule.apply(text);
        assert_eq!(redacted, "my key is [REDACTED:test] end");
    }

    #[test]
    fn test_org_rule_does_not_match_unrelated_text() {
        let rule = OrgRedactionRule::new("test".to_string(), r"AKIA[0-9A-Z]{16}").unwrap();
        let text = "this is unrelated text";
        let redacted = rule.apply(text);
        assert_eq!(redacted, "this is unrelated text");
    }
}
