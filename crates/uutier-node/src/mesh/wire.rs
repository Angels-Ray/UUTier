//! 线上帧编解码。

use thiserror::Error;

/// 帧头魔数。
pub const MAGIC: [u8; 4] = *b"UUT1";

/// 线上协议版本。
pub const WIRE_VERSION: u8 = 1;

/// 帧头长度，单位 byte。
pub const HEADER_LEN: usize = 16;

/// 数据帧中 nonce 的长度，单位 byte。
pub const DATA_NONCE_LEN: usize = 8;

/// Noise 认证标签长度，单位 byte。
pub const TAG_LEN: usize = 16;

/// 数据帧载荷的最小长度，单位 byte。
pub const MIN_DATA_PAYLOAD_LEN: usize = DATA_NONCE_LEN + TAG_LEN;

/// 帧类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum FrameKind {
    HandshakeInit = 1,
    HandshakeResp = 2,
    Data = 3,
    Keepalive = 4,
    Ping = 5,
    Pong = 6,
}

impl FrameKind {
    /// 返回线上字节值。
    pub fn as_u8(self) -> u8 {
        self as u8
    }

    /// 从线上字节值解析。
    pub fn from_u8(value: u8) -> Result<Self, WireError> {
        match value {
            1 => Ok(Self::HandshakeInit),
            2 => Ok(Self::HandshakeResp),
            3 => Ok(Self::Data),
            4 => Ok(Self::Keepalive),
            5 => Ok(Self::Ping),
            6 => Ok(Self::Pong),
            other => Err(WireError::UnknownKind(other)),
        }
    }
}

/// 帧头。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub kind: FrameKind,
    /// 握手帧为接收方 peer ID，数据帧为会话标识。
    pub session_id: u64,
}

impl Header {
    /// 写入帧头。
    pub fn encode(self, out: &mut [u8]) -> Result<(), WireError> {
        if out.len() < HEADER_LEN {
            return Err(WireError::TooShort {
                len: out.len(),
                need: HEADER_LEN,
            });
        }
        out[..4].copy_from_slice(&MAGIC);
        out[4] = WIRE_VERSION;
        out[5] = self.kind.as_u8();
        out[6..8].fill(0);
        out[8..16].copy_from_slice(&self.session_id.to_be_bytes());
        Ok(())
    }

    /// 解析帧头并返回载荷。
    pub fn decode(buf: &[u8]) -> Result<(Self, &[u8]), WireError> {
        if buf.len() < HEADER_LEN {
            return Err(WireError::TooShort {
                len: buf.len(),
                need: HEADER_LEN,
            });
        }
        if buf[..4] != MAGIC {
            return Err(WireError::BadMagic);
        }
        if buf[4] != WIRE_VERSION {
            return Err(WireError::UnsupportedVersion(buf[4]));
        }
        let kind = FrameKind::from_u8(buf[5])?;
        let flags = u16::from_be_bytes([buf[6], buf[7]]);
        if flags != 0 {
            return Err(WireError::ReservedFlags(flags));
        }
        let mut session_id = [0u8; 8];
        session_id.copy_from_slice(&buf[8..16]);
        Ok((
            Self {
                kind,
                session_id: u64::from_be_bytes(session_id),
            },
            &buf[HEADER_LEN..],
        ))
    }
}

/// 编码一个完整帧，返回总长度。
pub fn encode(
    kind: FrameKind,
    session_id: u64,
    payload: &[u8],
    out: &mut [u8],
) -> Result<usize, WireError> {
    let total = HEADER_LEN + payload.len();
    if out.len() < total {
        return Err(WireError::TooShort {
            len: out.len(),
            need: total,
        });
    }
    Header { kind, session_id }.encode(out)?;
    out[HEADER_LEN..total].copy_from_slice(payload);
    Ok(total)
}

/// 解析数据帧载荷，返回 nonce 和密文。
pub fn parse_data_payload(payload: &[u8]) -> Result<(u64, &[u8]), WireError> {
    if payload.len() < MIN_DATA_PAYLOAD_LEN {
        return Err(WireError::DataPayloadTooShort(payload.len()));
    }
    let mut nonce = [0u8; DATA_NONCE_LEN];
    nonce.copy_from_slice(&payload[..DATA_NONCE_LEN]);
    Ok((u64::from_be_bytes(nonce), &payload[DATA_NONCE_LEN..]))
}

/// 写入数据帧载荷，返回长度。
pub fn encode_data_payload(
    nonce: u64,
    ciphertext: &[u8],
    out: &mut [u8],
) -> Result<usize, WireError> {
    let total = DATA_NONCE_LEN + ciphertext.len();
    if out.len() < total {
        return Err(WireError::TooShort {
            len: out.len(),
            need: total,
        });
    }
    out[..DATA_NONCE_LEN].copy_from_slice(&nonce.to_be_bytes());
    out[DATA_NONCE_LEN..total].copy_from_slice(ciphertext);
    Ok(total)
}

/// 解析 ping 或 pong 的序号。
pub fn decode_sequence(payload: &[u8]) -> Result<u64, WireError> {
    if payload.len() != 8 {
        return Err(WireError::BadSequenceLength(payload.len()));
    }
    let mut sequence = [0u8; 8];
    sequence.copy_from_slice(payload);
    Ok(u64::from_be_bytes(sequence))
}

/// 编码 ping 或 pong 的序号。
pub fn encode_sequence(sequence: u64) -> [u8; 8] {
    sequence.to_be_bytes()
}

/// 帧编解码错误。
#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("frame is {len} bytes, need at least {need}")]
    TooShort { len: usize, need: usize },

    #[error("bad frame magic")]
    BadMagic,

    #[error("unsupported wire version {0}")]
    UnsupportedVersion(u8),

    #[error("unknown frame kind {0}")]
    UnknownKind(u8),

    #[error("reserved flags must be zero, got {0}")]
    ReservedFlags(u16),

    #[error("data payload is {0} bytes, need at least {MIN_DATA_PAYLOAD_LEN}")]
    DataPayloadTooShort(usize),

    #[error("sequence payload is {0} bytes, need 8")]
    BadSequenceLength(usize),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_kind() {
        let kinds = [
            FrameKind::HandshakeInit,
            FrameKind::HandshakeResp,
            FrameKind::Data,
            FrameKind::Keepalive,
            FrameKind::Ping,
            FrameKind::Pong,
        ];
        let mut buf = [0u8; 64];
        for kind in kinds {
            let len = encode(kind, 42, b"payload", &mut buf).unwrap();
            let (header, payload) = Header::decode(&buf[..len]).unwrap();
            assert_eq!(header.kind, kind);
            assert_eq!(header.session_id, 42);
            assert_eq!(payload, b"payload");
        }
    }

    #[test]
    fn rejects_bad_header() {
        let mut buf = [0u8; HEADER_LEN];
        Header {
            kind: FrameKind::Data,
            session_id: 1,
        }
        .encode(&mut buf)
        .unwrap();

        let mut bad = buf;
        bad[0] = b'X';
        assert_eq!(Header::decode(&bad).unwrap_err(), WireError::BadMagic);

        let mut bad = buf;
        bad[4] = 2;
        assert_eq!(
            Header::decode(&bad).unwrap_err(),
            WireError::UnsupportedVersion(2)
        );

        let mut bad = buf;
        bad[5] = 9;
        assert_eq!(Header::decode(&bad).unwrap_err(), WireError::UnknownKind(9));

        let mut bad = buf;
        bad[7] = 1;
        assert_eq!(
            Header::decode(&bad).unwrap_err(),
            WireError::ReservedFlags(1)
        );

        assert_eq!(
            Header::decode(&buf[..4]).unwrap_err(),
            WireError::TooShort { len: 4, need: 16 }
        );
    }

    #[test]
    fn round_trips_data_payload() {
        let ciphertext = [7u8; 32];
        let mut buf = [0u8; 40];
        let len = encode_data_payload(9, &ciphertext, &mut buf).unwrap();
        assert_eq!(len, DATA_NONCE_LEN + ciphertext.len());

        let (nonce, payload) = parse_data_payload(&buf[..len]).unwrap();
        assert_eq!(nonce, 9);
        assert_eq!(payload, ciphertext);
    }

    #[test]
    fn rejects_short_data_payload() {
        assert_eq!(
            parse_data_payload(&[0u8; 23]).unwrap_err(),
            WireError::DataPayloadTooShort(23)
        );
    }

    #[test]
    fn round_trips_sequence() {
        let payload = encode_sequence(77);
        assert_eq!(decode_sequence(&payload).unwrap(), 77);
        assert_eq!(
            decode_sequence(&[0u8; 7]).unwrap_err(),
            WireError::BadSequenceLength(7)
        );
    }
}
