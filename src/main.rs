// clap CLI; `flock` (RF-34); config load/validate; `umask(0o077)` before any open (RF-10); reactor construction and `Effect` application.
//
// Thin shell over the `xwindowlog` library crate (task 8.0): the module
// tree that used to be declared here now lives in `src/lib.rs`, `pub`, so
// `tests/*.rs` integration tests can exercise it. Argument parsing and
// daemon composition (reactor construction, `flock`, config load, `umask`)
// land in Phase 15-17; this binary currently only proves it links against
// the library.

fn main() {
    println!("xwindowlog {}", env!("CARGO_PKG_VERSION"));
}
