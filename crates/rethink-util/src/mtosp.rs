//! MTOSP framing: 0xAA | length BE16 | XML payload | CRC16 BE | 0xBB

use crate::crc16::crc16;

#[derive(Debug, PartialEq, Eq)]
pub enum MtospError {
    InvalidHeader,
    InvalidTrailer,
    InvalidChecksum,
}

impl std::fmt::Display for MtospError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidHeader => f.write_str("invalid header byte"),
            Self::InvalidTrailer => f.write_str("invalid trailer byte"),
            Self::InvalidChecksum => f.write_str("invalid checksum"),
        }
    }
}

impl std::error::Error for MtospError {}

/// Format an XML payload as an MTOSP frame.
pub fn format(xml: &str) -> Vec<u8> {
    let payload = xml.as_bytes();
    let mut buf = Vec::with_capacity(6 + payload.len());
    buf.push(0xaa);
    buf.push((payload.len() >> 8) as u8);
    buf.push((payload.len() & 0xff) as u8);
    buf.extend_from_slice(payload);
    let crc = crc16(&buf);
    buf.push((crc >> 8) as u8);
    buf.push((crc & 0xff) as u8);
    buf.push(0xbb);
    buf
}

/// Incremental byte-oriented MTOSP splitter.
pub struct Splitter {
    state: u8,
    prev: u8,
    total: i32,
    buf: Vec<u8>,
}

impl Default for Splitter {
    fn default() -> Self {
        Self::new()
    }
}

impl Splitter {
    pub fn new() -> Self {
        Self {
            state: 0,
            prev: 0,
            total: 0,
            buf: Vec::new(),
        }
    }

    /// Feed one byte. On complete frame, returns Some(xml_payload).
    pub fn feed(&mut self, byte: u8) -> Result<Option<String>, MtospError> {
        self.buf.push(byte);
        let mut result = None;

        match self.state {
            0 => {
                if byte != 0xaa {
                    return Err(MtospError::InvalidHeader);
                }
                self.state = 1;
            }
            1 => {
                self.state = 2;
            }
            2 => {
                self.total = i32::from(byte) | (i32::from(self.prev) << 8);
                self.state = 3;
            }
            3 => {
                if self.total > 0 {
                    self.total -= 1;
                } else {
                    self.state = 4;
                }
            }
            4 => {
                if crc16(&self.buf) != 0 {
                    return Err(MtospError::InvalidChecksum);
                }
                self.state = 5;
            }
            5 => {
                if byte != 0xbb {
                    return Err(MtospError::InvalidTrailer);
                }
                self.state = 0;
                let xml = String::from_utf8_lossy(&self.buf[3..self.buf.len() - 3]).into_owned();
                self.buf.clear();
                result = Some(xml);
            }
            _ => {}
        }
        self.prev = byte;
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_layout_is_aa_len_payload_crc_bb() {
        let xml = "<x/>";
        let framed = format(xml);
        assert_eq!(framed[0], 0xaa);
        assert_eq!(*framed.last().unwrap(), 0xbb);
        let len = ((framed[1] as usize) << 8) | framed[2] as usize;
        assert_eq!(len, xml.len());
        assert_eq!(&framed[3..3 + len], xml.as_bytes());
        assert_eq!(framed.len(), 3 + xml.len() + 3);
        // CRC-16 of (header + payload + crc bytes) is 0; BB is outside the CRC.
    }

    #[test]
    fn format_splitter_round_trip() {
        let xml = "<root><a>hello</a></root>";
        let framed = format(xml);
        let mut split = Splitter::new();
        let mut received = Vec::new();
        for b in framed {
            if let Some(s) = split.feed(b).unwrap() {
                received.push(s);
            }
        }
        assert_eq!(received, vec![xml]);
    }

    #[test]
    fn multiple_frames_back_to_back() {
        let a = format("<a/>");
        let b = format("<b>x</b>");
        let stream = [a, b].concat();
        let mut split = Splitter::new();
        let mut received = Vec::new();
        for byte in stream {
            if let Some(s) = split.feed(byte).unwrap() {
                received.push(s);
            }
        }
        assert_eq!(received, vec!["<a/>", "<b>x</b>"]);
    }

    #[test]
    fn bad_header_byte() {
        let mut split = Splitter::new();
        assert_eq!(split.feed(0x00).unwrap_err(), MtospError::InvalidHeader);
    }

    #[test]
    fn bad_trailer_byte() {
        let mut framed = format("<x/>");
        let last = framed.len() - 1;
        framed[last] = 0xcc;
        let mut split = Splitter::new();
        let mut err = None;
        for byte in framed {
            match split.feed(byte) {
                Ok(_) => {}
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        assert_eq!(err, Some(MtospError::InvalidTrailer));
    }

    #[test]
    fn real_captured_wtdn3_deviceinfo() {
        let xml = concat!(
            r#"<mTosp><response type="deviceinfo">"#,
            r#"<protocolVer>2.0</protocolVer>"#,
            r#"<mac>7c:1c:4e:c8:cc:53</mac>"#,
            r#"<uuid>48552db0-1ab4-11e9-b4fb-7c1c4ec8cc53</uuid>"#,
            r#"<deviceType>201</deviceType>"#,
            r#"<modelName>WTDN3</modelName>"#,
            r#"<softwareVer>1.3</softwareVer>"#,
            r#"<countryCode>WW</countryCode>"#,
            r#"<remainingTime>388</remainingTime>"#,
            r#"<errorcodeDisplay>0</errorcodeDisplay>"#,
            r#"<modemVer>QC_Modem_1.2.80</modemVer>"#,
            r#"<demandType>MODEM_3k_SoC</demandType>"#,
            r#"</response></mTosp>"#
        );
        assert_eq!(xml.len(), 0x01a5);

        let mut framed = vec![0xaa, 0x01, 0xa5];
        framed.extend_from_slice(xml.as_bytes());
        framed.extend_from_slice(&[0x73, 0xda, 0xbb]);

        let mut split = Splitter::new();
        let mut received = Vec::new();
        for byte in framed {
            if let Some(s) = split.feed(byte).unwrap() {
                received.push(s);
            }
        }
        assert_eq!(received, vec![xml]);
    }

    #[test]
    fn bad_checksum() {
        let mut framed = format("<x/>");
        framed[3] ^= 0x01;
        let mut split = Splitter::new();
        let mut err = None;
        for byte in framed {
            match split.feed(byte) {
                Ok(_) => {}
                Err(e) => {
                    err = Some(e);
                    break;
                }
            }
        }
        assert_eq!(err, Some(MtospError::InvalidChecksum));
    }
}
