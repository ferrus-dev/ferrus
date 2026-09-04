//! Exact JSON wire sizes without retaining an encoded copy of the value.

use std::io::{self, Write};

use serde::Serialize;

pub(crate) fn serialized_len(value: &(impl Serialize + ?Sized)) -> serde_json::Result<u64> {
    let mut counter = ByteCounter(0);
    serde_json::to_writer(&mut counter, value)?;
    Ok(counter.0)
}

struct ByteCounter(u64);

impl Write for ByteCounter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("JSON byte count overflow"))?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_wire_bytes_including_escaping_and_multibyte_text() {
        for value in [
            serde_json::json!(null),
            serde_json::json!({"text": "\u{00e9}\u{1f980}\n\t\"\\\u{0000}", "n": -123.5}),
            serde_json::json!([[], {}, true, 123456789, "x".repeat(100_000)]),
        ] {
            assert_eq!(
                serialized_len(&value).unwrap(),
                serde_json::to_vec(&value).unwrap().len() as u64
            );
        }
    }

    #[test]
    fn propagates_serialization_failure() {
        struct Invalid;
        impl Serialize for Invalid {
            fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("invalid"))
            }
        }
        assert!(serialized_len(&Invalid).is_err());
    }
}
