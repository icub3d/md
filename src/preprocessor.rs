/// Preprocess markdown to convert LaTeX math blocks into structured code block tags
/// so pulldown-cmark can parse them without inline conflicts.
///
/// Converts:
/// - $$ ... $$ display math into ```math\n...\n```
/// - $ ... $ inline math into `math:...`
pub fn preprocess_markdown(input: &str) -> String {
    let mut result = String::new();
    let mut chars = input.chars().peekable();

    let mut in_fenced_code = false;
    let mut fence_char = ' ';
    let mut fence_count = 0;

    let mut in_inline_code = false;
    let mut inline_backtick_count = 0;

    while let Some(c) = chars.next() {
        // Handle code blocks (only at line start)
        if !in_inline_code
            && (c == '`' || c == '~') && (result.ends_with('\n') || result.is_empty())
        {
            let mut count = 1;
            while chars.peek() == Some(&c) {
                count += 1;
                chars.next();
            }
            if in_fenced_code {
                if c == fence_char && count >= fence_count {
                    in_fenced_code = false;
                }
            } else {
                in_fenced_code = true;
                fence_char = c;
                fence_count = count;
            }
            for _ in 0..count { result.push(c); }
            continue;
        }

        if in_fenced_code {
            result.push(c);
            continue;
        }

        // Handle inline code
        if c == '`' {
            let mut count = 1;
            while chars.peek() == Some(&'`') {
                count += 1;
                chars.next();
            }
            if in_inline_code {
                if count == inline_backtick_count {
                    in_inline_code = false;
                }
            } else {
                in_inline_code = true;
                inline_backtick_count = count;
            }
            result.push_str(&"`".repeat(count));
            continue;
        }

        if in_inline_code {
            result.push(c);
            continue;
        }

        // Escape check for \$
        if c == '\\' && let Some(&'$') = chars.peek() {
            result.push('\\');
            result.push('$');
            chars.next();
            continue;
        }

        // Handle Display Math: $$
        if c == '$' && chars.peek() == Some(&'$') {
            chars.next(); // consume second '$'
            let mut math_content = String::new();
            let mut found_close = false;
            while let Some(nc) = chars.next() {
                if nc == '$' && chars.peek() == Some(&'$') {
                    chars.next(); // consume second '$'
                    found_close = true;
                    break;
                }
                math_content.push(nc);
            }
            if found_close {
                let content = math_content.trim();
                result.push_str("\n```math\n");
                result.push_str(content);
                result.push_str("\n```\n");
            } else {
                result.push_str("$$");
                result.push_str(&math_content);
            }
            continue;
        }

        // Handle Inline Math: $
        if c == '$' {
            let mut math_content = String::new();
            let mut found_close = false;
            let mut consumed_newline = false;
            for nc in chars.by_ref() {
                if nc == '$' {
                    found_close = true;
                    break;
                }
                if nc == '\n' {
                    consumed_newline = true;
                    break;
                }
                math_content.push(nc);
            }
            if found_close {
                result.push_str("`math:");
                result.push_str(&math_content);
                result.push('`');
            } else {
                result.push('$');
                result.push_str(&math_content);
                if consumed_newline {
                    result.push('\n');
                }
            }
            continue;
        }

        result.push(c);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_preprocess_inline() {
        let input = "This is a formula: $e^{i\\pi} + 1 = 0$ inside text.";
        let output = preprocess_markdown(input);
        assert_eq!(output, "This is a formula: `math:e^{i\\pi} + 1 = 0` inside text.");
    }

    #[test]
    fn test_preprocess_display() {
        let input = "Formula:\n$$\nx^2 + y^2 = r^2\n$$\nEnd.";
        let output = preprocess_markdown(input);
        assert_eq!(output, "Formula:\n\n```math\nx^2 + y^2 = r^2\n```\n\nEnd.");
    }

    #[test]
    fn test_preprocess_ignore_code() {
        let input = "Code block:\n```\nSome $math$ in code.\n```";
        let output = preprocess_markdown(input);
        assert_eq!(output, input);
    }
}
