mod normalize;

pub fn greeting(name: &str) -> String {
    format!("Hello, {}!", normalize::name(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_and_defaults() {
        assert_eq!(greeting(" Ada "), "Hello, Ada!");
        assert_eq!(greeting("  "), "Hello, guest!");
    }
}
