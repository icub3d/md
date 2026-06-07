#[derive(Clone, Debug, Default, PartialEq)]
pub struct Style {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub strikethrough: bool,
    pub fg: Option<Color>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Color {
    Heading(u32),
    Blockquote,
    Code,
    Link,
    Rule,
    List,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ThemeType {
    Terminal,
    Mocha,
    Latte,
}

pub struct Theme {
    pub theme_type: ThemeType,
}

impl Theme {
    pub fn new(name: &str) -> Self {
        let theme_type = match name.to_lowercase().as_str() {
            "mocha" => ThemeType::Mocha,
            "latte" => ThemeType::Latte,
            _ => ThemeType::Terminal,
        };
        Self { theme_type }
    }

    pub fn heading_color(&self, level: u32) -> Option<(u8, u8, u8)> {
        match self.theme_type {
            ThemeType::Terminal => None,
            ThemeType::Mocha => match level {
                1 => Some((243, 139, 168)), // Red
                2 => Some((250, 179, 135)), // Peach / Orange
                3 => Some((249, 226, 175)), // Yellow
                4 => Some((245, 194, 231)), // Pink
                5 => Some((203, 166, 247)), // Mauve
                _ => Some((137, 180, 250)), // Blue
            },
            ThemeType::Latte => match level {
                1 => Some((210, 15, 57)),   // Red
                2 => Some((254, 100, 11)),   // Peach / Orange
                3 => Some((223, 142, 29)),   // Yellow
                4 => Some((234, 118, 203)), // Pink
                5 => Some((136, 57, 239)),  // Mauve
                _ => Some((30, 102, 245)),  // Blue
            },
        }
    }

    pub fn blockquote_color(&self) -> Option<(u8, u8, u8)> {
        match self.theme_type {
            ThemeType::Terminal => None,
            ThemeType::Mocha => Some((245, 224, 220)), // Rosewater
            ThemeType::Latte => Some((220, 138, 120)), // Rosewater
        }
    }

    pub fn code_color(&self) -> Option<(u8, u8, u8)> {
        match self.theme_type {
            ThemeType::Terminal => None,
            ThemeType::Mocha => Some((250, 179, 135)), // Peach
            ThemeType::Latte => Some((254, 100, 11)),  // Peach
        }
    }

    pub fn link_color(&self) -> Option<(u8, u8, u8)> {
        match self.theme_type {
            ThemeType::Terminal => None,
            ThemeType::Mocha => Some((137, 180, 250)), // Blue
            ThemeType::Latte => Some((30, 102, 245)),  // Blue
        }
    }

    pub fn rule_color(&self) -> Option<(u8, u8, u8)> {
        match self.theme_type {
            ThemeType::Terminal => None,
            ThemeType::Mocha => Some((108, 112, 134)), // Overlay0 (Gray)
            ThemeType::Latte => Some((156, 160, 176)), // Overlay0 (Gray)
        }
    }

    pub fn list_color(&self) -> Option<(u8, u8, u8)> {
        match self.theme_type {
            ThemeType::Terminal => None,
            ThemeType::Mocha => Some((166, 227, 161)), // Green
            ThemeType::Latte => Some((64, 160, 43)),   // Green
        }
    }
}

impl Style {
    pub fn to_ansi(&self, theme: &Theme) -> String {
        let mut codes = Vec::new();
        if self.bold {
            codes.push("1".to_string());
        }
        if self.italic {
            codes.push("3".to_string());
        }
        if self.underline {
            codes.push("4".to_string());
        }
        if self.strikethrough {
            codes.push("9".to_string());
        }
        if let Some(fg) = &self.fg {
            match fg {
                Color::Heading(level) => {
                    if let Some(rgb) = theme.heading_color(*level) {
                        codes.push(format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2));
                    } else {
                        let ansi_color = match level {
                            1 => "31;1", // Red Bold
                            2 => "33;1", // Yellow/Peach/Orange Bold
                            3 => "93;1", // Bright Yellow Bold
                            4 => "35;1", // Magenta/Pink Bold
                            5 => "95;1", // Bright Magenta/Mauve Bold
                            _ => "34;1", // Blue Bold
                        };
                        codes.push(ansi_color.to_string());
                    }
                }
                Color::Blockquote => {
                    if let Some(rgb) = theme.blockquote_color() {
                        codes.push(format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2));
                    } else {
                        codes.push("37;3".to_string()); // Italic Gray
                    }
                }
                Color::Code => {
                    if let Some(rgb) = theme.code_color() {
                        codes.push(format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2));
                    } else {
                        codes.push("33".to_string()); // Yellow
                    }
                }
                Color::Link => {
                    if let Some(rgb) = theme.link_color() {
                        codes.push(format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2));
                    } else {
                        codes.push("34;4".to_string()); // Blue Underlined
                    }
                }
                Color::Rule => {
                    if let Some(rgb) = theme.rule_color() {
                        codes.push(format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2));
                    } else {
                        codes.push("90".to_string()); // Bright Black / Gray
                    }
                }
                Color::List => {
                    if let Some(rgb) = theme.list_color() {
                        codes.push(format!("38;2;{};{};{}", rgb.0, rgb.1, rgb.2));
                    } else {
                        codes.push("32;1".to_string()); // Green Bold
                    }
                }
            }
        }
        if codes.is_empty() {
            "".to_string()
        } else {
            format!("\x1b[{}m", codes.join(";"))
        }
    }
}
