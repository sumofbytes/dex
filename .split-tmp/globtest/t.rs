pub(crate) mod inner {
    pub(crate) fn used_via_glob() -> u8 { 1 }
    pub(crate) fn unused_fn() -> u8 { 2 }
}
pub(crate) use inner::{used_via_glob, unused_fn};
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn t() { assert_eq!(used_via_glob(), 1); }
}
