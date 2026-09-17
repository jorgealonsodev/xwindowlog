// RF-28 / design.md D-9: the reverse direction of the same proof — no
// conversion from `MonoInstant` back to `WallTs` exists either.
#[path = "../../../src/clock.rs"]
mod clock;

fn main() {
    let mono = clock::MonoInstant(std::time::Instant::now());
    let _wall: clock::WallTs = mono.into();
}
