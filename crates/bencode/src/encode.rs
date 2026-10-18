use crate::Value;
use std::io::Write as _;

/// Streaming bencode writer into a `Vec<u8>`.
///
/// The low-level calls (`begin_dict`, `bytes`, ...) emit exactly what they are
/// told; a caller streaming a dictionary by hand must supply keys in ascending
/// byte order itself. [`Encoder::value`] and [`encode`] always sort.
#[derive(Debug, Default, Clone)]
pub struct Encoder {
    buf: Vec<u8>,
}

impl Encoder {
    /// Creates an empty encoder.
    pub fn new() -> Self {
        Self::default()
    }

    /// Creates an encoder with `cap` bytes preallocated.
    pub fn with_capacity(cap: usize) -> Self {
        Encoder {
            buf: Vec::with_capacity(cap),
        }
    }

    /// Writes `i<n>e`.
    pub fn int(&mut self, n: i64) -> &mut Self {
        // Writing to a Vec cannot fail.
        let _ = write!(self.buf, "i{n}e");
        self
    }

    /// Writes `<len>:<bytes>`.
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        let _ = write!(self.buf, "{}:", b.len());
        self.buf.extend_from_slice(b);
        self
    }

    /// Writes a UTF-8 string as a byte string.
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }

    /// Opens a list; close it with [`Encoder::end`].
    pub fn begin_list(&mut self) -> &mut Self {
        self.buf.push(b'l');
        self
    }

    /// Opens a dictionary; close it with [`Encoder::end`].
    pub fn begin_dict(&mut self) -> &mut Self {
        self.buf.push(b'd');
        self
    }

    /// Closes the innermost list or dictionary.
    pub fn end(&mut self) -> &mut Self {
        self.buf.push(b'e');
        self
    }

    /// Appends already-encoded bytes verbatim (e.g. a raw span).
    pub fn raw(&mut self, encoded: &[u8]) -> &mut Self {
        self.buf.extend_from_slice(encoded);
        self
    }

    /// Writes a whole value, emitting dictionary keys in sorted order.
    pub fn value(&mut self, v: &Value<'_>) -> &mut Self {
        match v {
            Value::Int(n) => {
                self.int(*n);
            }
            Value::Bytes(b) => {
                self.bytes(b);
            }
            Value::List(items) => {
                self.begin_list();
                for item in items {
                    self.value(item);
                }
                self.end();
            }
            Value::Dict(entries) => {
                self.begin_dict();
                if entries.windows(2).all(|w| w[0].0 <= w[1].0) {
                    for (k, val) in entries {
                        self.bytes(k).value(val);
                    }
                } else {
                    let mut sorted: Vec<&(&[u8], Value<'_>)> = entries.iter().collect();
                    sorted.sort_by(|a, b| a.0.cmp(b.0));
                    for (k, val) in sorted {
                        self.bytes(k).value(val);
                    }
                }
                self.end();
            }
        }
        self
    }

    /// Bytes written so far.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }

    /// Consumes the encoder and returns the buffer.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }
}

/// Encodes `v` canonically (sorted dictionary keys).
pub fn encode(v: &Value<'_>) -> Vec<u8> {
    let mut e = Encoder::new();
    e.value(v);
    e.finish()
}
