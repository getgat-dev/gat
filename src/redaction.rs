//! User-facing diagnostic redaction policy: renders remote URLs for
//! humans (errors, `gat remote` output, verbose logs, config display, ...)
//! without ever leaking userinfo or query-string secrets. This policy is
//! fail-closed: input that can't be proven safe (either by successfully
//! parsing as a URL, or by being clearly free of URL-shaped/secret-bearing
//! structure) is never echoed back verbatim.
//!
//! Deliberately a crate-root module, not nested under [`crate::output`]:
//! Git-location and remote diagnostics need a shared presentation policy
//! without moving credential-bearing display logic into lower crates.
//! `redaction` has no dependency on `output`, `error`, or any rendering
//! type -- it only turns a raw `&str` into an already-redacted
//! [`RedactedUrl`].

use std::fmt;

/// An already-redacted remote URL, safe to embed in any user-facing
/// diagnostic, log line, or `gat remote`/`gat mount` display. Unlike a
/// plain `String`, a `RedactedUrl` can only be constructed via
/// [`RedactedUrl::render`] or [`render_remote_template`], so a caller can
/// never accidentally attach an un-redacted raw URL where a `RedactedUrl`
/// is expected -- the type itself is the guarantee that userinfo and query
/// values have already been stripped/replaced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RedactedUrl(String);

impl RedactedUrl {
    /// Redacts `raw` (a remote URL, scp-like Git location, or arbitrary
    /// configured string) for display: see [`display_url`] for the exact
    /// policy applied.
    pub fn render(raw: &str) -> Self {
        Self(display_url(raw))
    }

    /// Borrows the already-redacted text.
    pub const fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Display for RedactedUrl {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<RedactedUrl> for String {
    fn from(value: RedactedUrl) -> Self {
        value.0
    }
}

/// Renders a scp-like Git location (`[user@]host:path`) for diagnostics:
/// userinfo is always dropped (it's either a username of no diagnostic
/// value or, worse, a credential smuggled into the "user" position), while
/// the host and repository path -- including whether the path was
/// relative or absolute -- are preserved so the rendered form still
/// identifies which repository was addressed.
///
/// Unlike `gix`'s URL-syntax schemes (`https://`, `ssh://`, ...), scp-like
/// shorthand isn't URI syntax, so `gix` never percent-decodes it: a
/// `%0A`/`%1B` sequence in the input is preserved as that literal
/// four-character text in `location.path`, not turned into a real control
/// byte. What scp-like shorthand *does* allow through untouched is a
/// literal raw control byte typed directly into the host or path (a real
/// `\n`/`\x1b`/etc., no percent-encoding involved) -- `gix` performs no
/// sanitization of its own, and there is no upstream `gix`/`gix-url` API
/// (its own `write_to`/`to_bstring` serializers don't re-escape the path
/// for this scheme either) that guarantees a safely escaped rendering.
/// Rather than ever format `location.host`/`location.path` directly (which
/// would let a crafted location inject a real newline or terminal escape
/// sequence into diagnostic output), every non-printable/control
/// character is rendered through `escape_debug` (`\n`, `\r`, `\u{1b}`,
/// ...) -- ordinary printable characters are shown decoded and untouched.
/// Placeholder shown for input that failed structured URL parsing and
/// can't otherwise be proven safe to echo. Deliberately generic (no part
/// of the original input) so it can never repeat a credential or
/// query-string secret that happened to be embedded in malformed input.
const UNPARSEABLE_URL_PLACEHOLDER: &str = "<unparseable-url-redacted>";

/// Whether `raw` is clearly free of URL-shaped, credential-bearing
/// structure, so it's safe to display verbatim even though it didn't
/// parse as a URL (e.g. a bare local path, or an unexpanded `${VAR}` template
/// an environment variable rather than containing a secret itself).
///
/// This is intentionally conservative: it only allows raw passthrough when
/// none of the punctuation that typically carries a secret (userinfo's
/// `@`, a query string's `?`/`=`, percent-encoding's `%`, or a `://`
/// scheme separator) or control characters are present. Anything else
/// falls back to [`UNPARSEABLE_URL_PLACEHOLDER`] rather than risking a
/// leak merely because `url::Url::parse` happened to reject it.
fn looks_safe_to_display_raw(raw: &str) -> bool {
    !raw.contains('#')
        && !raw.contains('@')
        && !raw.contains('?')
        && !raw.contains('=')
        && !raw.contains('%')
        && !raw.contains("://")
        && !raw.chars().any(char::is_control)
}

/// A placeholder marker built entirely from RFC 3986 "unreserved"
/// characters (letters, digits, `-`, `.`, `_`, `~`), which `url::Url`
/// never percent-encodes. Shields `${VAR}` template references from
/// [`display_url`]'s URL parsing/serialization, which otherwise mangles
/// the literal `{`/`}` characters (notably percent-encoding them inside a
/// path segment, e.g. `${PREFIX}` becoming `$%7BPREFIX%7D`) so the
/// configured template would not display as configured.
const TEMPLATE_SHIELD_MARKER: &str = "gat-template-shield-";

fn with_templates_shielded(raw: &str, render: impl FnOnce(&str, &[String]) -> String) -> String {
    use gat_core::endpoint::{TemplateToken, tokenize_template};
    let Ok(tokens) = tokenize_template(raw) else {
        return UNPARSEABLE_URL_PLACEHOLDER.to_string();
    };
    // A fresh marker prevents literal or percent-encoded input from impersonating a reference.
    let mut marker = TEMPLATE_SHIELD_MARKER.to_string();
    if raw.contains('%') {
        marker.push_str(&"x".repeat(raw.len()));
    } else {
        let lower = raw.to_ascii_lowercase();
        while lower.contains(&marker) {
            marker.push('x');
        }
    }
    let mut shielded = String::new();
    let mut originals = Vec::new();
    let mut placeholders = Vec::new();
    for token in tokens {
        match token {
            TemplateToken::Literal(text) => shielded.push_str(text),
            TemplateToken::EscapedDollar => shielded.push_str("$$"),
            TemplateToken::Reference(name) => {
                let placeholder = format!("{marker}{}z", placeholders.len());
                shielded.push_str(&placeholder);
                placeholders.push(placeholder);
                originals.push(format!("${{{name}}}"));
            }
        }
    }
    let mut shown = render(&shielded, &placeholders);
    for (placeholder, original) in placeholders.iter().zip(originals).rev() {
        shown = shown.replace(placeholder, &original);
    }
    shown
}

/// Renders an original remote template without resolving variables or opening a backend.
/// Whole query references remain visible; Azure endpoints are sanitized as nested URLs.
pub fn render_remote_template(template: &gat_core::endpoint::RemoteUrlTemplate) -> RedactedUrl {
    RedactedUrl(with_templates_shielded(
        template.as_template_str(),
        render_template_url,
    ))
}

fn render_template_url(raw: &str, references: &[String]) -> String {
    if raw.chars().any(char::is_control) {
        return UNPARSEABLE_URL_PLACEHOLDER.to_string();
    }
    let Ok(parsed) = url::Url::parse(raw) else {
        return display_url_impl(raw);
    };
    let mut shown = display_url_impl(raw);
    let pairs: Vec<_> = parsed
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            let safe = if references.iter().any(|reference| reference == value) {
                value.to_string()
            } else if parsed.scheme() == "azblob" && key == "endpoint" {
                let decoded = url::form_urlencoded::parse(format!("v={value}").as_bytes())
                    .next()
                    .map(|(_, v)| v.into_owned())
                    .unwrap_or_default();
                match url::Url::parse(&decoded) {
                    Ok(endpoint)
                        if endpoint.has_host()
                            && !decoded.chars().any(char::is_control)
                            && !decoded.contains('\\') =>
                    {
                        let mut shown = display_url_impl(&decoded);
                        // Preserve an explicit default port, which Url serialization removes.
                        if endpoint.port().is_none()
                            && let Some((_, authority)) = decoded.split_once("://")
                            && let Some((_, port)) = authority
                                .split(['/', '?', '#'])
                                .next()
                                .unwrap_or_default()
                                .rsplit_once(':')
                            && port.parse::<u16>().is_ok()
                            && let Ok(sanitized) = url::Url::parse(&shown)
                        {
                            let end = sanitized[..url::Position::AfterHost].len();
                            shown.insert_str(end, &format!(":{port}"));
                        }
                        // URL serialization adds a slash to an empty HTTP path.
                        // Keep a configured authority-only endpoint exactly as written.
                        if endpoint.path() == "/"
                            && !decoded
                                .split_once("://")
                                .is_some_and(|(_, rest)| rest.contains('/'))
                            && shown.ends_with('/')
                        {
                            shown.pop();
                        }
                        shown
                    }
                    _ => "REDACTED".to_string(),
                }
            } else {
                "REDACTED".to_string()
            };
            // Keep nested URL punctuation readable, but protect outer query boundaries.
            format!("{key}={}", safe.replace('&', "%26").replace('#', "%23"))
        })
        .collect();
    if !pairs.is_empty()
        && let Some(query) = shown.find('?')
    {
        let fragment = if parsed.fragment().is_some() {
            "#REDACTED"
        } else {
            ""
        };
        shown.truncate(query);
        shown.push('?');
        shown.push_str(&pairs.join("&"));
        shown.push_str(fragment);
    }
    shown
}

/// Render a remote URL for humans (errors, `gat remote` output, verbose
/// logs, config display, ...): keeps the scheme/host/path so the backend and
/// target are still recognizable, but never leaks userinfo or query-string
/// values. Callers that need the *real* URL (i.e. `build_remote`, which
/// hands it to opendal) must keep using the original string — this helper
/// is for diagnostics only.
///
/// Userinfo is always dropped, and *every* query value is redacted
/// regardless of its key: a denylist of "known-sensitive" key names
/// inevitably misses provider-specific or unrecognized keys, so gat doesn't rely
/// on one to decide whether a value is safe to expose. Only the query
/// *keys* are preserved, so the shape of the configuration (which options
/// were set) stays visible without exposing any value. Git's scp-like
/// `user@host:path` shorthand is recognized separately from parseable URLs
/// and likewise has its userinfo stripped.
///
/// Template spelling in host/path is preserved, but query values stay
/// conservative even when they resemble references. Original typed templates
/// should use [`render_remote_template`] instead.
pub fn display_url(raw: &str) -> String {
    with_templates_shielded(raw, |raw, _| display_url_impl(raw))
}

fn display_url_impl(raw: &str) -> String {
    // Recognized through the presentation-only parser dependency, without
    // constructing an engine location value or crossing into `gat-io`.
    // Parse failures are discarded because they may retain the raw input.
    if let Some(redacted) = redacted_scp_like(raw) {
        return redacted;
    }
    let Ok(mut parsed) = url::Url::parse(raw) else {
        // Not a parseable URL. Failing to parse is not proof the input is
        // *safe* -- a credential-bearing URL with a stray space, bad port,
        // control character, or unsupported escaping can fail `Url::parse`
        // just as easily as a harmless bare path. Only echo the raw input
        // back when it's clearly free of URL-shaped, secret-carrying
        // structure; otherwise fail closed with a placeholder rather than
        // risk leaking a credential or query value.
        return if looks_safe_to_display_raw(raw) {
            raw.to_string()
        } else {
            UNPARSEABLE_URL_PLACEHOLDER.to_string()
        };
    };

    // Userinfo (user:pass@host) is never useful for diagnostics and often
    // *is* the secret, so drop it unconditionally.
    let _ = parsed.set_username("");
    let _ = parsed.set_password(None);

    let redacted_pairs: Vec<(String, String)> = parsed
        .query_pairs()
        .map(|(k, _v)| (k.into_owned(), "REDACTED".to_string()))
        .collect();

    if redacted_pairs.is_empty() {
        parsed.set_query(None);
    } else {
        parsed
            .query_pairs_mut()
            .clear()
            .extend_pairs(redacted_pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }

    // A URL fragment (`#...`) is exactly as capable of carrying a secret
    // as a query value (e.g. an OAuth implicit-grant `#access_token=...`,
    // or a signed-URL scheme that puts its signature after `#` instead of
    // `?`), so it gets the same fail-closed treatment as query values
    // rather than being left to pass through `url::Url`'s `Display`
    // unredacted.
    if parsed.fragment().is_some() {
        parsed.set_fragment(Some("REDACTED"));
    }

    parsed.into()
}

fn redacted_scp_like(raw: &str) -> Option<String> {
    let location = gix_url::parse(raw).ok()?;
    if location.scheme != gix_url::Scheme::Ssh || !location.serialize_alternative_form {
        return None;
    }

    let host: String = location
        .host
        .as_deref()
        .unwrap_or_default()
        .chars()
        .flat_map(char::escape_debug)
        .collect();
    let path: String = String::from_utf8_lossy(location.path.as_ref())
        .chars()
        .flat_map(char::escape_debug)
        .collect();
    Some(format!("{host}:{path}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_templates_preserve_only_safe_query_values() {
        // hygiene-ok: synthetic URLs used exclusively for pure redaction tests; never dialed.
        let cases = [
            (
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://${CONTAINER}/gat?endpoint=https://${STORAGE_ACCOUNT}.blob.core.windows.net",
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://${CONTAINER}/gat?endpoint=https://${STORAGE_ACCOUNT}.blob.core.windows.net",
            ),
            (
                "s3://bucket/path?sas_token=${SAS_TOKEN}&other=${OTHER}",
                "s3://bucket/path?sas_token=${SAS_TOKEN}&other=${OTHER}",
            ),
            (
                "s3://bucket/path?token=${TOKEN}-literal-secret&x=$${TOKEN}&y=%24%7BTOKEN%7D&z=${A}${B}",
                "s3://bucket/path?token=REDACTED&x=REDACTED&y=REDACTED&z=REDACTED",
            ),
            (
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://example.com:8443/base",
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://example.com:8443/base",
            ),
            (
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://user:secret@example.com/base%3Ftoken=secret%23secret",
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://example.com/base?token=REDACTED%23REDACTED",
            ),
            (
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://host:bad/secret",
                "azblob://bucket/path?endpoint=REDACTED",
            ),
            (
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://host/%0Asecret",
                "azblob://bucket/path?endpoint=REDACTED",
            ),
            (
                "azblob://bucket/path?endpoint=literal-secret",
                "azblob://bucket/path?endpoint=REDACTED",
            ),
            (
                "s3://host:bad/path?token=secret",
                UNPARSEABLE_URL_PLACEHOLDER,
            ),
            (
                "s3://bucket/path?token=${INVALID-secret}",
                UNPARSEABLE_URL_PLACEHOLDER,
            ),
            (
                "s3://bucket/path?token=${UNCLOSED",
                UNPARSEABLE_URL_PLACEHOLDER,
            ),
            (
                "s3://bucket/path\n?token=secret",
                UNPARSEABLE_URL_PLACEHOLDER,
            ),
            ("${REMOTE}", "${REMOTE}"),
            // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
            (
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://example.com:443/base",
                // hygiene-ok: synthetic nested URL for pure redaction; never dialed.
                "azblob://bucket/path?endpoint=https://example.com:443/base",
            ),
            (
                "azblob://bucket/path?endpoint=${SCHEME}://${HOST}/${PATH}",
                "azblob://bucket/path?endpoint=${SCHEME}://${HOST}/${PATH}",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(render_remote_template(&input.into()).as_str(), expected);
        }
    }

    #[test]
    pub fn display_url_redacts_userinfo() {
        let raw = format!(
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            "https://{}:{}@bucket.example.com/prefix",
            "ACCESSKEY", "secretvalue"
        );
        let shown = display_url(&raw);
        assert!(!shown.contains("ACCESSKEY"));
        assert!(!shown.contains("secretvalue"));
        assert!(shown.contains("bucket.example.com"));
        assert!(shown.contains("/prefix"));
    }

    #[test]
    pub fn display_url_redacts_every_query_value_known_or_not() {
        let shown = display_url(
            "s3://bucket/prefix?region=us-east-1&access_key_id=AKIA123&custom_param=maybe-a-secret",
        );
        assert!(!shown.contains("us-east-1"));
        assert!(!shown.contains("AKIA123"));
        assert!(!shown.contains("maybe-a-secret"));
        // keys are preserved so the shape of the configuration stays visible
        assert!(shown.contains("region=REDACTED"));
        assert!(shown.contains("access_key_id=REDACTED"));
        assert!(shown.contains("custom_param=REDACTED"));
    }

    #[test]
    pub fn display_url_falls_back_to_raw_string_when_clearly_safe() {
        // No URL-shaped or secret-carrying punctuation (`@`, `?`, `=`, `%`,
        // `://`), so it's safe to show as-is.
        assert_eq!(display_url("not a url"), "not a url");
    }

    #[test]
    pub fn display_url_fails_closed_when_unparseable_and_not_clearly_safe() {
        // Contains a `secret=` query-like fragment; must not be echoed back
        // merely because `Url::parse` rejects the input.
        let shown = display_url("not a url with a secret=hunter2");
        assert!(!shown.contains("hunter2"));
        assert!(!shown.contains("secret=hunter2"));
        assert_eq!(shown, UNPARSEABLE_URL_PLACEHOLDER);
    }

    #[test]
    pub fn display_url_redacts_scp_like_userinfo() {
        let shown = display_url("git@github.com:acme/models.git");
        assert_eq!(shown, "github.com:acme/models.git");
    }

    /// Table-driven regression suite for `display_url`: each case names a
    /// class of input the redaction logic must handle, a set of secret
    /// substrings that must never survive into the displayed form, and a
    /// set of substrings (scheme/host/path/keys) that must be preserved so
    /// the output stays useful for diagnostics. Kept as one table (rather
    /// than one-off tests) so any redaction regression shows up as a
    /// single failing row naming exactly which class of input broke.
    #[test]
    pub fn display_url_redaction_table() {
        struct Case<'a> {
            name: &'static str,
            input: &'a str,
            must_not_contain: &'static [&'static str],
            must_contain: &'static [&'static str],
        }

        let userinfo_url = format!(
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            "https://{}:{}@bucket.example.com/prefix",
            "ACCESSKEY", "secretvalue"
        );
        let percent_encoded_userinfo_url = format!(
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            "https://{}:{}@bucket.example.com/prefix",
            "someuser", "p%40ssw0rd"
        );
        // An invalid bracketed (IPv6-shaped) host with userinfo fails to
        // parse, keeping the `user:pass@` shape unresolved.
        let malformed_userinfo_url = format!(
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            "https://{}:{}@{}invalid/path?token=abc",
            "user", "supersecretpassword", "["
        );
        // A NUL control character makes the authority unparseable while a
        // credential-bearing query string sits right next to it.
        let malformed_control_char_url = format!(
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            "https://host{}{}/path?token=abc",
            '\u{0}', "supersecretpassword"
        );
        // A non-numeric port is rejected by `url::Url::parse`.
        let malformed_port_url =
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            format!("https://host:notaport/path?token={}", "supersecretpassword");
        // An invalid bracketed (IPv6-shaped) host fails to parse, with a
        // custom (non-denylisted) query key carrying the secret.
        let malformed_bracket_url = format!(
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            "https://{}invalid/path?api_key={}",
            "[", "abc123supersecret"
        );
        let malformed_percent_encoded_url =
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            format!("https://{}invalid/path?token={}", "[", "hunter%2Btwo");
        // A space inside the scheme breaks parsing entirely.
        let malformed_s3_style_url =
            format!("s3 ://bucket/prefix?access_key_id={}", "AKIA123supersecret");
        let malformed_azblob_style_url = format!(
            "azblob://{}invalid/container?sas_token={}",
            "[", "supersecretsas"
        );
        let malformed_gcs_style_url =
            format!("gcs://bucket:notaport/prefix?token={}", "supersecretgcs");
        let scp_raw_newline_url = format!("git@github.com:acme/models{}injected.git", '\n');
        let scp_raw_cr_url = format!("git@github.com:acme/models{}injected.git", '\r');
        let scp_raw_esc_url = format!("git@github.com:acme/models{}injected.git", '\u{1b}');
        let scp_raw_ansi_url = format!("git@github.com:acme/models{}[31minjected.git", '\u{1b}');

        let cases = [
            Case {
                name: "userinfo",
                input: userinfo_url.as_str(),
                must_not_contain: &["ACCESSKEY", "secretvalue"],
                must_contain: &["bucket.example.com", "/prefix"],
            },
            Case {
                name: "multiple query parameters",
                input: "s3://bucket/prefix?region=us-east-1&access_key_id=AKIA123&custom_param=maybe-a-secret",
                must_not_contain: &["us-east-1", "AKIA123", "maybe-a-secret"],
                must_contain: &[
                    "region=REDACTED",
                    "access_key_id=REDACTED",
                    "custom_param=REDACTED",
                ],
            },
            Case {
                name: "percent-encoded secret in query value",
                input: "s3://bucket/prefix?token=hunter%2Btwo%20words",
                must_not_contain: &["hunter", "hunter%2Btwo%20words", "hunter+two words"],
                must_contain: &["token=REDACTED"],
            },
            Case {
                name: "percent-encoded secret in userinfo",
                input: percent_encoded_userinfo_url.as_str(),
                must_not_contain: &["p%40ssw0rd", "someuser"],
                must_contain: &["bucket.example.com", "/prefix"],
            },
            Case {
                name: "non-secret url with no query or userinfo",
                input: "file:///absolute/path/to/repo",
                must_not_contain: &["REDACTED"],
                must_contain: &["file:///absolute/path/to/repo"],
            },
            Case {
                name: "scp-like git url",
                input: "git@github.com:acme/models.git",
                must_not_contain: &["git@"],
                must_contain: &["github.com:acme/models.git"],
            },
            Case {
                name: "scp-like git url without a user",
                input: "github.com:acme/models.git",
                must_not_contain: &[],
                must_contain: &["github.com:acme/models.git"],
            },
            Case {
                name: "scp-like git url with an absolute path is preserved as absolute",
                input: "git@github.com:/acme/models.git",
                must_not_contain: &["git@"],
                must_contain: &["github.com:/acme/models.git"],
            },
            Case {
                name: "malformed url with secret=hunter2 fails closed",
                input: "not a url with a secret=hunter2",
                must_not_contain: &["hunter2", "secret=hunter2"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "azblob with sas token query",
                input: "azblob://container/prefix?sas_token=sv%3D2021-01-01%26sig%3Dsupersecret",
                must_not_contain: &["supersecret", "sv%3D2021-01-01"],
                must_contain: &["sas_token=REDACTED"],
            },
            Case {
                name: "fragment carrying an implicit-grant-style access token is redacted",
                // hygiene-ok: synthetic test URL exercising fragment-token redaction; never dialed.
                input: "https://example.com/callback#access_token=supersecrettoken&type=bearer",
                must_not_contain: &["supersecrettoken"],
                must_contain: &["example.com/callback", "#REDACTED"],
            },
            Case {
                name: "fragment alongside a query value is redacted independently of the query",
                // hygiene-ok: synthetic test URL exercising fragment+query redaction; never dialed.
                input: "https://example.com/path?region=us-east-1#supersecretfragment",
                must_not_contain: &["supersecretfragment"],
                must_contain: &["region=REDACTED", "#REDACTED"],
            },
            Case {
                name: "malformed url with userinfo-like content and invalid host",
                input: malformed_userinfo_url.as_str(),
                must_not_contain: &["supersecretpassword", "token=abc"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "malformed url with control char breaks parsing but keeps secret shape",
                input: malformed_control_char_url.as_str(),
                must_not_contain: &["supersecretpassword", "token=abc"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "malformed url with invalid port and query secret",
                input: malformed_port_url.as_str(),
                must_not_contain: &["supersecretpassword"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "malformed url with invalid bracketed host and custom query key",
                input: malformed_bracket_url.as_str(),
                must_not_contain: &["abc123supersecret"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "malformed url with percent-encoded secret material",
                input: malformed_percent_encoded_url.as_str(),
                must_not_contain: &["hunter%2Btwo", "hunter+two"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "malformed s3-style url with leading space breaks scheme",
                input: malformed_s3_style_url.as_str(),
                must_not_contain: &["AKIA123supersecret"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "malformed azblob-style url with invalid host and sas token",
                input: malformed_azblob_style_url.as_str(),
                must_not_contain: &["supersecretsas"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "malformed gcs-style url with invalid port and token",
                input: malformed_gcs_style_url.as_str(),
                must_not_contain: &["supersecretgcs"],
                must_contain: &[UNPARSEABLE_URL_PLACEHOLDER],
            },
            Case {
                name: "clearly safe bare path stays raw",
                input: "not a url",
                must_not_contain: &[],
                must_contain: &["not a url"],
            },
            Case {
                name: "unexpanded ${VAR} template stays raw (no secret value present)",
                input: "${GAT_SOME_REMOTE_SECRET_VAR}",
                must_not_contain: &[],
                must_contain: &["${GAT_SOME_REMOTE_SECRET_VAR}"],
            },
            Case {
                name: "${VAR} template in host is shown as configured, not encoded",
                input: "s3://${BUCKET}/prefix",
                must_not_contain: &[],
                must_contain: &["${BUCKET}", "/prefix"],
            },
            Case {
                name: "${VAR} template in path is shown as configured, not percent-encoded",
                input: "s3://bucket/${PREFIX}/foo",
                must_not_contain: &["%7B", "%7D"],
                must_contain: &["${PREFIX}", "s3://bucket/", "/foo"],
            },
            Case {
                name: "${VAR} template in query value is still redacted like any other value",
                input: "s3://bucket/foo?token=${TOKEN}",
                must_not_contain: &["${TOKEN}"],
                must_contain: &["token=REDACTED"],
            },
            Case {
                name: "scp-like path with a literal raw newline is never emitted as a real newline",
                input: scp_raw_newline_url.as_str(),
                must_not_contain: &[],
                must_contain: &["github.com:acme/models"],
            },
            Case {
                name: "scp-like path with a literal raw carriage return is never emitted raw",
                input: scp_raw_cr_url.as_str(),
                must_not_contain: &[],
                must_contain: &["github.com:acme/models"],
            },
            Case {
                name: "scp-like path with a literal raw ESC byte is never emitted raw",
                input: scp_raw_esc_url.as_str(),
                must_not_contain: &[],
                must_contain: &["github.com:acme/models"],
            },
            Case {
                name: "scp-like path with a literal raw ANSI escape sequence cannot inject styling",
                input: scp_raw_ansi_url.as_str(),
                must_not_contain: &[],
                must_contain: &["github.com:acme/models"],
            },
            Case {
                name: "scp-like path with an ordinary percent-encoded sequence is left as-is (scp shorthand is never percent-decoded)",
                input: "git@github.com:acme/my%20models.git",
                must_not_contain: &[],
                must_contain: &["github.com:acme/my%20models.git"],
            },
        ];

        for case in cases {
            let shown = display_url(case.input);
            for secret in case.must_not_contain {
                assert!(
                    !shown.contains(secret),
                    "case `{}`: expected redacted output to not contain `{secret}`, got `{shown}`",
                    case.name
                );
            }
            for keep in case.must_contain {
                assert!(
                    shown.contains(keep),
                    "case `{}`: expected redacted output to contain `{keep}`, got `{shown}`",
                    case.name
                );
            }
        }
    }

    /// Byte/char-level regression for scp-like diagnostic rendering:
    /// scp-like shorthand isn't URI syntax, so `gix` never
    /// percent-decodes it -- a literal *raw* control byte typed directly
    /// into the input (not a `%`-encoded sequence) is what can reach
    /// `location.path`/`location.host` unsanitized. This asserts on the
    /// actual characters present in the rendered output -- not just a
    /// visual read of the string -- so a regression that reintroduces a
    /// raw control byte (e.g. by formatting `location.path` directly
    /// again) fails here even if it "looks fine" printed to a terminal.
    #[test]
    pub fn display_url_scp_like_never_emits_raw_control_bytes() {
        let cases: &[(String, char)] = &[
            (
                format!("git@github.com:acme/models{}injected.git", '\n'),
                '\n',
            ),
            (
                format!("git@github.com:acme/models{}injected.git", '\r'),
                '\r',
            ),
            (
                format!("git@github.com:acme/models{}injected.git", '\u{1b}'),
                '\u{1b}',
            ),
            (
                format!("git@github.com:acme/models{}[31minjected.git", '\u{1b}'),
                '\u{1b}',
            ),
        ];
        for (input, forbidden) in cases {
            let shown = display_url(input);
            assert!(
                !shown.chars().any(|c| c == *forbidden),
                "input `{input:?}`: rendered output `{shown:?}` contains a raw {forbidden:?} byte"
            );
            assert!(
                !shown.chars().any(char::is_control),
                "input `{input:?}`: rendered output `{shown:?}` contains a raw control byte"
            );
        }

        // An scp-like path that merely *looks* percent-encoded is left
        // as-is: scp shorthand is never percent-decoded by `gix`, so
        // decoding it here would misrepresent what the location actually
        // names.
        let shown = display_url("git@github.com:acme/my%20models.git");
        assert_eq!(shown, "github.com:acme/my%20models.git");
    }

    /// [`RedactedUrl`] itself (not just the free function it wraps) must
    /// uphold the "never exposes userinfo/query values" invariant, since
    /// it's the type production call sites now construct and pass around
    /// instead of an ordinary `String`.
    #[test]
    fn redacted_url_never_exposes_userinfo_or_query_values() {
        let url = RedactedUrl::render(
            // hygiene-ok: synthetic malformed/test URL string used only to exercise redaction logic; never dialed.
            "https://ACCESSKEY:secretvalue@bucket.example.com/prefix?token=hunter2",
        );
        assert!(!url.as_str().contains("ACCESSKEY"));
        assert!(!url.as_str().contains("secretvalue"));
        assert!(!url.as_str().contains("hunter2"));
        assert!(url.as_str().contains("bucket.example.com"));
        assert_eq!(url.to_string(), url.as_str());
    }
}
