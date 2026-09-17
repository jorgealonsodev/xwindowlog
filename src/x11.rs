//! `x11rb` capture; EWMH verification + `GetInputFocus` fallback (RF-24); BadWindow race (RF-22);
//! `SYNC`/`IDLETIME` alarms and the RF-25 degradation chain; XWayland warning (RF-29); title decode
//! by atom type + 512-char truncation + `/proc/<pid>/comm` fallback (RF-31); reconnect backoff (RF-32).
//! Emits `RawTitle`.

#[cfg(test)]
mod tests {}
