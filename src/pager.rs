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

/// Lines derived from the rendered content: the raw (styled) lines that get
/// drawn, plus the ANSI/image-stripped, lowercased lines used for searching.
struct ContentLines {
    raw: Vec<String>,
    clean: Vec<String>,
}

impl ContentLines {
    fn from_content(content: &str) -> Self {
        let raw: Vec<String> = content.split('\n').map(|s| s.to_string()).collect();
        let clean = raw
            .iter()
            .map(|line| strip_ansi_and_images(line).to_lowercase())
            .collect();
        Self { raw, clean }
    }
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

    let mut scroll_row = 0;
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
        let viewport_height = height.saturating_sub(2);

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
                    " ['q': quit | 'r': reload | 'w': watch ● | '/': search] "
                } else {
                    " ['q': quit | 'r': reload | 'w': watch ○ | '/': search] "
                }
            } else if reload_callback.is_some() {
                " ['q': quit | 'r': reload | '/': search | 'j'/'k': scroll] "
            } else {
                " ['q': quit | '/': search | 'j'/'k': scroll] "
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
                queue!(
                    stdout,
                    cursor::MoveTo(0, (i + 1) as u16)
                )?;

                if file_row < raw_lines.len() {
                    let line_content = &raw_lines[file_row];
                    // Highlight the line if it is the current search match
                    let is_current_match = search
                        .as_ref()
                        .map(|s| {
                            !s.matches.is_empty() && s.matches[s.current_match_idx] == file_row
                        })
                        .unwrap_or(false);

                    if is_current_match {
                        // Highlight match line with a yellow indicator or background style
                        queue!(
                            stdout,
                            style::PrintStyledContent("➔ ".yellow().bold()),
                            style::Print(line_content)
                        )?;
                    } else {
                        queue!(stdout, style::Print(line_content))?;
                    }
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
                    let pos = scroll_row + viewport_height;
                    (pos * 100 / raw_lines.len()).min(100)
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
                    " md viewer ".to_string()
                };

                let right_text = format!(
                    " Line {}/{} ({}%) ",
                    scroll_row + 1,
                    raw_lines.len(),
                    progress
                );

                let status_len = left_text.chars().count() + right_text.chars().count();
                let status_padding = width.saturating_sub(status_len);
                let status_text = format!("{}{}{}", left_text, " ".repeat(status_padding), right_text);

                let status_color = if search.is_some() {
                    Color::Yellow
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
        if let Event::Key(key) = ev {
            // Any keypress warrants a repaint (status line, scroll, search, …).
            dirty = true;
            let _ = reload_status.take();

            // If typing search query
            if let Some(mut current_input) = input_buffer.take() {
                match key.code {
                    KeyCode::Enter => {
                        let query = current_input.trim().to_lowercase();
                        if !query.is_empty() {
                            let mut matches = Vec::new();
                            for (idx, line) in clean_lines.iter().enumerate() {
                                if line.contains(&query) {
                                    matches.push(idx);
                                }
                            }

                            let search_state = SearchState {
                                query: current_input,
                                matches,
                                current_match_idx: 0,
                            };

                            if !search_state.matches.is_empty() {
                                // Jump to the first match
                                scroll_row = search_state.matches[0];
                            }
                            search = Some(search_state);
                        } else {
                            search = None;
                        }
                        input_buffer = None;
                    }
                    KeyCode::Esc => {
                        // Cancel searching
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
                continue;
            }

            // Normal pager controls
            let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('q') | KeyCode::Esc => {
                    break;
                }
                KeyCode::Char('r') => {
                    if let Some(ref mut reload) = reload_callback {
                        match reload() {
                            Ok(new_content) => {
                                let nc = ContentLines::from_content(&new_content);
                                raw_lines = nc.raw;
                                clean_lines = nc.clean;
                                // Adjust scroll_row if the new content is shorter
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
                KeyCode::Char('w') if watchable => {
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
                KeyCode::Up | KeyCode::Char('k') => {
                    scroll_row = scroll_row.saturating_sub(1);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    if scroll_row + viewport_height < raw_lines.len() {
                        scroll_row += 1;
                    }
                }
                KeyCode::PageUp => {
                    scroll_row = scroll_row.saturating_sub(viewport_height);
                }
                KeyCode::PageDown | KeyCode::Char(' ') => {
                    scroll_row = (scroll_row + viewport_height)
                        .min(raw_lines.len().saturating_sub(viewport_height));
                }
                KeyCode::Char('b') => {
                    scroll_row = scroll_row.saturating_sub(viewport_height);
                }
                KeyCode::Char('f') => {
                    scroll_row = (scroll_row + viewport_height)
                        .min(raw_lines.len().saturating_sub(viewport_height));
                }
                KeyCode::Char('u') => {
                    let amount = if is_ctrl {
                        (viewport_height / 2).max(1)
                    } else {
                        viewport_height
                    };
                    scroll_row = scroll_row.saturating_sub(amount);
                }
                KeyCode::Char('d') => {
                    let amount = if is_ctrl {
                        (viewport_height / 2).max(1)
                    } else {
                        viewport_height
                    };
                    scroll_row = (scroll_row + amount)
                        .min(raw_lines.len().saturating_sub(viewport_height));
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    scroll_row = 0;
                }
                KeyCode::End | KeyCode::Char('G') => {
                    scroll_row = raw_lines.len().saturating_sub(viewport_height);
                }
                KeyCode::Char('/') => {
                    input_buffer = Some(String::new());
                }
                KeyCode::Char('n') => {
                    if let Some(ref mut s) = search
                        && !s.matches.is_empty()
                    {
                        s.current_match_idx = (s.current_match_idx + 1) % s.matches.len();
                        scroll_row = s.matches[s.current_match_idx];
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
                        scroll_row = s.matches[s.current_match_idx];
                    }
                }
                _ => {}
            }
        }
    }

    Ok(())
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

