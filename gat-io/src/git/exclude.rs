//! Coherent physical access to the repository's `.git/info/exclude`.

use std::path::{Path, PathBuf};

use crate::RepositoryLayout;
use crate::atomic::AtomicError;
use crate::file_state::{FileStateError, StatProof};
use crate::state::ExcludeRecord;

const CONCURRENT_MODIFICATION_RETRIES: u32 = 3;

/// A pure mutation derived by a caller from one coherent exclude snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InfoExcludeMutation {
    Unchanged,
    Replace(String),
    Remove,
}

/// One coherently observed `.git/info/exclude` generation.
pub struct InfoExcludeSnapshot {
    contents: String,
}

impl InfoExcludeSnapshot {
    #[must_use]
    pub fn contents(&self) -> &str {
        &self.contents
    }
}

/// Result of an exclude mutation.
pub struct InfoExcludeUpdate {
    changed: bool,
    proof: Option<StatProof>,
}

impl InfoExcludeUpdate {
    #[must_use]
    pub const fn changed(&self) -> bool {
        self.changed
    }

    pub(crate) const fn proof(&self) -> Option<StatProof> {
        self.proof
    }

    #[cfg(test)]
    pub(crate) const fn for_test(proof: Option<StatProof>) -> Self {
        Self {
            changed: proof.is_some(),
            proof,
        }
    }
}

/// Opaque evidence that the recorded managed exclude block is current.
pub struct InfoExcludeVerification(Verification);

enum Verification {
    Stale,
    Current { refreshed_proof: Option<StatProof> },
}

impl InfoExcludeVerification {
    #[must_use]
    pub const fn is_current(&self) -> bool {
        matches!(self.0, Verification::Current { .. })
    }

    pub(crate) const fn into_refreshed_proof(self) -> Option<StatProof> {
        match self.0 {
            Verification::Stale => None,
            Verification::Current { refreshed_proof } => refreshed_proof,
        }
    }

    #[cfg(test)]
    pub(crate) const fn current_for_test(refreshed_proof: Option<StatProof>) -> Self {
        Self(Verification::Current { refreshed_proof })
    }
}

/// Physical failures while observing or publishing `.git/info/exclude`.
#[derive(Debug, thiserror::Error)]
pub enum InfoExcludeError {
    #[error(transparent)]
    OpenRepository(#[from] super::GitOpenError),

    #[error("could not read `{}`", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("`{}` is not a regular file; refusing to manage it", path.display())]
    NotRegularFile { path: PathBuf },

    #[error(transparent)]
    Write(#[from] AtomicError),

    #[error("could not remove `{}`", path.display())]
    Remove {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "`{}` was modified concurrently by another process; giving up after {attempts} attempts",
        path.display()
    )]
    ConcurrentModification { path: PathBuf, attempts: u32 },

    #[error(transparent)]
    FileState(#[from] FileStateError),
}

enum Source {
    Missing,
    Present(crate::file_state::CoherentObservation<String>),
}

impl Source {
    fn contents(&self) -> &str {
        match self {
            Self::Missing => "",
            Self::Present(observation) => &observation.value,
        }
    }
}

fn path(layout: &RepositoryLayout) -> Result<PathBuf, InfoExcludeError> {
    Ok(super::common_dir_at(layout.root_path())?
        .join("info")
        .join("exclude"))
}

fn acquire(path: &Path) -> Result<Source, InfoExcludeError> {
    match std::fs::symlink_metadata(path) {
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Source::Missing),
        Err(source) => Err(InfoExcludeError::Read {
            path: path.to_path_buf(),
            source,
        }),
        Ok(metadata) if !metadata.is_file() => Err(InfoExcludeError::NotRegularFile {
            path: path.to_path_buf(),
        }),
        Ok(_) => {
            let observation = crate::file_state::coherent_observation(path, || {
                #[cfg(any(test, feature = "test-support"))]
                {
                    test_support::record_content_read();
                    test_support::fire_before_read(path);
                }
                std::fs::read_to_string(path).map_err(|source| InfoExcludeError::Read {
                    path: path.to_path_buf(),
                    source,
                })
            })?;
            Ok(Source::Present(observation))
        }
    }
}

fn source_is_current(path: &Path, source: &Source) -> bool {
    match source {
        // No regular-file proof can also mean a symlink, directory, or stat
        // failure. Only NotFound confirms an originally absent source.
        Source::Missing => matches!(std::fs::symlink_metadata(path), Err(error)
            if error.kind() == std::io::ErrorKind::NotFound),
        Source::Present(prior) => crate::file_state::observe_regular_file_no_follow(path)
            .is_some_and(|current| current.matches(&prior.proof)),
    }
}

/// Coherently reads the exclude file, returning `None` only when it is absent.
pub fn read_info_exclude(
    layout: &RepositoryLayout,
) -> Result<Option<InfoExcludeSnapshot>, InfoExcludeError> {
    Ok(match acquire(&path(layout)?)? {
        Source::Missing => None,
        Source::Present(observation) => Some(InfoExcludeSnapshot {
            contents: observation.value,
        }),
    })
}

/// Verify the recorded managed block without exposing filesystem evidence.
///
/// A proof hit performs only one no-follow stat. A proof miss coherently reads
/// the file once and refreshes its proof only when the managed block identity
/// still matches.
pub(crate) fn verify_info_exclude(
    layout: &RepositoryLayout,
    record: &ExcludeRecord,
    begin: &str,
    end: &str,
) -> Result<InfoExcludeVerification, InfoExcludeError> {
    let path = path(layout)?;
    if let Some(prior) = record.proof()
        && let Some(current) = crate::file_state::observe_regular_file_no_follow(&path)
        && current.matches(&prior)
    {
        return Ok(InfoExcludeVerification(Verification::Current {
            refreshed_proof: None,
        }));
    }

    let Source::Present(observation) = acquire(&path)? else {
        return Ok(InfoExcludeVerification(Verification::Stale));
    };
    let identity = gat_core::managed_block::extract_body(&observation.value, begin, end)
        .map(|body| *blake3::hash(body.as_bytes()).as_bytes());
    let verification = if identity.is_some() && identity == record.block_identity() {
        Verification::Current {
            refreshed_proof: Some(observation.proof),
        }
    } else {
        Verification::Stale
    };
    Ok(InfoExcludeVerification(verification))
}

/// Applies a caller-provided pure transformation to one coherent source
/// generation, revalidating and retrying before physical publication.
pub fn mutate_info_exclude(
    layout: &RepositoryLayout,
    dry_run: bool,
    mut derive: impl FnMut(&str) -> InfoExcludeMutation,
) -> Result<InfoExcludeUpdate, InfoExcludeError> {
    let path = path(layout)?;
    for attempt in 0..CONCURRENT_MODIFICATION_RETRIES {
        let source = acquire(&path)?;
        let mutation = derive(source.contents());
        if mutation == InfoExcludeMutation::Unchanged {
            return Ok(InfoExcludeUpdate {
                changed: false,
                proof: None,
            });
        }
        if dry_run {
            return Ok(InfoExcludeUpdate {
                changed: true,
                proof: None,
            });
        }

        #[cfg(any(test, feature = "test-support"))]
        test_support::fire_before_revalidate(&path);
        if !source_is_current(&path, &source) {
            if attempt + 1 == CONCURRENT_MODIFICATION_RETRIES {
                return Err(InfoExcludeError::ConcurrentModification {
                    path,
                    attempts: CONCURRENT_MODIFICATION_RETRIES,
                });
            }
            continue;
        }

        let proof = match mutation {
            InfoExcludeMutation::Unchanged => unreachable!("handled above"),
            InfoExcludeMutation::Replace(contents) => {
                Some(crate::atomic::write_atomic_with_proof(&path, &contents)?.proof)
            }
            InfoExcludeMutation::Remove => {
                std::fs::remove_file(&path).map_err(|source| InfoExcludeError::Remove {
                    path: path.clone(),
                    source,
                })?;
                None
            }
        };
        return Ok(InfoExcludeUpdate {
            changed: true,
            proof,
        });
    }
    unreachable!("the retry loop always returns on its final iteration");
}

#[cfg(any(test, feature = "test-support"))]
pub mod test_support {
    use std::cell::Cell;
    use std::path::Path;
    use std::sync::Mutex;

    type Hook = Box<dyn FnMut(&Path) + Send>;

    static BEFORE_READ: Mutex<Option<Hook>> = Mutex::new(None);
    static BEFORE_REVALIDATE: Mutex<Option<Hook>> = Mutex::new(None);

    thread_local! {
        static CONTENT_READS: Cell<usize> = const { Cell::new(0) };
    }

    pub(crate) fn record_content_read() {
        CONTENT_READS.with(|count| count.set(count.get() + 1));
    }

    pub fn content_read_count() -> usize {
        CONTENT_READS.with(Cell::get)
    }

    /// # Panics
    /// Panics if the test hook mutex is poisoned.
    pub fn set(hook: impl FnMut(&Path) + Send + 'static) {
        *BEFORE_READ.lock().unwrap() = Some(Box::new(hook));
    }

    /// # Panics
    /// Panics if the test hook mutex is poisoned.
    pub fn clear() {
        *BEFORE_READ.lock().unwrap() = None;
    }

    pub(crate) fn fire_before_read(path: &Path) {
        if let Some(hook) = BEFORE_READ.lock().unwrap().as_mut() {
            hook(path);
        }
    }

    /// # Panics
    /// Panics if the test hook mutex is poisoned.
    pub fn set_before_revalidate(hook: impl FnMut(&Path) + Send + 'static) {
        *BEFORE_REVALIDATE.lock().unwrap() = Some(Box::new(hook));
    }

    /// # Panics
    /// Panics if the test hook mutex is poisoned.
    pub fn clear_before_revalidate() {
        *BEFORE_REVALIDATE.lock().unwrap() = None;
    }

    pub(crate) fn fire_before_revalidate(path: &Path) {
        if let Some(hook) = BEFORE_REVALIDATE.lock().unwrap().as_mut() {
            hook(path);
        }
    }
}
