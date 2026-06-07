# md: Terminal Markdown Renderer

`md` is a high-performance, native Rust CLI tool for rendering Markdown directly in your terminal. It goes beyond standard text viewers by leveraging modern terminal protocols and pure Rust layout engines to render syntax-highlighted code blocks, mathematical formulas (LaTeX), and diagram workflows (Mermaid) inline.

---

## Features

- 🎨 **Theme Aware**: Automatically aligns with your terminal colors or applies custom palettes, with built-in premium support for **Catppuccin Mocha** (dark) and **Catppuccin Latte** (light).
- 📐 **Self-Contained LaTeX Math**: Parses `$ ... $` (inline) and `$$ ... $$` (block) LaTeX equations and renders them inline using the pure-Rust `ratex` layout engine and `resvg` rasterization, with embedded KaTeX fonts for zero system dependencies.
- 🧜 **Mermaid Diagrams**: Compiles `mermaid` code blocks into SVGs natively via `mermaid-rs-renderer` and renders them directly inside the terminal.
- 🖼️ **Kitty Graphics Protocol**: Outputs high-resolution images for math formulas and diagrams using the Kitty terminal graphics protocol (supported by **Ghostty**, **WezTerm**, **Kitty**, and others).
- 🧱 **Aligned Unicode Tables**: Dynamically calculates column widths and draws neat table borders with proper text alignment (left, right, center).
- 📜 **Stream Piping**: Fully supports Unix pipes and redirection (e.g., `cat doc.md | md`).

---

## Installation & Compilation

Ensure you have Rust and Cargo installed, then build the release binary:

```bash
cargo build --release
```

The compiled binary will be available at `./target/release/md`. You can install it to your cargo bin directory using:

```bash
cargo install --path .
```

---

## Command-Line Usage

```
md: A CLI markdown renderer for the terminal

Usage: md [OPTIONS] [FILE]

Arguments:
  [FILE]  Markdown file to render. If not provided, reads from stdin.

Options:
  -t, --theme <THEME>  Color/style theme to use (options: terminal, mocha, latte) [default: terminal]
  -w, --width <WIDTH>  Width to wrap text to (defaults to terminal width)
      --no-images      Disable rendering of diagrams (Mermaid) and math formulas (LaTeX) as images
  -h, --help           Print help
  -V, --version        Print version
```

### Examples

Render a local file with the default terminal theme:
```bash
md document.md
```

Pipe text from another command and wrap it to 80 characters with the Catppuccin Mocha theme:
```bash
echo "# Hello World\nThis is **bold** text." | md -w 80 -t mocha
```

View a file with math and diagrams on WezTerm or Ghostty:
```bash
md research_notes.md
```

If your terminal does not support graphics or you want to output raw text, use `--no-images` to print the LaTeX and Mermaid source inside clean text boxes:
```bash
md research_notes.md --no-images
```

---

## How It Works Under the Hood

1. **Math Preprocessing**: Before parsing, `md` scans the document and wraps math delimiters into CommonMark code structures (`math:` inline tags or `math` block fences) while safely avoiding existing code blocks.
2. **Markdown Parsing**: Uses `pulldown-cmark` to generate a stream of structural events.
3. **Word Wrapping & Layout**: Words and formatting chunks are grouped and wrapped dynamically to fit your terminal columns, allowing styled text (bold, italic, links) to flow seamlessly.
4. **SVG Generation**:
   - Math equations are compiled using `ratex-parser` and `ratex-layout` into a display list, then serialized to SVG using `ratex-svg` in standalone mode (drawing glyph paths).
   - Mermaid diagrams are compiled using the `mermaid-rs-renderer` crate into SVGs.
5. **Rasterization**: SVGs are rasterized into high-resolution PNGs at the terminal aspect ratio using `resvg` and `tiny-skia`.
6. **Kitty Graphics Transfer**: PNGs are base64-encoded and outputted chunk-by-chunk using Kitty Graphics Protocol escape sequences (`\x1b_G...`), reserving exactly the calculated cell width/height and advancing the cursor dynamically.
