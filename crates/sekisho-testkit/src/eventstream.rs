//! AWS event-stream messages, as Bedrock's `converse-stream` sends them.
//!
//! A message is its total length and its headers' length (each a big-endian
//! `u32`), a CRC-32 of those 8 bytes, the headers, the payload, and a CRC-32
//! of everything before it. Each header is its name's length (`u8`), the
//! name, a value type (`7`, a string), the value's length (`u16`) and the
//! value. agentgateway reads them with `aws-smithy-eventstream`.

use crc::{CRC_32_ISO_HDLC, Crc};

const CRC32: Crc<u32> = Crc::<u32>::new(&CRC_32_ISO_HDLC);
/// The header value type for a string.
const STRING: u8 = 7;

/// One Converse stream event: `:event-type` names it and `payload` is its
/// JSON, as Bedrock sends them.
pub fn frame(event_type: &str, payload: &[u8]) -> Vec<u8> {
    message(
        &[
            (":event-type", event_type),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ],
        payload,
    )
}

fn message(headers: &[(&str, &str)], payload: &[u8]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for (name, value) in headers {
        encoded.push(u8::try_from(name.len()).expect("a header name under 256 bytes"));
        encoded.extend_from_slice(name.as_bytes());
        encoded.push(STRING);
        let value_len = u16::try_from(value.len()).expect("a header value under 64 KiB");
        encoded.extend_from_slice(&value_len.to_be_bytes());
        encoded.extend_from_slice(value.as_bytes());
    }
    let total =
        u32::try_from(12 + encoded.len() + payload.len() + 4).expect("a message under 4 GiB");
    let headers_len = u32::try_from(encoded.len()).expect("headers under 4 GiB");
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(&total.to_be_bytes());
    out.extend_from_slice(&headers_len.to_be_bytes());
    let prelude_crc = CRC32.checksum(&out);
    out.extend_from_slice(&prelude_crc.to_be_bytes());
    out.extend_from_slice(&encoded);
    out.extend_from_slice(payload);
    let message_crc = CRC32.checksum(&out);
    out.extend_from_slice(&message_crc.to_be_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crc32(bytes: &[u8]) -> u32 {
        crc::Crc::<u32>::new(&crc::CRC_32_ISO_HDLC).checksum(bytes)
    }

    fn be32(bytes: &[u8]) -> u32 {
        u32::from_be_bytes(bytes.try_into().unwrap())
    }

    #[test]
    fn frames_follow_the_aws_layout() {
        let payload = br#"{"x":1}"#;
        let f = frame("contentBlockDelta", payload);
        let len = f.len();
        assert_eq!(be32(&f[0..4]) as usize, len);
        let headers_len = be32(&f[4..8]) as usize;
        assert_eq!(12 + headers_len + payload.len() + 4, len);
        assert_eq!(be32(&f[8..12]), crc32(&f[0..8]), "prelude CRC");
        assert_eq!(be32(&f[len - 4..]), crc32(&f[..len - 4]), "message CRC");

        let mut headers = Vec::new();
        let mut rest = &f[12..12 + headers_len];
        while !rest.is_empty() {
            let name_len = rest[0] as usize;
            let name = std::str::from_utf8(&rest[1..1 + name_len]).unwrap();
            assert_eq!(rest[1 + name_len], 7, "{name} has a string value");
            let value_len = u16::from_be_bytes([rest[2 + name_len], rest[3 + name_len]]) as usize;
            let value = &rest[4 + name_len..4 + name_len + value_len];
            headers.push((name, std::str::from_utf8(value).unwrap()));
            rest = &rest[4 + name_len + value_len..];
        }
        assert_eq!(
            headers,
            [
                (":event-type", "contentBlockDelta"),
                (":content-type", "application/json"),
                (":message-type", "event"),
            ]
        );
        assert_eq!(&f[len - 4 - payload.len()..len - 4], payload);
    }

    /// AWS's own test vector for a message with no headers and no payload
    /// (`AAAAEAAAAAAFwkjrfZjI/w==`): the CRCs are standard CRC-32.
    #[test]
    fn an_empty_message_matches_the_aws_test_vector() {
        assert_eq!(
            message(&[], b""),
            [
                0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x05, 0xc2, 0x48, 0xeb, 0x7d, 0x98,
                0xc8, 0xff,
            ]
        );
    }
}
