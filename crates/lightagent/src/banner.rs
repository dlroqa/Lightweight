//! The `lightagent` welcome mark.
//!
//! Typing `lightagent` prints a true-colour terminal adaptation of the supplied
//! latest pixel-art logo: the gold star, cyan lightning bolt and block-pixel
//! `Lightagent` wordmark. An embedded RGBA derivative keeps the exact artwork
//! portable without terminal-specific image protocols or runtime image decoding.
//!
//! Three rules keep the decoration out of the way, matching the sibling binary:
//!
//!   * **stderr only** — `lightagent tools --json | jq` sees clean stdout.
//!   * **a terminal only** — piped or redirected, it is suppressed.
//!   * **`NO_COLOR` and `LIGHTAGENT_NO_BANNER`** — the first drops to a
//!     monochrome silhouette, the second turns the mark off entirely.
//!
//! Two image rows render into one terminal row with half-block characters.
//! The startup dashboard uses a larger, sharper derivative when the terminal is
//! wide enough to hold it beside the live details; narrower terminals and the
//! plain mark keep the compact one.

use std::collections::BTreeMap;
use std::io::IsTerminal as _;

const COMPACT_WIDTH: usize = 56;
const COMPACT_HEIGHT: usize = 52;
const COMPACT_RGBA: &[u8; COMPACT_WIDTH * COMPACT_HEIGHT * 4] =
    include_bytes!("../assets/lightagent-logo-56x52.rgba");

const LARGE_WIDTH: usize = 72;
const LARGE_HEIGHT: usize = 68;
const LARGE_RGBA: &[u8; LARGE_WIDTH * LARGE_HEIGHT * 4] =
    include_bytes!("../assets/lightagent-logo-72x68.rgba");

/// One embedded terminal-native RGBA derivative of the logo.
struct Logo {
    width: usize,
    height: usize,
    rgba: &'static [u8],
}

/// Fits within an 80-column terminal; used by the plain mark and narrow
/// startup dashboards.
const COMPACT_LOGO: Logo = Logo {
    width: COMPACT_WIDTH,
    height: COMPACT_HEIGHT,
    rgba: COMPACT_RGBA,
};

/// Closer to the artwork's native pixel grid, so the wordmark stays legible.
const LARGE_LOGO: Logo = Logo {
    width: LARGE_WIDTH,
    height: LARGE_HEIGHT,
    rgba: LARGE_RGBA,
};

/// Narrowest detail column kept beside the logo in the side-by-side layout.
const DETAIL_MIN_WIDTH: usize = 47;

/// Live chat metadata rendered beside the logo at startup.
pub(crate) struct StartupInfo<'a> {
    pub(crate) version: &'a str,
    pub(crate) release_date: &'a str,
    pub(crate) profile: &'a str,
    pub(crate) model: &'a str,
    pub(crate) session: &'a str,
    pub(crate) tools: &'a [String],
    pub(crate) skills: &'a [String],
    pub(crate) extensions: &'a [String],
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
    for line in logo_lines(&COMPACT_LOGO, colour) {
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

fn logo_lines(logo: &Logo, colour: bool) -> Vec<String> {
    let mut lines = Vec::with_capacity(logo.height.div_ceil(2));
    for top_row in (0..logo.height).step_by(2) {
        let mut line = String::from("  ");
        for column in 0..logo.width {
            let upper = logo_pixel(logo, top_row, column);
            let lower = logo_pixel(logo, top_row + 1, column);
            line.push_str(&image_cell(upper, lower, colour));
        }
        if colour {
            line.push_str("\x1b[0m");
        }
        lines.push(line);
    }
    lines
}

/// Read one terminal-native RGBA pixel. The embedded mark uses binary alpha and
/// a compact, non-dithered palette, so every visible source pixel maps directly
/// to one sharply rendered terminal pixel without a blended halo.
fn logo_pixel(logo: &Logo, row: usize, column: usize) -> Option<(u8, u8, u8)> {
    if row >= logo.height || column >= logo.width {
        return None;
    }
    let offset = (row * logo.width + column) * 4;
    if logo.rgba[offset + 3] < 128 {
        return None;
    }
    Some((
        logo.rgba[offset],
        logo.rgba[offset + 1],
        logo.rgba[offset + 2],
    ))
}

/// Visible columns reserved for a logo in the side-by-side layout: two leading
/// spaces from `logo_lines` plus a two-column gap before the details.
fn side_by_side_left_width(logo: &Logo) -> usize {
    logo.width + 4
}

/// Terminal width at which `logo` fits beside a usable detail column.
fn side_by_side_min_width(logo: &Logo) -> usize {
    side_by_side_left_width(logo) + 1 + DETAIL_MIN_WIDTH + 4
}

/// The live terminal width, with `COLUMNS` retained as a fallback for previews
/// and unusual terminals where the OS size probe is unavailable.
pub(crate) fn terminal_width() -> usize {
    dialoguer::console::Term::stdout()
        .size_checked()
        .map(|(_, columns)| usize::from(columns))
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()
                .and_then(|value| value.parse::<usize>().ok())
        })
        .unwrap_or(132)
        .max(64)
}

fn render_startup(info: &StartupInfo<'_>, width: usize, colour: bool) -> String {
    let width = width.max(64);
    let mark = if width >= side_by_side_min_width(&LARGE_LOGO) {
        &LARGE_LOGO
    } else {
        &COMPACT_LOGO
    };
    let left_width = side_by_side_left_width(mark);
    let logo = logo_lines(mark, colour);
    let content_width = width.saturating_sub(4);
    let side_by_side = width >= side_by_side_min_width(mark);
    let detail_width = if side_by_side {
        content_width.saturating_sub(left_width + 1)
    } else {
        content_width
    };
    let details = startup_details(info, detail_width);
    let title = format!(" Lightagent v{} ({}) ", info.version, info.release_date);
    let mut out = String::from("\n");
    out.push_str(&paint_border(
        &labelled_border('┌', '┐', &title, width),
        colour,
    ));
    out.push('\n');

    if side_by_side {
        let rows = logo.len().max(details.len());
        for row in 0..rows {
            let left = logo.get(row).map(String::as_str).unwrap_or("");
            let left_visible = if row < logo.len() { mark.width + 2 } else { 0 };
            let right = details.get(row).map(String::as_str).unwrap_or("");
            out.push_str(&paint_border("│", colour));
            out.push(' ');
            out.push_str(left);
            out.push_str(&" ".repeat(left_width.saturating_sub(left_visible)));
            // Deliberately use whitespace rather than a divider: the modern
            // mark and the live harness information share one open canvas.
            out.push(' ');
            out.push_str(&paint_detail(right, colour));
            out.push_str(&" ".repeat(detail_width.saturating_sub(right.chars().count())));
            out.push(' ');
            out.push_str(&paint_border("│", colour));
            out.push('\n');
        }
    } else {
        for line in &logo {
            push_startup_row(&mut out, line, mark.width + 2, content_width, colour, false);
        }
        push_startup_row(&mut out, "", 0, content_width, colour, false);
        for line in &details {
            push_startup_row(
                &mut out,
                line,
                line.chars().count(),
                content_width,
                colour,
                true,
            );
        }
    }

    out.push_str(&paint_border(&labelled_border('└', '┘', "", width), colour));
    out.push('\n');
    out
}

fn push_startup_row(
    out: &mut String,
    text: &str,
    visible_width: usize,
    width: usize,
    colour: bool,
    detail: bool,
) {
    out.push_str(&paint_border("│", colour));
    out.push(' ');
    if detail {
        out.push_str(&paint_detail(text, colour));
    } else {
        out.push_str(text);
    }
    out.push_str(&" ".repeat(width.saturating_sub(visible_width)));
    out.push(' ');
    out.push_str(&paint_border("│", colour));
    out.push('\n');
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
            lines.extend(wrap_line(
                &format!("{group}: {}", actions.join(", ")),
                width,
            ));
        }
    }
    lines.push(String::new());
    lines.push("Available Skills".to_owned());
    lines.extend(wrap_names(info.skills, width));
    lines.push(String::new());
    lines.push("Active Extensions".to_owned());
    if info.extensions.is_empty() {
        lines.push("(none active)".to_owned());
    } else {
        lines.extend(wrap_names(info.extensions, width));
    }
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
            lines.extend(wrap_line(&line, width));
            line = String::new();
        }
        if !line.is_empty() {
            line.push_str(", ");
        }
        line.push_str(name);
    }
    if !line.is_empty() {
        lines.extend(wrap_line(&line, width));
    }
    lines
}

fn wrap_line(line: &str, width: usize) -> Vec<String> {
    line.chars()
        .collect::<Vec<_>>()
        .chunks(width.max(1))
        .map(|part| part.iter().copied().collect())
        .collect()
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

/// One rendered character for an upper/lower image-pixel pair.
fn image_cell(up: Option<(u8, u8, u8)>, low: Option<(u8, u8, u8)>, colour: bool) -> String {
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
    fn embedded_logo_has_the_expected_rgba_geometry() {
        for logo in [&COMPACT_LOGO, &LARGE_LOGO] {
            assert_eq!(logo.rgba.len(), logo.width * logo.height * 4);
            assert_eq!(logo_lines(logo, false).len(), logo.height.div_ceil(2));
            assert!(logo_pixel(logo, logo.height / 2, logo.width / 2).is_some());
            let (pixels, remainder) = logo.rgba.as_chunks::<4>();
            assert!(remainder.is_empty());
            assert!(pixels.iter().all(|pixel| matches!(pixel[3], 0 | 255)));
            let colours = pixels
                .iter()
                .filter(|pixel| pixel[3] == 255)
                .map(|pixel| (pixel[0], pixel[1], pixel[2]))
                .collect::<std::collections::BTreeSet<_>>();
            assert!(
                (8..=32).contains(&colours.len()),
                "logo should retain a compact pixel-art palette"
            );
        }
    }

    #[test]
    fn startup_uses_the_large_logo_only_when_it_fits_beside_the_details() {
        let info = StartupInfo {
            version: "0.3.22",
            release_date: "2026-09-16",
            profile: "default",
            model: "model",
            session: "session",
            tools: &[],
            skills: &[],
            extensions: &[],
        };
        let large_min = side_by_side_min_width(&LARGE_LOGO);
        // The sparse details are shorter than either logo, so the rows
        // between the two borders are exactly the logo's rows.
        let logo_rows = |width| {
            render_startup(&info, width, false)
                .lines()
                .filter(|line| line.starts_with('│'))
                .count()
        };
        assert_eq!(logo_rows(large_min), LARGE_HEIGHT.div_ceil(2));
        assert_eq!(logo_rows(large_min - 1), COMPACT_HEIGHT.div_ceil(2));
        for width in [large_min - 1, large_min, 160] {
            for line in render_startup(&info, width, false)
                .lines()
                .filter(|line| !line.is_empty())
            {
                assert_eq!(line.chars().count(), width, "wrong width: {line:?}");
            }
        }
    }

    #[test]
    fn startup_places_live_metadata_beside_the_logo() {
        let tools = vec!["fs.read".to_owned(), "web.search".to_owned()];
        let skills = vec!["research".to_owned(), "notes".to_owned()];
        let extensions = vec!["my-tool".to_owned()];
        let dashboard = render_startup(
            &StartupInfo {
                version: "0.3.5",
                release_date: "2026-09-09",
                profile: "default",
                model: "minicpm5-1b@16k",
                session: "session-1",
                tools: &tools,
                skills: &skills,
                extensions: &extensions,
            },
            132,
            false,
        );
        assert!(dashboard.contains("Lightagent v0.3.5 (2026-09-09)"));
        assert!(dashboard.contains("Available Tools"));
        assert!(dashboard.contains("fs: read"));
        assert!(dashboard.contains("Available Skills"));
        assert!(dashboard.contains("research, notes"));
        assert!(dashboard.contains("Active Extensions"));
        assert!(dashboard.contains("my-tool"));
        assert!(dashboard.contains("Profile: default"));
        assert!(dashboard.contains("Model: minicpm5-1b@16k"));
        assert!(dashboard.contains("Session: session-1"));
        assert!(dashboard.lines().all(|line| line.chars().count() <= 132));
        let tools_row = dashboard
            .lines()
            .find(|line| line.contains("Available Tools"))
            .unwrap();
        assert_eq!(tools_row.matches('│').count(), 2, "no center divider");
    }

    #[test]
    fn startup_border_reaches_the_requested_terminal_edge() {
        let tools = vec!["rag.realtime".to_owned()];
        let dashboard = render_startup(
            &StartupInfo {
                version: "0.3.9",
                release_date: "2026-09-10",
                profile: "default",
                model: "model",
                session: "session",
                tools: &tools,
                skills: &[],
                extensions: &[],
            },
            220,
            false,
        );
        for line in dashboard.lines().filter(|line| !line.is_empty()) {
            assert_eq!(line.chars().count(), 220, "wrong width: {line:?}");
        }
    }

    #[test]
    fn narrow_startup_stacks_without_overflowing() {
        let dashboard = render_startup(
            &StartupInfo {
                version: "0.3.9",
                release_date: "2026-09-10",
                profile: "default",
                model: "model",
                session: "session",
                tools: &[],
                skills: &[],
                extensions: &[],
            },
            80,
            false,
        );
        assert!(dashboard.contains("Available Tools"));
        for line in dashboard.lines().filter(|line| !line.is_empty()) {
            assert_eq!(line.chars().count(), 80, "wrong width: {line:?}");
        }
    }

    #[test]
    fn startup_keeps_late_tools_visible_when_a_group_is_long() {
        let tools = (0..30)
            .map(|i| format!("mcp.server.tool{i:02}"))
            .collect::<Vec<_>>();
        let dashboard = render_startup(
            &StartupInfo {
                version: "test",
                release_date: "today",
                profile: "default",
                model: "model",
                session: "session",
                tools: &tools,
                skills: &[],
                extensions: &[],
            },
            80,
            false,
        );
        assert!(dashboard.contains("tool29"));
        assert!(
            dashboard
                .lines()
                .filter(|line| !line.is_empty())
                .all(|line| line.chars().count() == 80)
        );
    }
}
