//! Response decompression: the content codings a build asks for in
//! `Accept-Encoding` (RFC 9110 §12.5.3) and decodes (§8.4), one Cargo
//! feature each.
//!
//! See docs/design/parity.md, "Response decompression".

use std::io;

use bytes::Bytes;
use http::{HeaderValue, Method, Response, StatusCode, header};

use crate::Body;

/// Each content coding a build can decode, with whether this build does.
const CODINGS: [(&str, bool); 3] = [
    ("gzip", cfg!(feature = "gzip")),
    ("br", cfg!(feature = "brotli")),
    ("zstd", cfg!(feature = "zstd")),
];

/// The `Accept-Encoding` this build sends: the codings it decodes, or
/// `None` when it decodes none and so asks for none.
pub(crate) fn accept_encoding() -> Option<HeaderValue> {
    let codings: Vec<&str> = CODINGS
        .iter()
        .filter(|(_, enabled)| *enabled)
        .map(|(name, _)| *name)
        .collect();
    (!codings.is_empty())
        .then(|| HeaderValue::from_str(&codings.join(", ")))?
        .ok()
}

/// `response` to a request of `method`, its body decoded when it carries
/// one content coding this build decodes. The decoded response has no
/// `Content-Encoding` and no `Content-Length`, which described the encoded
/// bytes. Anything else is returned as it is.
pub(crate) fn decode(mut response: Response<Body>, method: &Method) -> Response<Body> {
    let status = response.status();
    if *method == Method::HEAD
        || status == StatusCode::NO_CONTENT
        || status == StatusCode::NOT_MODIFIED
    {
        return response;
    }
    let mut codings = response.headers().get_all(header::CONTENT_ENCODING).iter();
    let (Some(coding), None) = (codings.next(), codings.next()) else {
        return response;
    };
    let Some(decoder) = coding.to_str().ok().and_then(Decoder::for_coding) else {
        return response;
    };
    let headers = response.headers_mut();
    headers.remove(header::CONTENT_ENCODING);
    headers.remove(header::CONTENT_LENGTH);
    response.map(|body| Body::decoded(body, decoder))
}

/// A decoder for one content coding, fed the encoded body piece by piece.
pub(crate) enum Decoder {
    /// gzip (RFC 9110 §8.4.1.3, RFC 1952), every member of the stream.
    #[cfg(feature = "gzip")]
    Gzip(Box<flate2::write::MultiGzDecoder<Vec<u8>>>),
    /// Brotli (RFC 7932).
    #[cfg(feature = "brotli")]
    Brotli(Box<brotli_decompressor::DecompressorWriter<Vec<u8>>>),
    /// Zstandard (RFC 8878 §7.2).
    #[cfg(feature = "zstd")]
    Zstd(Box<zstd::Zstd>),
    /// A build with no coding has no decoder to make.
    #[cfg(not(any(feature = "gzip", feature = "brotli", feature = "zstd")))]
    #[expect(
        dead_code,
        reason = "uninhabited: it only makes the matches exhaustive"
    )]
    None(std::convert::Infallible),
}

impl Decoder {
    /// The decoder for the content coding named `coding`, if this build has
    /// one.
    fn for_coding(coding: &str) -> Option<Self> {
        let coding = coding.trim();
        #[cfg(feature = "gzip")]
        if coding.eq_ignore_ascii_case("gzip") || coding.eq_ignore_ascii_case("x-gzip") {
            return Some(Self::Gzip(Box::new(flate2::write::MultiGzDecoder::new(
                Vec::new(),
            ))));
        }
        #[cfg(feature = "brotli")]
        if coding.eq_ignore_ascii_case("br") {
            return Some(Self::Brotli(Box::new(
                brotli_decompressor::DecompressorWriter::new(Vec::new(), 4096),
            )));
        }
        #[cfg(feature = "zstd")]
        if coding.eq_ignore_ascii_case("zstd") {
            return Some(Self::Zstd(Box::default()));
        }
        let _ = coding;
        None
    }

    /// Decodes `input`, the next piece of the encoded body, into whatever
    /// output it completes.
    pub(crate) fn feed(&mut self, input: &[u8]) -> io::Result<Bytes> {
        #[cfg(any(feature = "gzip", feature = "brotli"))]
        use std::io::Write as _;
        let _ = input;
        match self {
            #[cfg(feature = "gzip")]
            Self::Gzip(decoder) => {
                decoder.write_all(input)?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
            #[cfg(feature = "brotli")]
            Self::Brotli(decoder) => {
                decoder.write_all(input)?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
            #[cfg(feature = "zstd")]
            Self::Zstd(decoder) => decoder.feed(input).map(Bytes::from),
            #[cfg(not(any(feature = "gzip", feature = "brotli", feature = "zstd")))]
            Self::None(never) => match *never {},
        }
    }

    /// The output left once the encoded body has ended, or an error when it
    /// ended before its coding's end.
    pub(crate) fn finish(&mut self) -> io::Result<Bytes> {
        match self {
            #[cfg(feature = "gzip")]
            Self::Gzip(decoder) => {
                decoder.try_finish()?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
            #[cfg(feature = "brotli")]
            Self::Brotli(decoder) => {
                decoder.close()?;
                Ok(std::mem::take(decoder.get_mut()).into())
            }
            #[cfg(feature = "zstd")]
            Self::Zstd(decoder) => decoder.finish().map(Bytes::from),
            #[cfg(not(any(feature = "gzip", feature = "brotli", feature = "zstd")))]
            Self::None(never) => match *never {},
        }
    }
}

/// A Zstandard decoder over a body that arrives in pieces of any size.
///
/// `ruzstd` decodes whole blocks from a reader, so the frame and block
/// headers are read here to hand it only complete blocks (RFC 8878 §3.1.1).
#[cfg(feature = "zstd")]
mod zstd {
    use std::io;

    use ruzstd::decoding::{BlockDecodingStrategy, FrameDecoder};

    /// The largest window a `zstd` content coding may use (RFC 9659 §3).
    const MAX_WINDOW: u64 = 8 << 20;

    /// The magic number that starts a Zstandard frame.
    const MAGIC: u32 = 0xFD2F_B528;

    pub(crate) struct Zstd {
        decoder: FrameDecoder,
        /// Received bytes not yet decoded.
        input: Vec<u8>,
        /// Inside a frame: whether its blocks end with a checksum.
        frame: Option<bool>,
    }

    impl Default for Zstd {
        fn default() -> Self {
            let mut decoder = FrameDecoder::new();
            decoder.set_max_window_size(MAX_WINDOW);
            Self {
                decoder,
                input: Vec::new(),
                frame: None,
            }
        }
    }

    fn invalid(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
        io::Error::new(io::ErrorKind::InvalidData, error)
    }

    impl Zstd {
        pub(crate) fn feed(&mut self, input: &[u8]) -> io::Result<Vec<u8>> {
            self.input.extend_from_slice(input);
            let mut output = Vec::new();
            loop {
                let Some(checksum) = self.frame else {
                    if !self.start_frame()? {
                        return Ok(output);
                    }
                    continue;
                };
                // A block is a three-byte header, then its content: the
                // block's size for a raw or compressed block, one byte for
                // an RLE block (§3.1.1.2).
                let Some(header) = self.input.get(..3) else {
                    return Ok(output);
                };
                let header = u32::from_le_bytes([header[0], header[1], header[2], 0]);
                let last = header & 1 == 1;
                let size = (header >> 3) as usize;
                let content = match (header >> 1) & 3 {
                    0 | 2 => size,
                    1 => 1,
                    _ => return Err(invalid("a reserved zstd block type")),
                };
                let whole = 3 + content + if last && checksum { 4 } else { 0 };
                if self.input.len() < whole {
                    return Ok(output);
                }
                let mut block = &self.input[..whole];
                self.decoder
                    .decode_blocks(&mut block, BlockDecodingStrategy::UptoBlocks(1))
                    .map_err(invalid)?;
                self.input.drain(..whole);
                if let Some(decoded) = self.decoder.collect() {
                    output.extend_from_slice(&decoded);
                }
                if last {
                    if checksum
                        && self.decoder.get_checksum_from_data()
                            != self.decoder.get_calculated_checksum()
                    {
                        return Err(invalid("a zstd frame failed its checksum"));
                    }
                    self.frame = None;
                }
            }
        }

        /// Starts the next frame, or skips a skippable one, once its header
        /// has arrived; `false` while it has not.
        fn start_frame(&mut self) -> io::Result<bool> {
            let Some(magic) = self.input.get(..4) else {
                return Ok(false);
            };
            let magic = u32::from_le_bytes([magic[0], magic[1], magic[2], magic[3]]);
            if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
                // A skippable frame (§3.1.2): its size, then that many bytes.
                let Some(size) = self.input.get(4..8) else {
                    return Ok(false);
                };
                let whole = 8 + u32::from_le_bytes([size[0], size[1], size[2], size[3]]) as usize;
                if self.input.len() < whole {
                    return Ok(false);
                }
                self.input.drain(..whole);
                return Ok(true);
            }
            if magic != MAGIC {
                return Err(invalid("not a zstd frame"));
            }
            // §3.1.1.1: the descriptor sets which optional fields follow.
            let Some(&descriptor) = self.input.get(4) else {
                return Ok(false);
            };
            let single_segment = descriptor & 0x20 != 0;
            let window = usize::from(!single_segment);
            let dictionary = [0, 1, 2, 4][usize::from(descriptor & 3)];
            let content_size = match descriptor >> 6 {
                0 => usize::from(single_segment),
                1 => 2,
                2 => 4,
                _ => 8,
            };
            let header = 5 + window + dictionary + content_size;
            if self.input.len() < header {
                return Ok(false);
            }
            let mut source = &self.input[..header];
            self.decoder.reset(&mut source).map_err(invalid)?;
            self.input.drain(..header);
            self.frame = Some(descriptor & 0x04 != 0);
            Ok(true)
        }

        pub(crate) fn finish(&mut self) -> io::Result<Vec<u8>> {
            if self.frame.is_some() || !self.input.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the zstd stream ended inside a frame",
                ));
            }
            Ok(Vec::new())
        }
    }
}

#[cfg(test)]
mod tests {
    #[cfg(any(feature = "gzip", feature = "brotli", feature = "zstd"))]
    use super::Decoder;
    use super::{accept_encoding, decode};
    use crate::Body;
    use http::{Method, Response, header};

    /// `hello, world\n` repeated 64 times, the plain text every fixture
    /// encodes (made with the `gzip`, `brotli` and `zstd` tools).
    #[cfg(any(feature = "gzip", feature = "brotli", feature = "zstd"))]
    fn plain() -> String {
        "hello, world\n".repeat(64)
    }

    #[test]
    fn accept_encoding_names_exactly_the_enabled_codings() {
        let expected: Vec<&str> = [
            (cfg!(feature = "gzip"), "gzip"),
            (cfg!(feature = "brotli"), "br"),
            (cfg!(feature = "zstd"), "zstd"),
        ]
        .into_iter()
        .filter_map(|(on, name)| on.then_some(name))
        .collect();
        let sent = accept_encoding().map(|value| value.to_str().expect("ascii").to_owned());
        assert_eq!(sent, (!expected.is_empty()).then(|| expected.join(", ")));
    }

    #[test]
    fn an_unknown_or_layered_coding_is_left_as_it_is() {
        for codings in [&["compress"][..], &["gzip", "br"]] {
            let mut response = Response::new(Body::from("x"));
            for coding in codings {
                response
                    .headers_mut()
                    .append(header::CONTENT_ENCODING, coding.parse().expect("a value"));
            }
            let response = decode(response, &Method::GET);
            assert_eq!(
                response
                    .headers()
                    .get_all(header::CONTENT_ENCODING)
                    .iter()
                    .count(),
                codings.len()
            );
        }
    }

    /// Feeds `encoded` to a decoder for `coding` in pieces of `step` bytes.
    #[cfg(any(feature = "gzip", feature = "brotli", feature = "zstd"))]
    fn decode_in_steps(coding: &str, encoded: &[u8], step: usize) -> std::io::Result<Vec<u8>> {
        let mut decoder = Decoder::for_coding(coding).expect("a decoder in this build");
        let mut out = Vec::new();
        for piece in encoded.chunks(step) {
            out.extend_from_slice(&decoder.feed(piece)?);
        }
        out.extend_from_slice(&decoder.finish()?);
        Ok(out)
    }

    /// Checks that `encoded` decodes to [`plain`] fed in pieces of any
    /// size, and that it fails truncated. Corrupted, it fails when
    /// `checksummed`, and otherwise never decodes to the original: brotli
    /// carries no checksum, so a flipped byte may still decode.
    #[cfg(any(feature = "gzip", feature = "brotli", feature = "zstd"))]
    fn check_coding(coding: &str, encoded: &[u8], checksummed: bool) {
        for step in [1, 7, 4096] {
            let decoded = decode_in_steps(coding, encoded, step).expect("decodes");
            assert_eq!(
                String::from_utf8(decoded).expect("utf-8"),
                plain(),
                "{step}"
            );
        }
        let truncated = &encoded[..encoded.len() - 3];
        assert!(decode_in_steps(coding, truncated, 5).is_err(), "truncated");
        let mut corrupt = encoded.to_vec();
        let middle = corrupt.len() / 2;
        corrupt[middle] ^= 0xff;
        let decoded = decode_in_steps(coding, &corrupt, 5);
        if checksummed {
            assert!(decoded.is_err(), "corrupt");
        } else {
            assert_ne!(decoded.ok(), Some(plain().into_bytes()), "corrupt");
        }
    }

    #[cfg(feature = "gzip")]
    #[test]
    fn gzip_decodes_in_any_pieces_and_rejects_truncation_and_corruption() {
        check_coding(
            "gzip",
            include_bytes!("../tests/parity/fixtures/hello.gz"),
            true,
        );
        assert!(Decoder::for_coding("X-GZIP").is_some());
    }

    #[cfg(feature = "brotli")]
    #[test]
    fn brotli_decodes_in_any_pieces_and_rejects_truncation() {
        check_coding(
            "br",
            include_bytes!("../tests/parity/fixtures/hello.br"),
            false,
        );
    }

    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_decodes_in_any_pieces_and_rejects_truncation_and_corruption() {
        check_coding(
            "zstd",
            include_bytes!("../tests/parity/fixtures/hello.zst"),
            true,
        );
    }

    /// Reads `encoded`, arriving as one piece, through a decoded body for
    /// `coding`, and checks it is 4 MiB of zeros handed out a bounded piece
    /// at a time, so a caller reading chunks holds no more than that.
    #[cfg(any(feature = "gzip", feature = "brotli", feature = "zstd"))]
    async fn check_output_is_bounded(coding: &str, encoded: &'static [u8]) {
        const MOST: usize = 64 << 10;
        let decoder = Decoder::for_coding(coding).expect("a decoder in this build");
        let mut body = Body::decoded(Body::from(encoded), decoder);
        let mut total = 0;
        while let Some(chunk) = body.chunk().await.expect("decodes") {
            assert!(chunk.len() <= MOST, "{coding}: {} bytes", chunk.len());
            assert!(chunk.iter().all(|&byte| byte == 0), "{coding}");
            total += chunk.len();
        }
        assert_eq!(total, 4 << 20, "{coding}");
    }

    /// `head -c 4194304 /dev/zero` through `gzip -9`, `brotli -q 11` and
    /// `zstd -19`: a few bytes that expand a thousandfold or more.
    #[cfg(feature = "gzip")]
    #[tokio::test]
    async fn gzip_output_is_handed_out_a_bounded_piece_at_a_time() {
        check_output_is_bounded("gzip", include_bytes!("../tests/parity/fixtures/zeros.gz")).await;
    }

    #[cfg(feature = "brotli")]
    #[tokio::test]
    async fn brotli_output_is_handed_out_a_bounded_piece_at_a_time() {
        check_output_is_bounded("br", include_bytes!("../tests/parity/fixtures/zeros.br")).await;
    }

    #[cfg(feature = "zstd")]
    #[tokio::test]
    async fn zstd_output_is_handed_out_a_bounded_piece_at_a_time() {
        check_output_is_bounded("zstd", include_bytes!("../tests/parity/fixtures/zeros.zst")).await;
    }

    /// `seq 1 60000` at level 19: several blocks, a window larger than one
    /// block, and a checksum; after a first frame, so frames follow frames.
    #[cfg(feature = "zstd")]
    #[test]
    fn zstd_decodes_frame_after_frame_of_many_blocks() {
        let mut encoded = include_bytes!("../tests/parity/fixtures/hello.zst").to_vec();
        encoded.extend_from_slice(include_bytes!("../tests/parity/fixtures/numbers.zst"));
        let numbers: String = (1..=60_000)
            .flat_map(|n: u32| [n.to_string(), "\n".into()])
            .collect();
        for step in [1000, 65_536] {
            let decoded = decode_in_steps("zstd", &encoded, step).expect("decodes");
            assert_eq!(
                String::from_utf8(decoded).expect("utf-8"),
                plain() + &numbers,
                "{step}"
            );
        }
    }
}
