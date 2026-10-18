/// A decoded bencode value borrowing from the input buffer.
///
/// Dictionaries keep their *original* key order (real torrents are sometimes
/// unsorted); use [`Value::is_canonical`] to find out whether re-encoding would
/// reproduce the input byte for byte.
///
/// size: 32 bytes per value; a dict entry is 48 bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value<'a> {
    /// A signed 64-bit integer.
    Int(i64),
    /// A byte string (not necessarily UTF-8).
    Bytes(&'a [u8]),
    /// An ordered list.
    List(Vec<Value<'a>>),
    /// A dictionary in original key order.
    Dict(Vec<(&'a [u8], Value<'a>)>),
}

impl<'a> Value<'a> {
    /// Returns the integer, if this is an `Int`.
    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            _ => None,
        }
    }

    /// Returns the bytes, if this is a `Bytes`.
    pub fn as_bytes(&self) -> Option<&'a [u8]> {
        match self {
            Value::Bytes(b) => Some(b),
            _ => None,
        }
    }

    /// Returns the bytes as UTF-8 text, if this is a `Bytes` holding valid UTF-8.
    pub fn as_str(&self) -> Option<&'a str> {
        self.as_bytes().and_then(|b| core::str::from_utf8(b).ok())
    }

    /// Returns the items, if this is a `List`.
    pub fn as_list(&self) -> Option<&[Value<'a>]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    /// Returns the entries in original order, if this is a `Dict`.
    pub fn as_dict(&self) -> Option<&[(&'a [u8], Value<'a>)]> {
        match self {
            Value::Dict(d) => Some(d),
            _ => None,
        }
    }

    /// Looks up `key` in a `Dict` (linear scan; torrent dicts are small, and keys
    /// are unique because the decoder rejects duplicates). `None` for non-dicts.
    pub fn get(&self, key: &[u8]) -> Option<&Value<'a>> {
        self.as_dict()?
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }

    /// True if every dictionary (recursively) has strictly ascending keys, i.e.
    /// `encode(self)` is byte-identical to the original input. Integers are
    /// always canonical because the decoder rejects the other forms.
    pub fn is_canonical(&self) -> bool {
        match self {
            Value::Int(_) | Value::Bytes(_) => true,
            Value::List(l) => l.iter().all(Value::is_canonical),
            Value::Dict(d) => {
                d.windows(2).all(|w| w[0].0 < w[1].0) && d.iter().all(|(_, v)| v.is_canonical())
            }
        }
    }
}
