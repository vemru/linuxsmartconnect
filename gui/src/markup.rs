//! Pango markup escaping for user-visible text.
//!
//! `AdwActionRow` titles/subtitles and `AdwPreferencesGroup` titles are parsed as Pango markup by
//! default. A bare `&`, `<` or `>` makes the parse fail, and GTK then leaves the label empty — the
//! `use_markup(false)` switch only re-sets the text after that failed parse, which still logs a
//! warning for every such row. Escape the text instead.

/// Escapes text so it renders literally in a markup-parsed label.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::escape;

    #[test]
    fn ampersands_and_angle_brackets_are_escaped() {
        assert_eq!(escape("Files & Storage"), "Files &amp; Storage");
        assert_eq!(escape("<tag> & more"), "&lt;tag&gt; &amp; more");
    }

    #[test]
    fn plain_text_is_unchanged() {
        assert_eq!(
            escape("Bluetooth adapter available"),
            "Bluetooth adapter available"
        );
        assert_eq!(escape(""), "");
    }
}
