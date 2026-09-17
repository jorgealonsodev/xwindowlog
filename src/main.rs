// clap CLI; `flock` (RF-34); config load/validate; `umask(0o077)` before any open (RF-10); reactor construction and `Effect` application.

mod clock;
mod control;
mod exclude;
mod logind;
mod reactor;
mod signals;
mod store;
mod tracker;
mod x11;

fn main() {
    println!("xwindowlog {}", env!("CARGO_PKG_VERSION"));
}

#[cfg(test)]
mod tests {}
