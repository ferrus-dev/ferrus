pub fn name(input: &str) -> &str {
    if input.is_empty() { "guest" } else { input }
}
