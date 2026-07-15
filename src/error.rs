//! Boundary error types for ChainView.
//!
//! [`ChainViewError`] is the shared boundary every provider, bundle, config,
//! registry, and terminal error converts into — no upstream error type ever
//! reaches a widget (`docs/01-domain-model.md` §11).
//!
//! The binding property is **redaction-safe by construction**: a secret cannot
//! be interpolated into any `Display` here, not by author discipline but by the
//! shape of the types. [`ProviderError`] carries **no free-form `String` from
//! adapter internals** — transport detail is a [`Redacted`] trait object (a
//! category plus a masked summary, e.g. `"transport: http 503"`, never a URL or
//! a body), and a normalize failure is the closed [`NormalizeKind`] enum naming
//! a **field**, never a value (`docs/03-data-providers.md` §6). The credential
//! guarantee these shapes enforce is stated in `docs/SECURITY.md` §1.

use std::fmt;
use std::time::Duration;

use crate::chain::ProviderId;

/// The single boundary error every ChainView layer converts into.
///
/// Each sub-boundary maps in either through `#[from]` (where the mapping is
/// unambiguous — `Bundle`/`Config`/`Registry`) or through the explicit
/// [`ChainViewError::provider`] helper (the `Provider` variant additionally
/// carries the `ProviderId`, so the conversion is deliberately not a blanket
/// `From`). No variant carries a raw upstream string or a credential.
#[derive(Debug, thiserror::Error)]
pub enum ChainViewError {
    /// A provider adapter failed. Carries the provider identity alongside the
    /// typed, redaction-safe [`ProviderError`]. Built via
    /// [`ChainViewError::provider`], never a blanket `From`, so the call site
    /// names the responsible provider.
    #[error("provider {provider}: {source}")]
    Provider {
        /// Which provider raised the error — its public, non-secret id.
        provider: ProviderId,
        /// The typed, redaction-safe provider failure.
        #[source]
        source: ProviderError,
    },
    /// A result-bundle read or validation failed (replay mode).
    #[error("result bundle: {0}")]
    Bundle(#[from] BundleError),
    /// Configuration was missing or invalid. Names the provider/field, never a
    /// credential value.
    #[error("config: {0}")]
    Config(#[from] ConfigError),
    /// The provider registry rejected an assembly (reserved/duplicate id or an
    /// empty set).
    #[error("provider registry: {0}")]
    Registry(#[from] RegistryError),
    /// A terminal-backend operation failed (raw mode, alternate screen, draw).
    /// The detail is a non-secret, ChainView-authored string.
    #[error("terminal: {0}")]
    Terminal(String),
}

impl ChainViewError {
    /// Wrap a [`ProviderError`] with the identity of the provider that raised
    /// it, producing a [`ChainViewError::Provider`].
    ///
    /// This is the deliberate replacement for a blanket `From<ProviderError>`:
    /// the `Provider` variant carries the `ProviderId`, so the conversion is
    /// explicit at every call site rather than an ambiguous auto-conversion.
    #[cold]
    #[inline(never)]
    #[must_use]
    pub fn provider(provider: ProviderId, source: ProviderError) -> Self {
        Self::Provider { provider, source }
    }
}

/// Raised while assembling the provider registry at startup
/// (`docs/02-tui-architecture.md` §11, ADR-0006). A collision is a typed error,
/// never a panic or a silent last-writer-wins. Every variant names only a
/// public provider id — never a credential.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// An external registration reused one of `RESERVED_PROVIDER_IDS`.
    #[error("provider id `{0}` is reserved for a built-in adapter")]
    ReservedId(ProviderId),
    /// Two registrations share the same id.
    #[error("provider id `{0}` is already registered")]
    DuplicateId(ProviderId),
    /// No providers were registered before startup.
    #[error("no providers registered")]
    Empty,
}

/// A failure reading or validating an IronCondor result bundle (replay mode).
///
/// Every variant is `String`-detailed with a **non-secret**, ChainView-authored
/// message — a bundle is trusted-but-verified local data, never a credential
/// source (`docs/04-replay-mode.md` §5).
#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    /// A required Parquet table (or manifest) was absent from the bundle
    /// directory.
    #[error("missing table: {0}")]
    MissingTable(String),
    /// The `manifest.schema` tag is not a supported bundle version.
    #[error("unsupported schema: {0}")]
    UnsupportedSchema(String),
    /// A cross-table or domain invariant was violated on load.
    #[error("invariant violated: {0}")]
    Invariant(String),
    /// A resource ceiling was exceeded before materialisation
    /// (`docs/04-replay-mode.md` §3).
    #[error("bundle too large: {0}")]
    TooLarge(String),
    /// A Parquet decode error, summarized without leaking file internals.
    #[error("parquet: {0}")]
    Parquet(String),
}

/// A configuration failure surfaced at startup.
///
/// [`ConfigError::MissingCredential`] names the **provider**, never the key or
/// the secret itself — the credential guarantee (`docs/SECURITY.md` §1) is why
/// no variant here can carry secret material.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A provider that requires authentication has no credential configured.
    /// Names the provider only — never the missing key or its value.
    #[error("missing credential for provider {0}")]
    MissingCredential(ProviderId),
    /// A configured provider id is not a known/registered provider.
    #[error("unknown provider: {0}")]
    UnknownProvider(String),
    /// A configuration value failed validation. `field` names the setting and
    /// `reason` explains why — neither carries a credential.
    #[error("invalid value for {field}: {reason}")]
    InvalidValue {
        /// The configuration field that failed validation.
        field: String,
        /// Why the value was rejected — a non-secret explanation.
        reason: String,
    },
}

/// Raised when a **cross-provider** overlay merge is refused because feed
/// identity does not prove contract equivalence (`docs/01-domain-model.md` §4
/// economic-equivalence gate).
///
/// It is a **per-leg, non-fatal** outcome: the offending overlay leg is dropped
/// (not merged), the source leg is kept, and the leg is badged overlay-refused
/// — it never aborts the app or blanks the screen, so it is deliberately **not**
/// a [`ChainViewError`] variant. It names the disagreeing spec dimension, never
/// a raw credential or payload.
///
/// `Display`/`Error` are hand-implemented rather than derived via `thiserror`:
/// the `source` field name (fixed by `docs/01-domain-model.md` §11) is reserved
/// by `thiserror` for the error-source chain, which would require `String:
/// Error`. Hand-implementing preserves the documented public field names exactly
/// while still yielding a typed [`std::error::Error`].
#[derive(Debug)]
pub enum OverlayError {
    /// A fingerprint dimension (multiplier / settlement / exercise / quote
    /// currency / venue product code) disagreed between the source and overlay
    /// feeds for one contract.
    SpecMismatch {
        /// The normalized contract label (non-secret).
        contract: String,
        /// The fingerprint dimension that disagreed. `&'static str` — a
        /// compile-time dimension name, so runtime data can never occupy this
        /// slot.
        field: &'static str,
        /// The source feed's value for that dimension.
        source: String,
        /// The overlay feed's value for that dimension.
        overlay: String,
    },
}

impl fmt::Display for OverlayError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SpecMismatch {
                contract,
                field,
                source,
                overlay,
            } => write!(
                f,
                "overlay spec mismatch on {field} for {contract}: \
                 source `{source}` vs overlay `{overlay}`"
            ),
        }
    }
}

impl std::error::Error for OverlayError {}

/// A redaction-safe detail attached to a transport failure.
///
/// Its `Display`/`Debug` output is what reaches a ChainView log or the UI, so
/// it **must** be safe: the contract is "emit a category and a masked summary,
/// never raw upstream text". ChainView provides the safe [`TransportDetail`]
/// implementation; an external adapter may implement `Redacted` for its own
/// detail and is contractually barred from interpolating a secret
/// (`docs/SECURITY.md` §5). The trait has no methods — it is a marker plus the
/// `Display + Debug + Send + Sync` bound that makes the detail loggable and
/// thread-safe.
pub trait Redacted: fmt::Display + fmt::Debug + Send + Sync {}

/// The category of a transport failure. A small, stable, closed set — safe to
/// render because it is a fixed vocabulary, never venue-controlled text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TransportKind {
    /// The request or connection timed out.
    Timeout,
    /// The connection was closed by the peer or dropped.
    Closed,
    /// A TLS/handshake failure.
    Tls,
    /// An HTTP-level failure (see the optional status).
    Http,
    /// A response could not be decoded at the transport layer.
    Decode,
}

/// Opaque, redaction-safe transport detail.
///
/// Built from a small closed set of causes plus an optional HTTP status —
/// **never** from `format!(upstream_err)`. Its `Display` emits only a category
/// and a status (e.g. `"transport: http 503"`); it has no field that could hold
/// a URL, a request body, or a token, so it cannot leak one by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransportDetail {
    /// The failure category.
    pub kind: TransportKind,
    /// The HTTP status code when the failure was HTTP-level — the status only,
    /// never the response body.
    pub http_status: Option<u16>,
}

impl TransportDetail {
    /// Construct a redaction-safe transport detail from a category and an
    /// optional HTTP status.
    #[cold]
    #[inline(never)]
    #[must_use]
    pub fn new(kind: TransportKind, http_status: Option<u16>) -> Self {
        Self { kind, http_status }
    }
}

impl fmt::Display for TransportDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            TransportKind::Timeout => "timeout",
            TransportKind::Closed => "closed",
            TransportKind::Tls => "tls",
            TransportKind::Http => "http",
            TransportKind::Decode => "decode",
        };
        match self.http_status {
            Some(status) => write!(f, "transport: {kind} {status}"),
            None => write!(f, "transport: {kind}"),
        }
    }
}

impl Redacted for TransportDetail {}

/// The typed, redaction-safe failure an adapter raises.
///
/// It carries **no free-form `String` from adapter internals**: transport
/// detail is a [`Redacted`] trait object and a normalize failure is the closed
/// [`NormalizeKind`] enum — so a token, an authenticated URL, or a raw payload
/// cannot reach displayed error text or a log (`docs/03-data-providers.md` §6).
/// Convert into [`ChainViewError`] via [`ChainViewError::provider`].
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    /// The provider does not support the requested operation. The message is a
    /// compile-time `&'static str`, never runtime data.
    #[error("not supported by this provider: {0}")]
    Unsupported(&'static str),
    /// Authentication failed. Never carries the credential.
    #[error("authentication failed")]
    Auth,
    /// An upstream transport failure, described by a redaction-safe
    /// [`Redacted`] detail — never a raw upstream string.
    #[error("upstream transport: {0}")]
    Transport(Box<dyn Redacted>),
    /// A payload would not map to the chain model. Carries the closed
    /// [`NormalizeKind`] reason (naming a field, not a value).
    #[error("normalize: {kind}")]
    Normalize {
        /// Why normalization failed — a closed set, no free-form payload text.
        kind: NormalizeKind,
    },
    /// The provider rate-limited the request. Carries the suggested retry delay
    /// when the upstream supplies one.
    #[error("rate limited; retry after {0:?}")]
    RateLimited(Option<Duration>),
    /// No chain exists for the requested underlying and expiration. Both are
    /// already-normalized, non-secret values.
    #[error("no chain for {underlying} @ {expiration}")]
    NoChain {
        /// The normalized underlying ticker.
        underlying: String,
        /// The normalized expiration label.
        expiration: String,
    },
}

/// Why a payload would not map to the chain model — a closed set, so a rejected
/// payload's raw bytes never ride along in the error.
///
/// `#[non_exhaustive]`: a new reason is a source-compatible addition; in-crate
/// match sites still exhaustiveness-check. Every data-bearing variant names a
/// **field** via a compile-time `&'static str`, never the offending value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum NormalizeKind {
    /// A required field was absent. Names the field, not the value.
    MissingField(&'static str),
    /// A field was present but outside its valid range. Names the field.
    OutOfRange(&'static str),
    /// A numeric field was NaN or infinite. Names the field.
    NonFinite(&'static str),
    /// An expiry could not be parsed to a single absolute UTC instant.
    UnparseableExpiry,
    /// An option style could not be resolved to call or put.
    UnknownStyle,
}

impl fmt::Display for NormalizeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingField(field) => write!(f, "missing field `{field}`"),
            Self::OutOfRange(field) => write!(f, "out-of-range field `{field}`"),
            Self::NonFinite(field) => write!(f, "non-finite field `{field}`"),
            Self::UnparseableExpiry => f.write_str("unparseable expiry"),
            Self::UnknownStyle => f.write_str("unknown option style"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider_id(id: &str) -> ProviderId {
        ProviderId::new(id)
    }

    #[test]
    fn test_transport_detail_display_emits_category_and_status_only() {
        let detail = TransportDetail::new(TransportKind::Http, Some(503));
        assert_eq!(detail.to_string(), "transport: http 503");
    }

    #[test]
    fn test_transport_detail_display_omits_status_when_absent() {
        let detail = TransportDetail::new(TransportKind::Timeout, None);
        assert_eq!(detail.to_string(), "transport: timeout");
    }

    #[test]
    fn test_transport_detail_display_never_emits_url_or_body() {
        // Regardless of category/status, Display carries only the category and
        // the status — there is no field that could hold a URL, a body, or a
        // token, so a leak is unrepresentable.
        for kind in [
            TransportKind::Timeout,
            TransportKind::Closed,
            TransportKind::Tls,
            TransportKind::Http,
            TransportKind::Decode,
        ] {
            let rendered = TransportDetail::new(kind, Some(418)).to_string();
            assert!(rendered.starts_with("transport: "));
            assert!(!rendered.contains("http://"));
            assert!(!rendered.contains("https://"));
            assert!(!rendered.contains("token"));
        }
    }

    #[test]
    fn test_provider_error_transport_accepts_only_redacted_detail() {
        // `Transport` is constructible only from a `Redacted` detail, never a
        // raw upstream string. This compiles precisely because the argument is
        // typed `Box<dyn Redacted>`.
        let detail: Box<dyn Redacted> =
            Box::new(TransportDetail::new(TransportKind::Http, Some(500)));
        let err = ProviderError::Transport(detail);
        assert_eq!(err.to_string(), "upstream transport: transport: http 500");
    }

    #[test]
    fn test_normalize_kind_display_names_field_not_value() {
        let kind = NormalizeKind::MissingField("strike");
        assert_eq!(kind.to_string(), "missing field `strike`");
    }

    #[test]
    fn test_provider_error_normalize_display_names_field() {
        let err = ProviderError::Normalize {
            kind: NormalizeKind::OutOfRange("delta"),
        };
        assert_eq!(err.to_string(), "normalize: out-of-range field `delta`");
    }

    #[test]
    fn test_provider_error_no_chain_display_uses_normalized_values() {
        let err = ProviderError::NoChain {
            underlying: "BTC".to_owned(),
            expiration: "2025-06-27T08:00:00Z".to_owned(),
        };
        assert_eq!(err.to_string(), "no chain for BTC @ 2025-06-27T08:00:00Z");
    }

    #[test]
    fn test_provider_error_rate_limited_display_carries_only_delay() {
        let err = ProviderError::RateLimited(Some(Duration::from_secs(5)));
        let rendered = err.to_string();
        assert!(rendered.starts_with("rate limited; retry after "));
        assert!(!rendered.contains("token"));
    }

    #[test]
    fn test_provider_error_unsupported_display_is_static_message() {
        let err = ProviderError::Unsupported("chain discovery");
        assert_eq!(
            err.to_string(),
            "not supported by this provider: chain discovery"
        );
    }

    #[test]
    fn test_config_error_missing_credential_display_names_provider() {
        let err = ConfigError::MissingCredential(provider_id("deribit"));
        let rendered = err.to_string();
        assert_eq!(rendered, "missing credential for provider deribit");
        assert!(!rendered.to_lowercase().contains("password"));
        assert!(!rendered.to_lowercase().contains("secret"));
        assert!(!rendered.to_lowercase().contains("key"));
    }

    #[test]
    fn test_config_error_invalid_value_display_names_field_and_reason() {
        let err = ConfigError::InvalidValue {
            field: "provider id".to_owned(),
            reason: "must match ^[a-z][a-z0-9_-]{1,31}$".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "invalid value for provider id: must match ^[a-z][a-z0-9_-]{1,31}$"
        );
    }

    #[test]
    fn test_registry_error_reserved_id_display_names_id() {
        let err = RegistryError::ReservedId(provider_id("deribit"));
        assert_eq!(
            err.to_string(),
            "provider id `deribit` is reserved for a built-in adapter"
        );
    }

    #[test]
    fn test_registry_error_duplicate_id_display_names_id() {
        let err = RegistryError::DuplicateId(provider_id("mybroker"));
        assert_eq!(
            err.to_string(),
            "provider id `mybroker` is already registered"
        );
    }

    #[test]
    fn test_registry_error_empty_display_is_category_message() {
        assert_eq!(RegistryError::Empty.to_string(), "no providers registered");
    }

    #[test]
    fn test_bundle_error_display_is_category_prefixed() {
        let err = BundleError::MissingTable("fills.parquet".to_owned());
        assert_eq!(err.to_string(), "missing table: fills.parquet");
    }

    #[test]
    fn test_chain_view_error_from_bundle_error_converts() {
        let err: ChainViewError = BundleError::UnsupportedSchema("bogus.v9".to_owned()).into();
        assert_eq!(
            err.to_string(),
            "result bundle: unsupported schema: bogus.v9"
        );
        assert!(matches!(err, ChainViewError::Bundle(_)));
    }

    #[test]
    fn test_chain_view_error_from_config_error_converts() {
        let err: ChainViewError = ConfigError::UnknownProvider("nope".to_owned()).into();
        assert_eq!(err.to_string(), "config: unknown provider: nope");
        assert!(matches!(err, ChainViewError::Config(_)));
    }

    #[test]
    fn test_chain_view_error_from_registry_error_converts() {
        let err: ChainViewError = RegistryError::Empty.into();
        assert_eq!(
            err.to_string(),
            "provider registry: no providers registered"
        );
        assert!(matches!(err, ChainViewError::Registry(_)));
    }

    #[test]
    fn test_chain_view_error_provider_helper_wraps_source() {
        let err = ChainViewError::provider(provider_id("deribit"), ProviderError::Auth);
        assert_eq!(err.to_string(), "provider deribit: authentication failed");
        assert!(matches!(
            err,
            ChainViewError::Provider {
                source: ProviderError::Auth,
                ..
            }
        ));
    }

    #[test]
    fn test_overlay_error_spec_mismatch_field_is_static_str() {
        let err = OverlayError::SpecMismatch {
            contract: "BTC-27JUN25-60000-C".to_owned(),
            field: "contract_multiplier",
            source: "100".to_owned(),
            overlay: "1".to_owned(),
        };
        assert_eq!(
            err.to_string(),
            "overlay spec mismatch on contract_multiplier for BTC-27JUN25-60000-C: \
             source `100` vs overlay `1`"
        );
        // The `field` slot is `&'static str`: destructuring it back out at that
        // exact type (no coercion) proves statically that a runtime value can
        // never occupy it.
        let field: &'static str = match err {
            OverlayError::SpecMismatch { field, .. } => field,
        };
        assert_eq!(field, "contract_multiplier");
    }
}
