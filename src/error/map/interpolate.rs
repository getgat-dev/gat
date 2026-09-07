//! `Failure` mapping for remote interpolation errors.

use super::super::{Diagnostic, ErrorCode};
use crate::presentation::UserLine;

pub(super) fn interpolation_syntax_diagnostic() -> Diagnostic {
    Diagnostic::new(
        ErrorCode::InvalidArgumentValue,
        "Invalid `${...}` interpolation",
    )
    .with_hint(
        "Use `${NAME}` with a name matching [A-Za-z_][A-Za-z0-9_]*; `$$` \
         escapes a literal `$`.",
    )
}

pub(super) fn missing_variable_diagnostic(name: &str) -> Diagnostic {
    Diagnostic::new(
        ErrorCode::InvalidArgumentValue,
        "A referenced environment variable is not set",
    )
    .with_subject(UserLine::identifier(name))
    .with_hint(UserLine::compose([
        UserLine::authored("Set the `"),
        UserLine::identifier(name),
        UserLine::authored("` environment variable and try again."),
    ]))
}
