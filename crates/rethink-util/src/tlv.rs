//! ThinQ TLV encode/decode (10-bit type + variable-length value).

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tlv {
    pub t: u16,
    pub l: Option<u8>,
    pub v: u32,
}

impl Tlv {
    pub fn new(t: u16, v: u32) -> Self {
        Self { t, l: None, v }
    }
}

/// One TLV element with byte range `[byte_start, byte_end)` in the parsed buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlvSpan {
    pub tlv: Tlv,
    pub byte_start: usize,
    pub byte_end: usize,
}

/// Walk a TLV sequence. Truncation stops iteration (no error).
///
/// `visit` receives `(tlv, byte_start, byte_end)`.
fn for_each_tlv(buf: &[u8], mut visit: impl FnMut(Tlv, usize, usize)) {
    let mut i = 0usize;
    while i < buf.len() {
        if i + 2 > buf.len() {
            return;
        }
        let start = i;
        let t = (u16::from(buf[i]) << 2) + u16::from(buf[i + 1] >> 6);
        let l = (buf[i + 1] >> 4) & 3;
        let mut v = u32::from(buf[i + 1] & 15);

        if i + 2 + l as usize > buf.len() {
            return;
        }

        if l > 0 {
            v = 0;
            for j in 0..l as usize {
                v = (v << 8) | u32::from(buf[i + 2 + j]);
            }
        }
        let end = i + 2 + l as usize;
        visit(
            Tlv {
                t,
                l: Some(l),
                v,
            },
            start,
            end,
        );
        i = end;
    }
}

/// Parse a TLV sequence. Truncation is tolerated (returns what was successfully parsed).
pub fn parse(buf: &[u8]) -> Vec<Tlv> {
    // l=0 encodings are 2 bytes; this is an upper bound, not a guess.
    let mut out = Vec::with_capacity(buf.len() / 2);
    for_each_tlv(buf, |tlv, _, _| out.push(tlv));
    out
}

/// Parse TLV and record each element's byte span within `buf`.
pub fn parse_with_spans(buf: &[u8]) -> Vec<TlvSpan> {
    let mut out = Vec::with_capacity(buf.len() / 2);
    for_each_tlv(buf, |tlv, byte_start, byte_end| {
        out.push(TlvSpan {
            tlv,
            byte_start,
            byte_end,
        });
    });
    out
}

/// Build a TLV byte sequence from elements.
pub fn build(elements: &[Tlv]) -> Vec<u8> {
    // Max encoding is 2 header bytes + 3 value bytes.
    let mut out = Vec::with_capacity(elements.len().saturating_mul(5));
    for el in elements {
        let t0 = ((el.t >> 2) & 255) as u8;
        out.push(t0);
        let tl = ((el.t & 3) << 6) as u8;

        if el.v < 0x10 {
            out.push(tl | (el.v as u8));
        } else if el.v < 0x100 {
            out.push(tl | 0x10);
            out.push(el.v as u8);
        } else if el.v < 0x10000 {
            out.push(tl | 0x20);
            out.push(((el.v >> 8) & 0xff) as u8);
            out.push((el.v & 0xff) as u8);
        } else {
            out.push(tl | 0x30);
            out.push(((el.v >> 16) & 0xff) as u8);
            out.push(((el.v >> 8) & 0xff) as u8);
            out.push((el.v & 0xff) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Case {
        name: &'static str,
        tlv: Tlv,
        bytes: Vec<u8>,
    }

    fn cases() -> Vec<Case> {
        vec![
            Case {
                name: "l=0 single nibble",
                tlv: Tlv::new(0x1f7, 1),
                bytes: vec![0x7d, 0xc1],
            },
            Case {
                name: "l=1 byte value",
                tlv: Tlv::new(0x1fe, 0x42),
                bytes: vec![0x7f, 0x90, 0x42],
            },
            Case {
                name: "l=2 word value",
                tlv: Tlv::new(0x1fd, 0x1234),
                bytes: vec![0x7f, 0x60, 0x12, 0x34],
            },
            Case {
                name: "l=3 24-bit value",
                tlv: Tlv::new(0x100, 0x123456),
                bytes: vec![0x40, 0x30, 0x12, 0x34, 0x56],
            },
        ]
    }

    #[test]
    fn build_and_parse_all_length_encodings() {
        for c in cases() {
            assert_eq!(build(&[c.tlv]), c.bytes, "build {}", c.name);
            let parsed = parse(&c.bytes);
            assert_eq!(parsed.len(), 1, "parse {}", c.name);
            assert_eq!(parsed[0].t, c.tlv.t, "parse t {}", c.name);
            assert_eq!(parsed[0].v, c.tlv.v, "parse v {}", c.name);
        }
    }

    #[test]
    fn parse_with_spans_covers_full_buffer() {
        let bytes = build(&[Tlv::new(0x1f7, 1), Tlv::new(0x1fa, 6)]);
        let spans = parse_with_spans(&bytes);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].byte_start, 0);
        assert_eq!(spans[0].byte_end, spans[1].byte_start);
        assert_eq!(spans[1].byte_end, bytes.len());
        assert_eq!(spans[0].tlv.t, 0x1f7);
        assert_eq!(spans[1].tlv.v, 6);
    }

    #[test]
    fn round_trip_mixed_sequence() {
        let seq = [
            Tlv::new(0x1f7, 0),
            Tlv::new(0x1f9, 4),
            Tlv::new(0x1fa, 8),
            Tlv::new(0x1fe, 42),
            Tlv::new(0x2da, 0xabcd),
            Tlv::new(0x300, 0x010203),
        ];
        let bytes = build(&seq);
        let back = parse(&bytes);
        assert_eq!(back.len(), seq.len());
        for (i, s) in seq.iter().enumerate() {
            assert_eq!(back[i].t, s.t);
            assert_eq!(back[i].v, s.v);
        }
    }

    #[test]
    fn parse_vector_from_real_capture() {
        let buf = crate::hex::decode("7E427DC17E837F502D7F902A").unwrap();
        let out = parse(&buf);
        let expected = [
            (0x1f9u16, 2u32),
            (0x1f7, 1),
            (0x1fa, 3),
            (0x1fd, 0x2d),
            (0x1fe, 0x2a),
        ];
        assert_eq!(out.len(), expected.len());
        for (i, (t, v)) in expected.iter().enumerate() {
            assert_eq!(out[i].t, *t);
            assert_eq!(out[i].v, *v);
        }
    }

    #[test]
    fn parse_tolerates_truncation() {
        let buf = [0x7f, 0x60, 0x12];
        assert!(parse(&buf).is_empty());
    }

    #[test]
    fn parse_tolerates_1_byte_truncation() {
        assert!(parse(&[0x7e]).is_empty());
    }

    #[test]
    fn parse_empty_and_many_l0() {
        assert!(parse(&[]).is_empty());
        assert!(parse_with_spans(&[]).is_empty());
        // 32 consecutive l=0 elements (2 bytes each).
        let mut seq = Vec::new();
        for i in 0..32u16 {
            seq.push(Tlv::new(0x100 + i, (i % 15) as u32));
        }
        let bytes = build(&seq);
        let back = parse(&bytes);
        assert_eq!(back.len(), 32);
        for (i, t) in seq.iter().enumerate() {
            assert_eq!(back[i].t, t.t);
            assert_eq!(back[i].v, t.v);
        }
        let spans = parse_with_spans(&bytes);
        assert_eq!(spans.len(), 32);
        assert_eq!(spans[0].byte_start, 0);
        assert_eq!(spans[31].byte_end, bytes.len());
    }

    #[test]
    fn parse_stops_before_truncated_tail_keeping_prefix() {
        let mut bytes = build(&[Tlv::new(0x1f7, 1), Tlv::new(0x1fd, 0x1234)]);
        bytes.pop(); // chop last value byte of the l=2 element
        let out = parse(&bytes);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].t, 0x1f7);
        assert_eq!(out[0].v, 1);
    }
}
