//! Lifecycle primitives: the product-surface identity vocabulary
//! ([`Surface`]), the three-state policy
//! ([`Status`]), the descriptive value a policy decision produces
//! ([`FeatureSpec`]), the runtime event it yields once observed
//! ([`Notice`]/[`NoticeKind`]/[`notice_for`]), the shared factual wording
//! used by lifecycle consumers
//! ([`lifecycle_reason`]), and the invocation-scoped deduplicating event
//! sink ([`Lifecycle`]).
//!
//! This module knows nothing about which concrete surfaces exist in the
//! product (no command names, no config keys), how a [`Surface`] maps to
//! a [`FeatureSpec`] (no registry/lookup table), Clap `--help` wording,
//! documentation targets, or terminal rendering -- all of that
//! is root policy that stays in the `gat` crate's `lifecycle` module,
//! which re-exports everything here and layers a concrete product
//! registry, a registry-aware `observe` extension, and presentation on
//! top of it.

use std::cell::RefCell;
use std::collections::HashSet;

/// One of the three lifecycle states a product surface can be in. Keep
/// the vocabulary to exactly these three words everywhere (no Alpha/Beta/
/// Preview aliases). Actual feature removal is
/// represented by deleting the surface's implementation and its
/// registry entry entirely, never by a lifecycle status -- this
/// contract has no "removed" state to keep in sync with anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// Normal supported behavior; the default for anything not
    /// explicitly marked otherwise.
    Stable,
    /// Available for use, but its interface or behavior may change or be
    /// removed between releases. Never a waiver for memory safety, data
    /// integrity, path safety, or repository readability/migratability --
    /// those guarantees hold regardless of lifecycle status.
    Experimental,
    /// Still supported; users should migrate to
    /// [`FeatureSpec::replacement`] where one exists, ahead of an
    /// earliest possible removal rather than a promised exact release.
    Deprecated,
}

impl Status {
    /// The exact user-facing word for this status, used identically in
    /// CLI notices, `--help` text, and command/configuration descriptions.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Stable => "Stable",
            Self::Experimental => "Experimental",
            Self::Deprecated => "Deprecated",
        }
    }
}

/// The kind of externally observable product surface a [`FeatureSpec`]
/// describes, carrying its own identity data (e.g. `Command("gc")`)
/// rather than a category tag plus a separately-formatted string id --
/// this is the type a product registry is keyed by, and the type
/// [`Lifecycle`] deduplicates by, so lookup never has to reconstruct an
/// id with `format!`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Surface<'a> {
    /// A whole CLI command, e.g. `Command("gc")` for `gat gc`.
    Command(&'a str),
    /// A CLI command's alternate spelling.
    CommandAlias { command: &'a str, alias: &'a str },
    /// A CLI command's option/flag, e.g. `--some-flag` on some command.
    CommandOption { command: &'a str, option: &'a str },
    /// A command option's alternate spelling.
    OptionAlias {
        command: &'a str,
        option: &'a str,
        alias: &'a str,
    },
    /// One enumerated value a command option accepts -- distinct from
    /// [`Surface::ConfigValue`] so a CLI-option value and a config value
    /// that happen to share a spelling can never collide.
    OptionValue {
        command: &'a str,
        option: &'a str,
        value: &'a str,
    },
    /// A config key.
    ConfigKey(&'a str),
    /// A config key's deprecated serialized alias, e.g.
    /// `ConfigAlias { canonical: "git.ignore_patterns", alias:
    /// "git.exclude_patterns" }`.
    ConfigAlias { canonical: &'a str, alias: &'a str },
    /// One enumerated value a config key accepts, e.g.
    /// `ConfigValue { key: "cache.ingest_strategy", value: "mmap" }`.
    ConfigValue { key: &'a str, value: &'a str },
}

/// Descriptive lifecycle metadata for one product surface. See
/// [`Surface`] for which kinds of surface this can describe. A concrete
/// product registry (root's `lifecycle::REGISTRY`) is a static table of
/// these, keyed by [`Self::surface`].
#[derive(Debug, Clone, Copy)]
pub struct FeatureSpec {
    /// Which product surface this describes, and its identity (e.g.
    /// `Surface::Command("gc")`) -- also the key a registry is looked up
    /// by and notices are deduplicated by, so there is no separate
    /// string id to keep in sync with it.
    pub surface: Surface<'static>,
    /// User-facing name shown in notices/docs, e.g. `` "`gat gc`" ``.
    pub subject: &'static str,
    /// Current lifecycle status.
    pub status: Status,
    /// The documented replacement, if any -- e.g. `"git.ignore_patterns"`
    /// for the `git.exclude_patterns` alias. Only set this when the named
    /// surface is an actual, semantically equivalent successor a user
    /// should migrate to -- never just "the stable default" (see
    /// [`Self::note`] for that case). Shown in deprecation
    /// notices/docs when present.
    pub replacement: Option<&'static str>,
    /// An additional factual sentence appended to a deprecation notice
    /// that has no [`Self::replacement`], e.g. "`safe` is the stable
    /// default." for `cache.ingest_strategy=mmap` -- keeps the wording
    /// accurate without misrepresenting a stable default as a semantic
    /// replacement for the deprecated value.
    pub note: Option<&'static str>,
}

/// The kind of one-shot message a [`FeatureSpec`] produces at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoticeKind {
    Experimental,
    Deprecated,
}

/// One lifecycle notice. Root's `output::notices` renders this to
/// stderr; this module only ever produces the typed value, never a
/// rendered string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    /// The originating [`FeatureSpec::surface`], reused as-is to
    /// deduplicate notices within one invocation -- see [`Lifecycle`].
    pub surface: Surface<'static>,
    pub subject: &'static str,
    pub replacement: Option<&'static str>,
    pub note: Option<&'static str>,
}

/// The factual sentence shared, word-for-word, between a runtime notice
/// and documentation wording -- one lifecycle wording decision
/// tree, not two independently maintained copies. Only the wrapping
/// around this sentence differs by renderer (plain-text prefix in a
/// terminal notice vs. Markdown bold status + doc link in generated
/// docs); both live in root.
#[must_use]
pub fn lifecycle_reason(kind: NoticeKind, replacement: Option<&str>, note: Option<&str>) -> String {
    match kind {
        NoticeKind::Experimental => {
            "its interface or behavior may change or be removed between releases.".to_string()
        }
        NoticeKind::Deprecated => match (replacement, note) {
            (Some(replacement), _) => {
                format!("this remains supported for compatibility. Use `{replacement}` instead.")
            }
            (None, Some(note)) => {
                format!("this remains available for compatibility. {note}")
            }
            (None, None) => "this remains available for compatibility.".to_string(),
        },
    }
}

/// Builds the [`Notice`] a [`FeatureSpec`] produces at runtime, or `None`
/// for [`Status::Stable`] (nothing to notice about). Callers report a
/// `spec` here only once they've actually observed the surface it
/// describes (see [`Lifecycle::record`]/root's `observe`) -- this
/// function itself has no opinion on when that is.
#[must_use]
pub const fn notice_for(spec: &FeatureSpec) -> Option<Notice> {
    match spec.status {
        Status::Stable => None,
        Status::Experimental => Some(Notice {
            kind: NoticeKind::Experimental,
            surface: spec.surface,
            subject: spec.subject,
            replacement: spec.replacement,
            note: spec.note,
        }),
        Status::Deprecated => Some(Notice {
            kind: NoticeKind::Deprecated,
            surface: spec.surface,
            subject: spec.subject,
            replacement: spec.replacement,
            note: spec.note,
        }),
    }
}

/// Invocation-scoped, deduplicating sink for lifecycle observations. This
/// type has no notion of a product registry: it only stores
/// already-decided [`Notice`] values and deduplicates them by
/// [`Notice::surface`]. Root's `lifecycle::LifecycleObserve` extension
/// trait adds the registry-aware `observe(surface)` call sites actually
/// use, by looking up a [`FeatureSpec`] in the concrete product registry,
/// turning it into a `Notice` via [`notice_for`], and calling
/// [`Self::record`].
#[derive(Default)]
pub struct Lifecycle {
    notices: RefCell<Vec<Notice>>,
    seen: RefCell<HashSet<Surface<'static>>>,
}

impl Lifecycle {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records `notice`, deduplicated by [`Notice::surface`] -- a no-op
    /// if this exact surface has already been recorded once this
    /// invocation. Returns whether it was newly recorded.
    pub fn record(&self, notice: Notice) -> bool {
        if self.seen.borrow_mut().insert(notice.surface) {
            self.notices.borrow_mut().push(notice);
            true
        } else {
            false
        }
    }

    /// Drains every notice recorded so far, in observation order,
    /// leaving this sink empty again.
    pub fn take_notices(&self) -> Vec<Notice> {
        std::mem::take(&mut *self.notices.borrow_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(surface: Surface<'static>, status: Status) -> FeatureSpec {
        FeatureSpec {
            surface,
            subject: "x",
            status,
            replacement: None,
            note: None,
        }
    }

    #[test]
    fn stable_status_produces_no_notice() {
        let spec = spec(Surface::Command("x"), Status::Stable);
        assert!(notice_for(&spec).is_none());
    }

    #[test]
    fn experimental_notice_states_interface_may_change() {
        let spec = spec(Surface::Command("x"), Status::Experimental);
        let notice = notice_for(&spec).expect("produces a notice");
        let reason = lifecycle_reason(notice.kind, notice.replacement, notice.note);
        assert!(reason.contains("may change or be removed"));
    }

    #[test]
    fn deprecated_notice_names_replacement_when_present() {
        let spec = FeatureSpec {
            replacement: Some("new.key"),
            ..spec(
                Surface::ConfigAlias {
                    canonical: "new.key",
                    alias: "old.key",
                },
                Status::Deprecated,
            )
        };
        let notice = notice_for(&spec).expect("deprecated produces a notice");
        assert_eq!(notice.kind, NoticeKind::Deprecated);
        let reason = lifecycle_reason(notice.kind, notice.replacement, notice.note);
        assert!(reason.contains("new.key"));
    }

    #[test]
    fn deprecated_notice_without_replacement_or_note_stays_factual() {
        let spec = spec(
            Surface::ConfigAlias {
                canonical: "y",
                alias: "x",
            },
            Status::Deprecated,
        );
        let notice = notice_for(&spec).unwrap();
        let reason = lifecycle_reason(notice.kind, notice.replacement, notice.note);
        assert!(!reason.contains("Use `"));
        assert!(reason.contains("remains available for compatibility"));
    }

    #[test]
    fn lifecycle_record_deduplicates_by_surface() {
        let lifecycle = Lifecycle::new();
        let notice = notice_for(&spec(Surface::Command("gc"), Status::Experimental)).unwrap();
        assert!(lifecycle.record(notice.clone()));
        assert!(!lifecycle.record(notice));
        assert_eq!(lifecycle.take_notices().len(), 1);
    }

    #[test]
    fn lifecycle_take_notices_drains_and_resets() {
        let lifecycle = Lifecycle::new();
        let notice = notice_for(&spec(Surface::Command("gc"), Status::Experimental)).unwrap();
        lifecycle.record(notice);
        assert_eq!(lifecycle.take_notices().len(), 1);
        assert!(lifecycle.take_notices().is_empty());
    }
}
