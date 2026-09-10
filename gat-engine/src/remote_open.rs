//! Semantic remote-opening failures exposed above the I/O boundary.

/// User-actionable classification for a remote that could not be validated
/// or opened. Classification contains no resolved endpoint values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RemoteOpenFailureKind {
    NonUnicodeVariable { name: String },
    InvalidInterpolation,
    MissingVariable { name: String },
    MalformedUrl,
    UnsupportedBackend,
    InvalidFileRemotePath { hint: &'static str },
    DisallowedScheme,
    UnsupportedCapability,
    PermissionDenied,
    NotFound,
    Unavailable,
    ReadinessTimedOut { budget: std::time::Duration },
    OperationFailed,
}

/// An opaque remote-opening failure that preserves the complete physical
/// source while exposing semantic classification and original typed context.
#[derive(Debug)]
pub struct RemoteOpenError {
    kind: RemoteOpenFailureKind,
    template: gat_core::endpoint::RemoteUrlTemplate,
    source: Box<gat_io::OpenRemoteError>,
}

impl RemoteOpenError {
    pub(crate) fn from_io(
        template: &gat_core::endpoint::RemoteUrlTemplate,
        source: gat_io::OpenRemoteError,
    ) -> Self {
        use gat_io::{InterpolateError, OpenRemoteError, RemoteError};

        let kind = match &source {
            OpenRemoteError::Interpolate(InterpolateError::NonUnicodeVariable { name }) => {
                RemoteOpenFailureKind::NonUnicodeVariable { name: name.clone() }
            }
            OpenRemoteError::Interpolate(
                InterpolateError::UnterminatedReference { .. }
                | InterpolateError::InvalidVariableName { .. },
            ) => RemoteOpenFailureKind::InvalidInterpolation,
            OpenRemoteError::Interpolate(InterpolateError::MissingVariable { name }) => {
                RemoteOpenFailureKind::MissingVariable { name: name.clone() }
            }
            OpenRemoteError::Remote(remote) => match remote {
                RemoteError::MalformedUrl { .. } => RemoteOpenFailureKind::MalformedUrl,
                RemoteError::UnsupportedScheme { .. } => RemoteOpenFailureKind::UnsupportedBackend,
                RemoteError::InvalidFileRemotePath { hint } => {
                    RemoteOpenFailureKind::InvalidFileRemotePath { hint }
                }
                RemoteError::DisallowedScheme => RemoteOpenFailureKind::DisallowedScheme,
                RemoteError::UnsupportedCapability { .. } => {
                    RemoteOpenFailureKind::UnsupportedCapability
                }
                RemoteError::PermissionDenied { .. } => RemoteOpenFailureKind::PermissionDenied,
                RemoteError::NotFound { .. } => RemoteOpenFailureKind::NotFound,
                RemoteError::ReadinessTimedOut { budget } => {
                    RemoteOpenFailureKind::ReadinessTimedOut { budget: *budget }
                }
                RemoteError::Unavailable { .. } => RemoteOpenFailureKind::Unavailable,
                RemoteError::OperationFailed { .. }
                | RemoteError::CleanupTimedOut
                | RemoteError::PayloadLimitExceeded { .. } => {
                    RemoteOpenFailureKind::OperationFailed
                }
            },
        };
        Self {
            kind,
            template: template.clone(),
            source: Box::new(source),
        }
    }

    /// Original configuration context; presentation must use template redaction.
    #[must_use]
    pub const fn template(&self) -> &gat_core::endpoint::RemoteUrlTemplate {
        &self.template
    }

    #[must_use]
    pub const fn kind(&self) -> &RemoteOpenFailureKind {
        &self.kind
    }
}

impl std::fmt::Display for RemoteOpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("could not validate or open the remote")
    }
}

impl std::error::Error for RemoteOpenError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.source.as_ref())
    }
}
