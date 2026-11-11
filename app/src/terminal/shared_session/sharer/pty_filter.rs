//! Strips in-band command echo and output from PTY bytes sent to viewers; hooks pass so block IDs stay in step.

const ESC: u8 = 0x1b;
const HOOK_START: [u8; 4] = [ESC, b'P', b'$', b'd'];

/// Tracks hook boundaries across PTY reads, since a read can end in the middle of a hook.
#[derive(Debug, Default)]
pub(crate) struct SharedPtyFilter {
    in_hook: bool,
    previous_was_esc: bool,
    /// How much of [`HOOK_START`] the bytes seen so far end with.
    matched: usize,
    /// How much of that partial match was already forwarded.
    emitted: usize,
}

impl SharedPtyFilter {
    /// Returns the bytes to send to viewers: all of them, or only hooks when `in_band` is set.
    pub(crate) fn filter(&mut self, bytes: &[u8], in_band: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(if in_band { 0 } else { bytes.len() });
        for &byte in bytes {
            if self.in_hook {
                out.push(byte);
                self.in_hook = !(self.previous_was_esc && byte == b'\\');
                self.previous_was_esc = byte == ESC;
                continue;
            }
            let matched = if byte == HOOK_START[self.matched] {
                self.matched + 1
            } else if byte == ESC {
                1
            } else {
                0
            };
            if !in_band {
                out.push(byte);
            }
            if matched == HOOK_START.len() {
                if in_band {
                    out.extend_from_slice(&HOOK_START[self.emitted.min(HOOK_START.len())..]);
                }
                self.in_hook = true;
                self.previous_was_esc = false;
                self.matched = 0;
                self.emitted = 0;
            } else {
                self.emitted = if matched == 0 {
                    0
                } else if in_band {
                    self.emitted.min(matched - 1)
                } else {
                    matched
                };
                self.matched = matched;
            }
        }
        out
    }
}

#[cfg(test)]
#[path = "pty_filter_tests.rs"]
mod tests;
