//! Flag + self-pipe registration and draining (D-4).

#[cfg(test)]
mod tests {
    // Acceptance check (assumption A-3, task 1.8): compile-only proof that
    // `signal_hook::flag::register(sig, Arc<AtomicBool>)` and
    // `signal_hook::low_level::pipe::register(sig, writer)` exist with the
    // shapes design.md D-4 assumes. Never called — its only job is to compile
    // under `cargo test` / `cargo check --tests`.
    #[allow(
        dead_code,
        reason = "compile-only acceptance check for assumption A-3, task 1.8"
    )]
    fn a3_signal_hook_shapes_compile(
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
        pipe_writer: std::os::unix::net::UnixStream,
    ) -> Result<(), std::io::Error> {
        signal_hook::flag::register(signal_hook::consts::SIGTERM, flag)?;
        signal_hook::low_level::pipe::register(signal_hook::consts::SIGTERM, pipe_writer)?;
        Ok(())
    }
}
