//! Untrusted text from public sites, reduced to bounded plain text.

use url::Url;

/// Longest title, name or label kept from a site, in characters.
pub(crate) const SHORT_TEXT: usize = 200;
/// Longest summary kept from a site, in characters.
pub(crate) const SUMMARY_TEXT: usize = 1000;
/// Longest refusal reason kept from a site, in characters.
pub(crate) const REASON_TEXT: usize = 200;
/// Longest web link kept from a site, in bytes.
const MAXIMUM_URL_BYTES: usize = 2048;
/// Longest file extension kept when a file name is shortened, in characters.
const MAXIMUM_EXTENSION_CHARACTERS: usize = 16;

/// Collapse whitespace, drop control, zero-width and text-direction
/// characters, which can hide text from a human reader, and bound the length.
pub(crate) fn bounded(value: &str, limit: usize) -> String {
    let mut text = String::new();
    let mut space = false;
    let mut count = 0;
    for character in value.chars() {
        if invisible(character) {
            continue;
        }
        if character.is_whitespace() || character.is_control() {
            space = !text.is_empty();
            continue;
        }
        if count == limit {
            text.push('…');
            break;
        }
        if space {
            text.push(' ');
            count += 1;
            space = false;
            if count == limit {
                text.push('…');
                break;
            }
        }
        text.push(character);
        count += 1;
    }
    text
}

/// Invisible formatting characters: soft hyphen, direction marks and
/// overrides, zero-width characters, interlinear annotation and tag characters.
fn invisible(character: char) -> bool {
    matches!(
        character,
        '\u{00AD}'
            | '\u{034F}'
            | '\u{061C}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{E0000}'..='\u{E007F}'
    )
}

/// A file name bounded like [`bounded`] that keeps its extension when shortened.
pub(crate) fn file_name(value: &str) -> String {
    let name = bounded(value, usize::MAX);
    if name.chars().count() <= SHORT_TEXT {
        return name;
    }
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty() && extension.chars().count() <= MAXIMUM_EXTENSION_CHARACTERS
        })
        .unwrap_or_default();
    let keep = SHORT_TEXT - extension.chars().count() - 2;
    let stem: String = name.chars().take(keep).collect();
    if extension.is_empty() {
        format!("{stem}…")
    } else {
        format!("{stem}….{extension}")
    }
}

/// A URL slug of ASCII letters, digits and hyphens, or nothing.
pub(crate) fn slug(value: Option<&str>) -> Option<&str> {
    value.filter(|slug| {
        !slug.is_empty()
            && slug.len() <= SHORT_TEXT
            && slug
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    })
}

/// A bounded HTTPS link without credentials, or nothing.
pub(crate) fn web_url(value: Option<&str>) -> Option<String> {
    let value = value?;
    let url = Url::parse(value).ok()?;
    (value.len() <= MAXIMUM_URL_BYTES
        && url.scheme() == "https"
        && url.username().is_empty()
        && url.password().is_none()
        && url.host_str().is_some())
    .then(|| url.into())
}

/// [`bounded`] for an optional value, treating blank text as absent.
pub(crate) fn optional(value: Option<&str>, limit: usize) -> Option<String> {
    value
        .map(|value| bounded(value, limit))
        .filter(|value| !value.is_empty())
}

/// Reduce an HTML fragment to bounded plain text. Script and style contents
/// are dropped, and the common character references are decoded. Only the
/// first `limit * 8` bytes are read, which bounds the cost of the scan.
pub(crate) fn from_html(value: &str, limit: usize) -> String {
    let mut end = value.len().min(limit.saturating_mul(8));
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    let value = &value[..end];
    let mut text = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('<') {
        text.push_str(&rest[..start]);
        text.push(' ');
        let tag = &rest[start + 1..];
        let Some(end) = tag.find('>') else {
            rest = "";
            break;
        };
        let name = tag[..end]
            .trim_start()
            .split(|c: char| c.is_whitespace() || c == '/')
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        rest = &tag[end + 1..];
        if matches!(name.as_str(), "script" | "style") {
            let closing = format!("</{name}");
            rest = match rest.to_ascii_lowercase().find(&closing) {
                Some(position) => rest[position..]
                    .find('>')
                    .map_or("", |close| &rest[position + close + 1..]),
                None => "",
            };
        }
    }
    text.push_str(rest);
    bounded(&decode_references(&text), limit)
}

fn decode_references(value: &str) -> String {
    let mut decoded = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        decoded.push_str(&rest[..start]);
        let candidate = &rest[start..];
        let Some(end) = candidate.find(';').filter(|end| *end <= 10) else {
            decoded.push('&');
            rest = &candidate[1..];
            continue;
        };
        let reference = &candidate[1..end];
        let character = match reference {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => reference
                .strip_prefix("#x")
                .or_else(|| reference.strip_prefix("#X"))
                .and_then(|hex| u32::from_str_radix(hex, 16).ok())
                .or_else(|| reference.strip_prefix('#')?.parse().ok())
                .and_then(char::from_u32),
        };
        match character {
            Some(character) => {
                decoded.push(character);
                rest = &candidate[end + 1..];
            }
            None => {
                decoded.push('&');
                rest = &candidate[1..];
            }
        }
    }
    decoded.push_str(rest);
    decoded
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_text_collapses_whitespace_and_marks_truncation() {
        assert_eq!(bounded("  a\n\tb\u{7}c  ", 10), "a b c");
        assert_eq!(bounded("abcdef", 3), "abc…");
        assert_eq!(bounded("ab cd", 3), "ab …");
        assert_eq!(bounded("abc", 3), "abc");
        assert_eq!(optional(Some(" \n "), 5), None);
        assert_eq!(bounded("safe\u{202E}lmth.exe\u{200B}", 20), "safelmth.exe");
        assert_eq!(bounded("a\u{E0041}\u{E0042}b\u{00AD}c", 20), "abc");
    }

    #[test]
    fn long_file_names_keep_their_extension() {
        let shortened = file_name(&format!("{}.stl", "a".repeat(300)));
        assert_eq!(shortened.chars().count(), SHORT_TEXT);
        assert!(shortened.ends_with("a….stl"), "{shortened}");
        assert_eq!(file_name("Tray.STL"), "Tray.STL");
        assert!(file_name(&"b".repeat(300)).ends_with("b…"));
    }

    #[test]
    fn slugs_and_links_must_be_plain_and_bounded() {
        assert_eq!(slug(Some("low-poly-bowl")), Some("low-poly-bowl"));
        for bad in ["", "a b", "a/b", "line\nbreak"] {
            assert_eq!(slug(Some(bad)), None, "{bad}");
        }
        assert_eq!(slug(Some(&"a".repeat(201))), None);
        assert_eq!(
            web_url(Some("https://media.printables.com/a.png")).as_deref(),
            Some("https://media.printables.com/a.png")
        );
        for bad in [
            "http://media.printables.com/a.png",
            "javascript:alert(1)",
            "https://user:pass@example.com/a.png",
            "not a link",
        ] {
            assert_eq!(web_url(Some(bad)), None, "{bad}");
        }
        assert_eq!(
            web_url(Some(&format!("https://example.com/{}", "a".repeat(2048)))),
            None
        );
    }

    #[test]
    fn html_is_reduced_to_plain_text_without_scripts() {
        let html = "<p>Print&nbsp;at <b>0.2&nbsp;mm</b> &amp; 15%</p><script>alert('x')</script><style>p{}</style>Done &#8212; &#x2713; &bogus; a & b";
        assert_eq!(
            from_html(html, 200),
            "Print at 0.2 mm & 15% Done — ✓ &bogus; a & b"
        );
        assert_eq!(from_html("<p>unterminated <b", 50), "unterminated");
        assert_eq!(from_html("x<SCRIPT >y</Script >z", 50), "x z");
    }
}
