//! RF-7 / §14.3: a `SafeTitle` may only be produced by putting a `RawTitle`
//! through `Excluder::evaluate`. No other module may mint one directly from a
//! string, or the sanitization boundary is a naming convention rather than a
//! guarantee. This fixture must fail to compile.

#[path = "../../../src/exclude.rs"]
mod exclude;

fn main() {
    // Fabricating a "safe" title that never passed through evaluate.
    let _bypassed = exclude::SafeTitle::from_sanitized(
        "keepassxc: bank password sk-abcdefghijklmnop".to_string(),
    );
}
