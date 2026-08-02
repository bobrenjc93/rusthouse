//! RustHouse is an experimental, compact analytical database.

/// Returns the product name while the first storage engine is being built.
///
/// # Examples
///
/// ```
/// assert_eq!(rusthouse::product_name(), "RustHouse");
/// ```
pub fn product_name() -> &'static str {
    "RustHouse"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifies_the_database() {
        assert_eq!(product_name(), "RustHouse");
    }
}
