pub fn status(code: u16) -> &'static str {
    if code == 200 { "ok" } else { "error" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_created_after_first_read() {
        assert_eq!(status(200), "ok");
        assert_eq!(status(201), "created");
    }
}
