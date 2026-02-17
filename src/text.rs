pub fn convert_html_to_plain_text(html: &str) -> String {
    let mut text = html.to_string();

    // Replace line-breaking tags with newlines
    text = text.replace("<br>", "\n");
    text = text.replace("<br/>", "\n");
    text = text.replace("<br />", "\n");
    text = text.replace("</div>", "\n");
    text = text.replace("</p>", "\n\n");
    text = text.replace("</li>", "\n");

    // Strip all other tags
    let mut stripped = String::new();
    let mut in_tag = false;
    for c in text.chars() {
        if c == '<' {
            in_tag = true;
        } else if c == '>' {
            in_tag = false;
        } else if !in_tag {
            stripped.push(c);
        }
    }

    // Decode common HTML entities
    let decoded = stripped
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");

    // Clean up whitespace: collapse multiple newlines and trim
    let mut final_text = String::new();
    let mut last_was_newline = false;

    for line in decoded.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !last_was_newline && !final_text.is_empty() {
                final_text.push('\n');
                last_was_newline = true;
            }
        } else {
            if last_was_newline && !final_text.is_empty() {
                // final_text.push('\n'); // already pushed one above
            }
            final_text.push_str(trimmed);
            final_text.push('\n');
            last_was_newline = false;
        }
    }

    final_text.trim().to_string()
}
