use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::Duration;
use crossterm::{
    cursor,
    event::{self, Event, KeyCode, KeyModifiers},
    execute, queue,
    style::{self, Color, Stylize},
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use notify_debouncer_mini::notify::{RecommendedWatcher, RecursiveMode};
use notify_debouncer_mini::{new_debouncer, DebounceEventResult, Debouncer};
use crate::renderer::ImageProtocol;

/// How long the pager blocks waiting for a key before looping back to poll the
/// file watcher. Short enough to feel instant on file changes, long enough to
/// keep the loop near-idle when nothing is happening.
const POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Run the interactive terminal pager on the rendered markdown content.
pub fn run_pager(
    content: &str,
    filename: &str,
    image_protocol: ImageProtocol,
    reload_callback: Option<Box<dyn FnMut() -> Result<String, String>>>,
    watch_path: Option<PathBuf>,
    watch_enabled: bool,
) -> io::Result<()> {
    let mut stdout = io::stdout();

    // Enable raw mode and enter alternate screen
    terminal::enable_raw_mode()?;
    execute!(stdout, EnterAlternateScreen, cursor::Hide)?;

    let res = pager_loop(
        &mut stdout,
        content,
        filename,
        image_protocol,
        reload_callback,
        watch_path,
        watch_enabled,
    );

    // Clean up any remaining Kitty images before leaving
    if image_protocol == ImageProtocol::Kitty {
        let _ = execute!(stdout, style::Print("\x1b_Ga=d\x1b\\"));
    }

    // Restore terminal state
    let _ = execute!(stdout, cursor::Show, LeaveAlternateScreen);
    let _ = terminal::disable_raw_mode();

    res
}

struct SearchState {
    query: String,
    matches: Vec<usize>,
    current_match_idx: usize,
}

/// Editing/selection mode, modeled loosely on Vim's normal and visual modes.
#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Normal,
    VisualChar,
    VisualLine,
}

fn strip_ansi_and_images(s: &str) -> String {
    let mut result = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            result.push(c);
            continue;
        }
        match chars.peek() {
            // ANSI escape code: ESC [ ... <alphabetic>
            Some('[') => {
                chars.next();
                for nc in chars.by_ref() {
                    if nc.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            // Kitty escape code: ESC _ G ... ESC \
            Some('_') => {
                chars.next();
                if chars.peek() == Some(&'G') {
                    chars.next();
                    while let Some(cc) = chars.next() {
                        if cc == '\x1b' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
            }
            // OSC/iTerm2 escape code: ESC ] 1337 ; ... BEL
            Some(']') => {
                chars.next();
                for cc in chars.by_ref() {
                    if cc == '\x07' {
                        break;
                    }
                }
            }
            _ => {}
        }
    }
    result
}

/// True if the line carries an inline image escape (Kitty graphics or iTerm2).
/// Such lines are painted by the terminal across multiple rows, so we treat
/// them — plus the blank rows reserved beneath them — as one atomic block.
fn is_image_line(s: &str) -> bool {
    s.contains("\x1b_G") || s.contains("\x1b]1337;")
}

/// Mark which lines the text cursor is allowed to land on. Image blocks (the
/// escape line and the blank rows reserved below it) are not cursorable, so the
/// cursor and viewport jump over them as a unit instead of getting stuck in the
/// blank reserved space.
fn compute_cursorable(raw: &[String]) -> Vec<bool> {
    let mut cursorable = vec![true; raw.len()];
    let mut i = 0;
    while i < raw.len() {
        if is_image_line(&raw[i]) {
            cursorable[i] = false;
            let mut j = i + 1;
            while j < raw.len() && raw[j].is_empty() {
                cursorable[j] = false;
                j += 1;
            }
            i = j;
        } else {
            i += 1;
        }
    }
    cursorable
}

fn first_cursorable(cursorable: &[bool]) -> Option<usize> {
    cursorable.iter().position(|&b| b)
}

fn last_cursorable(cursorable: &[bool]) -> Option<usize> {
    cursorable.iter().rposition(|&b| b)
}

fn next_cursorable(cursorable: &[bool], from: usize) -> Option<usize> {
    (from + 1..cursorable.len()).find(|&i| cursorable[i])
}

fn prev_cursorable(cursorable: &[bool], from: usize) -> Option<usize> {
    (0..from).rev().find(|&i| cursorable[i])
}

/// Step `n` cursorable lines downward, stopping at the last one.
fn step_down(cursorable: &[bool], from: usize, n: usize) -> usize {
    let mut cur = from;
    for _ in 0..n {
        match next_cursorable(cursorable, cur) {
            Some(nx) => cur = nx,
            None => break,
        }
    }
    cur
}

/// Step `n` cursorable lines upward, stopping at the first one.
fn step_up(cursorable: &[bool], from: usize, n: usize) -> usize {
    let mut cur = from;
    for _ in 0..n {
        match prev_cursorable(cursorable, cur) {
            Some(pv) => cur = pv,
            None => break,
        }
    }
    cur
}

fn line_char_count(s: &str) -> usize {
    s.chars().count()
}

/// Highest valid cursor column on a line (0 for an empty line).
fn max_col(s: &str) -> usize {
    line_char_count(s).saturating_sub(1)
}

/// Next word-start column after `col`, or `None` if there is none on this line.
fn word_forward(line: &str, col: usize) -> Option<usize> {
    let chars: Vec<char> = line.chars().collect();
    let n = chars.len();
    let mut i = col;
    while i < n && !chars[i].is_whitespace() {
        i += 1;
    }
    while i < n && chars[i].is_whitespace() {
        i += 1;
    }
    (i < n).then_some(i)
}

/// Previous word-start column before `col`, or `None` at the start of the line.
fn word_back(line: &str, col: usize) -> Option<usize> {
    if col == 0 {
        return None;
    }
    let chars: Vec<char> = line.chars().collect();
    let mut i = col - 1;
    while i > 0 && chars[i].is_whitespace() {
        i -= 1;
    }
    while i > 0 && !chars[i - 1].is_whitespace() {
        i -= 1;
    }
    Some(i)
}

/// Snap to the nearest cursorable line at or after `idx`, falling back to before.
fn snap_cursorable(cursorable: &[bool], idx: usize) -> usize {
    if cursorable.get(idx).copied().unwrap_or(false) {
        idx
    } else {
        next_cursorable(cursorable, idx)
            .or_else(|| prev_cursorable(cursorable, idx))
            .unwrap_or(idx)
    }
}

/// Next paragraph boundary below `from`: the blank line after the current
/// block. Image/code blocks count as content, so the motion stops at the gaps
/// around them rather than inside. Lands on a cursorable line.
fn next_paragraph(clean: &[String], cursorable: &[bool], from: usize) -> usize {
    let n = clean.len();
    let is_blank = |i: usize| cursorable[i] && clean[i].trim().is_empty();
    let mut i = from + 1;
    // Only skip a blank run when starting on one, so a motion from inside a
    // paragraph still stops at the boundary immediately below it.
    if is_blank(from) {
        while i < n && is_blank(i) {
            i += 1;
        }
    }
    while i < n && !is_blank(i) {
        i += 1;
    }
    snap_cursorable(cursorable, i.min(n.saturating_sub(1)))
}

/// Previous paragraph boundary above `from` (mirror of [`next_paragraph`]).
fn prev_paragraph(clean: &[String], cursorable: &[bool], from: usize) -> usize {
    if from == 0 {
        return 0;
    }
    let is_blank = |i: usize| cursorable[i] && clean[i].trim().is_empty();
    let mut i = from - 1;
    if is_blank(from) {
        while i > 0 && is_blank(i) {
            i -= 1;
        }
    }
    while i > 0 && !is_blank(i) {
        i -= 1;
    }
    snap_cursorable(cursorable, i)
}

/// Lines derived from the rendered content: the raw (styled) lines that get
/// drawn, the ANSI/image-stripped lines used for searching, cursor positioning
/// and copying, and a per-line flag for whether the cursor may rest there.
struct ContentLines {
    raw: Vec<String>,
    clean: Vec<String>,
    cursorable: Vec<bool>,
}

impl ContentLines {
    fn from_content(content: &str) -> Self {
        let raw: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
        let clean = raw.iter().map(|line| strip_ansi_and_images(line)).collect();
        let cursorable = compute_cursorable(&raw);
        Self { raw, clean, cursorable }
    }
}

/// Order two (row, col) positions into (start_row, start_col, end_row, end_col).
fn ordered(a: (usize, usize), b: (usize, usize)) -> (usize, usize, usize, usize) {
    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
    (lo.0, lo.1, hi.0, hi.1)
}

/// Visible-column range `[start, end)` to reverse-video on line `idx`, given the
/// cursor and selection state, or `None` if nothing on this line is highlighted.
fn highlight_for_line(
    idx: usize,
    cursor: (usize, usize),
    mode: Mode,
    anchor: (usize, usize),
    clean_lines: &[String],
) -> Option<(usize, usize)> {
    match mode {
        Mode::Normal => (idx == cursor.0).then(|| (cursor.1, cursor.1 + 1)),
        Mode::VisualChar => {
            let (sr, sc, er, ec) = ordered(anchor, cursor);
            if idx < sr || idx > er {
                return None;
            }
            let len = line_char_count(&clean_lines[idx]);
            let start = if idx == sr { sc } else { 0 };
            // The cell under the later endpoint is included (inclusive selection).
            let end = if idx == er { ec + 1 } else { len.max(1) };
            Some((start, end.max(start + 1)))
        }
        Mode::VisualLine => {
            let sr = anchor.0.min(cursor.0);
            let er = anchor.0.max(cursor.0);
            if idx < sr || idx > er {
                return None;
            }
            Some((0, line_char_count(&clean_lines[idx]).max(1)))
        }
    }
}

/// Render a styled line with the visible columns in `hl` reverse-videoed,
/// preserving the line's own ANSI styling outside (and color inside) the range.
/// Escape sequences are copied verbatim; a reset inside the range re-asserts the
/// reverse attribute so the highlight survives the line's per-chunk resets.
fn render_line(raw: &str, clean_len: usize, hl: Option<(usize, usize)>) -> String {
    let (start, end) = match hl {
        Some((s, e)) if e > s => (s, e),
        _ => return raw.to_string(),
    };

    let mut out = String::new();
    let mut col = 0;
    let mut in_hl = false;
    let mut chars = raw.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '\x1b' {
            out.push(c);
            match chars.peek() {
                // ESC [ ... <alpha>  (SGR / CSI)
                Some('[') => {
                    out.push(chars.next().unwrap());
                    let mut seq = String::new();
                    for nc in chars.by_ref() {
                        out.push(nc);
                        seq.push(nc);
                        if nc.is_ascii_alphabetic() {
                            break;
                        }
                    }
                    // A reset would clear our reverse attribute; re-assert it.
                    if in_hl && (seq == "0m" || seq == "m") {
                        out.push_str("\x1b[7m");
                    }
                }
                // ESC _ ... ESC \  (Kitty)
                Some('_') => {
                    out.push(chars.next().unwrap());
                    while let Some(cc) = chars.next() {
                        out.push(cc);
                        if cc == '\x1b' && chars.peek() == Some(&'\\') {
                            out.push(chars.next().unwrap());
                            break;
                        }
                    }
                }
                // ESC ] ... BEL  (OSC)
                Some(']') => {
                    out.push(chars.next().unwrap());
                    for cc in chars.by_ref() {
                        out.push(cc);
                        if cc == '\x07' {
                            break;
                        }
                    }
                }
                _ => {}
            }
            continue;
        }

        if col == start {
            out.push_str("\x1b[7m");
            in_hl = true;
        }
        if col == end && in_hl {
            out.push_str("\x1b[27m");
            in_hl = false;
        }
        out.push(c);
        col += 1;
    }

    if in_hl {
        out.push_str("\x1b[27m");
    }

    // Pad with reversed spaces when the highlight extends past the line's text
    // (cursor on an empty/short line, or a full-line selection of a short line).
    let pad = end.saturating_sub(clean_len.max(start));
    if pad > 0 {
        out.push_str("\x1b[7m");
        out.push_str(&" ".repeat(pad));
        out.push_str("\x1b[27m");
    }

    out
}

/// Clean up a fully-selected line for copying: drop single-column box borders
/// (code/math/mermaid boxes), unwrap single-column box *content* down to the
/// inner text, and trim trailing whitespace. Table borders and rows (which use
/// `┬`/`┴` or have several `│`) and blockquote lines are left untouched.
/// Returns `None` for lines that should be omitted entirely (box borders).
fn tidy_full_line(line: &str) -> Option<String> {
    let t = line.trim_start();
    // Box top/bottom borders with no column separators belong to a single-column
    // box (a code block or math/mermaid fallback), so they carry no content.
    if (t.starts_with('┌') && !t.contains('┬')) || (t.starts_with('└') && !t.contains('┴')) {
        return None;
    }
    // Single-column box content: "│ code      │" -> "code".
    if line.matches('│').count() == 2
        && let Some(first) = line.find('│')
        && let Some(last) = line.rfind('│')
        && first < last
    {
        let inner = &line[first + '│'.len_utf8()..last];
        // The renderer always inserts exactly one leading space after the border;
        // strip just that one so the code's own indentation is preserved.
        let inner = inner.strip_prefix(' ').unwrap_or(inner);
        return Some(inner.trim_end().to_string());
    }
    Some(line.trim_end().to_string())
}

/// Extract the selected text (ANSI-stripped) for copying. Image-block rows are
/// skipped so the clipboard never gets raw escape sequences or reserved blanks,
/// code-block borders are stripped, and trailing whitespace is trimmed.
fn extract_selection(
    clean_lines: &[String],
    cursorable: &[bool],
    mode: Mode,
    anchor: (usize, usize),
    cursor: (usize, usize),
) -> String {
    let result = match mode {
        Mode::VisualChar => {
            let (sr, sc, er, ec) = ordered(anchor, cursor);
            if sr == er {
                // A partial single-line selection is taken verbatim (the user is
                // selecting exact characters, so don't strip box decorations).
                clean_lines[sr]
                    .chars()
                    .skip(sc)
                    .take((ec + 1).saturating_sub(sc))
                    .collect::<String>()
            } else {
                // The first and last lines are partial; only the fully-covered
                // middle lines get box/border cleanup.
                let mut parts = vec![clean_lines[sr].chars().skip(sc).collect::<String>()];
                for r in sr + 1..er {
                    if cursorable[r]
                        && let Some(tidied) = tidy_full_line(&clean_lines[r])
                    {
                        parts.push(tidied);
                    }
                }
                parts.push(clean_lines[er].chars().take(ec + 1).collect::<String>());
                parts.join("\n")
            }
        }
        Mode::VisualLine => {
            let sr = anchor.0.min(cursor.0);
            let er = anchor.0.max(cursor.0);
            (sr..=er)
                .filter(|&r| cursorable[r])
                .filter_map(|r| tidy_full_line(&clean_lines[r]))
                .collect::<Vec<_>>()
                .join("\n")
        }
        Mode::Normal => String::new(),
    };
    result.trim_end().to_string()
}

/// Copy `text` to the system clipboard via the OSC 52 terminal escape. Works
/// without any external clipboard tool and forwards through SSH sessions.
fn osc52_copy(out: &mut impl Write, text: &str) -> io::Result<()> {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(text.as_bytes());
    write!(out, "\x1b]52;c;{}\x07", b64)?;
    out.flush()
}

/// Start watching the directory containing `path` for changes. We watch the
/// parent directory rather than the file itself so that editor "atomic saves"
/// (write-temp-then-rename, which swaps the inode) are still detected. Events
/// are filtered by filename in the loop. Returns `None` if watching can't start.
fn start_watcher(
    path: &Path,
    tx: Sender<DebounceEventResult>,
) -> Option<Debouncer<RecommendedWatcher>> {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut debouncer = new_debouncer(POLL_INTERVAL, tx).ok()?;
    debouncer
        .watcher()
        .watch(dir, RecursiveMode::NonRecursive)
        .ok()?;
    Some(debouncer)
}

/// Last modification time of `path`, if it can be read.
fn file_mtime(path: &Path) -> Option<std::time::SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Drain any pending debounced filesystem events and report whether one of them
/// touched the watched file (matched by filename).
fn watched_file_changed(rx: &Receiver<DebounceEventResult>, target_name: Option<&std::ffi::OsStr>) -> bool {
    let mut changed = false;
    while let Ok(result) = rx.try_recv() {
        if let Ok(events) = result
            && let Some(name) = target_name
            && events.iter().any(|e| e.path.file_name() == Some(name))
        {
            changed = true;
        }
    }
    changed
}

fn pager_loop(
    stdout: &mut io::Stdout,
    content: &str,
    filename: &str,
    image_protocol: ImageProtocol,
    mut reload_callback: Option<Box<dyn FnMut() -> Result<String, String>>>,
    watch_path: Option<PathBuf>,
    watch_enabled: bool,
) -> io::Result<()> {
    let initial = ContentLines::from_content(content);
    let mut raw_lines = initial.raw;
    let mut clean_lines = initial.clean;
    let mut cursorable = initial.cursorable;

    let mut scroll_row = 0;
    let mut cursor_row = first_cursorable(&cursorable).unwrap_or(0);
    let mut cursor_col = 0;
    let mut mode = Mode::Normal;
    let mut anchor = (0, 0);
    let mut search: Option<SearchState> = None;
    let mut input_buffer: Option<String> = None;
    let mut reload_status: Option<Result<String, String>> = None;

    // File watching. We can only watch when we have both a path and a way to
    // re-render it (the reload callback). The watcher can be toggled at runtime.
    let watchable = watch_path.is_some() && reload_callback.is_some();
    let target_name = watch_path
        .as_ref()
        .and_then(|p| p.file_name())
        .map(|n| n.to_owned());
    let (watch_tx, watch_rx) = channel::<DebounceEventResult>();
    // The debouncer is held only to keep its background watch thread alive;
    // dropping it stops watching.
    let mut watcher: Option<Debouncer<RecommendedWatcher>> = if watch_enabled && watchable {
        watch_path
            .as_ref()
            .and_then(|path| start_watcher(path, watch_tx.clone()))
    } else {
        None
    };
    let mut watching = watcher.is_some();
    // Track the file's modification time so we can ignore the filesystem events
    // caused by our *own* reads when reloading (reads bump atime, not mtime).
    let mut last_mtime = if watching {
        watch_path.as_deref().and_then(file_mtime)
    } else {
        None
    };

    // Only redraw when something actually changed, so the poll loop stays quiet
    // (and Kitty images don't flicker) while idle.
    let mut dirty = true;

    loop {
        let (width_val, height_val) = terminal::size()?;
        let width = width_val as usize;
        let height = height_val as usize;

        // Leave 1 line for top title bar and 1 line for bottom status bar
        let viewport_height = height.saturating_sub(2).max(1);

        // React to filesystem changes before drawing. We only reload when the
        // file's mtime actually advanced; this filters out the events generated
        // by our own reads, which would otherwise cause an endless reload loop.
        if watching && watched_file_changed(&watch_rx, target_name.as_deref()) {
            let current_mtime = watch_path.as_deref().and_then(file_mtime);
            if current_mtime != last_mtime
                && let Some(reload) = reload_callback.as_mut()
            {
                last_mtime = current_mtime;
                match reload() {
                    Ok(new_content) => {
                        let nc = ContentLines::from_content(&new_content);
                        raw_lines = nc.raw;
                        clean_lines = nc.clean;
                        cursorable = nc.cursorable;
                        clamp_cursor(&clean_lines, &cursorable, &mut cursor_row, &mut cursor_col);
                        mode = Mode::Normal;
                        if scroll_row + viewport_height > raw_lines.len() {
                            scroll_row = raw_lines.len().saturating_sub(viewport_height);
                        }
                        search = None;
                        reload_status = Some(Ok("Auto-reloaded".to_string()));
                    }
                    Err(err_msg) => {
                        reload_status = Some(Err(err_msg));
                    }
                }
                dirty = true;
            }
        }

        // Only repaint when something changed, so the poll loop stays quiet and
        // Kitty images don't flicker while idle.
        if dirty {
            dirty = false;

            // Clear the entire screen once at the beginning of the draw loop to
            // prevent stale/stuck images and text overlapping when redrawing or scrolling.
            queue!(stdout, terminal::Clear(terminal::ClearType::All))?;

            // Draw the top header bar
            let header_left = format!(" md ── {} ", filename);
            let header_right = if watchable {
                if watching {
                    " ['q' quit · 'v'/'V' select · 'y' copy · 'W' watch ● · '/'] "
                } else {
                    " ['q' quit · 'v'/'V' select · 'y' copy · 'W' watch ○ · '/'] "
                }
            } else if reload_callback.is_some() {
                " ['q' quit · 'v'/'V' select · 'y' copy · 'r' reload · '/'] "
            } else {
                " ['q' quit · 'v'/'V' select · 'y' copy · '/' search] "
            };
            let header_len = header_left.chars().count() + header_right.chars().count();
            let header_padding = width.saturating_sub(header_len);
            let header_text = format!("{}{}{}", header_left, " ".repeat(header_padding), header_right);

            queue!(
                stdout,
                cursor::MoveTo(0, 0),
                style::PrintStyledContent(
                    header_text
                        .bold()
                        .with(Color::Black)
                        .on(Color::Cyan)
                )
            )?;

            // Draw the viewport lines
            if image_protocol == ImageProtocol::Kitty {
                // Delete all visible Kitty graphics protocol images to prevent duplicating and getting stuck
                queue!(stdout, style::Print("\x1b_Ga=d\x1b\\"))?;
            }

            for i in 0..viewport_height {
                let file_row = scroll_row + i;
                queue!(stdout, cursor::MoveTo(0, (i + 1) as u16))?;

                if file_row >= raw_lines.len() {
                    continue;
                }

                let raw = &raw_lines[file_row];
                // Image-block lines are painted by the terminal and must never be
                // rewritten (highlighting would corrupt the escape sequence).
                if cursorable.get(file_row).copied().unwrap_or(false) {
                    let hl = highlight_for_line(
                        file_row,
                        (cursor_row, cursor_col),
                        mode,
                        anchor,
                        &clean_lines,
                    );
                    let rendered = render_line(raw, line_char_count(&clean_lines[file_row]), hl);
                    queue!(stdout, style::Print(rendered))?;
                } else {
                    queue!(stdout, style::Print(raw))?;
                }
            }

            // Draw the bottom status/input bar
            queue!(
                stdout,
                cursor::MoveTo(0, (height - 1) as u16),
                terminal::Clear(terminal::ClearType::CurrentLine)
            )?;

            if let Some(ref current_input) = input_buffer {
                // We are currently typing a search query
                let search_prompt = format!(" Search: /{}", current_input);
                let search_padding = width.saturating_sub(search_prompt.chars().count());
                let search_text = format!("{}{}", search_prompt, " ".repeat(search_padding));
                queue!(
                    stdout,
                    style::PrintStyledContent(
                        search_text
                            .bold()
                            .with(Color::Black)
                            .on(Color::Yellow)
                    )
                )?;
            } else if let Some(ref status) = reload_status {
                let (status_text, status_color) = match status {
                    Ok(msg) => (format!(" {} ", msg), Color::Green),
                    Err(err) => (format!(" Error: {} ", err), Color::Red),
                };
                let status_len = status_text.chars().count();
                let status_padding = width.saturating_sub(status_len);
                let full_text = format!("{}{}", status_text, " ".repeat(status_padding));
                queue!(
                    stdout,
                    style::PrintStyledContent(
                        full_text
                            .bold()
                            .with(Color::Black)
                            .on(status_color)
                    )
                )?;
            } else {
                // Standard status line
                let progress = if raw_lines.is_empty() {
                    100
                } else {
                    ((cursor_row + 1) * 100 / raw_lines.len()).min(100)
                };

                let left_text = if let Some(ref s) = search {
                    if s.matches.is_empty() {
                        format!(" No matches found for \"{}\" ", s.query)
                    } else {
                        format!(
                            " Match {} of {} for \"{}\" (press 'n'/'N' for next/prev) ",
                            s.current_match_idx + 1,
                            s.matches.len(),
                            s.query
                        )
                    }
                } else {
                    match mode {
                        Mode::Normal => " NORMAL ".to_string(),
                        Mode::VisualChar => " VISUAL ".to_string(),
                        Mode::VisualLine => " VISUAL LINE ".to_string(),
                    }
                };

                let right_text = format!(
                    " Ln {}:{} / {} ({}%) ",
                    cursor_row + 1,
                    cursor_col + 1,
                    raw_lines.len(),
                    progress
                );

                let status_len = left_text.chars().count() + right_text.chars().count();
                let status_padding = width.saturating_sub(status_len);
                let status_text = format!("{}{}{}", left_text, " ".repeat(status_padding), right_text);

                let status_color = if search.is_some() {
                    Color::Yellow
                } else if mode != Mode::Normal {
                    Color::Magenta
                } else {
                    Color::Green
                };

                queue!(
                    stdout,
                    style::PrintStyledContent(
                        status_text
                            .bold()
                            .with(Color::Black)
                            .on(status_color)
                    )
                )?;
            }

            stdout.flush()?;
        } // end `if dirty` repaint

        // Wait for input, but time out periodically so the file-watcher channel
        // keeps getting polled at the top of the loop.
        if !event::poll(POLL_INTERVAL)? {
            continue;
        }
        let ev = event::read()?;
        if let Event::Resize(..) = ev {
            dirty = true;
            continue;
        }
        let Event::Key(key) = ev else {
            continue;
        };

        // Any keypress warrants a repaint (status line, cursor, search, …).
        dirty = true;
        let _ = reload_status.take();

        // If typing a search query, the line editor consumes the key.
        if let Some(mut current_input) = input_buffer.take() {
            match key.code {
                KeyCode::Enter => {
                    let query = current_input.trim().to_lowercase();
                    if !query.is_empty() {
                        let matches: Vec<usize> = clean_lines
                            .iter()
                            .enumerate()
                            .filter(|(_, line)| line.to_lowercase().contains(&query))
                            .map(|(idx, _)| idx)
                            .collect();

                        let search_state = SearchState {
                            query: current_input,
                            matches,
                            current_match_idx: 0,
                        };

                        if !search_state.matches.is_empty() {
                            move_cursor_to_match(
                                &clean_lines,
                                search_state.matches[0],
                                &query,
                                &mut cursor_row,
                                &mut cursor_col,
                            );
                        }
                        search = Some(search_state);
                    } else {
                        search = None;
                    }
                    input_buffer = None;
                }
                KeyCode::Esc => {
                    input_buffer = None;
                }
                KeyCode::Backspace => {
                    current_input.pop();
                    input_buffer = Some(current_input);
                }
                KeyCode::Char(c) => {
                    current_input.push(c);
                    input_buffer = Some(current_input);
                }
                _ => {
                    input_buffer = Some(current_input);
                }
            }
            keep_cursor_visible(cursor_row, viewport_height, &mut scroll_row);
            continue;
        }

        let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Esc => {
                if mode != Mode::Normal {
                    mode = Mode::Normal;
                } else {
                    break;
                }
            }

            // --- Reload / watch ---
            KeyCode::Char('r') => {
                if let Some(ref mut reload) = reload_callback {
                    match reload() {
                        Ok(new_content) => {
                            let nc = ContentLines::from_content(&new_content);
                            raw_lines = nc.raw;
                            clean_lines = nc.clean;
                            cursorable = nc.cursorable;
                            clamp_cursor(&clean_lines, &cursorable, &mut cursor_row, &mut cursor_col);
                            mode = Mode::Normal;
                            if scroll_row + viewport_height > raw_lines.len() {
                                scroll_row = raw_lines.len().saturating_sub(viewport_height);
                            }
                            search = None;
                            reload_status = Some(Ok("Reloaded successfully".to_string()));
                        }
                        Err(err_msg) => {
                            reload_status = Some(Err(err_msg));
                        }
                    }
                }
            }
            KeyCode::Char('W') if watchable => {
                if watching {
                    // Dropping the debouncer stops the background watch thread.
                    let _ = watcher.take();
                    watching = false;
                    reload_status = Some(Ok("Watch disabled".to_string()));
                } else if let Some(ref path) = watch_path {
                    watcher = start_watcher(path, watch_tx.clone());
                    watching = watcher.is_some();
                    // Seed the baseline mtime so the first detected change is
                    // a real edit, not a stale event from before we started.
                    last_mtime = if watching { file_mtime(path) } else { None };
                    reload_status = Some(if watching {
                        Ok("Watch enabled".to_string())
                    } else {
                        Err("Failed to start file watcher".to_string())
                    });
                }
            }

            // --- Half/full page movement (Ctrl combos take priority) ---
            KeyCode::Char('f') if is_ctrl => {
                cursor_row = step_down(&cursorable, cursor_row, viewport_height);
            }
            KeyCode::Char('b') if is_ctrl => {
                cursor_row = step_up(&cursorable, cursor_row, viewport_height);
            }
            KeyCode::Char('d') if is_ctrl => {
                cursor_row = step_down(&cursorable, cursor_row, (viewport_height / 2).max(1));
            }
            KeyCode::Char('u') if is_ctrl => {
                cursor_row = step_up(&cursorable, cursor_row, (viewport_height / 2).max(1));
            }
            KeyCode::PageDown | KeyCode::Char(' ') => {
                cursor_row = step_down(&cursorable, cursor_row, viewport_height);
            }
            KeyCode::PageUp => {
                cursor_row = step_up(&cursorable, cursor_row, viewport_height);
            }

            // --- Cursor movement ---
            KeyCode::Up | KeyCode::Char('k') => {
                cursor_row = step_up(&cursorable, cursor_row, 1);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                cursor_row = step_down(&cursorable, cursor_row, 1);
            }
            KeyCode::Left | KeyCode::Char('h') => {
                cursor_col = cursor_col.saturating_sub(1);
            }
            KeyCode::Right | KeyCode::Char('l') => {
                cursor_col = (cursor_col + 1).min(max_col(&clean_lines[cursor_row]));
            }
            KeyCode::Char('0') | KeyCode::Home => {
                cursor_col = 0;
            }
            KeyCode::Char('$') | KeyCode::End => {
                cursor_col = max_col(&clean_lines[cursor_row]);
            }
            KeyCode::Char('w') => {
                if let Some(nc) = word_forward(&clean_lines[cursor_row], cursor_col) {
                    cursor_col = nc;
                } else if let Some(nr) = next_cursorable(&cursorable, cursor_row) {
                    cursor_row = nr;
                    cursor_col = 0;
                }
            }
            KeyCode::Char('b') => {
                if let Some(pc) = word_back(&clean_lines[cursor_row], cursor_col) {
                    cursor_col = pc;
                } else if let Some(pr) = prev_cursorable(&cursorable, cursor_row) {
                    cursor_row = pr;
                    cursor_col = max_col(&clean_lines[pr]);
                }
            }
            KeyCode::Char('g') => {
                cursor_row = first_cursorable(&cursorable).unwrap_or(0);
                cursor_col = 0;
            }
            KeyCode::Char('G') => {
                cursor_row = last_cursorable(&cursorable).unwrap_or(0);
                cursor_col = 0;
            }
            KeyCode::Char('}') => {
                cursor_row = next_paragraph(&clean_lines, &cursorable, cursor_row);
                cursor_col = 0;
            }
            KeyCode::Char('{') => {
                cursor_row = prev_paragraph(&clean_lines, &cursorable, cursor_row);
                cursor_col = 0;
            }

            // --- Visual selection ---
            KeyCode::Char('v') => {
                mode = if mode == Mode::VisualChar {
                    Mode::Normal
                } else {
                    anchor = (cursor_row, cursor_col);
                    Mode::VisualChar
                };
            }
            KeyCode::Char('V') => {
                mode = if mode == Mode::VisualLine {
                    Mode::Normal
                } else {
                    anchor = (cursor_row, cursor_col);
                    Mode::VisualLine
                };
            }
            KeyCode::Char('y') => {
                if mode != Mode::Normal {
                    let text = extract_selection(
                        &clean_lines,
                        &cursorable,
                        mode,
                        anchor,
                        (cursor_row, cursor_col),
                    );
                    let count = text.chars().count();
                    reload_status = Some(match osc52_copy(stdout, &text) {
                        Ok(()) => Ok(format!("Copied {} chars to clipboard", count)),
                        Err(e) => Err(format!("Copy failed: {}", e)),
                    });
                    mode = Mode::Normal;
                }
            }

            // --- Search ---
            KeyCode::Char('/') => {
                input_buffer = Some(String::new());
            }
            KeyCode::Char('n') => {
                if let Some(ref mut s) = search
                    && !s.matches.is_empty()
                {
                    s.current_match_idx = (s.current_match_idx + 1) % s.matches.len();
                    let m = s.matches[s.current_match_idx];
                    let q = s.query.to_lowercase();
                    move_cursor_to_match(&clean_lines, m, &q, &mut cursor_row, &mut cursor_col);
                }
            }
            KeyCode::Char('N') => {
                if let Some(ref mut s) = search
                    && !s.matches.is_empty()
                {
                    s.current_match_idx = if s.current_match_idx == 0 {
                        s.matches.len() - 1
                    } else {
                        s.current_match_idx - 1
                    };
                    let m = s.matches[s.current_match_idx];
                    let q = s.query.to_lowercase();
                    move_cursor_to_match(&clean_lines, m, &q, &mut cursor_row, &mut cursor_col);
                }
            }
            _ => {}
        }

        // Keep the cursor on a valid column for its (possibly new) line, then
        // scroll the viewport so the cursor stays visible.
        cursor_col = cursor_col.min(max_col(&clean_lines[cursor_row]));
        keep_cursor_visible(cursor_row, viewport_height, &mut scroll_row);
    }

    Ok(())
}

/// Clamp the cursor onto a valid cursorable line and column after content
/// changes (e.g. a reload that produced fewer/different lines).
fn clamp_cursor(
    clean_lines: &[String],
    cursorable: &[bool],
    cursor_row: &mut usize,
    cursor_col: &mut usize,
) {
    *cursor_row = (*cursor_row).min(clean_lines.len().saturating_sub(1));
    if !cursorable.get(*cursor_row).copied().unwrap_or(false) {
        *cursor_row = next_cursorable(cursorable, *cursor_row)
            .or_else(|| prev_cursorable(cursorable, *cursor_row))
            .or_else(|| first_cursorable(cursorable))
            .unwrap_or(0);
    }
    *cursor_col = (*cursor_col).min(max_col(&clean_lines[*cursor_row]));
}

/// Scroll the viewport so `cursor_row` stays within it.
fn keep_cursor_visible(cursor_row: usize, viewport_height: usize, scroll_row: &mut usize) {
    if cursor_row < *scroll_row {
        *scroll_row = cursor_row;
    } else if cursor_row >= *scroll_row + viewport_height {
        *scroll_row = cursor_row + 1 - viewport_height;
    }
}

/// Move the cursor onto a search match, placing the column on the matched text.
fn move_cursor_to_match(
    clean_lines: &[String],
    row: usize,
    lowercased_query: &str,
    cursor_row: &mut usize,
    cursor_col: &mut usize,
) {
    *cursor_row = row;
    *cursor_col = clean_lines[row]
        .to_lowercase()
        .find(lowercased_query)
        .map(|byte_idx| clean_lines[row].to_lowercase()[..byte_idx].chars().count())
        .unwrap_or(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_removes_ansi_kitty_and_osc() {
        let input = "\x1b[1;31mred\x1b[0m \x1b_Ga=T,i=1;PAYLOAD\x1b\\ mid \x1b]1337;File=:DATA\x07 end";
        assert_eq!(strip_ansi_and_images(input), "red  mid  end");
    }

    #[test]
    fn image_block_is_not_cursorable() {
        let raw = vec![
            "text above".to_string(),
            "  \x1b_Ga=T,i=1;DATA\x1b\\".to_string(), // image anchor
            "".to_string(),                              // reserved blank
            "".to_string(),                              // reserved blank
            "text below".to_string(),
        ];
        let c = compute_cursorable(&raw);
        assert_eq!(c, vec![true, false, false, false, true]);
        // The cursor jumps the whole block in a single step.
        assert_eq!(next_cursorable(&c, 0), Some(4));
        assert_eq!(prev_cursorable(&c, 4), Some(0));
    }

    #[test]
    fn render_line_reverses_requested_columns() {
        // Highlight the single cell at column 1 of "abc".
        let out = render_line("abc", 3, Some((1, 2)));
        assert_eq!(out, "a\x1b[7mb\x1b[27mc");
    }

    #[test]
    fn render_line_pads_cursor_past_end_of_line() {
        // Cursor on an empty line should still show a reversed cell.
        let out = render_line("", 0, Some((0, 1)));
        assert_eq!(out, "\x1b[7m \x1b[27m");
    }

    #[test]
    fn render_line_reasserts_reverse_after_reset() {
        // The line's own reset must not cancel the selection highlight.
        let out = render_line("a\x1b[0mb", 2, Some((0, 2)));
        assert!(out.starts_with("\x1b[7m"));
        assert!(out.contains("\x1b[0m\x1b[7m"), "reverse not re-asserted after reset: {out:?}");
    }

    #[test]
    fn extract_charwise_single_line() {
        let clean = vec!["hello world".to_string()];
        let cursorable = vec![true];
        // Select "world" (cols 6..=10).
        let text = extract_selection(&clean, &cursorable, Mode::VisualChar, (0, 6), (0, 10));
        assert_eq!(text, "world");
    }

    #[test]
    fn extract_linewise_skips_image_rows() {
        let clean = vec!["first".to_string(), "  ".to_string(), "third".to_string()];
        let cursorable = vec![true, false, true]; // middle row is an image block
        let text = extract_selection(&clean, &cursorable, Mode::VisualLine, (0, 0), (2, 0));
        assert_eq!(text, "first\nthird");
    }

    #[test]
    fn tidy_strips_code_box_decorations() {
        // Border lines are dropped, content lines unwrapped, indentation kept.
        assert_eq!(tidy_full_line("┌── rust ──────────┐"), None);
        assert_eq!(tidy_full_line("└──────────────────┘"), None);
        assert_eq!(tidy_full_line("│ fn main() {       │"), Some("fn main() {".to_string()));
        assert_eq!(
            tidy_full_line("│     let x = 1;     │"),
            Some("    let x = 1;".to_string())
        );
    }

    #[test]
    fn tidy_leaves_tables_and_quotes_alone() {
        // Table borders carry column separators; rows have multiple bars.
        assert_eq!(tidy_full_line("┌────┬────┐"), Some("┌────┬────┐".to_string()));
        assert_eq!(
            tidy_full_line("│ a  │ b  │"),
            Some("│ a  │ b  │".to_string())
        );
        // Blockquote prefix is a single bar and is preserved.
        assert_eq!(tidy_full_line("│ quoted text"), Some("│ quoted text".to_string()));
    }

    #[test]
    fn extract_linewise_unwraps_code_block() {
        let clean = vec![
            "┌── rust ──────────┐".to_string(),
            "│ fn main() {       │".to_string(),
            "│     println!(\"\"); │".to_string(),
            "│ }                 │".to_string(),
            "└──────────────────┘".to_string(),
        ];
        let cursorable = vec![true; 5];
        let text = extract_selection(&clean, &cursorable, Mode::VisualLine, (0, 0), (4, 0));
        assert_eq!(text, "fn main() {\n    println!(\"\");\n}");
    }

    #[test]
    fn paragraph_motions_jump_between_blank_lines() {
        let clean = vec![
            "para one".to_string(),  // 0
            "more one".to_string(),  // 1
            "".to_string(),          // 2  <- boundary
            "para two".to_string(),  // 3
            "".to_string(),          // 4  <- boundary
            "para three".to_string(), // 5
        ];
        let cursorable = vec![true; 6];
        // From inside the first paragraph, `}` lands on the next blank line.
        assert_eq!(next_paragraph(&clean, &cursorable, 0), 2);
        // From a boundary, `}` skips the blank run and the next paragraph.
        assert_eq!(next_paragraph(&clean, &cursorable, 2), 4);
        // `{` mirrors upward.
        assert_eq!(prev_paragraph(&clean, &cursorable, 5), 4);
        assert_eq!(prev_paragraph(&clean, &cursorable, 3), 2);
        assert_eq!(prev_paragraph(&clean, &cursorable, 0), 0);
    }

    #[test]
    fn paragraph_motion_treats_image_block_as_content() {
        let clean = vec![
            "text".to_string(),         // 0
            "".to_string(),             // 1 boundary
            "  ".to_string(),           // 2 image anchor (whitespace, non-cursorable)
            "".to_string(),             // 3 reserved blank (non-cursorable)
            "after".to_string(),        // 4
        ];
        let cursorable = vec![true, true, false, false, true];
        // The image block is content, so jumping down from the boundary at 1
        // skips the block and lands on the next cursorable line (the end).
        assert_eq!(next_paragraph(&clean, &cursorable, 1), 4);
    }

    #[test]
    fn word_motions_walk_words() {
        let line = "the quick brown";
        assert_eq!(word_forward(line, 0), Some(4)); // -> "quick"
        assert_eq!(word_forward(line, 4), Some(10)); // -> "brown"
        assert_eq!(word_forward(line, 10), None); // last word
        assert_eq!(word_back(line, 10), Some(4)); // -> "quick"
        assert_eq!(word_back(line, 4), Some(0)); // -> "the"
        assert_eq!(word_back(line, 0), None);
    }

    #[test]
    fn watcher_detects_file_change() {
        // Create a unique temp directory and file.
        let dir = std::env::temp_dir().join(format!("md_watch_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("doc.md");
        std::fs::write(&file, "before").unwrap();

        let (tx, rx) = channel::<DebounceEventResult>();
        let _watcher = start_watcher(&file, tx).expect("watcher should start");
        let target = file.file_name().map(|n| n.to_owned());

        // Modify the file; the debouncer should report a change within a short window.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&file).unwrap();
            f.write_all(b" after").unwrap();
            f.flush().unwrap();
        }

        let mut detected = false;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if watched_file_changed(&rx, target.as_deref()) {
                detected = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }

        let _ = std::fs::remove_dir_all(&dir);
        assert!(detected, "file change was not detected by the watcher");
    }

    #[test]
    fn watcher_ignores_other_files() {
        let target_name = std::ffi::OsStr::new("doc.md");
        let (tx, rx) = channel::<DebounceEventResult>();
        // Synthesize an event for a sibling file we are not watching.
        let other = notify_debouncer_mini::DebouncedEvent {
            path: PathBuf::from("/some/dir/other.md"),
            kind: notify_debouncer_mini::DebouncedEventKind::Any,
        };
        tx.send(Ok(vec![other])).unwrap();
        assert!(!watched_file_changed(&rx, Some(target_name)));
    }
}
