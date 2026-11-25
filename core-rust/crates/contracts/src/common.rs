use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// u64 carried as a decimal string on the JSON boundary (02 §2.1 / boundary rule 3).
/// Only for u64 values that can reach the Web: OSM / Valhalla derived IDs and revisions
/// (`trip_revision`, `route_revision`, edge ids). Everything else is a JSON number.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct U64Str(pub u64);

impl Serialize for U64Str {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(&self.0)
    }
}

impl<'de> Deserialize<'de> for U64Str {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        // Same rule as common.schema.json#/$defs/U64Str: no sign, no leading zeros.
        let ok = !s.is_empty()
            && s.len() <= 20
            && s.bytes().all(|b| b.is_ascii_digit())
            && (s == "0" || !s.starts_with('0'));
        if !ok {
            return Err(serde::de::Error::custom("U64Str: expected decimal string"));
        }
        s.parse::<u64>()
            .map(U64Str)
            .map_err(serde::de::Error::custom)
    }
}

/// Monotonic milliseconds (SystemClock.elapsedRealtime basis). Rust never reads a clock.
/// JSON integer (number), not a decimal string (ADR-0008 decision 2, 04 §2).
pub type MonoMs = u64;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteKey {
    pub trip_id: String,
    pub trip_revision: U64Str,
    pub route_revision: U64Str,
    pub graph_version: String,
}

/// `Option<T>` that must be present (may be `null`). Without `deserialize_with`, serde would
/// treat a missing `Option` field as `None`; tick.schema.json requires the key.
pub(crate) fn required_nullable<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::<T>::deserialize(d)
}
