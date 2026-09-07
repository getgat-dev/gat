/// Technical cache failure retained behind the transfer's semantic kind and path.
/// Storage variants and physical paths are not exposed as structured fields.
///
/// ```compile_fail
/// use gat_engine::TransferCacheSource;
/// fn storage_error(source: TransferCacheSource) {
///     let _ = source.0;
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
#[repr(transparent)]
pub struct TransferCacheSource(Box<gat_io::CacheError>);

impl TransferCacheSource {
    pub(crate) fn new(source: gat_io::CacheError) -> Self {
        Self(Box::new(source))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn opaque_source_retains_the_underlying_io_failure() {
        let source = TransferCacheSource::new(gat_io::CacheError::SourceUnreadable {
            source: std::io::Error::new(std::io::ErrorKind::PermissionDenied, "cache sentinel"),
        });
        let retained = source
            .source()
            .unwrap()
            .downcast_ref::<std::io::Error>()
            .expect("retain the original I/O cause");
        assert_eq!(retained.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(retained.to_string(), "cache sentinel");
    }
}
