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

/// Sort canonical JSON keys once per value, propagating serialization failures.
pub(crate) fn sort_by_json_key<T>(
    values: Vec<T>,
    key: impl Fn(&T) -> Result<Vec<u8>, serde_json::Error>,
) -> Result<Vec<T>, serde_json::Error> {
    let mut keyed = values
        .into_iter()
        .map(|value| key(&value).map(|key| (key, value)))
        .collect::<Result<Vec<_>, _>>()?;
    keyed.sort_by(|left, right| left.0.cmp(&right.0));
    Ok(keyed.into_iter().map(|(_, value)| value).collect())
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

    #[test]
    fn canonical_sort_keeps_equal_keys_in_order_and_serializes_once_per_value() {
        let calls = std::cell::Cell::new(0);
        let values = vec![("b", 0), ("a", 1), ("a", 2)];
        let sorted = sort_by_json_key(values, |(key, _)| {
            calls.set(calls.get() + 1);
            serde_json::to_vec(key)
        })
        .unwrap();
        assert_eq!(sorted, vec![("a", 1), ("a", 2), ("b", 0)]);
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn canonical_sort_propagates_key_serialization_failure() {
        let result = sort_by_json_key(vec![1, 2], |_| {
            Err(<serde_json::Error as serde::ser::Error>::custom(
                "invalid key",
            ))
        });
        assert!(result.is_err());
    }
}
