//! The RDATA of an HTTPS record (RFC 9460, section 2.2), parsed from its wire
//! form. Every platform hands the resolver this form, so it is parsed once,
//! here.

use std::fmt::Write as _;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::Arc;

/// The `SvcParamKey`s this parser understands (RFC 9460, section 14.3.2).
mod key {
    pub(super) const MANDATORY: u16 = 0;
    pub(super) const ALPN: u16 = 1;
    pub(super) const NO_DEFAULT_ALPN: u16 = 2;
    pub(super) const PORT: u16 = 3;
    pub(super) const IPV4HINT: u16 = 4;
    pub(super) const ECH: u16 = 5;
    pub(super) const IPV6HINT: u16 = 6;

    /// Whether a client that reads only the keys above understands `key`.
    pub(super) fn understood(key: u16) -> bool {
        key <= IPV6HINT
    }
}

/// One HTTPS record: an alias for another name, or a service endpoint with
/// the parameters a connection to it uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ServiceRecord {
    /// `SvcPriority`: 0 in `AliasMode`, otherwise the endpoint's preference,
    /// lowest first.
    pub(crate) priority: u16,
    /// `TargetName`, lowercase and without the trailing dot; `None` for the
    /// root name `.`, which in `ServiceMode` means the record's owner name and
    /// in `AliasMode` means the service is not available.
    pub(crate) target: Option<String>,
    /// The `alpn` protocol identifiers, in the record's order.
    pub(crate) alpn: Vec<Vec<u8>>,
    /// `no-default-alpn`: the endpoint does not also offer `http/1.1`.
    pub(crate) no_default_alpn: bool,
    /// `port`, where the endpoint is not on the origin's port.
    pub(crate) port: Option<u16>,
    /// `ipv4hint`.
    pub(crate) ipv4_hints: Vec<Ipv4Addr>,
    /// `ipv6hint`.
    pub(crate) ipv6_hints: Vec<Ipv6Addr>,
    /// `ech`: the `ECHConfigList`, recorded as received. Encrypted Client
    /// Hello is not offered with it (docs/design/https-svcb.md).
    pub(crate) ech: Option<Arc<[u8]>>,
}

impl ServiceRecord {
    /// Whether this is an `AliasMode` record (priority 0).
    pub(crate) fn is_alias(&self) -> bool {
        self.priority == 0
    }

    /// Whether the endpoint lists HTTP/3 (`h3`) among its protocols.
    pub(crate) fn offers_h3(&self) -> bool {
        self.alpn.iter().any(|id| id == b"h3")
    }
}

/// Why RDATA was not a record a client may use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Rejected {
    /// The RDATA breaks RFC 9460's wire format; the record is discarded
    /// (section 2.2).
    Malformed,
    /// A key the record makes mandatory is not one this client understands;
    /// the record is ignored (section 8).
    UnsupportedMandatory,
}

/// Parse the RDATA of an HTTPS (or SVCB) record.
pub(crate) fn parse(rdata: &[u8]) -> Result<ServiceRecord, Rejected> {
    let mut input = Reader(rdata);
    let priority = input.u16()?;
    let target = input.name()?;
    let mut record = ServiceRecord {
        priority,
        target,
        alpn: Vec::new(),
        no_default_alpn: false,
        port: None,
        ipv4_hints: Vec::new(),
        ipv6_hints: Vec::new(),
        ech: None,
    };
    if record.is_alias() {
        // SvcParams in AliasMode are ignored (section 2.4.2).
        return Ok(record);
    }
    let mut mandatory = Vec::new();
    let mut previous = None;
    while !input.0.is_empty() {
        let key = input.u16()?;
        // Keys appear in strictly increasing order (section 2.2).
        if previous.is_some_and(|previous| key <= previous) {
            return Err(Rejected::Malformed);
        }
        previous = Some(key);
        let length = usize::from(input.u16()?);
        let value = input.take(length)?;
        match key {
            key::MANDATORY => mandatory = parse_mandatory(value)?,
            key::ALPN => record.alpn = parse_alpn(value)?,
            key::NO_DEFAULT_ALPN if value.is_empty() => record.no_default_alpn = true,
            key::PORT => {
                let mut value = Reader(value);
                record.port = Some(value.u16()?);
                value.end()?;
            }
            key::IPV4HINT => {
                record.ipv4_hints = parse_hints(value, |chunk: [u8; 4]| Ipv4Addr::from(chunk))?;
            }
            key::ECH if !value.is_empty() => record.ech = Some(Arc::from(value)),
            key::IPV6HINT => {
                record.ipv6_hints = parse_hints(value, |chunk: [u8; 16]| Ipv6Addr::from(chunk))?;
            }
            key::NO_DEFAULT_ALPN | key::ECH => return Err(Rejected::Malformed),
            _ => {}
        }
    }
    // Every mandatory key is present (section 8).
    for &mandatory in &mandatory {
        if !key::understood(mandatory) {
            return Err(Rejected::UnsupportedMandatory);
        }
        if !is_present(&record, mandatory) {
            return Err(Rejected::Malformed);
        }
    }
    // `no-default-alpn` without `alpn` leaves no protocol (section 7.1.1).
    if record.no_default_alpn && record.alpn.is_empty() {
        return Err(Rejected::Malformed);
    }
    Ok(record)
}

/// Whether `record` carries the understood, non-`mandatory` key `key`.
fn is_present(record: &ServiceRecord, key: u16) -> bool {
    match key {
        key::ALPN => !record.alpn.is_empty(),
        key::NO_DEFAULT_ALPN => record.no_default_alpn,
        key::PORT => record.port.is_some(),
        key::IPV4HINT => !record.ipv4_hints.is_empty(),
        key::ECH => record.ech.is_some(),
        key::IPV6HINT => !record.ipv6_hints.is_empty(),
        _ => false,
    }
}

/// The `mandatory` value: a non-empty, strictly increasing list of keys
/// that does not name `mandatory` itself (section 8).
fn parse_mandatory(value: &[u8]) -> Result<Vec<u16>, Rejected> {
    if value.is_empty() || !value.len().is_multiple_of(2) {
        return Err(Rejected::Malformed);
    }
    let keys: Vec<u16> = value
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
        .collect();
    let increasing = keys.windows(2).all(|pair| pair[0] < pair[1]);
    if !increasing || keys.contains(&key::MANDATORY) {
        return Err(Rejected::Malformed);
    }
    Ok(keys)
}

/// The `alpn` value: a non-empty list of non-empty, length-prefixed
/// protocol identifiers (section 7.1.1).
fn parse_alpn(value: &[u8]) -> Result<Vec<Vec<u8>>, Rejected> {
    let mut input = Reader(value);
    let mut ids = Vec::new();
    while !input.0.is_empty() {
        let length = usize::from(input.u8()?);
        if length == 0 {
            return Err(Rejected::Malformed);
        }
        ids.push(input.take(length)?.to_vec());
    }
    if ids.is_empty() {
        return Err(Rejected::Malformed);
    }
    Ok(ids)
}

/// An `ipv4hint` or `ipv6hint` value: a non-empty list of `N`-byte
/// addresses (section 7.3).
fn parse_hints<const N: usize, T>(
    value: &[u8],
    address: impl Fn([u8; N]) -> T,
) -> Result<Vec<T>, Rejected> {
    if value.is_empty() || !value.len().is_multiple_of(N) {
        return Err(Rejected::Malformed);
    }
    Ok(value
        .as_chunks::<N>()
        .0
        .iter()
        .map(|chunk| {
            let mut bytes = [0; N];
            bytes.copy_from_slice(chunk);
            address(bytes)
        })
        .collect())
}

/// A cursor over wire-format bytes.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, length: usize) -> Result<&'a [u8], Rejected> {
        if self.0.len() < length {
            return Err(Rejected::Malformed);
        }
        let (taken, rest) = self.0.split_at(length);
        self.0 = rest;
        Ok(taken)
    }

    fn u8(&mut self) -> Result<u8, Rejected> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, Rejected> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn end(&self) -> Result<(), Rejected> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(Rejected::Malformed)
        }
    }

    /// An uncompressed domain name (section 2.2: `TargetName` is never
    /// compressed), lowercase, as dotted text without the trailing dot;
    /// `None` for the root. A byte that is not a letter, digit, `-` or `_`
    /// is written as `\DDD` (RFC 1035, section 5.1).
    fn name(&mut self) -> Result<Option<String>, Rejected> {
        let mut name = String::new();
        let mut wire_length = 1;
        loop {
            let length = usize::from(self.u8()?);
            if length == 0 {
                break;
            }
            // A label is at most 63 bytes; 0xC0 and up would be a pointer.
            if length > 63 {
                return Err(Rejected::Malformed);
            }
            wire_length += length + 1;
            if wire_length > 255 {
                return Err(Rejected::Malformed);
            }
            if !name.is_empty() {
                name.push('.');
            }
            for &byte in self.take(length)? {
                if byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_' {
                    name.push(char::from(byte.to_ascii_lowercase()));
                } else {
                    // Writing to a `String` cannot fail.
                    let _ = write!(name, "\\{byte:03}");
                }
            }
        }
        Ok((!name.is_empty()).then_some(name))
    }
}

#[cfg(test)]
mod tests {
    use super::{Rejected, ServiceRecord, parse};
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// Wire-format RDATA: a priority, a target name in dotted form (empty
    /// for the root), and `(key, value)` parameters in the order given.
    fn rdata(priority: u16, target: &str, params: &[(u16, &[u8])]) -> Vec<u8> {
        let mut out = priority.to_be_bytes().to_vec();
        for label in target.split('.').filter(|label| !label.is_empty()) {
            out.push(u8::try_from(label.len()).unwrap());
            out.extend_from_slice(label.as_bytes());
        }
        out.push(0);
        for (key, value) in params {
            out.extend_from_slice(&key.to_be_bytes());
            out.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
            out.extend_from_slice(value);
        }
        out
    }

    fn service(priority: u16, target: Option<&str>) -> ServiceRecord {
        ServiceRecord {
            priority,
            target: target.map(str::to_owned),
            alpn: Vec::new(),
            no_default_alpn: false,
            port: None,
            ipv4_hints: Vec::new(),
            ipv6_hints: Vec::new(),
            ech: None,
        }
    }

    // RFC 9460, appendix D.1: AliasMode.
    #[test]
    fn alias_mode_vector() {
        let wire = b"\x00\x00\x03foo\x07example\x03com\x00";
        assert_eq!(parse(wire), Ok(service(0, Some("foo.example.com"))));
        assert!(parse(wire).unwrap().is_alias());
    }

    // RFC 9460, appendix D.2: the simplest ServiceMode record.
    #[test]
    fn service_mode_root_target_vector() {
        assert_eq!(parse(b"\x00\x01\x00"), Ok(service(1, None)));
    }

    // RFC 9460, appendix D.2: a port.
    #[test]
    fn port_vector() {
        let wire = b"\x00\x10\x03foo\x07example\x03com\x00\x00\x03\x00\x02\x00\x35";
        let mut expected = service(16, Some("foo.example.com"));
        expected.port = Some(53);
        assert_eq!(parse(wire), Ok(expected));
    }

    // RFC 9460, appendix D.2: an IPv6 hint.
    #[test]
    fn ipv6_hint_vector() {
        let wire = b"\x00\x01\x03foo\x07example\x03com\x00\x00\x06\x00\x20\
            \x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x01\
            \x20\x01\x0d\xb8\x00\x00\x00\x00\x00\x00\x00\x00\x00\x53\x00\x01";
        let record = parse(wire).unwrap();
        assert_eq!(
            record.ipv6_hints,
            [
                "2001:db8::1".parse::<Ipv6Addr>().unwrap(),
                "2001:db8::53:1".parse().unwrap()
            ]
        );
    }

    // RFC 9460, appendix D.2: mandatory, alpn, and an IPv4 hint, with the
    // parameters reordered into increasing key order on the wire.
    #[test]
    fn mandatory_alpn_and_ipv4_hint_vector() {
        let wire = b"\x00\x10\x03foo\x07example\x03org\x00\
            \x00\x00\x00\x04\x00\x01\x00\x04\
            \x00\x01\x00\x09\x02h2\x05h3-19\
            \x00\x04\x00\x04\xc0\x00\x02\x01";
        let record = parse(wire).unwrap();
        assert_eq!(record.alpn, [b"h2".to_vec(), b"h3-19".to_vec()]);
        assert_eq!(record.ipv4_hints, [Ipv4Addr::new(192, 0, 2, 1)]);
        assert!(!record.offers_h3(), "h3-19 is a draft, not h3");
    }

    // RFC 9460, appendix D.3: parameters out of order are malformed.
    #[test]
    fn keys_out_of_order_are_malformed() {
        let wire = rdata(1, "foo.example.org", &[(1, b"\x02h2"), (0, b"\x00\x01")]);
        assert_eq!(parse(&wire), Err(Rejected::Malformed));
        let repeated = rdata(1, "", &[(3, b"\x01\xbb"), (3, b"\x01\xbb")]);
        assert_eq!(parse(&repeated), Err(Rejected::Malformed));
    }

    // RFC 9460, appendix D.3: a mandatory key that is missing.
    #[test]
    fn a_missing_mandatory_key_is_malformed() {
        let wire = rdata(1, "", &[(0, b"\x00\x01")]);
        assert_eq!(parse(&wire), Err(Rejected::Malformed));
    }

    #[test]
    fn an_unknown_mandatory_key_is_unsupported() {
        let wire = rdata(1, "", &[(0, b"\x00\x09"), (9, b"x")]);
        assert_eq!(parse(&wire), Err(Rejected::UnsupportedMandatory));
    }

    #[test]
    fn an_unknown_optional_key_is_skipped() {
        let wire = rdata(1, "", &[(1, b"\x02h3"), (9, b"anything")]);
        let record = parse(&wire).unwrap();
        assert!(record.offers_h3());
    }

    #[test]
    fn ech_is_recorded_as_received() {
        let wire = rdata(1, "", &[(5, b"\x00\x04abcd")]);
        assert_eq!(
            parse(&wire).unwrap().ech.as_deref(),
            Some(&b"\x00\x04abcd"[..])
        );
        assert_eq!(parse(&rdata(1, "", &[(5, b"")])), Err(Rejected::Malformed));
    }

    #[test]
    fn no_default_alpn_needs_alpn_and_an_empty_value() {
        assert_eq!(parse(&rdata(1, "", &[(2, b"")])), Err(Rejected::Malformed));
        assert_eq!(
            parse(&rdata(1, "", &[(1, b"\x02h3"), (2, b"x")])),
            Err(Rejected::Malformed)
        );
        let record = parse(&rdata(1, "", &[(1, b"\x02h3"), (2, b"")])).unwrap();
        assert!(record.no_default_alpn);
    }

    #[test]
    fn malformed_values_are_rejected() {
        for params in [
            &[(1_u16, &b""[..])][..],
            &[(1, b"\x00")],
            &[(1, b"\x05h3")],
            &[(3, b"\x01")],
            &[(3, b"\x01\xbb\x00")],
            &[(4, b"\x7f\x00\x00")],
            &[(6, b"\x00")],
            &[(0, b"")],
            &[(0, b"\x00\x00"), (1, b"\x02h3")],
            &[(0, b"\x00\x03\x00\x01"), (1, b"\x02h3"), (3, b"\x01\xbb")],
        ] {
            assert_eq!(
                parse(&rdata(1, "", params)),
                Err(Rejected::Malformed),
                "{params:?}"
            );
        }
    }

    #[test]
    fn truncated_rdata_is_malformed() {
        let wire = rdata(1, "example.com", &[(1, b"\x02h3")]);
        for end in 0..wire.len() {
            if end == 15 {
                // The record without its parameter is complete.
                continue;
            }
            assert_eq!(parse(&wire[..end]), Err(Rejected::Malformed), "{end}");
        }
    }

    #[test]
    fn a_compressed_target_is_malformed() {
        assert_eq!(parse(b"\x00\x01\xc0\x0c"), Err(Rejected::Malformed));
    }

    #[test]
    fn target_names_are_lowercased_and_escaped() {
        let record = parse(b"\x00\x01\x03W.w\x02Ex\x00").unwrap();
        assert_eq!(record.target.as_deref(), Some("w\\046w.ex"));
    }

    #[test]
    fn alias_mode_ignores_parameters() {
        let wire = rdata(0, "alias.example", &[(1, b"\x02h3"), (1, b"")]);
        assert_eq!(parse(&wire), Ok(service(0, Some("alias.example"))));
    }
}
