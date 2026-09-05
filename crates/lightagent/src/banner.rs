//! The `lightagent` welcome mark.
//!
//! Typing `lightagent` prints, before anything else, a deliberately simplified
//! pixel star with a lightning bolt through it — the product's mark — exactly as
//! the `lightweight` binary greets with its feather. The grid is committed here
//! rather than rendered from a raster at run time; the full-colour artwork lives
//! in `icon/lightagent-source.png` and this grid is the reduction of it.
//!
//! Three rules keep the decoration out of the way, matching the sibling binary:
//!
//!   * **stderr only** — `lightagent tools --json | jq` sees clean stdout.
//!   * **a terminal only** — piped or redirected, it is suppressed.
//!   * **`NO_COLOR` and `LIGHTAGENT_NO_BANNER`** — the first drops to a
//!     monochrome silhouette, the second turns the mark off entirely.
//!
//! Each character is one pixel: `Y` star yellow, `O` star orange, `C` bolt cyan,
//! `W` bolt highlight, `.` clear. Two pixel rows render into one terminal row
//! with the upper-half block `▀`, doubling the vertical resolution for free.

use std::io::IsTerminal as _;

/// One pixel per character; two rows per rendered line. 19 columns wide, so the
/// mark plus its two-space indent stays well within an 80-column terminal.
const STAR_BOLT: &[&str] = &[
    ".........Y.........",
    ".........Y.........",
    "........YYY.C......",
    "........YYYC.......",
    ".......YYYYC.......",
    "YYYYYYYYYYCYYYYYYYY",
    ".YYYYYYYYWCYYYYYYY.",
    "..YYYYYYYCYYYYYYY..",
    "...YYYYYCWYYYYYY...",
    "....OOOOCOOOOOO....",
    "....OOOCWOOOOOO....",
    "...OOOOC...OOOOO...",
    "..OOOOOW....OOOOO..",
    "..OOOOC......OOOO..",
    ".OOOO.C.......OOOO.",
    ".OOO..C........OOO.",
    "OOO.............OOO",
    "OO...............OO",
];

/// RGB for a pixel role, or `None` for a clear pixel.
fn rgb(pixel: u8) -> Option<(u8, u8, u8)> {
    match pixel {
        b'Y' => Some((250, 205, 45)),  // star, upper
        b'O' => Some((240, 150, 35)),  // star, lower
        b'C' => Some((52, 226, 212)),  // bolt
        b'W' => Some((228, 255, 255)), // bolt highlight
        _ => None,
    }
}

/// Whether the welcome mark should be shown for this run.
///
/// `json` is the parsed `--json` flag: machine-readable output never gets a
/// banner, so a script's first read is never a surprise.
pub fn should_show(json: bool) -> bool {
    !json && std::io::stderr().is_terminal() && std::env::var_os("LIGHTAGENT_NO_BANNER").is_none()
}

/// Print the mark and a one-line wordmark to stderr.
pub fn print(version: &str) {
    let colour = std::env::var_os("NO_COLOR").is_none();
    eprint!("{}", render(version, colour));
}

/// Build the mark as a string, so the choice of colour is testable without a
/// terminal.
pub fn render(version: &str, colour: bool) -> String {
    let cols = STAR_BOLT.iter().map(|row| row.len()).max().unwrap_or(0);
    let mut out = String::from("\n");
    let rows: Vec<&[u8]> = STAR_BOLT.iter().map(|row| row.as_bytes()).collect();

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

    if colour {
        out.push_str(&format!(
            "  \x1b[1;33mLight\x1b[1;36magent\x1b[0m \x1b[2m{version} — local intelligence with live tools\x1b[0m\n\n"
        ));
    } else {
        out.push_str(&format!(
            "  Lightagent {version} — local intelligence with live tools\n\n"
        ));
    }
    out
}

/// One rendered character for an upper/lower pixel pair.
fn cell(upper: u8, lower: u8, colour: bool) -> String {
    let (up, low) = (rgb(upper), rgb(lower));
    if !colour {
        return match (up.is_some(), low.is_some()) {
            (true, true) => "\u{2588}".to_owned(),  // full block
            (true, false) => "\u{2580}".to_owned(), // upper half
            (false, true) => "\u{2584}".to_owned(), // lower half
            (false, false) => " ".to_owned(),
        };
    }
    match (up, low) {
        (None, None) => " ".to_owned(),
        (Some((r, g, b)), None) => format!("\x1b[49m\x1b[38;2;{r};{g};{b}m\u{2580}"),
        (None, Some((r, g, b))) => format!("\x1b[49m\x1b[38;2;{r};{g};{b}m\u{2584}"),
        (Some((tr, tg, tb)), Some((br, bg, bb))) => {
            format!("\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg};{bb}m\u{2580}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(line: &str) -> String {
        let mut out = String::new();
        let mut bytes = line.bytes().peekable();
        while let Some(byte) = bytes.next() {
            if byte == 0x1b {
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

    #[test]
    fn no_line_exceeds_an_eighty_column_terminal() {
        for line in render("0.2.1", true).lines() {
            let visible = strip_ansi(line);
            assert!(visible.chars().count() <= 78, "too wide: {visible:?}");
        }
    }

    #[test]
    fn the_plain_form_carries_no_escape_sequences() {
        let plain = render("0.2.1", false);
        assert!(
            !plain.contains('\x1b'),
            "monochrome banner must be escape-free"
        );
        assert!(plain.contains("Lightagent"));
    }

    #[test]
    fn the_colour_form_uses_truecolour() {
        assert!(render("0.2.1", true).contains("\x1b[38;2;"));
    }

    #[test]
    fn the_mark_uses_half_blocks() {
        assert!(render("0.2.1", true).contains('\u{2580}'));
    }

    #[test]
    fn json_output_is_never_decorated() {
        assert!(!should_show(true));
    }

    #[test]
    fn every_grid_row_is_the_same_width() {
        let width = STAR_BOLT[0].len();
        for (index, row) in STAR_BOLT.iter().enumerate() {
            assert_eq!(row.len(), width, "row {index} has the wrong width");
        }
    }
}
