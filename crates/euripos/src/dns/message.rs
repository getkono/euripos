//! A DNS response message (RFC 1035, section 4), read for the HTTPS records
//! in its answer section and the negative-caching TTL in its authority
//! section (RFC 2308, section 5). Platforms that return the whole response
//! (Android's `android_res_nresult`) hand it here.

use super::{Answer, Record};

/// The HTTPS record type (RFC 9460, section 14.1).
pub(crate) const TYPE_HTTPS: u16 = 65;
/// The SOA record type.
const TYPE_SOA: u16 = 6;
/// The Internet class.
pub(crate) const CLASS_IN: u16 = 1;
/// `NOERROR`.
const RCODE_NOERROR: u8 = 0;
/// `NXDOMAIN`.
const RCODE_NXDOMAIN: u8 = 3;

/// The answer `message` gives to a query for HTTPS records: `None` where it
/// is not a well-formed response, or its response code is an error other
/// than `NXDOMAIN`.
pub(crate) fn answer(message: &[u8]) -> Option<Answer> {
    let header = message.get(..12)?;
    let rcode = header[3] & 0x0f;
    let count = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    let (questions, answers, authorities) = (count(4), count(6), count(8));
    let mut at = 12;
    for _ in 0..questions {
        at = skip_name(message, at)? + 4;
        message.get(..at)?;
    }
    let mut records = Vec::new();
    for _ in 0..answers {
        let (record, next) = resource(message, at)?;
        if record.kind == TYPE_HTTPS && record.class == CLASS_IN {
            records.push(Record {
                rdata: record.rdata.to_vec(),
                ttl: record.ttl,
            });
        }
        at = next;
    }
    match rcode {
        RCODE_NOERROR if !records.is_empty() => Some(Answer::Records(records)),
        RCODE_NOERROR | RCODE_NXDOMAIN => {
            let mut negative_ttl = None;
            for _ in 0..authorities {
                let (record, next) = resource(message, at)?;
                if record.kind == TYPE_SOA {
                    negative_ttl = Some(soa_negative_ttl(message, &record)?);
                }
                at = next;
            }
            Some(Answer::NoRecords { negative_ttl })
        }
        _ => None,
    }
}

/// One resource record, borrowed from the message.
struct Resource<'a> {
    kind: u16,
    class: u16,
    ttl: u32,
    /// Where the RDATA starts in the message, for names compressed in it.
    rdata_at: usize,
    rdata: &'a [u8],
}

/// The resource record at `at`, and where the next one starts.
fn resource(message: &[u8], at: usize) -> Option<(Resource<'_>, usize)> {
    let at = skip_name(message, at)?;
    let fixed = message.get(at..at + 10)?;
    let length = usize::from(u16::from_be_bytes([fixed[8], fixed[9]]));
    let rdata_at = at + 10;
    let rdata = message.get(rdata_at..rdata_at + length)?;
    let record = Resource {
        kind: u16::from_be_bytes([fixed[0], fixed[1]]),
        class: u16::from_be_bytes([fixed[2], fixed[3]]),
        ttl: u32::from_be_bytes([fixed[4], fixed[5], fixed[6], fixed[7]]),
        rdata_at,
        rdata,
    };
    Some((record, rdata_at + length))
}

/// The negative-caching TTL an SOA record gives: the lesser of its own TTL
/// and its `MINIMUM` field (RFC 2308, section 5).
fn soa_negative_ttl(message: &[u8], soa: &Resource<'_>) -> Option<u32> {
    let end = soa.rdata_at + soa.rdata.len();
    // MNAME and RNAME, which may be compressed against the whole message.
    let at = skip_name(message, soa.rdata_at)?;
    let at = skip_name(message, at)?;
    // SERIAL, REFRESH, RETRY, EXPIRE, MINIMUM.
    if at + 20 != end {
        return None;
    }
    let minimum = message.get(at + 16..end)?;
    let minimum = u32::from_be_bytes([minimum[0], minimum[1], minimum[2], minimum[3]]);
    Some(soa.ttl.min(minimum))
}

/// Where the domain name at `at` ends. A compression pointer ends the name
/// where it stands; the name it points to is not read.
fn skip_name(message: &[u8], mut at: usize) -> Option<usize> {
    loop {
        let length = *message.get(at)?;
        match length {
            0 => return Some(at + 1),
            1..=63 => {
                at += 1 + usize::from(length);
                message.get(..at)?;
            }
            0xc0..=0xff => {
                message.get(at + 1)?;
                return Some(at + 2);
            }
            _ => return None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CLASS_IN, TYPE_HTTPS, TYPE_SOA, answer};
    use crate::dns::{Answer, Record};

    /// A response to a query for `example.com`'s HTTPS records, with
    /// `answers` and `authorities` as `(type, ttl, rdata)` records whose
    /// owner names point back at the question.
    fn response(
        rcode: u8,
        answers: &[(u16, u32, &[u8])],
        authorities: &[(u16, u32, &[u8])],
    ) -> Vec<u8> {
        let mut out = vec![0x12, 0x34, 0x81, 0x80 | rcode, 0, 1];
        out.extend_from_slice(&u16::try_from(answers.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&u16::try_from(authorities.len()).unwrap().to_be_bytes());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(b"\x07example\x03com\x00");
        out.extend_from_slice(&TYPE_HTTPS.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        for (kind, ttl, rdata) in answers.iter().chain(authorities) {
            out.extend_from_slice(&[0xc0, 0x0c]);
            out.extend_from_slice(&kind.to_be_bytes());
            out.extend_from_slice(&CLASS_IN.to_be_bytes());
            out.extend_from_slice(&ttl.to_be_bytes());
            out.extend_from_slice(&u16::try_from(rdata.len()).unwrap().to_be_bytes());
            out.extend_from_slice(rdata);
        }
        out
    }

    /// SOA RDATA with compressed names and the given `MINIMUM`.
    fn soa(minimum: u32) -> Vec<u8> {
        let mut out = b"\x02ns\xc0\x0c\x04root\xc0\x0c".to_vec();
        for field in [1_u32, 7200, 900, 1_209_600, minimum] {
            out.extend_from_slice(&field.to_be_bytes());
        }
        out
    }

    #[test]
    fn https_records_are_read_from_the_answer_section() {
        let cname = b"\x03cdn\xc0\x0c";
        let message = response(
            0,
            &[(5, 60, cname), (TYPE_HTTPS, 300, b"\x00\x01\x00")],
            &[],
        );
        assert_eq!(
            answer(&message),
            Some(Answer::Records(vec![Record {
                rdata: b"\x00\x01\x00".to_vec(),
                ttl: 300
            }]))
        );
    }

    #[test]
    fn no_data_takes_its_ttl_from_the_soa() {
        let soa = soa(120);
        let message = response(0, &[], &[(TYPE_SOA, 3600, &soa)]);
        assert_eq!(
            answer(&message),
            Some(Answer::NoRecords {
                negative_ttl: Some(120)
            })
        );
        let message = response(0, &[], &[(TYPE_SOA, 30, &soa)]);
        assert_eq!(
            answer(&message),
            Some(Answer::NoRecords {
                negative_ttl: Some(30)
            })
        );
    }

    #[test]
    fn nxdomain_is_no_records() {
        let message = response(3, &[], &[]);
        assert_eq!(
            answer(&message),
            Some(Answer::NoRecords { negative_ttl: None })
        );
    }

    #[test]
    fn a_server_failure_is_no_answer() {
        assert_eq!(answer(&response(2, &[], &[])), None);
        assert_eq!(answer(&response(5, &[], &[])), None);
    }

    #[test]
    fn a_truncated_message_is_no_answer() {
        let message = response(0, &[(TYPE_HTTPS, 300, b"\x00\x01\x00")], &[]);
        for end in 0..message.len() {
            assert_eq!(answer(&message[..end]), None, "{end}");
        }
    }

    #[test]
    fn a_reserved_label_type_is_no_answer() {
        let mut message = response(0, &[], &[]);
        message[12] = 0x40;
        assert_eq!(answer(&message), None);
    }
}
