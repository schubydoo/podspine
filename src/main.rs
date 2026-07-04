//! Podspine binary entrypoint.
//!
//! Wires `config -> scanner -> http` (TAD §9.2). The pipeline crates land over
//! Sprint 1 (POC: prober -> splitter -> feed -> self-check) and Sprint 2
//! (index, config, scanner, http). Until then this is an intentional stub so
//! the workspace builds and CI is green from commit #1.

fn main() {
    println!("podspine: not yet implemented — see tasks.md Sprint 1");
}
