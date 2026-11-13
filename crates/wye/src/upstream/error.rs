//! Why an upstream gave no usable response, or broke its response after the
//! headers: one kind per cause the client stack can tell apart, read from
//! the error chain only when a request fails.
//!
//! hyper-util, hyper, h2, rustls, and the operating system each wrap the
//! error below them, so a kind is decided by the first link of the chain
//! that names one. Wye's own links come first: [`Unresolved`] from the
//! resolver and [`ConnectTimedOut`] from the connect timeout, which no
//! `io::ErrorKind` would tell apart.

use std::{error::Error, fmt, io};

/// Why an upstream gave no usable response, in records as `upstream_error`:
/// before a connection (`connect_*`), while waiting for the response headers
/// on one, or, for a `truncated` response, after the headers. A failing
/// client body is `ForwardError::Body` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum UpstreamError {
    /// The upstream's host name did not resolve.
    ConnectDns,
    /// The upstream refused the TCP connection.
    ConnectRefused,
    /// No route to the upstream's network or host: the network is down.
    ConnectUnreachable,
    /// No connection and TLS handshake within `connect_timeout`.
    ConnectTimeout,
    /// The TLS handshake failed: a certificate Wye does not trust, a TLS
    /// alert, or the connection closed or reset during the handshake.
    ConnectTls,
    /// Any other failure to connect.
    ConnectOther,
    /// No response headers within `response_header_timeout`.
    Timeout,
    /// The upstream closed the connection.
    Closed,
    /// The connection was reset (TCP RST).
    Reset,
    /// The operating system dropped the connection: the upstream stopped
    /// acknowledging what Wye sent (`ETIMEDOUT`), or its network or host
    /// became unreachable.
    Lost,
    /// The upstream ended the HTTP/2 connection with a `GOAWAY` that did not
    /// cover the request.
    GoAway(H2Code),
    /// The upstream reset the request's HTTP/2 stream (`RST_STREAM`).
    StreamReset(H2Code),
    /// The upstream broke HTTP: an unreadable response head, or an HTTP/2
    /// frame Wye's client refused.
    Protocol,
    /// Any other failure after connecting.
    Other,
    /// A response body that sent nothing for `stream_idle_timeout`.
    IdleTimeout,
}

impl UpstreamError {
    /// The record's `upstream_error`.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ConnectDns => "connect_dns",
            Self::ConnectRefused => "connect_refused",
            Self::ConnectUnreachable => "connect_unreachable",
            Self::ConnectTimeout => "connect_timeout",
            Self::ConnectTls => "connect_tls",
            Self::ConnectOther => "connect_other",
            Self::Timeout => "timeout",
            Self::Closed => "closed",
            Self::Reset => "reset",
            Self::Lost => "lost",
            Self::GoAway(_) => "goaway",
            Self::StreamReset(_) => "stream_reset",
            Self::Protocol => "protocol",
            Self::Other => "other",
            Self::IdleTimeout => "idle_timeout",
        }
    }

    /// The record's `h2_error_code`: the code of a `GOAWAY` or `RST_STREAM`.
    pub(crate) fn h2_error_code(self) -> Option<&'static str> {
        match self {
            Self::GoAway(code) | Self::StreamReset(code) => Some(code.as_str()),
            _ => None,
        }
    }

    /// Whether the attempt failed before a connection was made, so the
    /// request never reached the provider.
    pub(crate) fn before_connecting(self) -> bool {
        matches!(
            self,
            Self::ConnectDns
                | Self::ConnectRefused
                | Self::ConnectUnreachable
                | Self::ConnectTimeout
                | Self::ConnectTls
                | Self::ConnectOther
        )
    }

    /// The kind of a request that got no response headers.
    pub(crate) fn of_request(e: &hyper_util::client::legacy::Error) -> Self {
        if e.is_connect() {
            first(e, connect_link).unwrap_or(Self::ConnectOther)
        } else {
            first(e, connected_link).unwrap_or(Self::Other)
        }
    }

    /// The kind of a response body that failed after its headers.
    pub(crate) fn of_body(e: &(dyn Error + 'static)) -> Self {
        if e.is::<IdleTimeout>() {
            return Self::IdleTimeout;
        }
        first(e, connected_link).unwrap_or(Self::Other)
    }
}

/// An HTTP/2 error code (RFC 9113, section 7), as the peer sent it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct H2Code(u32);

impl H2Code {
    /// The RFC's name for the code, or `UNKNOWN` for a code it does not
    /// define.
    pub(crate) fn as_str(self) -> &'static str {
        match self.0 {
            0x0 => "NO_ERROR",
            0x1 => "PROTOCOL_ERROR",
            0x2 => "INTERNAL_ERROR",
            0x3 => "FLOW_CONTROL_ERROR",
            0x4 => "SETTINGS_TIMEOUT",
            0x5 => "STREAM_CLOSED",
            0x6 => "FRAME_SIZE_ERROR",
            0x7 => "REFUSED_STREAM",
            0x8 => "CANCEL",
            0x9 => "COMPRESSION_ERROR",
            0xa => "CONNECT_ERROR",
            0xb => "ENHANCE_YOUR_CALM",
            0xc => "INADEQUATE_SECURITY",
            0xd => "HTTP_1_1_REQUIRED",
            _ => "UNKNOWN",
        }
    }
}

/// The resolver's failure: the upstream's host name did not resolve.
#[derive(Debug)]
pub(crate) struct Unresolved(pub(crate) io::Error);

impl fmt::Display for Unresolved {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("upstream host name did not resolve")
    }
}

impl Error for Unresolved {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.0)
    }
}

/// The connect timeout's failure. Zero-sized, so boxing it allocates nothing.
#[derive(Debug)]
pub(crate) struct ConnectTimedOut;

impl fmt::Display for ConnectTimedOut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connect (TCP and TLS) timed out")
    }
}

impl Error for ConnectTimedOut {}

/// The stream idle timeout's failure. Zero-sized, so boxing it allocates
/// nothing.
#[derive(Debug)]
pub(crate) struct IdleTimeout;

impl fmt::Display for IdleTimeout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("upstream stream idle timeout")
    }
}

impl Error for IdleTimeout {}

/// The first kind `link` names along `e`'s chain, outermost first. An
/// `io::Error`'s `source` skips the error it wraps, so the walk steps into
/// that error instead.
fn first(
    e: &(dyn Error + 'static),
    link: fn(&(dyn Error + 'static)) -> Option<UpstreamError>,
) -> Option<UpstreamError> {
    let mut next = Some(e);
    while let Some(e) = next {
        if let Some(kind) = link(e) {
            return Some(kind);
        }
        next = match e.downcast_ref::<io::Error>() {
            Some(io) => io.get_ref().map(|inner| inner as &(dyn Error + 'static)),
            None => e.source(),
        };
    }
    None
}

/// The kind one link of a failed connect names. TCP's own connect reports a
/// reset as refused, so a connection closed or reset while connecting broke
/// the TLS handshake.
fn connect_link(e: &(dyn Error + 'static)) -> Option<UpstreamError> {
    if e.is::<Unresolved>() {
        return Some(UpstreamError::ConnectDns);
    }
    if e.is::<ConnectTimedOut>() {
        return Some(UpstreamError::ConnectTimeout);
    }
    if e.is::<rustls::Error>() {
        return Some(UpstreamError::ConnectTls);
    }
    let io = e.downcast_ref::<io::Error>()?;
    match io.kind() {
        io::ErrorKind::ConnectionRefused => Some(UpstreamError::ConnectRefused),
        io::ErrorKind::TimedOut => Some(UpstreamError::ConnectTimeout),
        io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkDown
        | io::ErrorKind::AddrNotAvailable => Some(UpstreamError::ConnectUnreachable),
        io::ErrorKind::ConnectionReset
        | io::ErrorKind::ConnectionAborted
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::BrokenPipe => Some(UpstreamError::ConnectTls),
        // Look inside: a TLS error travels wrapped in an `io::Error`.
        _ => None,
    }
}

/// The kind one link of a failure on an open connection names.
fn connected_link(e: &(dyn Error + 'static)) -> Option<UpstreamError> {
    if let Some(h2) = e.downcast_ref::<h2::Error>() {
        let code = h2.reason().map_or(H2Code(u32::MAX), |r| H2Code(r.into()));
        return Some(match h2.get_io() {
            Some(io) => connected_io(io.kind()).unwrap_or(UpstreamError::Other),
            None if h2.is_go_away() && h2.is_remote() => UpstreamError::GoAway(code),
            None if h2.is_reset() && h2.is_remote() => UpstreamError::StreamReset(code),
            // h2 itself found the peer's frames invalid.
            None => UpstreamError::Protocol,
        });
    }
    if let Some(hyper) = e.downcast_ref::<hyper::Error>() {
        if hyper.is_incomplete_message() || hyper.is_canceled() {
            return Some(UpstreamError::Closed);
        }
        if hyper.is_parse() {
            return Some(UpstreamError::Protocol);
        }
    }
    connected_io(e.downcast_ref::<io::Error>()?.kind())
}

/// The kind an I/O error on an open connection names. h2 reports a
/// connection that reached its end as a broken pipe.
fn connected_io(kind: io::ErrorKind) -> Option<UpstreamError> {
    match kind {
        io::ErrorKind::UnexpectedEof | io::ErrorKind::BrokenPipe => Some(UpstreamError::Closed),
        io::ErrorKind::ConnectionReset | io::ErrorKind::ConnectionAborted => {
            Some(UpstreamError::Reset)
        }
        io::ErrorKind::TimedOut
        | io::ErrorKind::NetworkUnreachable
        | io::ErrorKind::HostUnreachable
        | io::ErrorKind::NetworkDown => Some(UpstreamError::Lost),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `inner` as the I/O error a client library wraps it in.
    fn wrapped(kind: io::ErrorKind, inner: impl Error + Send + Sync + 'static) -> io::Error {
        io::Error::new(kind, inner)
    }

    #[test]
    fn a_connect_failure_is_named_by_the_innermost_link_that_names_one() {
        let connect = |e: &(dyn Error + 'static)| first(e, connect_link);
        let dns = Unresolved(io::Error::other("nodename nor servname provided"));
        assert_eq!(connect(&dns), Some(UpstreamError::ConnectDns));
        let tls = wrapped(
            io::ErrorKind::Other,
            wrapped(
                io::ErrorKind::InvalidData,
                rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
            ),
        );
        assert_eq!(connect(&tls), Some(UpstreamError::ConnectTls));
        let refused = io::Error::from(io::ErrorKind::ConnectionRefused);
        assert_eq!(connect(&refused), Some(UpstreamError::ConnectRefused));
        let unreachable = io::Error::from(io::ErrorKind::NetworkUnreachable);
        assert_eq!(
            connect(&unreachable),
            Some(UpstreamError::ConnectUnreachable)
        );
        assert_eq!(
            connect(&ConnectTimedOut),
            Some(UpstreamError::ConnectTimeout)
        );
        assert_eq!(connect(&io::Error::other("unnamed")), None);
    }

    #[test]
    fn a_failure_on_an_open_connection_is_named_by_its_io_kind() {
        let connected = |kind| first(&io::Error::from(kind), connected_link);
        assert_eq!(
            connected(io::ErrorKind::BrokenPipe),
            Some(UpstreamError::Closed)
        );
        assert_eq!(
            connected(io::ErrorKind::ConnectionReset),
            Some(UpstreamError::Reset)
        );
        assert_eq!(
            connected(io::ErrorKind::TimedOut),
            Some(UpstreamError::Lost)
        );
        assert_eq!(connected(io::ErrorKind::Other), None);
        assert_eq!(
            UpstreamError::of_body(&IdleTimeout),
            UpstreamError::IdleTimeout
        );
    }

    #[test]
    fn every_kind_has_its_own_string_and_only_h2_kinds_carry_a_code() {
        let code = H2Code(0xb);
        let all = [
            UpstreamError::ConnectDns,
            UpstreamError::ConnectRefused,
            UpstreamError::ConnectUnreachable,
            UpstreamError::ConnectTimeout,
            UpstreamError::ConnectTls,
            UpstreamError::ConnectOther,
            UpstreamError::Timeout,
            UpstreamError::Closed,
            UpstreamError::Reset,
            UpstreamError::Lost,
            UpstreamError::GoAway(code),
            UpstreamError::StreamReset(code),
            UpstreamError::Protocol,
            UpstreamError::Other,
            UpstreamError::IdleTimeout,
        ];
        let mut names: Vec<_> = all.iter().map(|e| e.as_str()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), all.len());
        for e in all {
            let h2 = matches!(e, UpstreamError::GoAway(_) | UpstreamError::StreamReset(_));
            assert_eq!(e.h2_error_code().is_some(), h2, "{e:?}");
            assert_eq!(e.before_connecting(), e.as_str().starts_with("connect_"));
        }
        assert_eq!(code.as_str(), "ENHANCE_YOUR_CALM");
        assert_eq!(H2Code(0xe).as_str(), "UNKNOWN");
    }
}
