#![no_main]
//! Fuzz the `.ffmeta` sidecar parser — the ffmpeg-metadata `[CHAPTER]` format
//! parsed from an attacker-influenceable file. Must never panic on arbitrary
//! bytes (TIMEBASE arithmetic, malformed sections, huge numbers, …).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let _ = podspine_chapters::parse_ffmeta(text);
    }
});
