//! Fail-closed destination admission for tenant-configured outbound HTTP.
//!
//! The admission result pins one DNS answer for one transport attempt. Callers
//! must build the attempt's client from [`AdmittedHttpDestination::socket_addrs`]
//! and invoke [`OutboundHttpPolicy::admit`] again before every retry.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tokio::net::lookup_host;
use url::{Host, Origin, Url};

/// A DNS lookup failure with no destination text in its diagnostic form.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OutboundHostResolutionError {
    /// The resolver could not produce an address set.
    #[error("outbound host resolution failed")]
    Failed,
}

/// Asynchronous host resolver used by outbound destination admission.
#[async_trait]
pub trait OutboundHostResolver: Send + Sync {
    /// Resolves one host and reviewed origin port for the current attempt.
    async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, OutboundHostResolutionError>;
}

/// Tokio-backed resolver used by production construction paths.
#[derive(Clone, Copy, Debug, Default)]
pub struct TokioOutboundHostResolver;

#[async_trait]
impl OutboundHostResolver for TokioOutboundHostResolver {
    async fn resolve(
        &self,
        host: &str,
        port: u16,
    ) -> Result<Vec<SocketAddr>, OutboundHostResolutionError> {
        lookup_host((host, port))
            .await
            .map(|addresses| addresses.collect())
            .map_err(|_| OutboundHostResolutionError::Failed)
    }
}

/// Fail-closed errors raised before an outbound request may be constructed.
///
/// Variants deliberately carry no URL, host, IP address, resolver text, or
/// caller input, keeping both `Debug` and `Display` safe for diagnostics.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OutboundHttpAdmissionError {
    /// The supplied value was not one canonical fixed HTTP(S) origin.
    #[error("outbound HTTP origin is invalid")]
    InvalidOrigin,
    /// Production destination admission requires HTTPS.
    #[error("outbound HTTP destination requires HTTPS")]
    HttpsRequired,
    /// The host representation was not already canonical.
    #[error("outbound HTTP host is not canonical")]
    NonCanonicalHost,
    /// DNS resolution failed without yielding an address set.
    #[error("outbound HTTP destination resolution failed")]
    ResolutionFailed,
    /// DNS resolution exceeded the caller's connect-time budget.
    #[error("outbound HTTP destination resolution timed out")]
    ResolutionTimedOut,
    /// DNS resolution returned no addresses.
    #[error("outbound HTTP destination resolved to no addresses")]
    EmptyAddressSet,
    /// At least one resolved address is not allowed by destination policy.
    #[error("outbound HTTP destination address is denied")]
    AddressDenied,
    /// A resolver returned an address outside the reviewed origin port.
    #[error("outbound HTTP destination resolved with an unexpected port")]
    PortMismatch,
}

/// Secret-safe failure while constructing one admitted outbound HTTP client.
///
/// The error deliberately carries no destination, address, proxy setting, or
/// transport diagnostic because callers may persist its stable code.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum OutboundHttpClientError {
    /// One or more transport limits were zero or internally inconsistent.
    #[error("outbound HTTP client limits are invalid")]
    InvalidLimits,
    /// Reqwest could not construct the admitted client.
    #[error("outbound HTTP client construction failed")]
    ConstructionFailed,
}

/// Bounded transport settings for one admitted outbound HTTP attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OutboundHttpClientLimits {
    connect_timeout: Duration,
    total_timeout: Duration,
    max_response_header_bytes: u32,
}

impl OutboundHttpClientLimits {
    /// Validates the time and response-header bounds used by a fresh client.
    pub fn new(
        connect_timeout: Duration,
        total_timeout: Duration,
        max_response_header_bytes: u32,
    ) -> Result<Self, OutboundHttpClientError> {
        if connect_timeout.is_zero()
            || total_timeout.is_zero()
            || connect_timeout > total_timeout
            || max_response_header_bytes == 0
        {
            return Err(OutboundHttpClientError::InvalidLimits);
        }
        Ok(Self {
            connect_timeout,
            total_timeout,
            max_response_header_bytes,
        })
    }

    /// Returns the bound applied while establishing the connection.
    #[must_use]
    pub const fn connect_timeout(self) -> Duration {
        self.connect_timeout
    }

    /// Returns the bound applied to the complete request attempt.
    #[must_use]
    pub const fn total_timeout(self) -> Duration {
        self.total_timeout
    }

    /// Returns the HTTP/2 response-header list bound.
    #[must_use]
    pub const fn max_response_header_bytes(self) -> u32 {
        self.max_response_header_bytes
    }
}

#[derive(Clone, Copy)]
enum AdmissionMode {
    Production,
    #[cfg(any(test, feature = "test-support"))]
    LoopbackHttpForTests,
}

/// Per-attempt outbound HTTP destination admission policy.
///
/// Production mode is permanently strict: it accepts only canonical HTTPS
/// origins whose complete resolved address set is publicly routable. The only
/// widening constructor is compiled exclusively for tests or explicit
/// `test-support` dependents.
#[derive(Clone)]
pub struct OutboundHttpPolicy {
    resolver: Arc<dyn OutboundHostResolver>,
    mode: AdmissionMode,
}

impl OutboundHttpPolicy {
    /// Builds strict production admission around an injected async resolver.
    #[must_use]
    pub fn production(resolver: Arc<dyn OutboundHostResolver>) -> Self {
        Self {
            resolver,
            mode: AdmissionMode::Production,
        }
    }

    /// Builds strict production admission using Tokio's system resolver.
    #[must_use]
    pub fn production_system() -> Self {
        Self::production(Arc::new(TokioOutboundHostResolver))
    }

    /// Builds admission for an isolated loopback HTTP fixture.
    ///
    /// This constructor is absent from ordinary production builds. In this
    /// mode, `http` is accepted only when every address is loopback; public and
    /// mixed HTTP address sets remain denied. HTTPS retains production rules.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn loopback_http_for_tests(resolver: Arc<dyn OutboundHostResolver>) -> Self {
        Self {
            resolver,
            mode: AdmissionMode::LoopbackHttpForTests,
        }
    }

    /// Admits and DNS-pins one fixed origin for exactly one transport attempt.
    ///
    /// `connect_timeout` also bounds hostname resolution, so admission cannot
    /// consume more than the attempt's connection budget. Call this method
    /// again for every retry; this type does not cache DNS answers.
    pub async fn admit(
        &self,
        origin: &str,
        connect_timeout: Duration,
    ) -> Result<AdmittedHttpDestination, OutboundHttpAdmissionError> {
        let parsed = parse_canonical_origin(origin)?;
        let scheme = parsed.scheme();

        let test_loopback_http = match self.mode {
            AdmissionMode::Production => {
                if scheme != "https" {
                    return Err(OutboundHttpAdmissionError::HttpsRequired);
                }
                false
            }
            #[cfg(any(test, feature = "test-support"))]
            AdmissionMode::LoopbackHttpForTests => match scheme {
                "https" => false,
                "http" => true,
                _ => return Err(OutboundHttpAdmissionError::InvalidOrigin),
            },
        };

        let port = parsed
            .port_or_known_default()
            .filter(|port| *port != 0)
            .ok_or(OutboundHttpAdmissionError::InvalidOrigin)?;
        let (host, mut addresses) = match parsed.host() {
            Some(Host::Domain(host)) => {
                let resolved =
                    tokio::time::timeout(connect_timeout, self.resolver.resolve(host, port))
                        .await
                        .map_err(|_| OutboundHttpAdmissionError::ResolutionTimedOut)?
                        .map_err(|_| OutboundHttpAdmissionError::ResolutionFailed)?;
                (host.to_owned(), resolved)
            }
            Some(Host::Ipv4(address)) => (
                address.to_string(),
                vec![SocketAddr::new(IpAddr::V4(address), port)],
            ),
            Some(Host::Ipv6(address)) => (
                address.to_string(),
                vec![SocketAddr::new(IpAddr::V6(address), port)],
            ),
            None => return Err(OutboundHttpAdmissionError::InvalidOrigin),
        };

        if addresses.is_empty() {
            return Err(OutboundHttpAdmissionError::EmptyAddressSet);
        }
        if addresses.iter().any(|address| address.port() != port) {
            return Err(OutboundHttpAdmissionError::PortMismatch);
        }

        let address_set_allowed = if test_loopback_http {
            addresses.iter().all(|address| address.ip().is_loopback())
        } else {
            addresses
                .iter()
                .all(|address| is_public_destination(address.ip()))
        };
        if !address_set_allowed {
            return Err(OutboundHttpAdmissionError::AddressDenied);
        }

        addresses.sort_unstable();
        addresses.dedup();

        Ok(AdmittedHttpDestination {
            canonical_origin: parsed,
            host,
            port,
            socket_addrs: addresses,
        })
    }
}

/// Canonical, DNS-pinned destination admitted for one request attempt.
///
/// The type intentionally has no `Debug`, `Display`, or serialization
/// implementation. Callers may use its getters to configure the transport but
/// should not attach destination details to logs or persisted failures.
#[derive(Clone)]
pub struct AdmittedHttpDestination {
    canonical_origin: Url,
    host: String,
    port: u16,
    socket_addrs: Vec<SocketAddr>,
}

impl AdmittedHttpDestination {
    /// Returns the canonical fixed origin, with no path beyond `/`.
    #[must_use]
    pub fn canonical_origin(&self) -> &Url {
        &self.canonical_origin
    }

    /// Returns the canonical host used when pinning the request client.
    #[must_use]
    pub fn host(&self) -> &str {
        &self.host
    }

    /// Returns the exact reviewed origin port.
    #[must_use]
    pub const fn port(&self) -> u16 {
        self.port
    }

    /// Returns the complete, validated socket-address set for this attempt.
    #[must_use]
    pub fn socket_addrs(&self) -> &[SocketAddr] {
        &self.socket_addrs
    }
}

/// Builds one fresh, DNS-pinned client for an admitted HTTP attempt.
///
/// The client never consults environment or system proxies, never follows a
/// redirect, and never retries. Its resolver override is the complete address
/// set admitted for this attempt, so callers must run destination admission and
/// call this function again before every subsequent protocol leg or retry.
pub fn build_admitted_http_client(
    destination: &AdmittedHttpDestination,
    limits: OutboundHttpClientLimits,
) -> Result<reqwest::Client, OutboundHttpClientError> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .referer(false)
        .connect_timeout(limits.connect_timeout())
        .timeout(limits.total_timeout())
        .resolve_to_addrs(destination.host(), destination.socket_addrs())
        .http2_max_header_list_size(limits.max_response_header_bytes())
        .build()
        .map_err(|_| OutboundHttpClientError::ConstructionFailed)
}

/// Parses one already-canonical fixed HTTP(S) origin.
///
/// The input must contain only a scheme, host, and optional non-default port.
/// User information, paths, queries, fragments, wildcard syntax, control
/// characters, and non-canonical host spellings are rejected. A lone trailing
/// slash is accepted because it does not change the origin.
pub fn parse_canonical_origin(origin: &str) -> Result<Url, OutboundHttpAdmissionError> {
    if origin.is_empty()
        || origin.trim() != origin
        || origin.contains(['*', '{', '}'])
        || origin.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(OutboundHttpAdmissionError::InvalidOrigin);
    }

    let parsed = Url::parse(origin).map_err(|_| OutboundHttpAdmissionError::InvalidOrigin)?;
    if !matches!(parsed.scheme(), "http" | "https")
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.path() != "/"
        || parsed.query().is_some()
        || parsed.fragment().is_some()
    {
        return Err(OutboundHttpAdmissionError::InvalidOrigin);
    }
    let Origin::Tuple(_, _, _) = parsed.origin() else {
        return Err(OutboundHttpAdmissionError::InvalidOrigin);
    };
    if matches!(parsed.host(), Some(Host::Domain(host)) if host.ends_with('.')) {
        return Err(OutboundHttpAdmissionError::NonCanonicalHost);
    }
    let canonical = parsed.origin().ascii_serialization();
    if origin != canonical && origin != format!("{canonical}/") {
        return Err(OutboundHttpAdmissionError::NonCanonicalHost);
    }

    Ok(parsed)
}

fn is_public_destination(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => !is_denied_ipv4(address),
        IpAddr::V6(address) => !is_denied_ipv6(address),
    }
}

fn is_denied_ipv4(address: Ipv4Addr) -> bool {
    const DENIED: &[(u32, u8)] = &[
        (0x0000_0000, 8),  // Current network and unspecified.
        (0x0a00_0000, 8),  // Private-use.
        (0x6440_0000, 10), // Shared address space and provider metadata.
        (0x7f00_0000, 8),  // Loopback.
        (0xa83f_8110, 32), // Azure platform virtual address.
        (0xa9fe_0000, 16), // Link-local and common metadata endpoints.
        (0xac10_0000, 12), // Private-use.
        (0xc000_0000, 24), // IETF protocol assignments.
        (0xc000_0200, 24), // Documentation TEST-NET-1.
        (0xc01f_c400, 24), // AS112-v4.
        (0xc034_c100, 24), // AMT.
        (0xc058_6300, 24), // Deprecated 6to4 relay anycast.
        (0xc0a8_0000, 16), // Private-use.
        (0xc0af_3000, 24), // Direct Delegation AS112 service.
        (0xc612_0000, 15), // Benchmarking.
        (0xc633_6400, 24), // Documentation TEST-NET-2.
        (0xcb00_7100, 24), // Documentation TEST-NET-3.
        (0xe000_0000, 4),  // Multicast.
        (0xf000_0000, 4),  // Reserved and limited broadcast.
    ];

    DENIED
        .iter()
        .any(|(network, prefix)| ipv4_in_prefix(address, *network, *prefix))
}

fn is_denied_ipv6(address: Ipv6Addr) -> bool {
    const GLOBAL_UNICAST: u128 = 0x2000_0000_0000_0000_0000_0000_0000_0000;
    const DENIED: &[(u128, u8)] = &[
        (0, 96),                                         // IPv4-compatible and low-address space.
        (0x0000_0000_0000_0000_0000_ffff_0000_0000, 96), // IPv4-mapped.
        (0x0064_ff9b_0000_0000_0000_0000_0000_0000, 96), // NAT64 well-known.
        (0x0064_ff9b_0001_0000_0000_0000_0000_0000, 48), // NAT64 local-use.
        (0x0100_0000_0000_0000_0000_0000_0000_0000, 64), // Discard-only.
        (0x2001_0000_0000_0000_0000_0000_0000_0000, 23), // IETF special-purpose.
        (0x2001_0db8_0000_0000_0000_0000_0000_0000, 32), // Documentation.
        (0x2002_0000_0000_0000_0000_0000_0000_0000, 16), // 6to4.
        (0x2620_004f_8000_0000_0000_0000_0000_0000, 48), // Direct Delegation AS112 service.
        (0x3fff_0000_0000_0000_0000_0000_0000_0000, 20), // Documentation.
        (0x5f00_0000_0000_0000_0000_0000_0000_0000, 16), // Segment-routing SIDs.
        (0xfc00_0000_0000_0000_0000_0000_0000_0000, 7),  // Unique-local.
        (0xfe80_0000_0000_0000_0000_0000_0000_0000, 10), // Link-local.
        (0xfec0_0000_0000_0000_0000_0000_0000_0000, 10), // Deprecated site-local.
        (0xff00_0000_0000_0000_0000_0000_0000_0000, 8),  // Multicast.
    ];

    !ipv6_in_prefix(address, GLOBAL_UNICAST, 3)
        || DENIED
            .iter()
            .any(|(network, prefix)| ipv6_in_prefix(address, *network, *prefix))
}

fn ipv4_in_prefix(address: Ipv4Addr, network: u32, prefix: u8) -> bool {
    let mask = u32::MAX.checked_shl(u32::from(32 - prefix)).unwrap_or(0);
    u32::from(address) & mask == network & mask
}

fn ipv6_in_prefix(address: Ipv6Addr, network: u128, prefix: u8) -> bool {
    let mask = u128::MAX.checked_shl(u32::from(128 - prefix)).unwrap_or(0);
    u128::from(address) & mask == network & mask
}

#[cfg(test)]
mod tests;
