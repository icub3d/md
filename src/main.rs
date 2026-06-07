use std::fs::File;
use std::io::{self, Read};
use std::path::PathBuf;
use clap::Parser;

mod preprocessor;
mod theme;
mod renderer;

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
}

fn supports_kitty_graphics() -> bool {
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        let tp = term_program.to_lowercase();
        if tp.contains("kitty") || tp.contains("wezterm") || tp.contains("ghostty") || tp.contains("konsole") || tp.contains("rio") {
            return true;
        }
    }
    if let Ok(term) = std::env::var("TERM") {
        let t = term.to_lowercase();
        if t.contains("kitty") || t.contains("wezterm") || t.contains("ghostty") {
            return true;
        }
    }
    if let Ok(lc_term) = std::env::var("LC_TERMINAL") {
        let lt = lc_term.to_lowercase();
        if lt.contains("kitty") || lt.contains("wezterm") || lt.contains("ghostty") {
            return true;
        }
    }
    false
}

fn main() {
    let args = Args::parse();
    
    // Read input markdown
    let mut input = String::new();
    if let Some(path) = &args.file {
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
    } else {
        // Read from stdin
        if let Err(e) = io::stdin().read_to_string(&mut input) {
            eprintln!("Error: failed to read from stdin: {}", e);
            std::process::exit(1);
        }
    }

    // Determine target width
    let width = args.width.unwrap_or_else(|| {
        if let Ok((w, _)) = crossterm::terminal::size() {
            w as usize
        } else {
            80
        }
    });

    // Initialize the theme
    let theme = theme::Theme::new(&args.theme);

    // Preprocess markdown to parse math safely
    let preprocessed = preprocessor::preprocess_markdown(&input);

    // Parse options
    let mut options = pulldown_cmark::Options::empty();
    options.insert(pulldown_cmark::Options::ENABLE_TABLES);
    options.insert(pulldown_cmark::Options::ENABLE_TASKLISTS);
    options.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);

    let parser = pulldown_cmark::Parser::new_ext(&preprocessed, options);

    // Render output
    let no_images = args.no_images || !supports_kitty_graphics();
    let mut renderer = renderer::MarkdownRenderer::new(&theme, width, no_images);
    let output = renderer.render_events(parser);

    // Write to standard output
    print!("{}", output);
}
