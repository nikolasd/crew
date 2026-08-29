//! Prompt delivery framing for vendor TUIs (CREW-4).
//!
//! Every reserved TUI vendor's `compose_input` is "the message bytes plus
//! one trailing CR" -- the CR is the submit keystroke, and the adapter
//! shell already splits it off so it can be delivered separately once the
//! PTY has gone idle (see `run_pipeline`'s phase-1/phase-2 comments).
//!
//! That leaves the *text* half, which used to travel as one unframed PTY
//! write. Any newline inside it reached the vendor as a literal Enter, so
//! a multi-line prompt was submitted line-by-line: the vendor started a
//! turn on the first line and dropped or queued the rest, and whatever
//! was still sitting in the composer when the adapter's own CR arrived
//! was submitted as "the prompt" -- a mid-sentence tail fragment, with no
//! error anywhere.
//!
//! This module frames the text as one **bracketed paste** so every byte
//! inside it is content rather than keystrokes, and splits it into
//! write-sized chunks so the adapter can pace delivery and observe the
//! vendor draining it instead of writing megabytes blind.

/// Bracketed-paste introducer -- `ESC [ 2 0 0 ~`.
pub(crate) const PASTE_START: &[u8] = b"\x1b[200~";
/// Bracketed-paste terminator -- `ESC [ 2 0 1 ~`.
pub(crate) const PASTE_END: &[u8] = b"\x1b[201~";

/// The smallest chunk this module will emit: a chunk has to be able to
/// hold a framing marker plus at least one 4-byte UTF-8 scalar, or
/// chunking could make no forward progress.
const MIN_CHUNK: usize = 16;

/// Frames `text` as exactly one bracketed paste and splits the result
/// into chunks of at most `chunk_size` bytes.
///
/// The payload is normalized so that nothing inside the paste can be read
/// as a submit or as an early end-of-paste:
///
/// * `\r\n` and bare `\r` become `\n`. A vendor that ignores paste mode
///   would otherwise treat an embedded CR as Enter -- the exact failure
///   this framing exists to prevent, so it is closed twice.
/// * A literal `ESC [ 2 0 1 ~` occurring *in the prompt text* has its
///   escape byte dropped, so prompt content can never terminate the
///   paste early and have its tail interpreted as keystrokes.
///
/// Chunk boundaries never split a framing marker or a multi-byte UTF-8
/// scalar. An empty `text` still yields one framed (empty) paste: the
/// caller's contract is "these bytes are the prompt", and a vendor that
/// receives an empty paste does nothing, which is the correct outcome.
pub(crate) fn paste_chunks(text: &str, chunk_size: usize) -> Vec<Vec<u8>> {
    let payload = normalize_payload(text);
    let budget = chunk_size.max(MIN_CHUNK);

    let mut chunks: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::with_capacity(budget);
    current.extend_from_slice(PASTE_START);

    for ch in payload.chars() {
        // Flush before appending, never mid-scalar: a chunk boundary that
        // split a multi-byte scalar would hand the vendor an invalid
        // UTF-8 prefix, which is exactly the corruption this is here to
        // prevent. `current.len() > PASTE_START.len()` keeps the very
        // first flush from emitting a chunk that is only the introducer.
        if current.len() + ch.len_utf8() > budget && current.len() > PASTE_START.len() {
            chunks.push(std::mem::take(&mut current));
            current = Vec::with_capacity(budget);
        }
        let mut buf = [0u8; 4];
        current.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
    }

    // The terminator gets its own chunk rather than displacing content
    // when the last content chunk has no room left for it.
    if !current.is_empty() && current.len() + PASTE_END.len() > budget {
        chunks.push(std::mem::take(&mut current));
    }
    current.extend_from_slice(PASTE_END);
    chunks.push(current);
    chunks
}

/// `text` with every carriage return folded to a newline and any literal
/// paste terminator defanged. See [`paste_chunks`] for why each matters.
fn normalize_payload(text: &str) -> String {
    let end = std::str::from_utf8(PASTE_END).expect("the paste terminator is valid UTF-8");
    // Dropping the escape byte is enough to defang it, and keeps the
    // visible characters the author typed.
    let defanged = &end[1..];
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace(end, defanged)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reassembles what the vendor would see: the concatenation of every
    /// chunk, with the outer framing removed.
    fn delivered_payload(chunks: &[Vec<u8>]) -> String {
        let flat: Vec<u8> = chunks.concat();
        assert!(
            flat.starts_with(PASTE_START),
            "the first chunk must open the paste"
        );
        assert!(
            flat.ends_with(PASTE_END),
            "the last chunk must close the paste"
        );
        let body = &flat[PASTE_START.len()..flat.len() - PASTE_END.len()];
        String::from_utf8(body.to_vec()).expect("payload stays valid UTF-8")
    }

    #[test]
    fn a_short_prompt_is_one_framed_chunk() {
        let chunks = paste_chunks("hello", 4096);
        assert_eq!(chunks.len(), 1, "a short prompt needs no splitting");
        assert_eq!(chunks[0], b"\x1b[200~hello\x1b[201~".to_vec());
    }

    #[test]
    fn the_framing_markers_appear_exactly_once() {
        let chunks = paste_chunks(&"line\n".repeat(500), 256);
        let flat = chunks.concat();
        assert_eq!(
            flat.windows(PASTE_START.len())
                .filter(|w| *w == PASTE_START)
                .count(),
            1,
            "exactly one paste introducer"
        );
        assert_eq!(
            flat.windows(PASTE_END.len())
                .filter(|w| *w == PASTE_END)
                .count(),
            1,
            "exactly one paste terminator"
        );
    }

    #[test]
    fn a_multi_line_prompt_survives_chunking_intact() {
        let prompt = (0..200)
            .map(|i| format!("line {i} of the prompt"))
            .collect::<Vec<_>>()
            .join("\n");
        let chunks = paste_chunks(&prompt, 128);
        assert!(chunks.len() > 1, "this prompt must actually be split");
        assert_eq!(
            delivered_payload(&chunks),
            prompt,
            "every line must arrive, in order"
        );
    }

    #[test]
    fn no_chunk_exceeds_the_budget() {
        let prompt = "abcdefghij".repeat(4096);
        let chunks = paste_chunks(&prompt, 512);
        // Asserted before the per-chunk bound so the bound can never be
        // satisfied vacuously by an empty or unsplit result.
        assert!(chunks.len() > 1, "a 40KB prompt must be split at 512 bytes");
        assert_eq!(delivered_payload(&chunks), prompt);
        for (i, chunk) in chunks.iter().enumerate() {
            assert!(
                chunk.len() <= 512,
                "chunk {i} is {} bytes, over the 512 budget",
                chunk.len()
            );
        }
    }

    #[test]
    fn a_64kb_prompt_arrives_byte_for_byte() {
        let prompt = "The quick brown fox jumps over the lazy dog.\n".repeat(1500);
        assert!(prompt.len() > 64 * 1024, "fixture must exceed 64KB");
        let chunks = paste_chunks(&prompt, 4096);
        assert_eq!(delivered_payload(&chunks), prompt);
    }

    /// The CREW-4 size matrix. Multi-line at every size, since embedded
    /// newlines -- not length -- are the mechanism that corrupted the
    /// prompt; plus a single-line control at the largest size, which is
    /// the shape that would have passed even before the fix.
    #[test]
    fn prompts_of_every_size_arrive_intact_multi_line_and_single_line() {
        for target in [1024usize, 8 * 1024, 64 * 1024] {
            let multi_line = {
                let mut s = String::new();
                let mut i = 0;
                while s.len() < target {
                    s.push_str(&format!("line {i} of a prompt that must survive\n"));
                    i += 1;
                }
                s
            };
            assert!(multi_line.len() >= target);
            assert_eq!(
                delivered_payload(&paste_chunks(&multi_line, PASTE_CHUNK_TEST_BYTES)),
                multi_line,
                "a {target}-byte multi-line prompt must arrive intact"
            );

            let single_line = "x".repeat(target);
            assert_eq!(
                delivered_payload(&paste_chunks(&single_line, PASTE_CHUNK_TEST_BYTES)),
                single_line,
                "a {target}-byte single-line prompt must arrive intact"
            );
        }
    }

    /// The production chunk size, so the matrix above exercises the real
    /// number of writes rather than a test-only one.
    const PASTE_CHUNK_TEST_BYTES: usize = 1024;

    #[test]
    fn a_multibyte_scalar_is_never_split_across_chunks() {
        // Every char is 4 bytes, and the budget is deliberately not a
        // multiple of 4 -- a naive byte split would tear one apart and
        // `delivered_payload`'s UTF-8 decode would fail.
        let prompt = "𝄞".repeat(500);
        let chunks = paste_chunks(&prompt, 23);
        assert_eq!(delivered_payload(&chunks), prompt);
    }

    #[test]
    fn carriage_returns_inside_the_prompt_become_newlines() {
        let chunks = paste_chunks("first\r\nsecond\rthird", 4096);
        assert_eq!(
            delivered_payload(&chunks),
            "first\nsecond\nthird",
            "no CR may survive inside the paste: a vendor ignoring paste \
             mode would read it as Enter"
        );
        assert!(
            !chunks.concat().contains(&b'\r'),
            "not one CR byte reaches the pty inside the framing"
        );
    }

    #[test]
    fn a_paste_terminator_in_the_prompt_text_cannot_end_the_paste() {
        let chunks = paste_chunks("before\x1b[201~ after", 4096);
        let flat = chunks.concat();
        assert_eq!(
            flat.windows(PASTE_END.len())
                .filter(|w| *w == PASTE_END)
                .count(),
            1,
            "the only paste terminator is the adapter's own closing one"
        );
        assert_eq!(delivered_payload(&chunks), "before[201~ after");
    }

    #[test]
    fn an_empty_prompt_is_still_a_well_formed_paste() {
        let chunks = paste_chunks("", 4096);
        assert_eq!(chunks.concat(), b"\x1b[200~\x1b[201~".to_vec());
    }

    #[test]
    fn an_unusable_chunk_size_is_floored_rather_than_looping() {
        let chunks = paste_chunks("abcdef", 0);
        assert_eq!(delivered_payload(&chunks), "abcdef");
    }
}
