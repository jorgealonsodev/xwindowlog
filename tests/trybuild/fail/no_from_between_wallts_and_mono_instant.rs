// RF-28 / design.md D-9: proves no `From`/`Into` conversion exists between
// `WallTs` and `MonoInstant` in either direction. If either impl is ever
// added, both attempts below start compiling and this fixture must fail.
#[path = "../../../src/clock.rs"]
mod clock;

fn main() {
    let wall = clock::WallTs(0);
    let _mono: clock::MonoInstant = wall.into();
}
