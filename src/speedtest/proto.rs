//! Minimal protobuf wire encoder/decoder for the Devin Connect-RPC calls.
//! Only the field shapes llmstat actually sends are built; the reader walks
//! arbitrary messages so response parsing can pick the fields it needs.

/// Wire types.
const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const LEN: u8 = 2;
const FIXED32: u8 = 5;

/// Accumulates one protobuf message.
#[derive(Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Self::default()
    }

    fn varint(&mut self, mut v: u64) {
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                return;
            }
            self.buf.push(b | 0x80);
        }
    }

    fn tag(&mut self, field: u32, wire: u8) {
        self.varint(((field as u64) << 3) | wire as u64);
    }

    /// varint fields: uint/int/enum/bool all encode the same way.
    pub fn varint_field(&mut self, field: u32, v: u64) {
        self.tag(field, VARINT);
        self.varint(v);
    }

    pub fn bool_field(&mut self, field: u32, v: bool) {
        self.varint_field(field, v as u64);
    }

    pub fn double(&mut self, field: u32, v: f64) {
        self.tag(field, FIXED64);
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }

    #[allow(dead_code)]
    pub fn float(&mut self, field: u32, v: f32) {
        self.tag(field, FIXED32);
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }

    pub fn string(&mut self, field: u32, s: &str) {
        self.bytes(field, s.as_bytes());
    }

    pub fn bytes(&mut self, field: u32, b: &[u8]) {
        self.tag(field, LEN);
        self.varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    pub fn message(&mut self, field: u32, m: &Writer) {
        self.bytes(field, &m.buf);
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// One decoded field value.
pub enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Fixed32(u32),
    Bytes(&'a [u8]),
}

impl<'a> Value<'a> {
    pub fn as_str(&self) -> Option<&'a str> {
        match self {
            Value::Bytes(b) => std::str::from_utf8(b).ok(),
            _ => None,
        }
    }

    /// f64 regardless of whether the field is declared double or float.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Fixed64(b) => Some(f64::from_bits(*b)),
            Value::Fixed32(b) => Some(f32::from_bits(*b) as f64),
            _ => None,
        }
    }
}

/// Iterates the fields of one protobuf message.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = *self.buf.get(self.pos)?;
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
            if shift >= 64 {
                return None;
            }
        }
    }
}

impl<'a> Iterator for Reader<'a> {
    /// (field number, value)
    type Item = (u32, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        let key = self.varint()?;
        let field = (key >> 3) as u32;
        let value = match (key & 7) as u8 {
            VARINT => Value::Varint(self.varint()?),
            FIXED64 => {
                let b = self.buf.get(self.pos..self.pos + 8)?;
                self.pos += 8;
                Value::Fixed64(u64::from_le_bytes(b.try_into().ok()?))
            }
            LEN => {
                let len = self.varint()? as usize;
                let b = self.buf.get(self.pos..self.pos + len)?;
                self.pos += len;
                Value::Bytes(b)
            }
            FIXED32 => {
                let b = self.buf.get(self.pos..self.pos + 4)?;
                self.pos += 4;
                Value::Fixed32(u32::from_le_bytes(b.try_into().ok()?))
            }
            _ => return None,
        };
        Some((field, value))
    }
}
