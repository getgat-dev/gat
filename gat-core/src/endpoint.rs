//! [`RemoteUrlTemplate`] stores a remote endpoint before `${VAR}` expansion.
//!
//! Templates may contain credentials. Raw access is explicit, and `Debug`
//! always renders an opaque marker. Presentation code uses
//! `redaction::render_remote_template` for a sanitized URL.
//!
//! The I/O layer expands templates when validating or opening a remote;
//! this type preserves the configured text without resolving it.

use std::fmt;

/// A configured object-storage remote's endpoint, exactly as written in
/// `gat.yaml`/passed to `gat remote add`/`gat remote update` -- still
/// containing any unexpanded `${VAR}` references, and potentially
/// embedding credentials. See the module documentation for why this is
/// not a plain `String`.
#[derive(Clone, PartialEq, Eq)]
pub struct RemoteUrlTemplate(String);

impl RemoteUrlTemplate {
    /// Stores the configured template without copying or expanding it.
    #[must_use]
    pub const fn from_string(value: String) -> Self {
        Self(value)
    }

    /// Borrows the unexpanded template, which may contain credentials.
    /// Callers must redact it before displaying or logging it.
    #[must_use]
    pub fn as_template_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for RemoteUrlTemplate {
    fn from(value: String) -> Self {
        Self::from_string(value)
    }
}

impl From<&str> for RemoteUrlTemplate {
    fn from(value: &str) -> Self {
        Self::from_string(value.to_string())
    }
}

/// Keeps credentials out of debug logs, including enclosing structs' output.
impl fmt::Debug for RemoteUrlTemplate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RemoteUrlTemplate")
            .field(&"<redacted>")
            .finish()
    }
}

/// Persists the original template text without expansion or redaction.
impl serde::Serialize for RemoteUrlTemplate {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0)
    }
}

/// Deserializes via [`RemoteUrlTemplate::from_string`], retaining the
/// deserialized `String`'s own allocation.
impl<'de> serde::Deserialize<'de> for RemoteUrlTemplate {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        String::deserialize(deserializer).map(Self::from_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn as_template_str_returns_the_exact_stored_text() {
        let template = RemoteUrlTemplate::from_string("s3://bucket/${PREFIX}/objects".to_string());
        assert_eq!(template.as_template_str(), "s3://bucket/${PREFIX}/objects");
    }

    #[test]
    fn debug_redacts_embedded_query_secrets() {
        let template =
            RemoteUrlTemplate::from_string("file:///tmp/source?token=SUPER-SECRET".to_string());
        let rendered = format!("{template:?}");
        assert!(!rendered.contains("SUPER-SECRET"), "rendered = {rendered}");
    }

    #[test]
    fn serde_round_trips_the_exact_template_text() {
        let template = RemoteUrlTemplate::from_string("s3://bucket/${PREFIX}".to_string());
        let json = serde_json::to_string(&template).unwrap();
        assert_eq!(json, "\"s3://bucket/${PREFIX}\"");
        let round_tripped: RemoteUrlTemplate = serde_json::from_str(&json).unwrap();
        assert_eq!(round_tripped, template);
    }
}

/// A lexical template token. Debug remains opaque because literals may contain secrets.
#[derive(Clone, PartialEq, Eq)]
pub enum TemplateToken<'a> {
    /// Text used verbatim.
    Literal(&'a str),
    /// `$$`, expanded to one literal dollar.
    EscapedDollar,
    /// A validated environment variable name.
    Reference(&'a str),
}

/// Template syntax errors carry offsets only, never input text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TemplateSyntaxError {
    /// Missing closing brace.
    UnterminatedReference { offset: usize },
    /// Invalid variable name.
    InvalidVariableName { offset: usize },
}

impl fmt::Debug for TemplateToken<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TemplateToken(<redacted>)")
    }
}

/// Tokenizes interpolation syntax without reading the environment or decoding URLs.
/// Invalid syntax fails without exposing any part of the input.
pub fn tokenize_template(input: &str) -> Result<Vec<TemplateToken<'_>>, TemplateSyntaxError> {
    let mut tokens = Vec::new();
    let mut offset = 0;
    while let Some(dollar) = input[offset..].find('$') {
        tokens.push(TemplateToken::Literal(&input[offset..offset + dollar]));
        offset += dollar;
        let rest = &input[offset..];
        if rest.starts_with("$$") {
            tokens.push(TemplateToken::EscapedDollar);
            offset += 2;
        } else if let Some(body) = rest.strip_prefix("${") {
            let close = body
                .find('}')
                .ok_or(TemplateSyntaxError::UnterminatedReference { offset })?;
            let name = &body[..close];
            let mut chars = name.bytes();
            if !chars
                .next()
                .is_some_and(|c| c == b'_' || c.is_ascii_alphabetic())
                || !chars.all(|c| c == b'_' || c.is_ascii_alphanumeric())
            {
                return Err(TemplateSyntaxError::InvalidVariableName { offset });
            }
            tokens.push(TemplateToken::Reference(name));
            offset += close + 3;
        } else {
            tokens.push(TemplateToken::Literal("$"));
            offset += 1;
        }
    }
    tokens.push(TemplateToken::Literal(&input[offset..]));
    Ok(tokens)
}
