use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use clap::Parser;

mod preprocessor;
mod theme;
mod renderer;
mod pager;

pub use renderer::ImageProtocol;

/// md: A CLI markdown renderer for the terminal
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Markdown file to render. If not provided, reads from stdin.
    #[arg(value_name = "FILE")]
    file: Option<PathBuf>,

    /// Color/style theme to use.
    /// Options: terminal, mocha, latte
    #[arg(short, long, default_value = "mocha")]
    theme: String,

    /// Width to wrap text to. Defaults to terminal width.
    #[arg(short, long)]
    width: Option<usize>,

    /// Disable rendering of diagrams (Mermaid) and math formulas (LaTeX) as images.
    #[arg(long)]
    no_images: bool,

    /// Disable pager even when output would normally be paged.
    #[arg(long)]
    no_pager: bool,

    /// Watch the input file and auto-reload the pager when it changes.
    /// Can be toggled at runtime with the 'w' key. Has no effect when reading from stdin.
    #[arg(long)]
    watch: bool,
}

/// Detect which inline image protocol (if any) the current terminal supports.
fn detect_image_protocol() -> ImageProtocol {
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        let tp = term_program.to_lowercase();
        if tp.contains("iterm") {
            return ImageProtocol::ITerm2;
        }
        if tp.contains("kitty") || tp.contains("wezterm") || tp.contains("ghostty")
            || tp.contains("konsole") || tp.contains("rio")
        {
            return ImageProtocol::Kitty;
        }
    }
    if let Ok(term) = std::env::var("TERM") {
        let t = term.to_lowercase();
        if t.contains("kitty") || t.contains("wezterm") || t.contains("ghostty") {
            return ImageProtocol::Kitty;
        }
    }
    if let Ok(lc_term) = std::env::var("LC_TERMINAL") {
        let lt = lc_term.to_lowercase();
        if lt.contains("iterm") {
            return ImageProtocol::ITerm2;
        }
        if lt.contains("kitty") || lt.contains("wezterm") || lt.contains("ghostty") {
            return ImageProtocol::Kitty;
        }
    }
    ImageProtocol::None
}

/// Return true if stdout is a real terminal (tty).
fn is_tty() -> bool {
    use std::io::IsTerminal;
    io::stdout().is_terminal()
}

/// Resolve the wrap width: the explicit value if given, otherwise the terminal
/// width, falling back to 80 columns when that can't be determined.
fn resolve_width(width: Option<usize>) -> usize {
    width.unwrap_or_else(|| {
        crossterm::terminal::size()
            .map(|(w, _)| w as usize)
            .unwrap_or(80)
    })
}

/// Run the full markdown pipeline (preprocess, parse, render) and return the
/// ANSI-styled output ready for the terminal or pager.
fn render_markdown(input: &str, theme_name: &str, width: usize, image_protocol: ImageProtocol) -> String {
    let theme = theme::Theme::new(theme_name);
    let preprocessed = preprocessor::preprocess_markdown(input);

    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_TASKLISTS);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);

    let parser = pulldown_cmark::Parser::new_ext(&preprocessed, options);
    let mut renderer = renderer::MarkdownRenderer::new(&theme, width, image_protocol);
    renderer.render_events(parser)
}

fn main() {
    let args = Args::parse();

    // Read input markdown
    let mut input = String::new();
    let filename_label = if let Some(path) = &args.file {
        match File::open(path) {
            Ok(mut file) => {
                if let Err(e) = file.read_to_string(&mut input) {
                    eprintln!("Error: failed to read file: {}", e);
                    std::process::exit(1);
                }
            }
            Err(e) => {
                eprintln!("Error: failed to open file {:?}: {}", path, e);
                std::process::exit(1);
            }
        }
        path.file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "document".to_string())
    } else {
        // Read from stdin
        if let Err(e) = io::stdin().read_to_string(&mut input) {
            eprintln!("Error: failed to read from stdin: {}", e);
            std::process::exit(1);
        }
        "stdin".to_string()
    };

    // Determine target width
    let width = resolve_width(args.width);

    // Detect image protocol support
    let image_protocol = if args.no_images {
        ImageProtocol::None
    } else {
        detect_image_protocol()
    };

    // Render output
    let output = render_markdown(&input, &args.theme, width, image_protocol);

    // Page output using our custom interactive pager if stdout is a tty and not disabled.
    let use_pager = !args.no_pager && is_tty();

    if use_pager {
        let mut reload_callback: Option<Box<dyn FnMut() -> Result<String, String>>> = None;

        if let Some(ref path) = args.file {
            let file_path = path.clone();
            let theme_name = args.theme.clone();
            let target_width = args.width;

            reload_callback = Some(Box::new(move || {
                let mut input = String::new();
                match File::open(&file_path) {
                    Ok(mut file) => {
                        if let Err(e) = file.read_to_string(&mut input) {
                            return Err(format!("Failed to read file: {}", e));
                        }
                    }
                    Err(e) => {
                        return Err(format!("Failed to open file: {}", e));
                    }
                }

                // Re-resolve the width on reload so resizing the terminal is picked up,
                // but reuse the image protocol detected at startup (it can't change).
                let width = resolve_width(target_width);
                Ok(render_markdown(&input, &theme_name, width, image_protocol))
            }));
        }

        // The watcher needs a concrete path; resolve to an absolute path so directory
        // watching and filename matching are reliable regardless of the cwd.
        let watch_path = args
            .file
            .as_ref()
            .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()));

        if let Err(e) = pager::run_pager(
            &output,
            &filename_label,
            image_protocol,
            reload_callback,
            watch_path,
            args.watch,
        ) {
            eprintln!("Pager error: {}", e);
            print!("{}", output);
        }
    } else {
        print!("{}", output);
    }
}
