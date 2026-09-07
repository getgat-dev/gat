//! Engine-facing access to standalone configuration files.

use std::path::Path;

use gat_core::config::Config;

/// Failure to load and validate a standalone `gat.yaml`.
/// Created by the engine's load operation, without exposing a conversion
/// from the storage layer's error type.
///
/// ```compile_fail
/// fn forward(error: gat_io::ConfigError) -> gat_engine::ConfigFileLoadError {
///     error.into()
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ConfigFileLoadError(gat_io::ConfigError);

/// Failure to serialize or save a standalone `gat.yaml`.
///
/// ```compile_fail
/// fn forward(error: gat_io::ConfigWriteError) -> gat_engine::ConfigFileSaveError {
///     error.into()
/// }
/// ```
#[derive(Debug, thiserror::Error)]
#[error(transparent)]
pub struct ConfigFileSaveError(gat_io::ConfigWriteError);

/// Loads and validates a standalone `gat.yaml`.
pub fn load(path: &Path) -> Result<Config, ConfigFileLoadError> {
    gat_io::ConfigStore::load_file(path).map_err(ConfigFileLoadError)
}

/// Serializes and atomically saves a standalone `gat.yaml`.
pub fn save(path: &Path, config: &Config) -> Result<(), ConfigFileSaveError> {
    gat_io::ConfigStore::save_file(path, config).map_err(ConfigFileSaveError)
}
