//! Single-pass `${VAR}` expansion for `remotes.<name>.url` templates.
//! Query-value substitutions are form-encoded for `OpenDAL`; other substitutions
//! are inserted verbatim. Expansion never evaluates shell expressions.

/// Expansion failures contain only offsets or validated variable names,
/// never raw template text or malformed references that could contain secrets.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InterpolateError {
    /// A `${...}` reference is missing its closing `}`.
    #[error("invalid `${{...}}` interpolation at byte offset {offset}: missing closing `}}`")]
    UnterminatedReference { offset: usize },
    /// A `${...}` reference's body doesn't match the environment
    /// variable name grammar (`[A-Za-z_][A-Za-z0-9_]*`) -- reported by
    /// offset only, since the reference's actual contents may not be a
    /// mistyped name at all (e.g. `${TOKEN=SUPERSECRET}`).
    #[error(
        "invalid `${{...}}` interpolation at byte offset {offset}: reference does not match the \
         expected environment variable name grammar [A-Za-z_][A-Za-z0-9_]*"
    )]
    InvalidVariableName { offset: usize },
    /// A referenced environment variable is not set. `name` alone is
    /// safe to surface: it's validated against the variable-name grammar
    /// before this variant is ever constructed, so it can't itself carry
    /// secret content from elsewhere in the input.
    #[error("environment variable `{name}` is not set")]
    MissingVariable { name: String },
}

pub(crate) type Result<T> = std::result::Result<T, InterpolateError>;

/// Expands `${NAME}` using the process environment. Names must match
/// `[A-Za-z_][A-Za-z0-9_]*`; missing or non-Unicode values are errors.
/// `$$` produces a literal dollar, and bare `$NAME` stays unchanged.
/// Replacement values are never scanned for further references.
pub(super) fn interpolate_env(input: &str) -> Result<String> {
    interpolate_with(input, |name| {
        std::env::var(name).map_err(|_| InterpolateError::MissingVariable {
            name: name.to_string(),
        })
    })
}

/// Injected lookup keeps tests independent of process environment mutations.
fn interpolate_with(input: &str, lookup: impl Fn(&str) -> Result<String>) -> Result<String> {
    use gat_core::endpoint::{TemplateSyntaxError, TemplateToken, tokenize_template};
    let tokens = tokenize_template(input).map_err(|error| match error {
        TemplateSyntaxError::UnterminatedReference { offset } => {
            InterpolateError::UnterminatedReference { offset }
        }
        TemplateSyntaxError::InvalidVariableName { offset } => {
            InterpolateError::InvalidVariableName { offset }
        }
    })?;
    let mut out = String::with_capacity(input.len());
    for token in tokens {
        match token {
            TemplateToken::Literal(text) => out.push_str(text),
            TemplateToken::EscapedDollar => out.push('$'),
            TemplateToken::Reference(name) => {
                let value = lookup(name)?;
                // Only escape a substituted query value. Leave literal URL syntax
                // and whole-URL references for OpenDAL to interpret as before.
                let in_query_value = url::Url::parse(&out).is_ok_and(|url| {
                    url.fragment().is_none()
                        && url
                            .query()
                            .and_then(|query| query.rsplit('&').next())
                            .is_some_and(|pair| pair.contains('='))
                });
                if in_query_value {
                    out.extend(url::form_urlencoded::byte_serialize(value.as_bytes()));
                } else {
                    out.push_str(&value);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map_lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Result<String> {
        move |name: &str| {
            vars.iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| (*value).to_string())
                .ok_or_else(|| InterpolateError::MissingVariable {
                    name: name.to_string(),
                })
        }
    }

    #[test]
    fn whole_value_interpolation() {
        let lookup = map_lookup(&[("FOO", "hello")]);
        assert_eq!(interpolate_with("${FOO}", lookup).unwrap(), "hello");
    }

    #[test]
    fn interpolation_in_host() {
        let lookup = map_lookup(&[("BUCKET", "my-bucket")]);
        assert_eq!(
            interpolate_with("s3://${BUCKET}/foo", lookup).unwrap(),
            "s3://my-bucket/foo"
        );
    }

    #[test]
    fn interpolation_in_path() {
        let lookup = map_lookup(&[("PREFIX", "prefix-value")]);
        assert_eq!(
            interpolate_with("s3://bucket/${PREFIX}/foo", lookup).unwrap(),
            "s3://bucket/prefix-value/foo"
        );
    }

    #[test]
    fn query_substitutions_reach_opendal_credentials_unchanged() {
        use opendal::{Configurator, OperatorUri, services::S3Config};

        for token in [
            "",
            "plain-token",
            "a+b",
            "a%2Fb",
            "a&tail=1",
            "a#tail",
            "a=b",
            "a?b/c",
            "spaces and Unicode: café",
            "${NOT_EXPANDED}",
        ] {
            let expanded = interpolate_with(
                "s3://gat-testing?region=eu-central-1&session_token=${TOKEN}",
                map_lookup(&[("TOKEN", token)]),
            )
            .unwrap();
            let uri = OperatorUri::new(&expanded, []).unwrap();
            let config = S3Config::from_uri(&uri).unwrap();
            assert_eq!(config.bucket, "gat-testing");
            assert_eq!(config.region.as_deref(), Some("eu-central-1"));
            assert_eq!(uri.option("session_token"), Some(token));
            // OpenDAL treats an empty optional credential as unset.
            assert_eq!(
                config.session_token.as_deref(),
                (!token.is_empty()).then_some(token)
            );
            assert_eq!(uri.options().len(), 2);
        }
    }

    #[test]
    fn nested_endpoint_and_sas_token_preserve_their_own_url_syntax() {
        use opendal::{Configurator, OperatorUri, services::AzblobConfig};

        // hygiene-ok: parser-only fixture; no operator or network request is created.
        let endpoint = "https://account.blob.core.windows.net?first=a+b&second=%2F";
        let token = "sv=2026-01-01&sig=a+b%2F&sp=rl";
        let expanded = interpolate_with(
            "azblob://assets/project?endpoint=${ENDPOINT}&sas_token=${TOKEN}",
            map_lookup(&[("ENDPOINT", endpoint), ("TOKEN", token)]),
        )
        .unwrap();
        let uri = OperatorUri::new(&expanded, []).unwrap();
        let config = AzblobConfig::from_uri(&uri).unwrap();
        assert_eq!(config.endpoint.as_deref(), Some(endpoint));
        assert_eq!(config.sas_token.as_deref(), Some(token));
        assert_eq!(config.container, "assets");
        assert_eq!(config.root.as_deref(), Some("project"));
    }

    #[test]
    fn query_encoding_applies_only_to_substituted_values() {
        use opendal::OperatorUri;

        let expanded = interpolate_with(
            "${BASE}?${KEY}=prefix-${TOKEN}-${TOKEN}&literal=a%2Bb&encoded=%24%7BNO_LOOKUP%7D&escaped=$${LITERAL}#${FRAGMENT}",
            map_lookup(&[
                ("BASE", "s3://bucket/nested/path"),
                ("KEY", "session_token"),
                ("TOKEN", "a&b+c%2F"),
                ("FRAGMENT", "fragment+text"),
            ]),
        )
        .unwrap();
        let uri = OperatorUri::new(&expanded, []).unwrap();
        assert_eq!(uri.name(), Some("bucket"));
        assert_eq!(uri.root(), Some("nested/path"));
        assert_eq!(
            uri.option("session_token"),
            Some("prefix-a&b+c%2F-a&b+c%2F")
        );
        assert_eq!(uri.option("literal"), Some("a+b"));
        assert_eq!(uri.option("encoded"), Some("${NO_LOOKUP}"));
        assert_eq!(uri.option("escaped"), Some("${LITERAL}"));
        assert!(expanded.ends_with("#fragment+text"));
    }

    #[test]
    fn multiple_variables() {
        let lookup = map_lookup(&[("A", "aa"), ("B", "bb")]);
        assert_eq!(interpolate_with("${A}/${B}", lookup).unwrap(), "aa/bb");
    }

    #[test]
    fn bare_dollar_name_is_unsupported_and_left_unchanged() {
        let lookup = map_lookup(&[]);
        assert_eq!(interpolate_with("foo-$BAR", lookup).unwrap(), "foo-$BAR");
    }

    #[test]
    fn double_dollar_escapes_to_literal_brace_form() {
        let lookup = map_lookup(&[]);
        assert_eq!(interpolate_with("$${FOO}", lookup).unwrap(), "${FOO}");
    }

    #[test]
    fn missing_variable_is_a_clear_error() {
        let lookup = map_lookup(&[]);
        let err = interpolate_with("${MISSING}", lookup).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("MISSING"), "{msg}");
    }

    #[test]
    fn invalid_name_is_a_syntax_error() {
        let lookup = map_lookup(&[]);
        assert!(interpolate_with("${1BAD}", lookup).is_err());
    }

    #[test]
    fn unterminated_brace_is_a_syntax_error() {
        let lookup = map_lookup(&[]);
        assert!(interpolate_with("${FOO", lookup).is_err());
    }

    #[test]
    fn expansion_is_single_pass_not_recursive() {
        let lookup = map_lookup(&[("A2", "${B2}")]);
        assert_eq!(interpolate_with("${A2}", lookup).unwrap(), "${B2}");
    }

    #[test]
    fn plain_string_without_dollar_is_unchanged() {
        let lookup = map_lookup(&[]);
        assert_eq!(
            interpolate_with("just-a-plain-string", lookup).unwrap(),
            "just-a-plain-string"
        );
    }

    // --- Error messages must never echo the full `input` -----------------
    //
    // `input` can be a remote URL carrying an unrelated secret elsewhere in
    // the same string (e.g. a query-string token); an interpolation error
    // must report only the offending reference/offset, never the raw
    // `input`, or that secret would leak through the error before the URL
    // redaction boundary is ever reached.

    #[test]
    fn invalid_name_error_never_echoes_the_full_input() {
        let lookup = map_lookup(&[]);
        let input = "s3://bucket/x?token=SUPERSECRET&foo=${BAD-NAME}";
        let err = interpolate_with(input, lookup).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("SUPERSECRET"), "{msg}");
        assert!(!msg.contains(input), "{msg}");
        assert!(!msg.contains("BAD-NAME"), "{msg}");
    }

    #[test]
    fn invalid_name_error_never_echoes_the_malformed_reference_contents() {
        // This branch runs specifically because the reference body failed
        // the name grammar, so it can contain arbitrary content up to the
        // next `}` -- not just a mistyped variable name. Make sure such
        // content (here, a secret-shaped value) never surfaces in the
        // error message either.
        let lookup = map_lookup(&[]);
        let input = "s3://bucket/x?foo=${TOKEN=SUPERSECRET}";
        let err = interpolate_with(input, lookup).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("SUPERSECRET"), "{msg}");
        assert!(!msg.contains("TOKEN=SUPERSECRET"), "{msg}");
        assert!(!msg.contains(input), "{msg}");
    }

    #[test]
    fn unterminated_reference_error_never_echoes_the_full_input() {
        let lookup = map_lookup(&[]);
        let input = "s3://bucket/x?token=SUPERSECRET&foo=${UNCLOSED";
        let err = interpolate_with(input, lookup).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("SUPERSECRET"), "{msg}");
        assert!(!msg.contains(input), "{msg}");
    }

    #[test]
    fn missing_variable_error_never_echoes_unrelated_input_around_it() {
        let lookup = map_lookup(&[]);
        let input = "s3://bucket/x?token=SUPERSECRET&foo=${MISSING_VAR}";
        let err = interpolate_with(input, lookup).unwrap_err();
        let msg = err.to_string();
        assert!(!msg.contains("SUPERSECRET"), "{msg}");
        assert!(!msg.contains(input), "{msg}");
        assert!(msg.contains("MISSING_VAR"), "{msg}");
    }

    /// A single test covering the real-environment production wrapper (as
    /// opposed to `interpolate_with`'s map-based lookup used everywhere
    /// else in this module): confirms `interpolate_env` itself reads the
    /// real process environment, with no in-process env mutation at all.
    /// Process-boundary behavior is covered separately from this pure
    /// interpolation test.
    #[test]
    fn interpolate_env_reads_the_real_process_environment() {
        // A variable essentially guaranteed to be set and stable in any
        // environment gat's tests run in, so this needs no `set_var`.
        let path = std::env::var("PATH").expect("PATH must be set to run tests at all");
        assert_eq!(interpolate_env("${PATH}").unwrap(), path);
    }
}
