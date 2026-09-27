mod buffer;

pub use buffer::Buffer;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_api() {
        let buffer = Buffer::new(8);
        assert_eq!(buffer.capacity_hint(), 8);
    }
}
