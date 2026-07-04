#![no_main]
//! Fuzz the `.cue` sidecar parser — an attacker-influenceable file parsed into
//! chapter cut points (75 fps `INDEX 01`). Must never panic on arbitrary bytes.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = podspine_chapters::parse_cue(text, 3600.0);
    }
});
