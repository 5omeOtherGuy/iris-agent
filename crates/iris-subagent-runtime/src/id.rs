use std::fmt;
use std::str::FromStr;

use rand::random;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::RuntimeError;

macro_rules! opaque_id {
    ($name:ident, $prefix:literal, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(String);

        impl $name {
            /// Creates a collision-resistant identifier.
            #[must_use]
            pub fn new() -> Self {
                Self(format!(concat!($prefix, "_{:032x}"), random::<u128>()))
            }

            /// Parses and validates an identifier received from storage or a host.
            pub fn parse(value: impl Into<String>) -> Result<Self, RuntimeError> {
                let value = value.into();
                let suffix = value.strip_prefix(concat!($prefix, "_")).ok_or_else(|| {
                    RuntimeError::InvalidId {
                        kind: stringify!($name),
                        value: value.clone(),
                    }
                })?;
                if suffix.len() != 32 || !suffix.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                    return Err(RuntimeError::InvalidId {
                        kind: stringify!($name),
                        value,
                    });
                }
                Ok(Self(value))
            }

            /// Returns the stable string representation.
            #[must_use]
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.debug_tuple(stringify!($name)).field(&self.0).finish()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = RuntimeError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::parse(value)
            }
        }

        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                serializer.serialize_str(&self.0)
            }
        }

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                let value = String::deserialize(deserializer)?;
                Self::parse(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

opaque_id!(
    WorkerId,
    "wrk",
    "Opaque identifier for one worker execution."
);
opaque_id!(GroupId, "grp", "Opaque identifier for a worker group.");
opaque_id!(
    WorktreeId,
    "wtr",
    "Opaque identifier for a managed worktree."
);
opaque_id!(
    ApplyPlanId,
    "apl",
    "Opaque identifier for an immutable apply plan."
);
opaque_id!(ArtifactId, "art", "Content-addressed artifact identifier.");
opaque_id!(
    InstanceId,
    "ins",
    "Opaque identifier for one runtime owner instance."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip_and_reject_path_material() {
        let id = WorkerId::new();
        let encoded = serde_json::to_string(&id).unwrap();
        assert_eq!(serde_json::from_str::<WorkerId>(&encoded).unwrap(), id);
        assert!(WorkerId::parse("wrk_../escape").is_err());
        assert_ne!(WorkerId::new(), WorkerId::new());
    }
}
