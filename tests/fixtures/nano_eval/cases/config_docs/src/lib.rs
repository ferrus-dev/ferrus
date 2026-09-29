pub const DEFAULT_RETRIES: usize = 2;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_and_docs_agree() {
        assert_eq!(DEFAULT_RETRIES, 3);
        assert!(include_str!("../README.md").contains("3 retries"));
    }
}
