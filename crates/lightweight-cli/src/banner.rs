//! The `lightweight` welcome mark.
//!
//! Typing `lightweight` should confirm, at a glance, that you are in the right
//! place — so it prints the product's feather in colour before it does anything
//! else. The mark is the same one the app icon carries: a silver-edged feather,
//! a teal lattice, a cyan bolt through it.
//!
//! Three rules keep the decoration from ever being in the way:
//!
//!   * **It goes to stderr, never stdout.** `lightweight sysinfo --json | jq`
//!     must see clean JSON; a banner on stdout would be the first parse error.
//!   * **Only for a terminal.** Piped or redirected, stderr is not a TTY and the
//!     banner is suppressed — a log file does not want escape codes.
//!   * **`NO_COLOR` and `LIGHTWEIGHT_NO_BANNER` are honoured.** The first drops
//!     to a monochrome silhouette; the second turns the mark off entirely.
//!
//! The grid below was reduced from `icon/source.png` once and committed, so the
//! art travels with the source rather than being redrawn from an image at build
//! time. Each character is one pixel: `s` silver, `T` teal, `c` cyan, `.` clear.
//! Two pixel rows render into one terminal row with the upper-half block `▀`,
//! foreground painting the top pixel and background the bottom, which doubles
//! the vertical resolution for free.

use std::io::IsTerminal as _;

/// One pixel per character; two rows per rendered line. 20 columns wide.
const FEATHER: &[&str] = &[
    ".................T..",
    ".................T..",
    "................TT..",
    "...............TsTT.",
    "..............TsTTT.",
    ".............TssTTT.",
    "............TssTTTT.",
    "...........TssTTTTT.",
    "..........TssTTTTTT.",
    ".........TssTTTTTT..",
    "........TssTTTTTTT..",
    ".......TssTTTTTTT...",
    "......TsscTTTTTT....",
    "......ssTccTTTTT....",
    ".....TsTTcTTTTT.....",
    "....TssTTTTTTT......",
    "....TssTTTTTTT......",
    "...TssTTTTTTT.......",
    "...TsTTTTTTT........",
    "...TTTTTTTT.........",
    "....sTTTT...........",
    "...sT.T.............",
    "..TT................",
    ".T..................",
];

/// RGB for a pixel role, or `None` for a clear pixel.
fn rgb(pixel: u8) -> Option<(u8, u8, u8)> {
    match pixel {
        b's' => Some((198, 209, 221)),
        b'T' => Some((64, 150, 148)),
        b'c' => Some((52, 226, 212)),
        _ => None,
    }
}

/// Whether the welcome mark should be shown for this run.
///
/// `json` is the parsed `--json` flag: machine-readable output never gets a
/// banner, on stdout or stderr, so a script's first read is never a surprise.
pub fn should_show(json: bool) -> bool {
    !json && std::io::stderr().is_terminal() && std::env::var_os("LIGHTWEIGHT_NO_BANNER").is_none()
}

/// Print the mark and a one-line wordmark to stderr.
pub fn print(version: &str) {
    let colour = std::env::var_os("NO_COLOR").is_none();
    eprint!("{}", render(version, colour));
}

/// Build the mark as a string, so the choice of colour is testable without a
/// terminal.
pub fn render(version: &str, colour: bool) -> String {
    let cols = FEATHER.iter().map(|row| row.len()).max().unwrap_or(0);
    let mut out = String::from("\n");
    let rows: Vec<&[u8]> = FEATHER.iter().map(|row| row.as_bytes()).collect();

    for pair in rows.chunks(2) {
        out.push_str("  ");
        let top = pair[0];
        let bottom = pair.get(1).copied().unwrap_or(b"");
        for col in 0..cols {
            let upper = top.get(col).copied().unwrap_or(b'.');
            let lower = bottom.get(col).copied().unwrap_or(b'.');
            out.push_str(&cell(upper, lower, colour));
        }
        if colour {
            out.push_str("\x1b[0m");
        }
        out.push('\n');
    }

    // A wordmark beside, not on, the mark: one line under it, quiet.
    if colour {
        out.push_str(&format!(
            "  \x1b[1;36mLightweight\x1b[0m \x1b[2m{version} — local CPU inference\x1b[0m\n\n"
        ));
    } else {
        out.push_str(&format!(
            "  Lightweight {version} — local CPU inference\n\n"
        ));
    }
    out
}

/// One rendered character for an upper/lower pixel pair.
fn cell(upper: u8, lower: u8, colour: bool) -> String {
    let (up, low) = (rgb(upper), rgb(lower));
    if !colour {
        // A monochrome silhouette in the terminal's own foreground.
        return match (up.is_some(), low.is_some()) {
            (true, true) => "\u{2588}".to_owned(),  // full block
            (true, false) => "\u{2580}".to_owned(), // upper half
            (false, true) => "\u{2584}".to_owned(), // lower half
            (false, false) => " ".to_owned(),
        };
    }
    match (up, low) {
        (None, None) => " ".to_owned(),
        (Some((r, g, b)), None) => {
            format!("\x1b[49m\x1b[38;2;{r};{g};{b}m\u{2580}")
        }
        (None, Some((r, g, b))) => {
            format!("\x1b[49m\x1b[38;2;{r};{g};{b}m\u{2584}")
        }
        (Some((tr, tg, tb)), Some((br, bg, bb))) => {
            format!("\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg};{bb}m\u{2580}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_line_exceeds_an_eighty_column_terminal() {
        // Escapes do not take screen columns; strip them before measuring.
        for line in render("0.1.2", true).lines() {
            let visible: String = strip_ansi(line);
            assert!(visible.chars().count() <= 78, "too wide: {visible:?}");
        }
    }

    #[test]
    fn the_plain_form_carries_no_escape_sequences() {
        let plain = render("0.1.2", false);
        assert!(
            !plain.contains('\x1b'),
            "monochrome banner must be escape-free"
        );
        assert!(plain.contains("Lightweight"));
    }

    #[test]
    fn the_colour_form_uses_truecolour() {
        assert!(render("0.1.2", true).contains("\x1b[38;2;"));
    }

    #[test]
    fn json_output_is_never_decorated() {
        // The TTY check cannot be forced here, but the json gate is independent
        // and is the one that protects a pipe.
        assert!(!should_show(true));
    }

    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let mut bytes = line.bytes().peekable();
        while let Some(byte) = bytes.next() {
            if byte == 0x1b {
                // Skip until the terminating 'm' of the CSI sequence.
                for next in bytes.by_ref() {
                    if next == b'm' {
                        break;
                    }
                }
            } else {
                out.push(byte as char);
            }
        }
        out
    }
}
