//! Root-owned lifecycle registry and runtime observation: the concrete
//! product registry ([`REGISTRY`]) describing which real CLI commands
//! and config keys/values are non-[`Status::Stable`] today, the
//! registry-aware [`LifecycleObserve::observe`] call sites use to turn a
//! [`Surface`] into a recorded [`Notice`], and the
//! warning projection (`DocTarget`/`DocWarning`/`DocPage`/`command_page`/
//! `config_key`) used by lifecycle consumers.
//!
//! The neutral lifecycle vocabulary this module builds on --
//! [`Status`], [`Surface`], [`FeatureSpec`], [`NoticeKind`], [`Notice`],
//! [`notice_for`], [`lifecycle_reason`], and the deduplicating
//! [`Lifecycle`] sink itself -- lives in [`gat_core::lifecycle`] and is
//! re-exported here unchanged, since none of it references a concrete
//! command name, config key, or Clap/documentation concern.
//!
//! This module does not construct [`crate::presentation::UserLine`]
//! values or perform terminal rendering. It does own the shared
//! lifecycle-policy wording used by both runtime notice presentation and
//! lifecycle warning text (`gat_core::lifecycle::lifecycle_reason`,
//! `doc_warning_text`, [`DocWarning`]), so those product surfaces cannot
//! drift independently. `output::notices` owns terminal-specific
//! composition and turns a [`Notice`] into an approved
//! [`crate::presentation::UserLine`] (see that module's `notice_line`).
//! It also never parses, persists, or gates anything, and
//! [`REGISTRY`]/[`FeatureSpec`] never decide *when* a surface was
//! actually used -- only [`LifecycleObserve::observe`]'s call sites (CLI
//! dispatch, config decode/read/write, or actual consumption of a
//! persisted value) decide that.

pub use gat_core::lifecycle::{
    FeatureSpec, Lifecycle, Notice, NoticeKind, Status, Surface, lifecycle_reason, notice_for,
};

/// Lifecycle wording that must not appear in command descriptions.
/// Lifecycle status is rendered from [`REGISTRY`] as runtime notices.
pub const EXPERIMENTAL_COMMAND_HELP_PREFIX: &str = "Experimental: ";

/// Builds a `const` [`FeatureSpec`], reducing the repeated `None`
/// boilerplate for the (usually absent) `replacement`/`note`
/// fields to named, optional arguments. Deliberately just a data-literal
/// shorthand -- it doesn't generate any CLI/config *implementation*,
/// only the static description table below.
macro_rules! feature_spec {
    (
        surface: $surface:expr,
        subject: $subject:expr,
        status: $status:expr
        $(, replacement: $replacement:expr)?
        $(, note: $note:expr)?
        $(,)?
    ) => {
        FeatureSpec {
            surface: $surface,
            subject: $subject,
            status: $status,
            replacement: feature_spec!(@opt $($replacement)?),
            note: feature_spec!(@opt $($note)?),
        }
    };
    (@opt) => { None };
    (@opt $val:expr) => { Some($val) };
}

/// The single lifecycle source of truth: every non-Stable product
/// surface this repository currently describes, across every
/// [`Surface`] kind in use. Both the CLI notice path
/// ([`LifecycleObserve::observe`] call sites below) and
/// all lifecycle consumers read this one table instead of each re-encoding
/// which things are experimental or deprecated; adding a new
/// lifecycle-tagged surface means adding one entry here, not touching
/// notice plumbing and reference metadata independently.
///
/// Command descriptions deliberately omit lifecycle wording.
/// Runtime notices and lifecycle metadata must agree with this table while
/// command help remains lifecycle-neutral.
pub const REGISTRY: &[FeatureSpec] = &[
    feature_spec!(
        surface: Surface::Command("selection"),
        subject: "`gat selection`",
        status: Status::Experimental,
    ),
    feature_spec!(
        surface: Surface::Command("gc"),
        subject: "`gat gc`",
        status: Status::Experimental,
    ),
    feature_spec!(
        surface: Surface::Command("mount"),
        subject: "`gat mount`",
        status: Status::Experimental,
    ),
    feature_spec!(
        surface: Surface::Command("route"),
        subject: "`gat route`",
        status: Status::Experimental,
    ),
    feature_spec!(
        surface: Surface::Command("system"),
        subject: "`gat system`",
        status: Status::Experimental,
    ),
    feature_spec!(
        surface: Surface::SettingKey("lock.shard_levels"),
        subject: "`lock.shard_levels`",
        status: Status::Experimental,
    ),
    feature_spec!(
        surface: Surface::ConfigValue {
            key: "cache.ingest_strategy",
            value: "safe",
        },
        subject: "`cache.ingest_strategy=safe`",
        status: Status::Stable,
    ),
    feature_spec!(
        surface: Surface::ConfigValue {
            key: "cache.ingest_strategy",
            value: "hybrid",
        },
        subject: "`cache.ingest_strategy=hybrid`",
        status: Status::Experimental,
    ),
    // `safe` is the stable default, not a semantic replacement for
    // `mmap`; keep this factual
    // rather than implying a drop-in migration.
    feature_spec!(
        surface: Surface::ConfigValue {
            key: "cache.ingest_strategy",
            value: "mmap",
        },
        subject: "`cache.ingest_strategy=mmap`",
        status: Status::Deprecated,
        note: "`safe` is the stable default.",
    ),
    feature_spec!(
        surface: Surface::ConfigAlias {
            canonical: "git.ignore_patterns",
            alias: "git.exclude_patterns",
        },
        subject: "`git.exclude_patterns`",
        status: Status::Deprecated,
        replacement: "git.ignore_patterns",
    ),
];

/// Looks up the [`FeatureSpec`] whose [`FeatureSpec::surface`] equals
/// `surface` in [`REGISTRY`] -- the one lookup primitive every typed
/// helper below, and [`LifecycleObserve::observe`], delegate to, so
/// adding a new [`Surface`] kind never means adding a new lookup
/// mechanism, only a new thin wrapper (if one is convenient) around
/// this.
pub(crate) fn find(surface: Surface<'_>) -> Option<&'static FeatureSpec> {
    REGISTRY.iter().find(|spec| spec.surface == surface)
}

/// Every [`Surface::Command`] entry in [`REGISTRY`], for
/// `tests/lifecycle_consistency.rs` to enumerate.
pub fn commands() -> impl Iterator<Item = &'static FeatureSpec> {
    REGISTRY
        .iter()
        .filter(|spec| matches!(spec.surface, Surface::Command(_)))
}

/// The [`Status::Experimental`] subset of [`commands`]. Distinct from
/// `commands()` so a status-specific invariant (e.g. "must have
/// Experimental `--help` text and produce an
/// [`NoticeKind::Experimental`] notice") can be tested against exactly
/// the commands it applies to, without assuming every registered command
/// surface shares one status.
pub fn experimental_commands() -> impl Iterator<Item = &'static FeatureSpec> {
    commands().filter(|spec| spec.status == Status::Experimental)
}

/// Looks up a command [`FeatureSpec`] by its top-level clap command name
/// (e.g. `"gc"`, `"source"`, `"mount"`) -- used by tests; runtime dispatch
/// (`app::run`) reports through `lifecycle_sink.observe(Surface::Command(name))`
/// directly instead of looking the spec up itself.
#[must_use]
pub fn command(name: &str) -> Option<&'static FeatureSpec> {
    find(Surface::Command(name))
}

/// Where a [`Surface`]'s warning is assigned -- a small, feature-agnostic
/// enum consumers match on instead of
/// interpreting [`Surface`]/[`Status`] itself. See [`doc_target_for`] for
/// the (exhaustive, per-[`Surface`]-variant) mapping.
#[derive(Debug, Clone, Copy)]
pub enum DocTarget<'a> {
    /// The command page identified by `<slug>`.
    CommandPage(&'a str),
    /// The configuration section for a canonical `gat.yaml` key.
    SettingKey(&'a str),
}

/// Compares by variant + string content only, independent of lifetime,
/// so a caller-provided borrow (e.g. `command_page`'s `slug: &str`) can
/// be compared against a `'static` [`DocTarget`] derived from
/// [`REGISTRY`] without forcing either side to be `'static`.
impl<'b> PartialEq<DocTarget<'b>> for DocTarget<'_> {
    fn eq(&self, other: &DocTarget<'b>) -> bool {
        match (self, other) {
            (DocTarget::CommandPage(a), DocTarget::CommandPage(b)) => a == b,
            (DocTarget::SettingKey(a), DocTarget::SettingKey(b)) => a == b,
            (DocTarget::CommandPage(_), DocTarget::SettingKey(_))
            | (DocTarget::SettingKey(_), DocTarget::CommandPage(_)) => false,
        }
    }
}
impl Eq for DocTarget<'_> {}

/// Maps a [`Surface`] to the [`DocTarget`] that owns its warning.
///
/// Exhaustive over every [`Surface`] variant on purpose: ownership is a decision
/// made once here, at the type boundary, rather than a `doc_target`
/// override on [`FeatureSpec`] or a second mapping table, so adding a new
/// `Surface` variant forces a compile-time decision about where its
/// warnings belong instead of silently falling back to bespoke generator
/// logic.
#[must_use]
pub const fn doc_target_for(surface: Surface<'static>) -> DocTarget<'static> {
    match surface {
        Surface::Command(command) => DocTarget::CommandPage(command),
        Surface::CommandAlias { command, .. } => DocTarget::CommandPage(command),
        Surface::CommandOption { command, .. } => DocTarget::CommandPage(command),
        Surface::OptionAlias { command, .. } => DocTarget::CommandPage(command),
        Surface::OptionValue { command, .. } => DocTarget::CommandPage(command),
        Surface::SettingKey(key) => DocTarget::SettingKey(key),
        Surface::ConfigAlias { canonical, .. } => DocTarget::SettingKey(canonical),
        Surface::ConfigValue { key, .. } => DocTarget::SettingKey(key),
    }
}

/// One warning derived from a non-Stable [`REGISTRY`] entry: plain warning
/// text plus the generic status
/// label a renderer may use as page metadata (e.g. a Mintlify `tag`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocWarning {
    pub target: DocTarget<'static>,
    /// The originating [`FeatureSpec::surface`] this warning was
    /// projected from -- carried through unchanged so a caller (or a
    /// test) can prove a specific [`REGISTRY`] entry appears in its doc
    /// page, rather than only being able to count warnings by status
    /// label.
    pub surface: Surface<'static>,
    pub text: String,
    pub tag: &'static str,
}

/// Renders the plain warning text for a non-Stable [`FeatureSpec`], or
/// `None` for [`Status::Stable`] (nothing to warn about). Shares the same
/// [`lifecycle_reason`] wording decision tree as the runtime notice path
/// -- only the Markdown bold status word and the doc link wrapped around
/// it are specific to this renderer.
fn doc_warning_text(spec: &FeatureSpec) -> Option<String> {
    let kind = match spec.status {
        Status::Stable => return None,
        Status::Experimental => NoticeKind::Experimental,
        Status::Deprecated => NoticeKind::Deprecated,
    };
    Some(format!(
        "{} is **{}**: {} See [Feature lifecycle](/references/feature-lifecycle).",
        spec.subject,
        spec.status.label(),
        lifecycle_reason(kind, spec.replacement, spec.note)
    ))
}

/// Every [`REGISTRY`] entry projected into a [`DocWarning`], skipping
/// [`Status::Stable`] entries (nothing to warn about). This is the single
/// feature-agnostic feed consumers read instead of
/// inspecting [`FeatureSpec`]/[`Status`]/[`Surface`] itself.
pub fn doc_warnings() -> impl Iterator<Item = DocWarning> {
    REGISTRY.iter().filter_map(|spec| {
        let text = doc_warning_text(spec)?;
        Some(DocWarning {
            target: doc_target_for(spec.surface),
            surface: spec.surface,
            text,
            tag: spec.status.label(),
        })
    })
}

/// All warnings and metadata for one [`DocTarget`],
/// e.g. every warning that belongs on the `gat gc` command page or in
/// `git.ignore_patterns`'s configuration section. `tag` is the page-level
/// status label, if this exact target is itself a non-Stable surface
/// (e.g. `Surface::Command("gc")` for `DocTarget::CommandPage("gc")`) --
/// distinct from `warnings`, which also includes non-Stable *child*
/// surfaces (aliases/options/values) that merely render a warning on the
/// page without making the whole page's own status non-Stable.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DocPage {
    pub tag: Option<&'static str>,
    pub warnings: Vec<DocWarning>,
}

/// Collects every [`DocWarning`] whose [`DocTarget`] equals `target`,
/// used by both [`command_page`] and [`config_key`] below. A page's own
/// `tag` is only set by the entry whose `Surface` *is* that page's
/// top-level surface (`Surface::Command` for a command page,
/// `Surface::SettingKey` for a config section) -- a non-Stable child
/// surface (alias/option/value) still renders a warning on the page
/// without making the whole page's own status non-Stable.
fn doc_page(target: DocTarget<'_>) -> DocPage {
    let mut page = DocPage::default();
    for spec in REGISTRY {
        let spec_target = doc_target_for(spec.surface);
        if spec_target != target {
            continue;
        }
        let Some(text) = doc_warning_text(spec) else {
            continue;
        };
        let is_owning_surface = matches!(
            (spec_target, spec.surface),
            (DocTarget::CommandPage(_), Surface::Command(_))
                | (DocTarget::SettingKey(_), Surface::SettingKey(_))
        );
        if is_owning_surface {
            page.tag = Some(spec.status.label());
        }
        page.warnings.push(DocWarning {
            target: spec_target,
            surface: spec.surface,
            text,
            tag: spec.status.label(),
        });
    }
    page
}

/// The [`DocPage`] for the command identified by `<slug>`,
/// covering the command itself and every child surface (aliases,
/// options, option aliases, option values) documented on the same page.
#[must_use]
pub fn command_page(slug: &str) -> DocPage {
    doc_page(DocTarget::CommandPage(slug))
}

/// The [`DocPage`] for the configuration key `key`,
/// covering the config key itself and every alias/value documented
/// beneath it (e.g. `git.exclude_patterns` beneath `git.ignore_patterns`,
/// or every `cache.ingest_strategy` value beneath
/// `cache.ingest_strategy`).
#[must_use]
pub fn config_key(key: &str) -> DocPage {
    doc_page(DocTarget::SettingKey(key))
}

/// Registry-aware extension of the neutral core [`Lifecycle`] sink:
/// looks `surface` up in [`REGISTRY`], turns it into a [`Notice`] via
/// [`notice_for`], and [`Lifecycle::record`]s the result. This is a
/// separate trait (rather than an inherent method) because [`Lifecycle`]
/// itself is defined in `gat_core` and has no registry to look
/// `surface` up in -- keeping this lookup in root is exactly what keeps
/// the core type product-agnostic.
///
/// One [`Lifecycle`] lives on `app::Context` for the whole process
/// invocation (see `Context::lifecycle`); the process boundary
/// (`main.rs`) drains and renders it via `output::notices::emit` after
/// progress is cleared, but *before* the dispatched command's own
/// success/error result is handled -- so a failing experimental command
/// still surfaces its notice, without `app::run`'s signature growing a
/// notices list alongside `Result<Outcome>`.
pub trait LifecycleObserve {
    /// Records that `surface` was actually observed at some semantic
    /// boundary -- a command was dispatched, a config key was read or
    /// written, or an already-persisted config value was consumed by
    /// behavior. Callers state only *which* surface they observed;
    /// looking it up in [`REGISTRY`] and deciding whether it warrants a
    /// notice is entirely this method's job, not the call site's. A
    /// no-op if `surface` has no registry entry, if its status is
    /// [`Status::Stable`] (see [`notice_for`]), or if it has already
    /// been observed once this invocation.
    fn observe(&self, surface: Surface<'_>);
}

impl LifecycleObserve for Lifecycle {
    fn observe(&self, surface: Surface<'_>) {
        let Some(spec) = find(surface) else {
            return;
        };
        let Some(notice) = notice_for(spec) else {
            return;
        };
        self.record(notice);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mmap` has no direct replacement (`safe` is the stable default, not
    /// a semantic successor for `mmap`), so the notice stays factual rather
    /// than implying a drop-in migration.
    #[test]
    fn mmap_deprecated_notice_has_no_replacement_and_names_the_stable_default() {
        let mmap = find(Surface::ConfigValue {
            key: "cache.ingest_strategy",
            value: "mmap",
        })
        .unwrap();
        assert_eq!(mmap.replacement, None);
        let notice = notice_for(mmap).unwrap();
        assert_eq!(notice.kind, NoticeKind::Deprecated);
        assert_eq!(notice.replacement, None);
        let reason = lifecycle_reason(notice.kind, notice.replacement, notice.note);
        assert_eq!(reason, "`safe` is the stable default.");
    }

    #[test]
    fn command_finds_gc_mount_route_selection_system_and_nothing_else() {
        for name in ["gc", "mount", "route", "selection", "system"] {
            assert!(command(name).is_some(), "missing spec for {name}");
        }
        assert!(command("status").is_none());
        assert!(command("hooks").is_none());
    }

    #[test]
    fn ingest_strategy_value_matches_all_three_values() {
        assert_eq!(
            find(Surface::ConfigValue {
                key: "cache.ingest_strategy",
                value: "safe",
            })
            .unwrap()
            .status,
            Status::Stable
        );
        assert_eq!(
            find(Surface::ConfigValue {
                key: "cache.ingest_strategy",
                value: "hybrid",
            })
            .unwrap()
            .status,
            Status::Experimental
        );
        assert_eq!(
            find(Surface::ConfigValue {
                key: "cache.ingest_strategy",
                value: "mmap",
            })
            .unwrap()
            .status,
            Status::Deprecated
        );
        assert!(
            find(Surface::ConfigValue {
                key: "cache.ingest_strategy",
                value: "teleport",
            })
            .is_none()
        );
    }

    /// A [`Surface::ConfigValue`] must never collide with a
    /// [`Surface::Command`]/[`Surface::ConfigAlias`] sharing a similarly
    /// spelled field, since [`Surface`] variants (and therefore `find`'s
    /// equality check) are distinguished by variant, not just field
    /// content.
    #[test]
    fn find_is_scoped_by_surface_variant_not_just_field_content() {
        assert!(
            find(Surface::ConfigValue {
                key: "cmd",
                value: "gc",
            })
            .is_none()
        );
        assert!(find(Surface::Command("gc")).is_some());
    }

    #[test]
    fn commands_iterator_yields_exactly_gc_mount_route_selection_system() {
        let mut names: Vec<&str> = commands()
            .map(|spec| match spec.surface {
                Surface::Command(name) => name,
                other => panic!("unexpected non-command surface in commands(): {other:?}"),
            })
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["gc", "mount", "route", "selection", "system"]);
    }

    #[test]
    fn experimental_commands_iterator_yields_only_experimental_status_commands() {
        let mut names: Vec<&str> = experimental_commands()
            .map(|spec| match spec.surface {
                Surface::Command(name) => name,
                other => {
                    panic!("unexpected non-command surface in experimental_commands(): {other:?}")
                }
            })
            .collect();
        names.sort_unstable();
        assert_eq!(names, ["gc", "mount", "route", "selection", "system"]);
        assert!(experimental_commands().all(|spec| spec.status == Status::Experimental));
    }

    #[test]
    fn doc_target_for_is_exhaustive_and_derives_command_and_config_ownership() {
        assert_eq!(
            doc_target_for(Surface::Command("gc")),
            DocTarget::CommandPage("gc")
        );
        assert_eq!(
            doc_target_for(Surface::CommandAlias {
                command: "gc",
                alias: "garbage-collect"
            }),
            DocTarget::CommandPage("gc")
        );
        assert_eq!(
            doc_target_for(Surface::CommandOption {
                command: "gc",
                option: "--dry-run"
            }),
            DocTarget::CommandPage("gc")
        );
        assert_eq!(
            doc_target_for(Surface::OptionAlias {
                command: "gc",
                option: "--dry-run",
                alias: "-n"
            }),
            DocTarget::CommandPage("gc")
        );
        assert_eq!(
            doc_target_for(Surface::OptionValue {
                command: "gc",
                option: "--mode",
                value: "aggressive"
            }),
            DocTarget::CommandPage("gc")
        );
        assert_eq!(
            doc_target_for(Surface::SettingKey("git.ignore_patterns")),
            DocTarget::SettingKey("git.ignore_patterns")
        );
        assert_eq!(
            doc_target_for(Surface::ConfigAlias {
                canonical: "git.ignore_patterns",
                alias: "git.exclude_patterns"
            }),
            DocTarget::SettingKey("git.ignore_patterns")
        );
        assert_eq!(
            doc_target_for(Surface::ConfigValue {
                key: "cache.ingest_strategy",
                value: "mmap"
            }),
            DocTarget::SettingKey("cache.ingest_strategy")
        );
    }

    #[test]
    fn doc_warnings_skip_stable_entries() {
        assert!(
            doc_warnings().all(|w| w.tag != Status::Stable.label()),
            "a Stable REGISTRY entry must never surface a doc warning"
        );
    }

    /// Every non-Stable [`REGISTRY`] entry with a doc target must round
    /// trip into exactly one [`command_page`]/[`config_key`] warning,
    /// identified by its own [`Surface`] -- not merely "at least one
    /// warning with a matching status label", which two Experimental
    /// child surfaces on the same page could satisfy even if the specific
    /// entry under test were missing or duplicated. Proves the projection
    /// is generic over the whole registry, not hard-coded to today's
    /// specific features.
    #[test]
    fn every_non_stable_registry_entry_appears_exactly_once_in_its_doc_page() {
        for spec in REGISTRY {
            if spec.status == Status::Stable {
                continue;
            }
            let page = match doc_target_for(spec.surface) {
                DocTarget::CommandPage(slug) => command_page(slug),
                DocTarget::SettingKey(key) => config_key(key),
            };
            let matches = page
                .warnings
                .iter()
                .filter(|w| w.surface == spec.surface)
                .count();
            assert_eq!(
                matches, 1,
                "expected {:?} to surface exactly one warning identified by its own surface \
                 in its doc page, found {matches}",
                spec.surface
            );
        }
    }

    #[test]
    fn command_page_tag_is_set_only_by_the_command_surface_itself_not_by_child_surfaces() {
        let gc = command_page("gc");
        assert_eq!(gc.tag, Some("Experimental"));

        // `cache.ingest_strategy`'s non-Stable *values* must not force a
        // page-level tag on their own -- only a `Surface::SettingKey`
        // entry for that key would (there is none registered today).
        let ingest_strategy = config_key("cache.ingest_strategy");
        assert_eq!(ingest_strategy.tag, None);
        assert!(ingest_strategy.warnings.len() >= 2);
    }

    #[test]
    fn config_alias_warning_surfaces_under_the_canonical_key_not_the_alias() {
        let canonical = config_key("git.ignore_patterns");
        assert!(
            canonical
                .warnings
                .iter()
                .any(|w| w.text.contains("git.exclude_patterns"))
        );
        let alias_page = config_key("git.exclude_patterns");
        assert!(
            alias_page.warnings.is_empty(),
            "the alias's own (non-canonical) key must not be a doc target"
        );
    }

    #[test]
    fn ingest_strategy_values_iterator_yields_exactly_three_values() {
        let mut values: Vec<&str> = REGISTRY
            .iter()
            .filter_map(|spec| match spec.surface {
                Surface::ConfigValue {
                    key: "cache.ingest_strategy",
                    value,
                } => Some(value),
                _ => None,
            })
            .collect();
        values.sort_unstable();
        assert_eq!(values, ["hybrid", "mmap", "safe"]);
    }

    #[test]
    fn lifecycle_observe_deduplicates_by_surface() {
        let lifecycle = Lifecycle::new();
        lifecycle.observe(Surface::Command("gc"));
        lifecycle.observe(Surface::Command("gc"));
        assert_eq!(lifecycle.take_notices().len(), 1);
    }

    #[test]
    fn lifecycle_observe_keeps_distinct_surfaces() {
        let lifecycle = Lifecycle::new();
        lifecycle.observe(Surface::Command("gc"));
        lifecycle.observe(Surface::Command("mount"));
        assert_eq!(lifecycle.take_notices().len(), 2);
    }

    #[test]
    fn lifecycle_observe_ignores_a_surface_with_no_registry_entry() {
        let lifecycle = Lifecycle::new();
        lifecycle.observe(Surface::Command("not-in-registry"));
        assert!(lifecycle.take_notices().is_empty());
    }

    /// `observe` looks the registry entry's status up itself (not just
    /// deduplication) -- a Stable surface must stay a no-op.
    #[test]
    fn lifecycle_observe_ignores_a_stable_surface() {
        let lifecycle = Lifecycle::new();
        lifecycle.observe(Surface::ConfigValue {
            key: "cache.ingest_strategy",
            value: "safe",
        });
        assert!(
            lifecycle.take_notices().is_empty(),
            "safe is Stable: no notice"
        );
    }

    #[test]
    fn lifecycle_take_notices_drains_and_resets() {
        let lifecycle = Lifecycle::new();
        lifecycle.observe(Surface::Command("gc"));
        assert_eq!(lifecycle.take_notices().len(), 1);
        assert!(lifecycle.take_notices().is_empty());
    }
}
