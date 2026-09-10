//! The `lightagent` welcome mark.
//!
//! Typing `lightagent` prints a terminal adaptation of the supplied pixel-art
//! logo: a gold star, a white/cyan lightning bolt, sparkles and pixel lettering.
//! The character grid keeps the artwork portable without image-protocol support
//! or runtime image decoding.
//!
//! Three rules keep the decoration out of the way, matching the sibling binary:
//!
//!   * **stderr only** — `lightagent tools --json | jq` sees clean stdout.
//!   * **a terminal only** — piped or redirected, it is suppressed.
//!   * **`NO_COLOR` and `LIGHTAGENT_NO_BANNER`** — the first drops to a
//!     monochrome silhouette, the second turns the mark off entirely.
//!
//! Each character is one pixel in the logo palette; `.` is transparent.
//! Two pixel rows render into one terminal row with the upper-half block `▀`.

use std::collections::BTreeMap;
use std::io::IsTerminal as _;

/// The 56-column logo fits comfortably in a standard 80-column terminal.
const STAR_BOLT: &[&str] = &[
    ".........................................CCC............",
    "........................................CCBC............",
    ".......................................CCBCC............",
    ".........................CCCC.........CCBBC.............",
    ".........................CBBC........CCBCCC.............",
    "........................CCBBCC......CCBCBC..............",
    "........................CBBBBC.....CCBWBCC..............",
    "............Y.....PB...CCBYYBCC...CCBWCBC...............",
    "...........YHY.........CBBWYBBC..CCBWWBCC....CC.........",
    "..........YHWHY.......CCBBWYBBC.CCBCWCBC....CC..........",
    ".....CC....YHY........CBBYWYYBCCCBCWCBCC...CC....Y......",
    "......CC....Y.........CBBWWYYBCCBCWWCCC.........YHY.....",
    ".......CC............CCBBWWYYBCBCWWCBC.........YHWHY....",
    "........CC...........CBBWWWYCCBCWWCBCC..........YHY.....",
    "....PB..............CCBBWWWCCBCCWWCBC............Y......",
    "....................CBBBWWCCBCCWWCBCC...................",
    "...................CCBBWWCCBCCWWWCBC.........PB.........",
    "...CCCCCCCCCCCCCCCCCBBBWCCBCCWWWCBCCCCCCCCCCCCCCCC......",
    "...CBBBBBBBBBBBBBBBBBBBCCBCCWWWCCBBBBBBBBBBBBBBBBC......",
    "...CCBBBBBBBBBBBBBBBBBCCBCCWWWWCBCBBBBBBBBBBBBBBCC......",
    "....CCBYYYHHHHHHHHHYYCCBCCCWWWCBBCYHHHHHHHHHYYBCC.......",
    ".....CCBYYYHHHHHHHYYCCBCCCWWWWCBCCCCHHHHHHYYYBCC........",
    "......CCBBYYHHHHHYYCCBCCCWWWWCBBBBBCHHHHYYYBBCC.........",
    ".......CCBBYYYYYYYCCBCCCWWWWCCCCCBCCYYYYYYBBCC..........",
    "........CCBBBAAAACCBCCCWWWWWWWWCBCCAAAAABBBCC...........",
    ".........CCBBBAACCBCCCCCCCCWWWCBCCAAAAABBBCC............",
    "..........CCBBBBCBBBBBBBBCWWWCBCCAAAABBBBCC.............",
    "...........CCBBBCCCCCCCBCWWWCBCCAAAABBBBCC..............",
    "............CCBBBBAACCBCWWWCBCCAAAABBBBCC...............",
    ".............CCBBBAACBBWWWCBCCAAAAABBBCC................",
    ".............CCBBAACCBCWWCBCCAAAAAAABBC.................",
    ".............CBBBAYCBCWWCBCCAAAYYYAABBCC..........PB....",
    ".............CBBBYCCBWWCBCCOOOOOYYYOBBBC................",
    "..........PB.CBBBCCBWWCBCCBBOOOOOYYOBBBC................",
    ".......CC....CBBOCBCWCBCCBBBBOOOOWYYBBBC................",
    "......CC....CCBBCCCWCBCCBBBBBBOOOOYYOBBC................",
    ".....CC.....CBBCCBWCBCCBBBCCBBBBOOWYOBBCC....CC.........",
    ".....YHY....CBBCBWCBCCBBCCCCCCBBBOOWYBBBC.....CC........",
    "....YHWHY...CBCCWCBCCBCCC....CCBBBBOOBBBC......CC.......",
    ".....YHY....CBCBCBCCCCC.......CCCBBBOOBBC...............",
    "......Y....CCBBCBCBCC...........CCCBBOBBC...............",
    "...........CBBCBCCCC........Y.....CCCBBBCC..............",
    "...........CCBBCCC.........YHY......CCBBBC..............",
    "...........CCBCC..........YHWHY......CCCBC..............",
    "..........CCBCC.........PB.YHY..PB.....CCC..............",
    "..........CBCC..............Y...........................",
    "..........CCC...........................................",
    "........................................................",
    ".......W....W......W.....W.......................W......",
    ".......YB...BB.....YB....YB......................YB.....",
    ".......YB...Y.YYYY.YYY..YYY.YYY..YYYY.YYYY.YYY..YYY.....",
    ".......YB...YBYBBYBYBBY.BYBBBBBY.YBBYBYBBYBYBBY.BYBB....",
    ".......YB...YBYYYYBYB.YB.YB.YYYYBYYYYBYYYYBYB.YB.YB.....",
    ".......OB...OBBBBOBOB.OB.OB.OBBOBBBBOBOBBBBOB.OB.OB.....",
    ".......OOOO.OBOOOOBOB.OB.OO.OOOOBOOOOBOOOO.OB.OB.OO.....",
    ".......BBBBBBBBBBBBBB.BB.BBBBBBBBBBBBBBBBBBBB.BB.BBB....",
    "............................Y...........................",
    "...........................YHY..........................",
    "........CCCCCCCCCCCCCCCCPCYHWHYCPCCCCCCCCCCCCCCCC.......",
    "...........................YHY..........................",
    "............................Y...........................",
];

/// Live chat metadata rendered beside the logo at startup.
pub(crate) struct StartupInfo<'a> {
    pub(crate) version: &'a str,
    pub(crate) release_date: &'a str,
    pub(crate) profile: &'a str,
    pub(crate) model: &'a str,
    pub(crate) session: &'a str,
    pub(crate) tools: &'a [String],
    pub(crate) skills: &'a [String],
}

/// RGB for a pixel role, or `None` for a clear pixel.
fn rgb(pixel: u8) -> Option<(u8, u8, u8)> {
    match pixel {
        b'Y' => Some((255, 232, 0)),   // gold star and lettering
        b'H' => Some((255, 255, 130)), // star glints
        b'A' => Some((255, 163, 0)),   // amber
        b'O' => Some((255, 112, 0)),   // orange shading
        b'C' => Some((0, 238, 255)),   // cyan bolt and outline
        b'B' => Some((20, 24, 174)),   // deep blue edging
        b'P' => Some((170, 0, 255)),   // purple sparkles
        b'W' => Some((245, 255, 255)), // white highlights
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

/// Print the interactive startup dashboard to stderr.
pub(crate) fn print_startup(info: &StartupInfo<'_>) {
    let colour = std::env::var_os("NO_COLOR").is_none();
    eprint!("{}", render_startup(info, terminal_width(), colour));
}

/// Build the mark as a string, so the choice of colour is testable without a
/// terminal.
pub fn render(version: &str, colour: bool) -> String {
    let mut out = String::from("\n");
    for line in logo_lines(colour) {
        out.push_str(&line);
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

fn logo_lines(colour: bool) -> Vec<String> {
    let cols = STAR_BOLT.iter().map(|row| row.len()).max().unwrap_or(0);
    let rows: Vec<&[u8]> = STAR_BOLT.iter().map(|row| row.as_bytes()).collect();
    let mut lines = Vec::with_capacity(rows.len().div_ceil(2));

    for pair in rows.chunks(2) {
        let mut line = String::from("  ");
        let top = pair[0];
        let bottom = pair.get(1).copied().unwrap_or(b"");
        for col in 0..cols {
            let upper = top.get(col).copied().unwrap_or(b'.');
            let lower = bottom.get(col).copied().unwrap_or(b'.');
            line.push_str(&cell(upper, lower, colour));
        }
        if colour {
            line.push_str("\x1b[0m");
        }
        lines.push(line);
    }
    lines
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(132)
        .clamp(112, 180)
}

fn render_startup(info: &StartupInfo<'_>, width: usize, colour: bool) -> String {
    const LEFT_WIDTH: usize = 60;
    let width = width.max(112);
    let right_width = width.saturating_sub(LEFT_WIDTH + 7);
    let logo = logo_lines(colour);
    let details = startup_details(info, right_width);
    let rows = logo.len().max(details.len());
    let title = format!(" Lightagent v{} ({}) ", info.version, info.release_date);
    let mut out = String::from("\n");
    out.push_str(&paint_border(
        &labelled_border('┌', '┐', &title, width),
        colour,
    ));
    out.push('\n');

    for row in 0..rows {
        let left = logo.get(row).map(String::as_str).unwrap_or("");
        let left_visible = if row < logo.len() {
            STAR_BOLT[0].len() + 2
        } else {
            0
        };
        let right = details.get(row).map(String::as_str).unwrap_or("");
        out.push_str(&paint_border("│", colour));
        out.push(' ');
        out.push_str(left);
        out.push_str(&" ".repeat(LEFT_WIDTH.saturating_sub(left_visible)));
        out.push(' ');
        out.push_str(&paint_border("│", colour));
        out.push(' ');
        out.push_str(&paint_detail(right, colour));
        out.push_str(&" ".repeat(right_width.saturating_sub(right.chars().count())));
        out.push(' ');
        out.push_str(&paint_border("│", colour));
        out.push('\n');
    }

    out.push_str(&paint_border(&labelled_border('└', '┘', "", width), colour));
    out.push_str("\n\n");
    out
}

fn startup_details(info: &StartupInfo<'_>, width: usize) -> Vec<String> {
    let mut lines = vec!["Available Tools".to_owned()];
    let mut groups: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for tool in info.tools {
        let (group, action) = tool.split_once('.').unwrap_or(("other", tool.as_str()));
        groups.entry(group).or_default().push(action);
    }
    if groups.is_empty() {
        lines.push("(none enabled)".to_owned());
    } else {
        for (group, actions) in groups {
            lines.push(fit_line(&format!("{group}: {}", actions.join(", ")), width));
        }
    }
    lines.push(String::new());
    lines.push("Available Skills".to_owned());
    lines.extend(wrap_names(info.skills, width));
    lines.push(String::new());
    lines.push(fit_line(&format!("Profile: {}", info.profile), width));
    lines.push(fit_line(&format!("Model: {}", info.model), width));
    lines.push(fit_line(&format!("Session: {}", info.session), width));
    lines.push(String::new());
    lines.push(fit_line(
        &format!(
            "{} tools · {} skills · /help for commands",
            info.tools.len(),
            info.skills.len()
        ),
        width,
    ));
    lines
}

fn wrap_names(names: &[String], width: usize) -> Vec<String> {
    if names.is_empty() {
        return vec!["(none installed)".to_owned()];
    }
    let mut lines = Vec::new();
    let mut line = String::new();
    for name in names {
        let separator = if line.is_empty() { "" } else { ", " };
        if !line.is_empty() && line.chars().count() + separator.len() + name.chars().count() > width
        {
            lines.push(line);
            line = String::new();
        }
        if !line.is_empty() {
            line.push_str(", ");
        }
        line.push_str(name);
    }
    if !line.is_empty() {
        lines.push(fit_line(&line, width));
    }
    lines
}

fn fit_line(line: &str, width: usize) -> String {
    if line.chars().count() <= width {
        return line.to_owned();
    }
    let keep = width.saturating_sub(1);
    format!("{}…", line.chars().take(keep).collect::<String>())
}

fn labelled_border(left: char, right: char, label: &str, width: usize) -> String {
    let mut line = left.to_string();
    line.push('─');
    line.push_str(label);
    let remaining = width.saturating_sub(line.chars().count() + 1);
    line.push_str(&"─".repeat(remaining));
    line.push(right);
    line
}

fn paint_border(text: &str, colour: bool) -> String {
    if colour {
        format!("\x1b[38;2;190;112;18m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

fn paint_detail(text: &str, colour: bool) -> String {
    if !colour || text.is_empty() {
        return text.to_owned();
    }
    if matches!(text, "Available Tools" | "Available Skills") {
        return format!("\x1b[1;33m{text}\x1b[0m");
    }
    if let Some((label, value)) = text.split_once(": ") {
        return format!("\x1b[38;2;160;112;0m{label}:\x1b[0m \x1b[38;2;255;252;214m{value}\x1b[0m");
    }
    format!("\x1b[38;2;164;121;16m{text}\x1b[0m")
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
        (None, None) => "\x1b[0m ".to_owned(),
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
        let mut chars = line.chars();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for next in chars.by_ref() {
                    if next == 'm' {
                        break;
                    }
                }
            } else {
                out.push(ch);
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

    #[test]
    fn startup_places_live_metadata_beside_the_logo() {
        let tools = vec!["fs.read".to_owned(), "web.search".to_owned()];
        let skills = vec!["research".to_owned(), "notes".to_owned()];
        let dashboard = render_startup(
            &StartupInfo {
                version: "0.3.5",
                release_date: "2026-09-09",
                profile: "default",
                model: "minicpm5-1b@16k",
                session: "session-1",
                tools: &tools,
                skills: &skills,
            },
            132,
            false,
        );
        assert!(dashboard.contains("Lightagent v0.3.5 (2026-09-09)"));
        assert!(dashboard.contains("Available Tools"));
        assert!(dashboard.contains("fs: read"));
        assert!(dashboard.contains("Available Skills"));
        assert!(dashboard.contains("research, notes"));
        assert!(dashboard.contains("Profile: default"));
        assert!(dashboard.contains("Model: minicpm5-1b@16k"));
        assert!(dashboard.contains("Session: session-1"));
        assert!(dashboard.lines().all(|line| line.chars().count() <= 132));
    }
}
