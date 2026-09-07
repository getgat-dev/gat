//! Persistent, lexical identity types for remotes, routes, mounts, and selections.
//!
//! [`RemoteName`], [`RouteName`], and [`MountName`] wrap the map-key text
//! Gat already accepts for `remotes:`, `routes:`, and `mounts:` config
//! entries (and the corresponding `gat remote`/`route`/`mount` CLI
//! arguments) without introducing any new identifier grammar: today's
//! accepted character set is unchanged, and reserved-name policy (the
//! remote name `"default"`, the synthetic route name `"*"`) stays in the
//! command/config-validation layers that already enforce it rather than
//! being encoded here.
//!
//! These wrappers keep independently meaningful
//! kinds of persisted name from being confused with each other, or with
//! unrelated `String`s, at the type level -- mirroring the opaque,
//! unvalidated style of [`crate::git::GitRevisionSpec`] rather than
//! the strict canonicalization of [`crate::lexical_path::GatPath`].

use std::borrow::Borrow;
use std::fmt;

macro_rules! persistent_name {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Wraps an owned name `String`, moving its existing
            /// allocation rather than copying it -- e.g. a CLI argument,
            /// or a config map key straight off deserialization.
            pub const fn from_string(value: String) -> Self {
                Self(value)
            }

            /// Borrows the name text, for map lookups, comparisons, or
            /// rendering in diagnostics/display.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str {
                &self.0
            }
        }

        impl Borrow<str> for $name {
            fn borrow(&self) -> &str {
                &self.0
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self::from_string(value)
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self::from_string(value.to_string())
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl PartialEq<str> for $name {
            fn eq(&self, other: &str) -> bool {
                self.0 == other
            }
        }

        impl PartialEq<$name> for str {
            fn eq(&self, other: &$name) -> bool {
                self == other.0
            }
        }

        impl PartialEq<&str> for $name {
            fn eq(&self, other: &&str) -> bool {
                self.0 == *other
            }
        }

        impl PartialEq<$name> for &str {
            fn eq(&self, other: &$name) -> bool {
                *self == other.0
            }
        }

        /// Serializes as the unchanged name text, so round-tripping this
        /// value through persisted config reproduces exactly what the
        /// user/config wrote.
        impl serde::Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        /// Deserializes via `from_string`, retaining the
        /// deserialized `String`'s own allocation -- no additional
        /// identifier-grammar validation is performed here (see the
        /// type's own doc comment).
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                String::deserialize(deserializer).map(Self::from_string)
            }
        }
    };
}

persistent_name!(
    /// The persistent name of a configured remote -- a `remotes:` config
    /// map key, e.g. `"origin"` or the reserved `"default"` (reserved-ness
    /// is enforced by `commands::remote`, not by this type).
    RemoteName
);

persistent_name!(
    /// The persistent name of a configured route -- a `routes:` config
    /// map key, e.g. `"models"` (the synthetic `"*"` default-route name is
    /// enforced/rejected by `gat-command` and config validation, not by
    /// this type).
    RouteName
);

persistent_name!(
    /// The persistent name of a configured mount -- a `mounts:` config
    /// map key, e.g. `"models"`.
    MountName
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_through_serde_as_a_scalar_string() {
        let name = RemoteName::from_string("origin".to_string());
        let json = serde_json::to_string(&name).unwrap();
        assert_eq!(json, "\"origin\"");
        let back: RemoteName = serde_json::from_str(&json).unwrap();
        assert_eq!(back, name);
    }

    #[test]
    fn compares_equal_to_borrowed_str() {
        let name = RouteName::from_string("models".to_string());
        assert_eq!(name, "models");
        assert_eq!("models", name);
    }

    #[test]
    fn borrows_as_str_for_map_lookups() {
        use std::collections::BTreeMap;
        let mut map: BTreeMap<MountName, u32> = BTreeMap::new();
        map.insert(MountName::from_string("models".to_string()), 1);
        assert_eq!(map.get("models"), Some(&1));
    }

    #[test]
    fn displays_as_the_underlying_text() {
        let name = RemoteName::from_string("backup".to_string());
        assert_eq!(name.to_string(), "backup");
        assert_eq!(name.as_str(), "backup");
    }
}

persistent_name!(/// A reusable path selection name.
SelectionName);
