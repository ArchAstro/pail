use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A BLAKE3 content hash. Identity for every immutable object and file content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Hash(pub [u8; 32]);

impl Hash {
    pub fn of(data: &[u8]) -> Hash {
        Hash(*blake3::hash(data).as_bytes())
    }

    pub fn hex(&self) -> String {
        hex::encode(self.0)
    }

    pub fn short(&self) -> String {
        hex::encode(&self.0[..6])
    }

    pub fn from_hex(s: &str) -> anyhow::Result<Hash> {
        let v = hex::decode(s)?;
        let arr: [u8; 32] = v
            .try_into()
            .map_err(|_| anyhow::anyhow!("hash must be 32 bytes: {s}"))?;
        Ok(Hash(arr))
    }
}

impl fmt::Display for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for Hash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Hash({})", self.short())
    }
}

impl Serialize for Hash {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for Hash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Hash, D::Error> {
        let s = String::deserialize(d)?;
        Hash::from_hex(&s).map_err(serde::de::Error::custom)
    }
}
