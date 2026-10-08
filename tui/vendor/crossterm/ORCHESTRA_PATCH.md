# crossterm 0.28.1, patched for orchestra

This is crossterm 0.28.1 from crates.io with one file changed, `src/event/source/unix/mio.rs` (plus the rustix `event` feature in `Cargo.toml`). It is used through `[patch.crates-io]` in `tui/Cargo.toml`.

## The bug

The Unix event reader reads the terminal in a loop until a read fails with `WouldBlock`. The terminal file descriptor is blocking, so that never happens: once the bytes read so far don't make a complete event, the next `read()` blocks until more input arrives, ignoring the poll timeout. The program's main loop stops, so nothing is redrawn and no keys are handled.

Usually each keypress completes an event, so this goes unnoticed. But after a bracketed-paste start (`ESC[200~`) without its end marker (`ESC[201~`), the parser treats all further input as part of the paste, and the reader blocks on every read. orchestra's session list froze this way until the user pasted something, which supplied the missing end marker.

## The patch

1. The read loop only reads again when `poll()` says the terminal has bytes waiting, so it returns to the caller's poll timeout instead of blocking.
2. If an escape sequence is still incomplete 500 ms after its last byte, the parser gives up on it: a bracketed paste is delivered as received, and anything else (a mouse report cut off after `ESC[<`, for example) is dropped. Otherwise a lost end would swallow all later input, including arrows and Ctrl+C.

To update crossterm, replace this directory with the new version and reapply both changes, or drop the patch once upstream fixes the reader.
