//! Faststart detection for whole-file MP4 (`.m4a`/`.m4b`) — pure byte parsing,
//! **no ffprobe**.
//!
//! An MP4 seeks quickly only when its `moov` atom (the index) sits BEFORE the
//! `mdat` payload ("faststart"); if `mdat` comes first, a player must read to the
//! end of the file before it can seek. Podspine serves whole-file episodes in
//! place (Sprint 6.2), so it detects this at ingest and can optionally remux to
//! faststart (Sprint 6.3, `PODSPINE_REMUX_NON_FASTSTART`).
//!
//! Detection reads only the top-level box headers (seeking past each payload), so
//! it touches a few dozen bytes regardless of file size, and never shells out.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// Upper bound on top-level boxes scanned, so a malformed or hostile file can't
/// spin the loop. A real MP4 reaches `moov`/`mdat` within a handful of boxes.
const MAX_BOXES: usize = 256;

/// Whether `path` is a **non-faststart MP4** — i.e. it IS an MP4 (a `ftyp` box is
/// present) and its `mdat` box precedes `moov`.
///
/// Returns `false` for a faststart MP4 (`moov` first), a non-MP4 (MP3/OGG/FLAC —
/// no `ftyp`/`moov`/`mdat` boxes), or any read/parse error. Failing to `false` is
/// deliberate: a file we can't classify is served in place unchanged, never
/// remuxed on a guess.
pub fn needs_faststart(path: &Path) -> bool {
    scan(path).unwrap_or(false)
}

fn scan(path: &Path) -> std::io::Result<bool> {
    let mut f = File::open(path)?;
    let len = f.metadata()?.len();
    let mut pos: u64 = 0;
    let mut saw_ftyp = false;

    for _ in 0..MAX_BOXES {
        // Saturating, not `pos + 8`: a crafted 64-bit `largesize` can push `pos`
        // near u64::MAX, and a plain add would overflow (panic under
        // overflow-checks). Saturating breaks cleanly instead.
        if pos.saturating_add(8) > len {
            break;
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut hdr = [0u8; 8];
        f.read_exact(&mut hdr)?;
        let size32 = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]);
        let kind = [hdr[4], hdr[5], hdr[6], hdr[7]];

        // Box size field: 1 => a 64-bit largesize follows the type; 0 => the box
        // runs to EOF (only ever the last box).
        let box_size = match size32 {
            1 => {
                let mut ext = [0u8; 8];
                f.read_exact(&mut ext)?;
                u64::from_be_bytes(ext)
            }
            0 => len - pos,
            n => u64::from(n),
        };
        if box_size < 8 {
            break; // malformed header — give up (serve in place)
        }

        match &kind {
            b"ftyp" => saw_ftyp = true,
            // `moov` seen before any `mdat` => already faststart.
            b"moov" => return Ok(false),
            // `mdat` seen before `moov` => needs faststart, but only for a real
            // MP4 (a stray `mdat`-like tag in a non-MP4 shouldn't trigger a remux).
            b"mdat" => return Ok(saw_ftyp),
            _ => {}
        }

        // Advance to the next top-level box; stop on overflow or a non-advancing
        // size so a crafted file can't loop.
        match pos.checked_add(box_size) {
            Some(next) if next > pos => pos = next,
            _ => break,
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal MP4 box: `[u32 size][4-byte type][payload]`.
    fn mp4_box(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut v = size.to_be_bytes().to_vec();
        v.extend_from_slice(kind);
        v.extend_from_slice(payload);
        v
    }

    fn write(name: &str, chunks: &[Vec<u8>]) -> std::path::PathBuf {
        let p = std::env::temp_dir().join(format!("podspine-faststart-{name}"));
        let bytes: Vec<u8> = chunks.iter().flatten().copied().collect();
        std::fs::write(&p, bytes).unwrap();
        p
    }

    #[test]
    fn moov_before_mdat_is_faststart() {
        let p = write(
            "fast",
            &[
                mp4_box(b"ftyp", b"M4A isom"),
                mp4_box(b"moov", &[0u8; 16]),
                mp4_box(b"mdat", &[0u8; 64]),
            ],
        );
        assert!(!needs_faststart(&p), "moov-first mp4 is already faststart");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn mdat_before_moov_needs_faststart() {
        let p = write(
            "slow",
            &[
                mp4_box(b"ftyp", b"M4A isom"),
                mp4_box(b"mdat", &[0u8; 64]),
                mp4_box(b"moov", &[0u8; 16]),
            ],
        );
        assert!(needs_faststart(&p), "mdat-first mp4 needs faststart");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn mdat_first_without_ftyp_is_not_treated_as_mp4() {
        // No `ftyp` => not an MP4 we should touch, even if an `mdat`-like tag leads.
        let p = write(
            "nottyp",
            &[mp4_box(b"mdat", &[0u8; 32]), mp4_box(b"moov", &[0u8; 8])],
        );
        assert!(!needs_faststart(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn non_mp4_bytes_are_false() {
        // MP3-ish / arbitrary leading bytes have no valid box structure.
        let p = write(
            "mp3ish",
            &[b"ID3\x04\x00\x00\x00\x00\x00\x21rest-of-file".to_vec()],
        );
        assert!(!needs_faststart(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn short_or_missing_file_is_false() {
        let p = write("tiny", &[vec![0u8, 1, 2]]);
        assert!(!needs_faststart(&p));
        let _ = std::fs::remove_file(&p);
        assert!(!needs_faststart(std::path::Path::new(
            "/nonexistent/podspine/faststart"
        )));
    }

    #[test]
    fn a_box_smaller_than_its_8_byte_header_is_malformed() {
        // A size field below the 8-byte box header (here 4) is malformed → give up
        // (return false), never loop or misread.
        let p = write("tinybox", &[vec![0, 0, 0, 4, b'f', b't', b'y', b'p']]);
        assert!(!needs_faststart(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_box_size_overflowing_the_offset_terminates() {
        // A valid `ftyp`, then a 64-bit largesize box whose size overflows
        // `pos + size` → `checked_add` returns None → clean break (no panic/loop).
        let mut bytes = mp4_box(b"ftyp", b"isom");
        bytes.extend_from_slice(&1u32.to_be_bytes()); // size32 = 1 (largesize follows)
        bytes.extend_from_slice(b"free");
        bytes.extend_from_slice(&u64::MAX.to_be_bytes());
        let p = write("overflow-add", &[bytes]);
        assert!(!needs_faststart(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_crafted_64bit_largesize_box_does_not_overflow() {
        // First box: size32 == 1 (a 64-bit largesize follows the type), type
        // "free", largesize near u64::MAX. Advancing `pos` by it must not overflow
        // the `pos + 8` guard (would panic under overflow-checks); returns false.
        let mut bytes = 1u32.to_be_bytes().to_vec(); // size32 = 1
        bytes.extend_from_slice(b"free");
        bytes.extend_from_slice(&(u64::MAX - 4).to_be_bytes()); // largesize
        let p = write("largesize", &[bytes]);
        assert!(!needs_faststart(&p));
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn a_zero_size_box_does_not_loop() {
        // size32 == 0 means "to EOF"; must terminate, not spin.
        let mut bytes = mp4_box(b"ftyp", b"isom");
        bytes.extend_from_slice(&0u32.to_be_bytes()); // size 0
        bytes.extend_from_slice(b"free");
        bytes.extend_from_slice(&[0u8; 16]);
        let p = write("zerobox", &[bytes]);
        // free-to-EOF with no moov/mdat => false, and returns promptly.
        assert!(!needs_faststart(&p));
        let _ = std::fs::remove_file(&p);
    }
}
