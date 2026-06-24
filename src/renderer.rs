use crate::theme::{Style, Color, Theme, ThemeType};
use pulldown_cmark::{Event, Tag, TagEnd, CodeBlockKind};

/// Which inline-image protocol (if any) the current terminal supports.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ImageProtocol {
    /// No image support — fall back to ASCII boxes.
    None,
    /// Kitty Graphics Protocol (Kitty, WezTerm, Ghostty, …).
    Kitty,
    /// iTerm2 Inline Images Protocol (iTerm2 on macOS).
    ITerm2,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum LastBlockKind {
    None,
    Heading,
    Paragraph,
    ListItem,
    CodeBlock,
    Table,
    Rule,
}

pub struct MarkdownRenderer<'a> {
    theme: &'a Theme,
    width: usize,
    image_protocol: ImageProtocol,
    
    // Style stack
    style_stack: Vec<Style>,
    current_style: Style,
    
    // Lists and Blockquotes
    list_stack: Vec<ListState>,
    blockquote_depth: usize,
    
    // Buffering active block
    current_block: Option<Block>,
    
    // Buffering table
    current_table: Option<TableState>,
    
    image_id_counter: std::cell::Cell<u32>,
    syntax_theme: syntect::highlighting::Theme,
    syntax_set: syntect::parsing::SyntaxSet,
    fontdb: resvg::usvg::fontdb::Database,

    // Spacing state
    last_block: LastBlockKind,
    last_blockquote_depth: usize,
    current_heading_level: u32,
}

struct ListState {
    ordered: bool,
    index: usize,
}

pub struct TableState {
    pub alignments: Vec<pulldown_cmark::Alignment>,
    pub rows: Vec<TableRowState>,
    pub in_header: bool,
}

pub struct TableRowState {
    pub cells: Vec<Vec<Token>>,
}

#[derive(Clone, Debug)]
pub struct StyledChunk {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug)]
pub struct Word {
    pub chunks: Vec<StyledChunk>,
    pub width: usize,
}

#[derive(Clone, Debug)]
pub enum Token {
    Word(Word),
    Space(usize),
    SoftBreak,
    HardBreak,
}

struct Block {
    kind: BlockKind,
    tokens: Vec<Token>,
}

enum BlockKind {
    Paragraph,
    Heading(u32),
    Item,
    CodeBlock { lang: String, content: String },
}

impl<'a> MarkdownRenderer<'a> {
    pub fn new(theme: &'a Theme, width: usize, image_protocol: ImageProtocol) -> Self {
        let syntax_theme = match theme.theme_type {
            ThemeType::Mocha => {
                let bytes = include_bytes!("assets/Catppuccin-mocha.tmTheme");
                syntect::highlighting::ThemeSet::load_from_reader(&mut std::io::Cursor::new(bytes))
                    .expect("Failed to load Catppuccin Mocha theme")
            }
            ThemeType::Latte => {
                let bytes = include_bytes!("assets/Catppuccin-latte.tmTheme");
                syntect::highlighting::ThemeSet::load_from_reader(&mut std::io::Cursor::new(bytes))
                    .expect("Failed to load Catppuccin Latte theme")
            }
            ThemeType::Terminal => {
                let ts = syntect::highlighting::ThemeSet::load_defaults();
                ts.themes
                    .get("base16-ocean.dark")
                    .or_else(|| ts.themes.values().next())
                    .cloned()
                    .expect("syntect has no built-in themes")
            }
        };

        let syntax_set = syntect::parsing::SyntaxSet::load_defaults_newlines();

        let mut fontdb = resvg::usvg::fontdb::Database::new();
        if image_protocol != ImageProtocol::None {
            fontdb.load_system_fonts();
        }

        Self {
            theme,
            width,
            image_protocol,
            style_stack: Vec::new(),
            current_style: Style::default(),
            list_stack: Vec::new(),
            blockquote_depth: 0,
            current_block: None,
            current_table: None,
            image_id_counter: std::cell::Cell::new(1),
            syntax_theme,
            syntax_set,
            fontdb,
            last_block: LastBlockKind::None,
            last_blockquote_depth: 0,
            current_heading_level: 1,
        }
    }

    pub fn render_events(&mut self, events: impl IntoIterator<Item = Event<'a>>) -> String {
        self.last_block = LastBlockKind::None;
        self.last_blockquote_depth = 0;
        self.current_heading_level = 1;
        let mut output = String::new();
        let mut cell_buffer: Option<Vec<Token>> = None;
        let mut code_block_content = String::new();
        let mut in_code_block = false;
        let mut code_block_lang = String::new();
        
        for event in events {
            if in_code_block {
                match event {
                    Event::End(TagEnd::CodeBlock) => {
                        in_code_block = false;
                        let block = Block {
                            kind: BlockKind::CodeBlock {
                                lang: std::mem::take(&mut code_block_lang),
                                content: std::mem::take(&mut code_block_content),
                            },
                            tokens: Vec::new(),
                        };
                        let rendered = self.render_block(block);
                        self.append_block(&mut output, rendered, LastBlockKind::CodeBlock);
                    }
                    Event::Text(text) => {
                        code_block_content.push_str(&text);
                    }
                    _ => {}
                }
                continue;
            }
            
            match event {
                Event::Start(tag) => {
                    match tag {
                        Tag::Paragraph => {
                            self.current_block = Some(Block {
                                kind: BlockKind::Paragraph,
                                tokens: Vec::new(),
                            });
                        }
                        Tag::Heading { level, .. } => {
                            let lvl = match level {
                                pulldown_cmark::HeadingLevel::H1 => 1,
                                pulldown_cmark::HeadingLevel::H2 => 2,
                                pulldown_cmark::HeadingLevel::H3 => 3,
                                pulldown_cmark::HeadingLevel::H4 => 4,
                                pulldown_cmark::HeadingLevel::H5 => 5,
                                pulldown_cmark::HeadingLevel::H6 => 6,
                            };
                            self.current_heading_level = lvl;
                            self.current_block = Some(Block {
                                kind: BlockKind::Heading(lvl),
                                tokens: Vec::new(),
                            });
                            self.push_style(|s| {
                                s.bold = true;
                                s.fg = Some(Color::Heading(lvl));
                            });
                        }
                        Tag::BlockQuote(_) => {
                            self.blockquote_depth += 1;
                        }
                        Tag::List(start_idx) => {
                            let ordered = start_idx.is_some();
                            let index = start_idx.unwrap_or(1) as usize;
                            self.list_stack.push(ListState { ordered, index });
                        }
                        Tag::Item => {
                            self.current_block = Some(Block {
                                kind: BlockKind::Item,
                                tokens: Vec::new(),
                            });
                        }
                        Tag::Table(alignments) => {
                            self.current_table = Some(TableState {
                                alignments,
                                rows: Vec::new(),
                                in_header: false,
                            });
                        }
                        Tag::TableHead => {
                            if let Some(table) = &mut self.current_table {
                                table.in_header = true;
                                table.rows.push(TableRowState { cells: Vec::new() });
                            }
                        }
                        Tag::TableRow => {
                            if let Some(table) = &mut self.current_table {
                                table.rows.push(TableRowState { cells: Vec::new() });
                            }
                        }
                        Tag::TableCell => {
                            cell_buffer = Some(Vec::new());
                        }
                        Tag::Emphasis => {
                            self.push_style(|s| s.italic = true);
                        }
                        Tag::Strong => {
                            self.push_style(|s| s.bold = true);
                        }
                        Tag::Strikethrough => {
                            self.push_style(|s| s.strikethrough = true);
                        }
                        Tag::Link { .. } => {
                            self.push_style(|s| {
                                s.underline = true;
                                s.fg = Some(Color::Link);
                            });
                        }
                        Tag::CodeBlock(kind) => {
                            in_code_block = true;
                            code_block_lang = match kind {
                                CodeBlockKind::Fenced(lang) => lang.to_string(),
                                CodeBlockKind::Indented => String::new(),
                            };
                            code_block_content.clear();
                        }
                        _ => {}
                    }
                }
                
                Event::End(tag) => {
                    match tag {
                        TagEnd::Paragraph => {
                            if let Some(block) = self.current_block.take() {
                                let rendered = self.render_block(block);
                                self.append_block(&mut output, rendered, LastBlockKind::Paragraph);
                            }
                        }
                        TagEnd::Heading(_) => {
                            if let Some(block) = self.current_block.take() {
                                let rendered = self.render_block(block);
                                self.append_block(&mut output, rendered, LastBlockKind::Heading);
                            }
                            self.pop_style();
                        }
                        TagEnd::Item => {
                            if let Some(block) = self.current_block.take() {
                                let rendered = self.render_block(block);
                                self.append_block(&mut output, rendered, LastBlockKind::ListItem);
                            }
                            if let Some(list_state) = self.list_stack.last_mut() {
                                list_state.index += 1;
                            }
                        }
                        TagEnd::BlockQuote(_) => {
                            self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                        }
                        TagEnd::List(_) => {
                            self.list_stack.pop();
                        }
                        TagEnd::Table => {
                            if let Some(table) = self.current_table.take() {
                                let rendered = self.render_table(table);
                                self.append_block(&mut output, rendered, LastBlockKind::Table);
                            }
                        }
                        TagEnd::TableHead => {
                            if let Some(table) = &mut self.current_table {
                                table.in_header = false;
                            }
                        }
                        TagEnd::TableRow => {}
                        TagEnd::TableCell => {
                            if let Some(tokens) = cell_buffer.take()
                                && let Some(table) = &mut self.current_table
                                && let Some(row) = table.rows.last_mut()
                            {
                                row.cells.push(tokens);
                            }
                        }
                        TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough | TagEnd::Link => {
                            self.pop_style();
                        }
                        _ => {}
                    }
                }
                
                Event::Text(text) => {
                    let tokens = self.tokenize_text(&text, &self.current_style);
                    if let Some(ref mut cell) = cell_buffer {
                        cell.extend(tokens);
                    } else if self.current_block.is_some() {
                        self.add_tokens(tokens);
                    } else {
                        self.current_block = Some(Block {
                            kind: BlockKind::Paragraph,
                            tokens: Vec::new(),
                        });
                        self.add_tokens(tokens);
                    }
                }
                Event::Code(code) => {
                    if let Some(math_str) = code.strip_prefix("math:") {
                        let math_tokens = self.render_inline_math(math_str);
                        if let Some(ref mut cell) = cell_buffer {
                            cell.extend(math_tokens);
                        } else if self.current_block.is_some() {
                            self.add_tokens(math_tokens);
                        }
                    } else {
                        let mut code_style = self.current_style.clone();
                        code_style.fg = Some(Color::Code);
                        let tokens = self.tokenize_text(&code, &code_style);
                        if let Some(ref mut cell) = cell_buffer {
                            cell.extend(tokens);
                        } else if self.current_block.is_some() {
                            self.add_tokens(tokens);
                        }
                    }
                }
                Event::SoftBreak => {
                    if let Some(ref mut cell) = cell_buffer {
                        cell.push(Token::SoftBreak);
                    } else if self.current_block.is_some() {
                        self.add_tokens(vec![Token::SoftBreak]);
                    }
                }
                Event::HardBreak => {
                    if let Some(ref mut cell) = cell_buffer {
                        cell.push(Token::HardBreak);
                    } else if self.current_block.is_some() {
                        self.add_tokens(vec![Token::HardBreak]);
                    }
                }
                Event::Rule => {
                    self.current_heading_level = 1;
                    let doc_indent = self.document_indent();
                    let bq_prefix = self.blockquote_prefix();
                    let hr_style = Style {
                        bold: false,
                        italic: false,
                        underline: false,
                        strikethrough: false,
                        fg: Some(Color::Rule),
                    };
                    let ansi = hr_style.to_ansi(self.theme);
                    let reset = "\x1b[0m";
                    let bq_len = self.blockquote_prefix_width();
                    let rule_width = self.width.saturating_sub(bq_len).saturating_sub(doc_indent);
                    let rendered = format!("{}{}{}{}{}\n", " ".repeat(doc_indent), bq_prefix, ansi, "─".repeat(rule_width), reset);
                    self.append_block(&mut output, rendered, LastBlockKind::Rule);
                }
                _ => {}
            }
        }
        
        output
    }

    fn push_style(&mut self, modifier: impl FnOnce(&mut Style)) {
        let mut next = self.current_style.clone();
        modifier(&mut next);
        self.style_stack.push(self.current_style.clone());
        self.current_style = next;
    }

    fn pop_style(&mut self) {
        if let Some(prev) = self.style_stack.pop() {
            self.current_style = prev;
        }
    }

    fn tokenize_text(&self, text: &str, style: &Style) -> Vec<Token> {
        let mut tokens = Vec::new();
        let mut current_word = String::new();
        let mut current_word_width = 0;
        
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c.is_whitespace() {
                if !current_word.is_empty() {
                    tokens.push(Token::Word(Word {
                        chunks: vec![StyledChunk {
                            text: current_word.clone(),
                            style: style.clone(),
                        }],
                        width: current_word_width,
                    }));
                    current_word.clear();
                    current_word_width = 0;
                }
                
                let mut space_count = 1;
                while let Some(&nc) = chars.peek() {
                    if nc.is_whitespace() {
                        space_count += 1;
                        chars.next();
                    } else {
                        break;
                    }
                }
                tokens.push(Token::Space(space_count));
            } else {
                current_word.push(c);
                current_word_width += 1;
            }
        }
        
        if !current_word.is_empty() {
            tokens.push(Token::Word(Word {
                chunks: vec![StyledChunk {
                    text: current_word,
                    style: style.clone(),
                }],
                width: current_word_width,
            }));
        }
        
        tokens
    }

    fn add_tokens(&mut self, new_tokens: Vec<Token>) {
        if let Some(block) = &mut self.current_block {
            for token in new_tokens {
                match token {
                    Token::Word(mut new_word) => {
                        if let Some(Token::Word(last_word)) = block.tokens.last_mut() {
                            last_word.chunks.append(&mut new_word.chunks);
                            last_word.width += new_word.width;
                        } else {
                            block.tokens.push(Token::Word(new_word));
                        }
                    }
                    other => block.tokens.push(other),
                }
            }
        }
    }

    fn wrap_block_tokens(&self, tokens: &[Token], max_width: usize) -> Vec<Vec<Token>> {
        let mut lines = Vec::new();
        let mut current_line = Vec::new();
        let mut current_width = 0;
        
        for token in tokens {
            match token {
                Token::Space(w) => {
                    if current_line.is_empty() {
                        continue;
                    }
                    if current_width + w <= max_width {
                        current_line.push(Token::Space(*w));
                        current_width += w;
                    } else {
                        lines.push(current_line);
                        current_line = Vec::new();
                        current_width = 0;
                    }
                }
                Token::Word(word) => {
                    if current_line.is_empty() {
                        current_line.push(Token::Word(word.clone()));
                        current_width = word.width;
                    } else if current_width + word.width <= max_width {
                        current_line.push(Token::Word(word.clone()));
                        current_width += word.width;
                    } else {
                        lines.push(current_line);
                        current_line = vec![Token::Word(word.clone())];
                        current_width = word.width;
                    }
                }
                Token::SoftBreak => {
                    if current_line.is_empty() {
                        continue;
                    }
                    if current_width < max_width {
                        current_line.push(Token::Space(1));
                        current_width += 1;
                    } else {
                        lines.push(current_line);
                        current_line = Vec::new();
                        current_width = 0;
                    }
                }
                Token::HardBreak => {
                    lines.push(current_line);
                    current_line = Vec::new();
                    current_width = 0;
                }
            }
        }
        
        if !current_line.is_empty() {
            lines.push(current_line);
        }
        
        lines
    }

    fn format_line_tokens(&self, line: &[Token]) -> String {
        let mut out = String::new();
        for token in line {
            match token {
                Token::Space(w) => {
                    out.push_str(&" ".repeat(*w));
                }
                Token::Word(word) => {
                    for chunk in &word.chunks {
                        let ansi = chunk.style.to_ansi(self.theme);
                        out.push_str(&ansi);
                        out.push_str(&chunk.text);
                        out.push_str("\x1b[0m");
                    }
                }
                _ => {}
            }
        }
        out
    }

    fn blockquote_prefix_at_depth(&self, depth: usize) -> String {
        let mut prefix = String::new();
        if depth > 0 {
            let style = Style {
                bold: false,
                italic: false,
                underline: false,
                strikethrough: false,
                fg: Some(Color::Blockquote),
            };
            let ansi = style.to_ansi(self.theme);
            let reset = "\x1b[0m";
            for _ in 0..depth {
                prefix.push_str(&format!("{}│{} ", ansi, reset));
            }
        }
        prefix
    }

    fn blockquote_prefix(&self) -> String {
        self.blockquote_prefix_at_depth(self.blockquote_depth)
    }

    fn blockquote_prefix_width(&self) -> usize {
        self.blockquote_depth * 2
    }

    fn document_indent(&self) -> usize {
        (self.current_heading_level.saturating_sub(1) as usize) * 2
    }

    fn append_block(&mut self, output: &mut String, rendered: String, new_block_kind: LastBlockKind) {
        if rendered.is_empty() {
            return;
        }

        let current_depth = self.blockquote_depth;
        
        if self.last_block != LastBlockKind::None {
            let mut insert_blank = true;
            
            // Rule 1: No blank line between consecutive list items
            if self.last_block == LastBlockKind::ListItem && new_block_kind == LastBlockKind::ListItem {
                insert_blank = false;
            }
            
            // Rule 2: No blank line if blockquote depth changes and both are > 0 (e.g. nested blockquotes)
            if self.last_blockquote_depth > 0 && current_depth > 0 && self.last_blockquote_depth != current_depth {
                insert_blank = false;
            }
            
            if insert_blank {
                // The blank line should have the blockquote prefix at the minimum depth of the transition
                let min_depth = self.last_blockquote_depth.min(current_depth);
                let bq_prefix = self.blockquote_prefix_at_depth(min_depth);
                if bq_prefix.is_empty() {
                    output.push('\n');
                } else {
                    let doc_indent = self.document_indent();
                    output.push_str(&format!("{}{}\n", " ".repeat(doc_indent), bq_prefix));
                }
            }
        }
        
        output.push_str(&rendered);
        self.last_block = new_block_kind;
        self.last_blockquote_depth = current_depth;
    }

    fn render_block(&mut self, block: Block) -> String {
        let mut out = String::new();
        match block.kind {
            BlockKind::Paragraph => {
                let doc_indent = self.document_indent();
                let bq_prefix = self.blockquote_prefix();
                let bq_len = self.blockquote_prefix_width();
                let max_width = self.width.saturating_sub(doc_indent).saturating_sub(bq_len);
                let wrapped_lines = self.wrap_block_tokens(&block.tokens, max_width);
                for line in wrapped_lines {
                    let line_str = self.format_line_tokens(&line);
                    out.push_str(&format!("{}{}{}\n", " ".repeat(doc_indent), bq_prefix, line_str));
                }
            }
            BlockKind::Heading(level) => {
                let doc_indent = self.document_indent();
                let bq_prefix = self.blockquote_prefix();
                let bq_len = self.blockquote_prefix_width();
                let max_width = self.width.saturating_sub(doc_indent).saturating_sub(bq_len);
                let wrapped_lines = self.wrap_block_tokens(&block.tokens, max_width);
                
                let style = Style {
                    bold: true,
                    italic: false,
                    underline: false,
                    strikethrough: false,
                    fg: Some(Color::Heading(level)),
                };
                let ansi = style.to_ansi(self.theme);
                let reset = "\x1b[0m";
                
                if !wrapped_lines.is_empty() {
                    for line in wrapped_lines {
                        let line_str = self.format_line_tokens(&line);
                        out.push_str(&format!("{}{}{}{}{}\n", " ".repeat(doc_indent), bq_prefix, ansi, line_str, reset));
                    }
                }
            }
            BlockKind::Item => {
                let doc_indent = self.document_indent();
                let bq_prefix = self.blockquote_prefix();
                let bq_len = self.blockquote_prefix_width();
                
                let max_width = self.width.saturating_sub(doc_indent).saturating_sub(bq_len);
                let indent_depth = self.list_stack.len().saturating_sub(1);
                let indent_width = indent_depth * 2;
                
                let mut bullet = String::new();
                if let Some(list_state) = self.list_stack.last() {
                    if list_state.ordered {
                        bullet = format!("{}. ", list_state.index);
                    } else {
                        bullet = match indent_depth % 3 {
                            0 => "● ".to_string(),
                            1 => "○ ".to_string(),
                            _ => "■ ".to_string(),
                        };
                    }
                }
                
                let prefix_width = indent_width + bullet.chars().count();
                let available_width = if max_width > prefix_width { max_width - prefix_width } else { 20 };
                let wrapped_lines = self.wrap_block_tokens(&block.tokens, available_width);
                
                let bullet_style = Style { fg: Some(Color::List), bold: true, ..Style::default() };
                let bullet_ansi = bullet_style.to_ansi(self.theme);
                let reset = "\x1b[0m";
                
                for (i, line) in wrapped_lines.iter().enumerate() {
                    let line_str = self.format_line_tokens(line);
                    if i == 0 {
                        out.push_str(&format!(
                            "{}{}{}{}{}{}{}\n",
                            " ".repeat(doc_indent), bq_prefix, " ".repeat(indent_width), bullet_ansi, bullet, reset, line_str
                        ));
                    } else {
                        out.push_str(&format!(
                            "{}{}{}{}\n",
                            " ".repeat(doc_indent), bq_prefix, " ".repeat(prefix_width), line_str
                        ));
                    }
                }
            }
            BlockKind::CodeBlock { lang, content } => {
                if lang == "mermaid" {
                    out.push_str(&self.render_mermaid(&content));
                } else if lang == "math" {
                    out.push_str(&self.render_math(&content));
                } else {
                    out.push_str(&self.render_code_block_highlighted(&lang, &content));
                }
            }
        }
        out
    }

    fn render_code_block_highlighted(&self, lang: &str, content: &str) -> String {
        use syntect::easy::HighlightLines;
        use syntect::highlighting::Style as SyntectStyle;
        use syntect::util::as_24_bit_terminal_escaped;

        let doc_indent = self.document_indent();
        let bq_prefix = self.blockquote_prefix();
        let mut out = String::new();

        let syntax = self.syntax_set.find_syntax_by_token(lang)
            .unwrap_or_else(|| self.syntax_set.find_syntax_plain_text());

        let mut h = HighlightLines::new(syntax, &self.syntax_theme);

        let box_style = Style {
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            fg: Some(Color::Code),
        };
        let box_ansi = box_style.to_ansi(self.theme);
        let reset = "\x1b[0m";

        let box_width = self.width.saturating_sub(self.blockquote_prefix_width() + doc_indent + 2);
        let tag = if lang.is_empty() { "code" } else { lang };
        let top_border = self.render_box_top(tag, box_width, &box_ansi);
        out.push_str(&format!("{}{}{}\n", " ".repeat(doc_indent), bq_prefix, top_border));

        for line in content.lines() {
            let ranges: Vec<(SyntectStyle, &str)> = h.highlight_line(line, &self.syntax_set)
                .unwrap_or_else(|_| vec![(SyntectStyle::default(), line)]);
            let escaped = as_24_bit_terminal_escaped(&ranges[..], false);
            let pad_width = box_width.saturating_sub(line.chars().count() + 3);
            out.push_str(&format!(
                "{}{}{}│{} {}{}{}│{}\n",
                " ".repeat(doc_indent), bq_prefix, box_ansi, reset, escaped, " ".repeat(pad_width), box_ansi, reset
            ));
        }

        let bottom_border = self.render_box_bottom(box_width, &box_ansi);
        out.push_str(&format!("{}{}{}\n", " ".repeat(doc_indent), bq_prefix, bottom_border));

        out
    }

    fn render_table(&self, table: TableState) -> String {
        let mut out = String::new();
        let doc_indent = self.document_indent();
        let bq_prefix = format!("{}{}", " ".repeat(doc_indent), self.blockquote_prefix());
        
        let col_count = table.alignments.len();
        if col_count == 0 || table.rows.is_empty() {
            return out;
        }
        
        // Calculate max word length and unwrapped widths for each column
        let mut max_word_lens = vec![0; col_count];
        let mut unwrapped_widths = vec![0; col_count];
        for row in &table.rows {
            for (col_idx, cell) in row.cells.iter().enumerate() {
                if col_idx < col_count {
                    let mut max_w = 0;
                    let mut total_w = 0;
                    for t in cell {
                        match t {
                            Token::Word(w) => {
                                max_w = max_w.max(w.width);
                                total_w += w.width;
                            }
                            Token::Space(sp) => {
                                total_w += *sp;
                            }
                            _ => {}
                        }
                    }
                    max_word_lens[col_idx] = max_word_lens[col_idx].max(max_w);
                    unwrapped_widths[col_idx] = unwrapped_widths[col_idx].max(total_w);
                }
            }
        }
        
        // Determine available table width
        let bq_len = self.blockquote_prefix_width();
        let max_table_width = self.width.saturating_sub(bq_len).saturating_sub(doc_indent);
        let decorations_width = 3 * col_count + 1;
        let max_content_width = max_table_width.saturating_sub(decorations_width);
        
        let mut col_widths = vec![0; col_count];
        let sum_unwrapped: usize = unwrapped_widths.iter().sum();
        
        if sum_unwrapped <= max_content_width || max_content_width == 0 {
            col_widths = unwrapped_widths;
        } else {
            // We need to wrap cells to fit max_content_width
            // Calculate minimum width for each column: max of max_word_len (capped at 15) and 3
            let mut min_widths = vec![0; col_count];
            for i in 0..col_count {
                min_widths[i] = max_word_lens[i].clamp(3, 15).min(unwrapped_widths[i]);
            }
            
            let sum_min: usize = min_widths.iter().sum();
            if sum_min > max_content_width {
                // Allocate proportionally to unwrapped_widths, ensuring each gets at least 1
                let mut allocated = 0;
                for i in 0..col_count {
                    let w = (max_content_width * unwrapped_widths[i] / sum_unwrapped).max(1);
                    col_widths[i] = w;
                    allocated += w;
                }
                // Adjust allocated total if it doesn't match max_content_width
                if allocated != max_content_width {
                    let mut diff = max_content_width as i32 - allocated as i32;
                    if diff > 0 {
                        for w in col_widths.iter_mut() {
                            if diff == 0 { break; }
                            *w += 1;
                            diff -= 1;
                        }
                    } else if diff < 0 {
                        for w in col_widths.iter_mut() {
                            if diff == 0 { break; }
                            if *w > 1 {
                                *w -= 1;
                                diff += 1;
                            }
                        }
                    }
                }
            } else {
                col_widths = min_widths;
                let mut remaining = max_content_width - sum_min;
                
                let demands: Vec<usize> = (0..col_count)
                    .map(|i| unwrapped_widths[i].saturating_sub(col_widths[i]))
                    .collect();
                let sum_demand: usize = demands.iter().sum();
                
                let mut allocated_extra = 0;
                let mut extra_widths = vec![0; col_count];
                for (i, &demand) in demands.iter().enumerate() {
                    let extra = (remaining * demand).checked_div(sum_demand).unwrap_or(0);
                    extra_widths[i] = extra;
                    allocated_extra += extra;
                }
                remaining -= allocated_extra;

                for (&demand, extra) in demands.iter().zip(extra_widths.iter_mut()) {
                    if remaining == 0 { break; }
                    if demand > *extra {
                        *extra += 1;
                        remaining -= 1;
                    }
                }

                for (col_width, extra) in col_widths.iter_mut().zip(extra_widths.iter()) {
                    *col_width += extra;
                }
            }
        }
        
        let padded_widths: Vec<usize> = col_widths.iter().map(|w| w + 2).collect();
        
        let draw_border = |left: &str, middle: &str, right: &str, cross: &str| -> String {
            let mut line = String::new();
            line.push_str(left);
            for (i, w) in padded_widths.iter().enumerate() {
                line.push_str(&middle.repeat(*w));
                if i < col_count - 1 {
                    line.push_str(cross);
                }
            }
            line.push_str(right);
            line
        };
        
        let border_style = Style {
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            fg: Some(Color::Code),
        };
        let border_ansi = border_style.to_ansi(self.theme);
        let reset = "\x1b[0m";
        
        let top = draw_border("┌", "─", "┐", "┬");
        out.push_str(&format!("{}{}{}{}\n", bq_prefix, border_ansi, top, reset));
        
        for (row_idx, row) in table.rows.iter().enumerate() {
            // Wrap the cells of this row to the allocated column widths
            let mut cell_lines: Vec<Vec<Vec<Token>>> = Vec::new();
            for (col_idx, &col_width) in col_widths.iter().enumerate() {
                let cell = row.cells.get(col_idx).cloned().unwrap_or_default();
                let wrapped = self.wrap_block_tokens(&cell, col_width);
                cell_lines.push(wrapped);
            }
            
            let max_lines = cell_lines.iter().map(|lines| lines.len()).max().unwrap_or(0).max(1);
            
            for line_idx in 0..max_lines {
                out.push_str(&bq_prefix);
                out.push_str(&border_ansi);
                out.push('│');
                out.push_str(reset);
                
                for col_idx in 0..col_count {
                    let cell_tokens = if line_idx < cell_lines[col_idx].len() {
                        cell_lines[col_idx][line_idx].clone()
                    } else {
                        Vec::new()
                    };
                    
                    let cell_str = self.format_line_tokens(&cell_tokens);
                    let cell_width: usize = cell_tokens.iter().map(|t| match t {
                        Token::Word(w) => w.width,
                        Token::Space(sp) => *sp,
                        _ => 0
                    }).sum();
                    
                    let total_pad = padded_widths[col_idx].saturating_sub(cell_width);
                    let (left_pad, right_pad) = if total_pad == 0 {
                        (0, 0)
                    } else {
                        match table.alignments[col_idx] {
                            pulldown_cmark::Alignment::Left | pulldown_cmark::Alignment::None => (1, total_pad - 1),
                            pulldown_cmark::Alignment::Right => (total_pad - 1, 1),
                            pulldown_cmark::Alignment::Center => {
                                let lp = total_pad / 2;
                                (lp, total_pad - lp)
                            }
                        }
                    };
                    
                    out.push_str(&" ".repeat(left_pad));
                    out.push_str(&cell_str);
                    out.push_str(&" ".repeat(right_pad));
                    
                    out.push_str(&border_ansi);
                    out.push('│');
                    out.push_str(reset);
                }
                out.push('\n');
            }
            
            if row_idx == 0 && table.rows.len() > 1 {
                let sep = draw_border("╞", "═", "╡", "╪");
                out.push_str(&format!("{}{}{}{}\n", bq_prefix, border_ansi, sep, reset));
            } else if row_idx < table.rows.len() - 1 {
                let sep = draw_border("├", "─", "┤", "┼");
                out.push_str(&format!("{}{}{}{}\n", bq_prefix, border_ansi, sep, reset));
            }
        }
        
        let bottom = draw_border("└", "─", "┘", "┴");
        out.push_str(&format!("{}{}{}{}\n", bq_prefix, border_ansi, bottom, reset));
        
        out
    }

    // --- Graph and Math Renderers ---

    fn render_mermaid(&self, content: &str) -> String {
        if self.image_protocol == ImageProtocol::None {
            return self.render_mermaid_fallback(content);
        }
        
        use mermaid_rs_renderer::render;
        let svg = match render(content) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Mermaid render error: {}", e);
                return self.render_mermaid_fallback(content);
            }
        };
        
        let (png_bytes, (svg_width, svg_height)) = match self.svg_to_png(&svg) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("SVG to PNG conversion error: {}", e);
                return self.render_mermaid_fallback(content);
            }
        };

        let (cols, rows) = self.calculate_terminal_cells(svg_width, svg_height, 50);
        let image_esc = self.render_image(&png_bytes, cols, rows);
        
        let doc_indent = self.document_indent();
        let bq_prefix = self.blockquote_prefix();
        let total_width = self.width.saturating_sub(self.blockquote_prefix_width() + doc_indent);
        let padding = if total_width > cols { (total_width - cols) / 2 } else { 0 };
        
        format!("{}{}{}{}", " ".repeat(doc_indent), bq_prefix, " ".repeat(padding), image_esc)
    }

    fn render_math(&self, content: &str) -> String {
        if self.image_protocol == ImageProtocol::None {
            return self.render_math_fallback(content);
        }

        let svg = match self.latex_to_svg(content) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Math compile error: {}", e);
                return self.render_math_fallback(content);
            }
        };

        let (png_bytes, (svg_width, svg_height)) = match self.svg_to_png(&svg) {
            Ok(result) => result,
            Err(e) => {
                eprintln!("SVG to PNG error: {}", e);
                return self.render_math_fallback(content);
            }
        };

        let (cols, rows) = self.calculate_terminal_cells(svg_width, svg_height, 40);
        let image_esc = self.render_image(&png_bytes, cols, rows);

        let doc_indent = self.document_indent();
        let bq_prefix = self.blockquote_prefix();
        let total_width = self.width.saturating_sub(self.blockquote_prefix_width() + doc_indent);
        let padding = if total_width > cols { (total_width - cols) / 2 } else { 0 };

        format!("{}{}{}{}", " ".repeat(doc_indent), bq_prefix, " ".repeat(padding), image_esc)
    }

    fn render_inline_math(&self, content: &str) -> Vec<Token> {
        if self.image_protocol == ImageProtocol::None {
            return self.tokenize_text(&format!("[Math: {}]", content), &self.current_style);
        }
        
        let svg = match self.latex_to_svg(content) {
            Ok(s) => s,
            Err(_) => return self.tokenize_text(&format!("[Math: {}]", content), &self.current_style),
        };
        
        let (png_bytes, (svg_width, svg_height)) = match self.svg_to_png(&svg) {
            Ok(result) => result,
            Err(_) => return self.tokenize_text(&format!("[Math: {}]", content), &self.current_style),
        };

        let aspect_ratio = svg_height / svg_width;
        let cols = ((2.0 / aspect_ratio).round() as usize).clamp(2, 30);

        let escape_code = self.render_image(&png_bytes, cols, 1);
        
        vec![Token::Word(Word {
            chunks: vec![StyledChunk {
                text: escape_code,
                style: Style::default(),
            }],
            width: cols,
        })]
    }

    fn latex_to_svg(&self, latex: &str) -> Result<String, String> {
        use ratex_parser::parse;
        use ratex_layout::{layout, to_display_list, LayoutOptions};
        use ratex_svg::{render_to_svg, SvgOptions};
        
        let nodes = parse(latex)
            .map_err(|e| format!("LaTeX parsing error: {:?}", e))?;
            
        let mut layout_opts = LayoutOptions::default();
        
        let fg_color = match self.theme.theme_type {
            ThemeType::Mocha => ratex_types::color::Color::rgb(205.0 / 255.0, 214.0 / 255.0, 244.0 / 255.0),
            ThemeType::Latte => ratex_types::color::Color::rgb(76.0 / 255.0, 79.0 / 255.0, 105.0 / 255.0),
            ThemeType::Terminal => ratex_types::color::Color::rgb(1.0, 1.0, 1.0),
        };
        layout_opts = layout_opts.with_color(fg_color);
        
        let layout_box = layout(&nodes, &layout_opts);
        let display_list = to_display_list(&layout_box);
        
        let svg_opts = SvgOptions { embed_glyphs: true, ..SvgOptions::default() };
        
        let svg_str = render_to_svg(&display_list, &svg_opts);
        Ok(svg_str)
    }

    fn svg_to_png(&self, svg_str: &str) -> Result<(Vec<u8>, (f64, f64)), String> {
        let mut opt = resvg::usvg::Options::default();
        // Text-to-path conversion now happens inside `Tree::from_str`, driven by
        // the fontdb carried on the options, so inject our preloaded database.
        *opt.fontdb_mut() = self.fontdb.clone();
        let usvg_tree = resvg::usvg::Tree::from_str(svg_str, &opt)
            .map_err(|e| format!("usvg parse error: {}", e))?;

        let size = usvg_tree.size();
        let svg_width = size.width() as f64;
        let svg_height = size.height() as f64;

        let target_width = (self.width as f64 * 12.0).clamp(200.0, 1200.0);
        let scale = target_width / svg_width;
        let target_height = svg_height * scale;

        let mut pixmap = resvg::tiny_skia::Pixmap::new(target_width as u32, target_height as u32)
            .ok_or_else(|| "Failed to create Pixmap".to_string())?;

        resvg::render(
            &usvg_tree,
            resvg::usvg::Transform::from_scale(scale as f32, scale as f32),
            &mut pixmap.as_mut(),
        );

        let png_bytes = pixmap.encode_png()
            .map_err(|e| format!("Failed to encode PNG: {}", e))?;

        Ok((png_bytes, (svg_width, svg_height)))
    }

    fn calculate_terminal_cells(&self, width_px: f64, height_px: f64, max_cols: usize) -> (usize, usize) {
        let cols = self.width.saturating_sub(4).min(max_cols).max(10);
        let rows = (((height_px / width_px) * (cols as f64) * 0.55).round() as usize).max(1);
        (cols, rows)
    }

    fn render_box_top(&self, title: &str, box_width: usize, border_ansi: &str) -> String {
        let reset = "\x1b[0m";
        let prefix = format!("┌── {} ──", title);
        let prefix_chars = prefix.chars().count();
        
        if box_width > prefix_chars + 1 {
            let dashes_needed = box_width - prefix_chars - 1;
            format!("{}{}{}┐{}", border_ansi, prefix, "─".repeat(dashes_needed), reset)
        } else {
            format!("{}┌{}┐{}", border_ansi, "─".repeat(box_width.saturating_sub(2)), reset)
        }
    }

    fn render_box_bottom(&self, box_width: usize, border_ansi: &str) -> String {
        let reset = "\x1b[0m";
        format!("{}└{}┘{}", border_ansi, "─".repeat(box_width.saturating_sub(2)), reset)
    }

    fn get_png_dimensions(png_bytes: &[u8]) -> Option<(u32, u32)> {
        if png_bytes.len() < 24 {
            return None;
        }
        if png_bytes[0..8] != [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a] {
            return None;
        }
        if &png_bytes[12..16] != b"IHDR" {
            return None;
        }
        let width = u32::from_be_bytes([png_bytes[16], png_bytes[17], png_bytes[18], png_bytes[19]]);
        let height = u32::from_be_bytes([png_bytes[20], png_bytes[21], png_bytes[22], png_bytes[23]]);
        Some((width, height))
    }

    /// Dispatch to the appropriate image renderer based on detected protocol.
    fn render_image(&self, png_bytes: &[u8], cols: usize, rows: usize) -> String {
        match self.image_protocol {
            ImageProtocol::Kitty => self.render_kitty_image(png_bytes, cols, rows),
            ImageProtocol::ITerm2 => self.render_iterm2_image(png_bytes, cols, rows),
            ImageProtocol::None => String::new(),
        }
    }

    fn render_kitty_image(&self, png_bytes: &[u8], cols: usize, rows: usize) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        
        let image_id = self.image_id_counter.get();
        self.image_id_counter.set(image_id + 1);
        
        let (width_px, height_px) = Self::get_png_dimensions(png_bytes).unwrap_or((0, 0));
        
        let chunk_size = 4096;
        let mut out = String::new();
        
        let mut dim_params = format!("c={},r={}", cols, rows);
        if width_px > 0 && height_px > 0 {
            dim_params.push_str(&format!(",s={},v={}", width_px, height_px));
        }
        
        if b64.len() <= chunk_size {
            // Single chunk — q=2 suppresses terminal acknowledgment responses
            out.push_str(&format!(
                "\x1b_Ga=T,t=d,f=100,q=2,{},i={};{}\x1b\\",
                dim_params, image_id, b64
            ));
        } else {
            // Multi-chunk transmission — q=2 suppresses terminal acknowledgment responses
            let mut offset = 0;
            let mut first = true;
            while offset < b64.len() {
                let end = (offset + chunk_size).min(b64.len());
                let chunk = &b64[offset..end];
                let is_last = end == b64.len();
                let m_val = if is_last { 0 } else { 1 };

                if first {
                    out.push_str(&format!(
                        "\x1b_Ga=T,t=d,f=100,q=2,{},i={},m={};{}\x1b\\",
                        dim_params, image_id, m_val, chunk
                    ));
                    first = false;
                } else {
                    out.push_str(&format!(
                        "\x1b_Gm={};{}\x1b\\",
                        m_val, chunk
                    ));
                }
                offset = end;
            }
        }
        
        // Advance the cursor by `rows` lines so subsequent text is not drawn on top.
        out.push_str(&"\n".repeat(rows));
        out
    }

    /// Render a PNG using the iTerm2 Inline Images Protocol.
    ///
    /// Protocol: `ESC ] 1337 ; File=inline=1;width=<cols>;height=<rows>;size=<bytes>:<b64> BEL`
    /// Reference: https://iterm2.com/documentation-images.html
    fn render_iterm2_image(&self, png_bytes: &[u8], cols: usize, rows: usize) -> String {
        use base64::Engine;
        let b64 = base64::engine::general_purpose::STANDARD.encode(png_bytes);
        let size = png_bytes.len();

        // width/height in character cells; iTerm2 also accepts pixel values with the
        // "px" suffix, but cell counts work well for our layout model.
        let mut out = format!(
            "\x1b]1337;File=inline=1;width={cols};height={rows};size={size};preserveAspectRatio=1:{b64}\x07"
        );
        // Advance cursor past the image rows.
        out.push_str(&"\n".repeat(rows));
        out
    }

    fn render_math_fallback(&self, content: &str) -> String {
        let doc_indent = self.document_indent();
        let bq_prefix = self.blockquote_prefix();
        let border_style = Style {
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            fg: Some(Color::Code),
        };
        let border_ansi = border_style.to_ansi(self.theme);
        let reset = "\x1b[0m";
        let box_width = self.width.saturating_sub(self.blockquote_prefix_width() + doc_indent + 2);

        let mut out = String::new();
        let top_border = self.render_box_top("Math Formula", box_width, &border_ansi);
        out.push_str(&format!("{}{}{}\n", " ".repeat(doc_indent), bq_prefix, top_border));

        for line in content.lines() {
            let pad_width = box_width.saturating_sub(line.chars().count() + 3);
            out.push_str(&format!(
                "{}{}{}│{} {}{}{}│{}\n",
                " ".repeat(doc_indent), bq_prefix, border_ansi, reset, line, " ".repeat(pad_width), border_ansi, reset
            ));
        }

        let bottom_border = self.render_box_bottom(box_width, &border_ansi);
        out.push_str(&format!("{}{}{}\n", " ".repeat(doc_indent), bq_prefix, bottom_border));
        out
    }

    fn render_mermaid_fallback(&self, content: &str) -> String {
        let doc_indent = self.document_indent();
        let bq_prefix = self.blockquote_prefix();
        let border_style = Style {
            bold: false,
            italic: false,
            underline: false,
            strikethrough: false,
            fg: Some(Color::Code),
        };
        let border_ansi = border_style.to_ansi(self.theme);
        let reset = "\x1b[0m";
        let box_width = self.width.saturating_sub(self.blockquote_prefix_width() + doc_indent + 2);
        
        let mut out = String::new();
        let top_border = self.render_box_top("Mermaid Diagram", box_width, &border_ansi);
        out.push_str(&format!("{}{}{}\n", " ".repeat(doc_indent), bq_prefix, top_border));
        
        for line in content.lines() {
            let pad_width = box_width.saturating_sub(line.chars().count() + 3);
            out.push_str(&format!(
                "{}{}{}│{} {}{}{}│{}\n",
                " ".repeat(doc_indent), bq_prefix, border_ansi, reset, line, " ".repeat(pad_width), border_ansi, reset
            ));
        }
        
        let bottom_border = self.render_box_bottom(box_width, &border_ansi);
        out.push_str(&format!("{}{}{}\n", " ".repeat(doc_indent), bq_prefix, bottom_border));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mermaid_to_png() {
        let theme = Theme::new("terminal");
        let renderer = MarkdownRenderer::new(&theme, 80, ImageProtocol::Kitty);
        let content = "flowchart TD\n    A[Start] --> B{Is supporting terminal?}\n    B -->|Yes| C[Render high-res image via Kitty protocol]\n    B -->|No| D[Render standard ASCII box fallback]\n    C --> E[Done]\n    D --> E";
        let svg = mermaid_rs_renderer::render(content).unwrap();
        let (png, _) = renderer.svg_to_png(&svg).unwrap();
        let dims = MarkdownRenderer::get_png_dimensions(&png);
        assert!(dims.is_some());
        let (w, h) = dims.unwrap();
        assert!(w > 0 && h > 0);
        // Verify the PNG actually has rendered pixel content (not all-transparent)
        assert!(png.iter().filter(|&&b| b != 0).count() > 100, "PNG is transparent");
    }

    #[test]
    fn test_math_to_png() {
        let theme = Theme::new("terminal");
        let renderer = MarkdownRenderer::new(&theme, 80, ImageProtocol::Kitty);
        let svg = renderer.latex_to_svg("e^{i\\pi} + 1 = 0").unwrap();
        let (png, _) = renderer.svg_to_png(&svg).unwrap();
        let dims = MarkdownRenderer::get_png_dimensions(&png);
        assert!(dims.is_some());
        let (w, h) = dims.unwrap();
        assert!(w > 0 && h > 0);
        assert!(png.iter().filter(|&&b| b != 0).count() > 100, "Math PNG is transparent");
    }

    #[test]
    fn test_iterm2_image_escape() {
        let theme = Theme::new("terminal");
        let renderer = MarkdownRenderer::new(&theme, 80, ImageProtocol::ITerm2);
        // Minimal 1x1 PNG
        let png_bytes: Vec<u8> = vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a,
            0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
            0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01,
            0x08, 0x02, 0x00, 0x00, 0x00, 0x90, 0x77, 0x53,
            0xde, 0x00, 0x00, 0x00, 0x0c, 0x49, 0x44, 0x41,
            0x54, 0x08, 0xd7, 0x63, 0xf8, 0xcf, 0xc0, 0x00,
            0x00, 0x00, 0x02, 0x00, 0x01, 0xe2, 0x21, 0xbc,
            0x33, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e,
            0x44, 0xae, 0x42, 0x60, 0x82,
        ];
        let out = renderer.render_iterm2_image(&png_bytes, 10, 2);
        assert!(out.contains("\x1b]1337;"), "Missing OSC 1337 header");
        assert!(out.contains("inline=1"), "Missing inline=1");
        assert!(out.contains("width=10"), "Missing width");
        assert!(out.contains("height=2"), "Missing height");
        assert!(out.ends_with("\n\n"), "Missing trailing newlines");
    }
}
