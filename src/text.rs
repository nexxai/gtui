/// Convert basic HTML to plain text by stripping tags and decoding entities.
pub fn convert_html_to_plain_text(html: &str) -> String {
    // Replace block-level tags with newlines before stripping
    let with_breaks = html
        .replace("<br>", "\n")
        .replace("<br/>", "\n")
        .replace("<br />", "\n")
        .replace("</div>", "\n")
        .replace("</p>", "\n\n")
        .replace("</li>", "\n");

    // Strip remaining HTML tags
    let stripped = strip_tags(&with_breaks);

    // Decode common HTML entities
    let decoded = decode_entities(&stripped);

    // Collapse excessive blank lines
    collapse_blank_lines(&decoded)
}

fn strip_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;

    for c in html.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }

    out
}

fn decode_entities(text: &str) -> String {
    text.replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// Collapse runs of blank/whitespace-only lines down to at most one blank line,
/// and trim trailing whitespace from each line.
fn collapse_blank_lines(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut prev_was_blank = true; // start true to skip leading blanks

    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if !prev_was_blank && !result.is_empty() {
                result.push('\n');
                prev_was_blank = true;
            }
        } else {
            if !result.is_empty() && !prev_was_blank {
                result.push('\n');
            }
            result.push_str(trimmed);
            prev_was_blank = false;
        }
    }

    result
}
