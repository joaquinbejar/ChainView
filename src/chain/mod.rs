//! The normalized option-chain domain.
//!
//! Will define the `ChainStore` (poll → stream merge over the
//! `optionstratlib` chain model), instrument identity, and the normalized
//! streaming update events. Lands with issues #4–#7.
//!
//! For now it carries only the minimal [`ProviderId`] placeholder that the
//! boundary error types (issue #2) need to name a provider; issue #4 replaces
//! it with the full open, validated newtype.

use std::fmt;

/// A market-data provider identity — the registry key, the config namespace
/// segment, and the log label for an adapter.
///
/// **Placeholder (issue #2).** This minimal form exists so the boundary error
/// types can name a provider without depending on the full validated newtype.
/// Issue #4 reconciles it into the open, validated `^[a-z][a-z0-9_-]{1,31}$`
/// newtype with `serde` support and reserved-id handling
/// (`docs/01-domain-model.md` §4). Until then it performs **no** grammar
/// validation and its constructor is infallible. It carries no credential —
/// the inner string is the public, non-secret provider id.
///
/// Ordering (`PartialOrd`/`Ord`) delegates to the inner string and is present
/// so `ProviderId` can key a `BTreeMap` — `Config::providers`
/// (`docs/07-configuration.md` §3) — ahead of issue #4; the final newtype
/// derives the same ordering.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    /// Construct a provider id from any string-like value.
    ///
    /// **Placeholder:** performs no grammar validation yet — issue #4 makes
    /// this validated and fallible (`Result<Self, ConfigError>`). Callers
    /// should not rely on the current infallible signature.
    #[must_use]
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice — its canonical config/log/wire form.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
