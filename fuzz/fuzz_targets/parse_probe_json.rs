#![no_main]
//! Fuzz the ffprobe-JSON parser — Podspine parses whatever `ffprobe` prints for
//! an input file into a `ProbedBook`. Must return a typed error, never panic, on
//! arbitrary/hostile JSON.
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(json) = std::str::from_utf8(data) {
        let _ = podspine_prober::parse_probe_json(json);
    }
});
