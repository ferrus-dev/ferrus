pub fn is_even(value: i32) -> bool {
    value % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parity() {
        assert!(is_even(2));
        assert!(!is_even(3));
    }
}
