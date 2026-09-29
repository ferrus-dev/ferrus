pub fn clamp(value: i32, low: i32, high: i32) -> i32 {
    value.max(low).max(high)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamps_both_sides() {
        assert_eq!(clamp(-2, 0, 10), 0);
        assert_eq!(clamp(5, 0, 10), 5);
        assert_eq!(clamp(12, 0, 10), 10);
    }
}
