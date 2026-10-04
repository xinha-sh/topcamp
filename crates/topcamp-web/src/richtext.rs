//! Message body pipeline: a pragmatic port of upstream
//! `topcamp-richtext` covering plain-text and basic-HTML bodies.
//!
//! `canonicalize_plain` stores composer textarea input as block HTML;
//! `present` renders a stored body as `message_presentation` does
//! (sanitize, then `auto_link`, in a `lexxy-content` wrapper).
//!
//! Deferred to later slices: mentions and embeds (UI-09), `/play`
//! sound commands (UI-06). Unknown elements
//! (e.g. `actiontext-attachment`) unwrap to their text, as Loofah
//! does; `script`-like and foreign elements drop with contents.
//! `all_emoji` approximates `\p{Extended_Pictographic}` with a range
//! table (no `regex` in the tree): the common emoji set matches
//! exactly, exotic edge cases may differ.

/// `ERB::Util.html_escape`: escapes `& < > " '`.
pub fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Active Support `blank?`: empty or only whitespace.
fn is_blank(text: &str) -> bool {
    text.chars().all(char::is_whitespace)
}

/// Ruby `strip` whitespace: NUL, tab, LF, VT, FF, CR, space.
fn strip(text: &str) -> &str {
    text.trim_matches(['\0', '\t', '\n', '\u{0b}', '\u{0c}', '\r', ' '])
}

/// Decode the entities a stored body may carry, so the sanitizer
/// re-escapes them exactly once. Unknown entities pass through.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&i| i <= 12) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let entity = &rest[1..semi];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" | "#39" | "#x27" | "#X27" => Some('\''),
            _ if let Some(num) = entity.strip_prefix('#') => {
                let code = if let Some(hex) = num.strip_prefix(['x', 'X']) {
                    u32::from_str_radix(hex, 16).ok()
                } else {
                    num.parse::<u32>().ok()
                };
                code.and_then(char::from_u32)
            }
            _ => None,
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// `DEFAULT_ALLOWED_TAGS` + `EDITOR_FORMATTING_TAGS`: the tags the
/// final presentation keeps (`SafeList::auto_link`).
const ALLOWED_TAGS: &[&str] = &[
    "a",
    "abbr",
    "acronym",
    "address",
    "b",
    "big",
    "blockquote",
    "br",
    "cite",
    "code",
    "dd",
    "del",
    "dfn",
    "div",
    "dl",
    "dt",
    "em",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "hr",
    "i",
    "img",
    "ins",
    "kbd",
    "li",
    "mark",
    "ol",
    "p",
    "pre",
    "s",
    "samp",
    "small",
    "span",
    "strong",
    "sub",
    "sup",
    "table",
    "tbody",
    "td",
    "tfoot",
    "th",
    "thead",
    "time",
    "tr",
    "tt",
    "u",
    "ul",
    "var",
];

/// `DEFAULT_ALLOWED_ATTRIBUTES` (minus `name`) + `data-language`.
const ALLOWED_ATTRIBUTES: &[&str] = &[
    "abbr",
    "alt",
    "cite",
    "class",
    "data-language",
    "datetime",
    "height",
    "href",
    "lang",
    "src",
    "title",
    "width",
    "xml:lang",
];

/// Elements dropped with their contents (`script`-likes, frames,
/// forms stays out; foreign elements).
const DROP_WITH_CONTENTS: &[&str] = &[
    "applet", "base", "basefont", "embed", "frame", "frameset", "head", "html", "iframe", "link",
    "math", "meta", "noscript", "object", "script", "style", "svg", "template",
];

const VOID_TAGS: &[&str] = &[
    "area", "br", "col", "hr", "img", "input", "source", "track", "wbr",
];

/// Loofah's URI check, as upstream configures it: relative URLs and
/// these schemes survive on `href`/`src`.
const ALLOWED_SCHEMES: &[&str] = &["http", "https", "mailto"];

/// Whether a URL value survives sanitization: relative, or an
/// allowlisted scheme. Control characters and surrounding whitespace
/// are stripped before the scheme check.
fn url_allowed(value: &str) -> bool {
    let clean: String = value.chars().filter(|c| !c.is_control()).collect();
    let clean = clean.trim();
    let Some(colon) = clean.find(':') else {
        return true;
    };
    let before = &clean[..colon];
    if before.contains(['/', '#', '?', '@']) {
        return true;
    }
    let scheme = before.to_ascii_lowercase();
    ALLOWED_SCHEMES.contains(&scheme.as_str())
}

/// Find the `>` ending a tag starting at `text`, honoring quotes.
/// Returns the byte index just past `>`, or `None` when unterminated.
fn tag_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut quote = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
        } else if b == b'"' || b == b'\'' {
            quote = Some(b);
        } else if b == b'>' {
            return Some(i + 1);
        }
        i += 1;
    }
    None
}

/// Parse one attribute (`name="value"`, `name='value'`, `name=value`,
/// bare `name`) at the head of `text`; returns the attribute and the
/// byte length consumed, or `None` when no attribute starts there.
fn parse_attribute(text: &str) -> Option<((String, Option<String>), usize)> {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i < bytes.len() && (bytes[i].is_ascii_whitespace() || bytes[i] == b'/') {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'=' {
        i += 1;
    }
    if start == i {
        return None;
    }
    let name = text[start..i].to_ascii_lowercase();
    while i < bytes.len() && bytes[i].is_ascii_whitespace() {
        i += 1;
    }
    if i < bytes.len() && bytes[i] == b'=' {
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i < bytes.len() && (bytes[i] == b'"' || bytes[i] == b'\'') {
            let quote = bytes[i];
            i += 1;
            let value_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let value = text[value_start..i].to_string();
            i = (i + 1).min(bytes.len());
            return Some(((name, Some(value)), i));
        }
        let value_start = i;
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        return Some(((name, Some(text[value_start..i].to_string())), i));
    }
    Some(((name, None), i))
}

/// Sanitize `html`: keep allowlisted tags/attributes, unwrap the
/// rest, drop `script`-likes with contents, escape text.
pub fn sanitize(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&escape(&decode_entities(&rest[..lt])));
        rest = &rest[lt..];
        let Some(end) = tag_end(rest) else {
            out.push_str(&escape(&decode_entities(rest)));
            return out;
        };
        let tag = &rest[..end];
        rest = &rest[end..];
        let inner = &tag[1..tag.len() - 1];
        if inner.starts_with("!--") {
            continue;
        }
        if inner.starts_with(['!', '?']) {
            continue;
        }
        let closing = inner.starts_with('/');
        let body = if closing { &inner[1..] } else { inner };
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        if name.is_empty() || !name.bytes().next().is_some_and(|b| b.is_ascii_alphabetic()) {
            out.push_str(&escape(&decode_entities(tag)));
            continue;
        }
        if DROP_WITH_CONTENTS.contains(&name.as_str()) {
            if !closing && !VOID_TAGS.contains(&name.as_str()) {
                rest = skip_element(rest, &name);
            }
            continue;
        }
        if !ALLOWED_TAGS.contains(&name.as_str()) {
            continue;
        }
        if VOID_TAGS.contains(&name.as_str()) {
            if closing {
                continue;
            }
            out.push_str(&open_tag(&name, &body[name_end..]));
            continue;
        }
        if closing {
            out.push_str(&format!("</{name}>"));
        } else {
            out.push_str(&open_tag(&name, &body[name_end..]));
        }
    }
    out.push_str(&escape(&decode_entities(rest)));
    out
}

/// Render an opening tag with its allowlisted attributes. Boolean
/// attributes render bare (`<video controls>`); values re-escape.
fn open_tag(name: &str, attrs: &str) -> String {
    let mut out = format!("<{name}");
    let mut rest = attrs;
    while let Some(((key, value), used)) = parse_attribute(rest) {
        rest = &rest[used..];
        if !ALLOWED_ATTRIBUTES.contains(&key.as_str()) {
            continue;
        }
        match value {
            None => {
                out.push(' ');
                out.push_str(&key);
            }
            Some(value) => {
                let value = decode_entities(&value);
                if (key == "href" || key == "src") && !url_allowed(&value) {
                    continue;
                }
                out.push(' ');
                out.push_str(&key);
                out.push_str("=\"");
                out.push_str(&escape(&value));
                out.push('"');
            }
        }
    }
    out.push('>');
    out
}

/// Skip `rest` past the matching close tag for an already-consumed
/// `<name>`, honoring nesting of the same element.
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let mut depth = 1;
    let lower = rest.to_ascii_lowercase();
    let mut offset = 0;
    while depth > 0 {
        let Some(lt) = lower[offset..].find('<') else {
            return "";
        };
        let abs = offset + lt;
        let Some(end) = tag_end(&rest[abs..]) else {
            return "";
        };
        let tag = lower[abs..abs + end].to_string();
        let inner = tag[1..tag.len() - 1].trim_start_matches('/').to_string();
        let tag_name = inner
            .split([' ', '/', '\t', '\n', '\r'])
            .next()
            .unwrap_or("");
        if tag_name == name {
            if tag.starts_with("</") {
                depth -= 1;
            } else if !VOID_TAGS.contains(&name) {
                depth += 1;
            }
        }
        offset = abs + end;
        if depth == 0 {
            return &rest[offset..];
        }
    }
    rest
}

/// Block elements: boundaries become newlines in plain text.
fn is_block(name: &str) -> bool {
    matches!(
        name,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "dd"
            | "div"
            | "dl"
            | "dt"
            | "figcaption"
            | "figure"
            | "footer"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "ol"
            | "p"
            | "pre"
            | "table"
            | "td"
            | "th"
            | "tr"
            | "ul"
    )
}

/// `body.to_plain_text`: tags stripped, blocks separated by
/// newlines, entities decoded, ends stripped.
pub fn plain_text(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(lt) = rest.find('<') {
        out.push_str(&decode_entities(&rest[..lt]));
        rest = &rest[lt..];
        let Some(end) = tag_end(rest) else {
            out.push_str(&decode_entities(rest));
            break;
        };
        let tag = &rest[..end];
        rest = &rest[end..];
        let inner = &tag[1..tag.len() - 1];
        if inner.starts_with("!--") || inner.starts_with(['!', '?']) {
            continue;
        }
        let body = inner.trim_start_matches('/');
        let name_end = body
            .find(|c: char| c.is_whitespace() || c == '/')
            .unwrap_or(body.len());
        let name = body[..name_end].to_ascii_lowercase();
        if DROP_WITH_CONTENTS.contains(&name.as_str()) && !inner.starts_with('/') {
            rest = skip_element(rest, &name);
            continue;
        }
        if (name == "br" || is_block(&name)) && !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
    }
    if rest.find('<').is_none() {
        out.push_str(&decode_entities(rest));
    }
    strip(&out).to_string()
}

/// Store textarea input as block HTML: one `<div>` per line, `<br>`
/// for blank lines. Blank input stores as the empty string.
pub fn canonicalize_plain(text: &str) -> String {
    if is_blank(text) {
        return String::new();
    }
    strip(text)
        .split('\n')
        .map(|line| {
            let line = line.strip_suffix('\r').unwrap_or(line);
            if is_blank(line) {
                "<div><br></div>".to_string()
            } else {
                format!("<div>{}</div>", escape(line.trim()))
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Unwrap a lone top-level `<div>`: `<div>x</div>` presents as `x`,
/// while several blocks keep their wrappers.
fn unwrap_single_div(html: &str) -> &str {
    let trimmed = strip(html);
    if !trimmed.starts_with("<div>") || !trimmed.ends_with("</div>") {
        return html;
    }
    let inner = &trimmed["<div>".len()..trimmed.len() - "</div>".len()];
    if inner.to_ascii_lowercase().contains("<div") {
        return html;
    }
    inner
}

/// The text branch of `message_presentation`: sanitize, `auto_link`,
/// wrapped in the `lexxy-content` layout. Blank bodies present as
/// the empty string.
pub fn present(stored: &str) -> String {
    if is_blank(&plain_text(stored)) {
        return String::new();
    }
    let sanitized = sanitize(stored);
    let inner = unwrap_single_div(&sanitized);
    let linked = auto_link(inner);
    format!("<div class=\"lexxy-content\">\n  {linked}\n</div>\n")
}

// --- auto_link ---------------------------------------------------------------

/// `AUTO_LINK_RE`'s schemes, each requiring `://` after it.
const LINK_SCHEMES: &[&str] = &[
    "ed2k", "ftp", "http", "https", "irc", "mailto", "news", "gopher", "nntp", "telnet", "webcal",
    "xmpp", "callto", "feed", "svn", "urn", "aim", "rsync", "tag", "ssh", "sftp", "rtsp", "afs",
    "file",
];

/// Characters ending a link match (`[^ \t\r\n\v\f<\u{A0}\"]`).
fn is_link_char(c: char) -> bool {
    !matches!(
        c,
        ' ' | '\t' | '\r' | '\n' | '\u{0b}' | '\u{0c}' | '<' | '\u{a0}' | '"'
    )
}

/// Ruby `\w` is Unicode word characters; Rust's alphanumeric plus
/// `_` covers what trailing punctuation can follow in practice.
fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Byte ranges a link match must not start inside: every `<...>`
/// tag, plus the text of every `<a>...</a>` element.
fn protected_ranges(html: &str) -> Vec<(usize, usize)> {
    let mut ranges = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        let Some(lt) = lower[i..].find('<') else {
            break;
        };
        let abs = i + lt;
        let Some(end) = tag_end(&html[abs..]) else {
            break;
        };
        ranges.push((abs, abs + end));
        let tag = &lower[abs..abs + end];
        if tag.starts_with("<a")
            && tag[2..].starts_with([' ', '\t', '\n', '\r', '\u{0c}', '/', '>'])
        {
            match lower[abs + end..].find("</a>") {
                Some(close) => ranges.push((abs + end, abs + end + close)),
                None => ranges.push((abs + end, html.len())),
            }
        }
        i = abs + end;
    }
    ranges
}

fn is_protected(ranges: &[(usize, usize)], start: usize) -> bool {
    ranges.iter().any(|&(s, e)| start >= s && start < e)
}

fn opening_bracket(closing: char) -> Option<char> {
    match closing {
        ']' => Some('['),
        ')' => Some('('),
        '}' => Some('{'),
        _ => None,
    }
}

/// Split a raw link match into its link and trailing punctuation,
/// following `auto_link_urls` (word characters plus `/`, `-`, `=`,
/// `;` survive; unbalanced closers stay punctuated).
fn split_trailing(raw: &str) -> (&str, &str) {
    let mut end = raw.len();
    let mut counts = [0usize; 6];
    const BRACKETS: [char; 6] = ['[', ']', '(', ')', '{', '}'];
    for c in raw.chars() {
        if let Some(i) = BRACKETS.iter().position(|&b| b == c) {
            counts[i] += 1;
        }
    }
    let count = |counts: &[usize; 6], bracket: char| {
        BRACKETS
            .iter()
            .position(|&b| b == bracket)
            .map_or(0, |i| counts[i])
    };
    while let Some(c) = raw[..end].chars().last() {
        if is_word_char(c) || matches!(c, '/' | '-' | '=' | ';') {
            break;
        }
        end -= c.len_utf8();
        if let Some(i) = BRACKETS.iter().position(|&b| b == c) {
            counts[i] = counts[i].saturating_sub(1);
        }
        if let Some(opening) = opening_bracket(c)
            && count(&counts, opening) > count(&counts, c)
        {
            end += c.len_utf8();
            break;
        }
    }
    let mut link = &raw[..end];
    let mut punctuation = &raw[end..];
    if let Some(stripped) = link.strip_suffix("&gt;") {
        link = stripped;
        punctuation = &raw[link.len()..];
    }
    (link, punctuation)
}

/// Find a URL match starting exactly at `start`: `scheme://` or
/// `www.` + word char. Returns the byte end of the raw match.
fn url_match_at(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let scheme_end = rest.find("://").filter(|&i| {
        i > 0
            && rest[..i].chars().all(|c| c.is_ascii_alphabetic())
            && LINK_SCHEMES.contains(&rest[..i].to_ascii_lowercase().as_str())
    });
    let www = rest
        .strip_prefix("www.")
        .filter(|r| {
            r.chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(|_| "www.");
    if scheme_end.is_none() && www.is_none() {
        return None;
    }
    let mut end = start;
    for (i, c) in rest.char_indices() {
        if !is_link_char(c) {
            break;
        }
        end = start + i + c.len_utf8();
    }
    (end > start).then_some(end)
}

/// Whether `c` may start or continue an email local part check.
fn is_email_local_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_.!#$%&'*/=?^`{|}~+-".contains(c)
}

fn is_email_first_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_.!#$%+-".contains(c)
}

fn is_email_rest_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "_.!#$%&'*/=?^`{|}~+-".contains(c)
}

fn is_domain_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.'
}

/// Match `AUTO_EMAIL_RE` at `start`: local part, `@`, dotted
/// domain with at least one dot and a non-empty final label.
fn email_match_at(text: &str, start: usize) -> Option<usize> {
    let rest = &text[start..];
    let mut chars = rest.char_indices();
    let (_, first) = chars.next()?;
    if !is_email_first_char(first) {
        return None;
    }
    let mut i = first.len_utf8();
    let bytes = rest.as_bytes();
    if bytes.get(i) == Some(&b'.') {
        i += 1;
    }
    while i < bytes.len() && is_email_rest_char(bytes[i] as char) {
        i += 1;
    }
    if bytes.get(i) != Some(&b'@') {
        return None;
    }
    i += 1;
    let domain_start = i;
    while i < bytes.len() && is_domain_char(bytes[i] as char) {
        i += 1;
    }
    let domain = &rest[domain_start..i];
    let mut labels = domain.split('.');
    let first_label = labels.next().unwrap_or("");
    if first_label.is_empty() {
        return None;
    }
    let mut dots = 0;
    for label in labels {
        if label.is_empty() {
            i -= 1;
            break;
        }
        dots += 1;
        let _ = label;
    }
    if dots == 0 {
        return None;
    }
    while rest[..i].ends_with('.') {
        i -= 1;
    }
    Some(start + i)
}

/// `ERB::Util.url_encode`: percent-encode everything but unreserved.
fn url_encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Link URLs and email addresses in sanitized `html`, skipping
/// matches inside tags and existing anchors.
pub fn auto_link(html: &str) -> String {
    let ranges = protected_ranges(html);
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    let mut offset = 0;
    while !rest.is_empty() {
        let mut matched = None;
        if let Some(end) = url_match_at(rest, 0)
            && !is_protected(&ranges, offset)
        {
            matched = Some((end, true));
        } else if (offset == 0
            || !html[..offset]
                .chars()
                .last()
                .is_some_and(is_email_local_char))
            && let Some(end) = email_match_at(rest, 0)
            && !is_protected(&ranges, offset)
        {
            matched = Some((end, false));
        }
        match matched {
            Some((end, true)) => {
                let (link, punctuation) = split_trailing(&rest[..end]);
                let mut href = link.to_string();
                if !link.contains("://") {
                    href = format!("http://{link}");
                }
                out.push_str(&format!(
                    "<a target=\"_blank\" href=\"{}\">{link}</a>{}",
                    href.replace('"', "&quot;"),
                    escape(punctuation)
                ));
                offset += end;
                rest = &rest[end..];
            }
            Some((end, false)) => {
                let email = &rest[..end];
                let href = format!("mailto:{}", url_encode(email).replace("%40", "@"));
                out.push_str(&format!(
                    "<a target=\"_blank\" href=\"{}\">{}</a>",
                    escape(&href),
                    escape(email)
                ));
                offset += end;
                rest = &rest[end..];
            }
            None => {
                let c = rest.chars().next().unwrap_or_default();
                out.push(c);
                offset += c.len_utf8();
                rest = &rest[c.len_utf8()..];
            }
        }
    }
    out
}

// --- all_emoji -----------------------------------------------------------------

/// `Extended_Pictographic` ranges (plus `FE00-FE0F`, which covers
/// `FE0F`): every `Emoji_Presentation` character is pictographic,
/// so one table answers both. Approximates upstream's regex where
/// Unicode splits blocks (arrows, geometric shapes, dingbats).
const PICTOGRAPHIC_RANGES: &[(u32, u32)] = &[
    (0x00A9, 0x00A9),
    (0x00AE, 0x00AE),
    (0x203C, 0x203C),
    (0x2049, 0x2049),
    (0x2122, 0x2122),
    (0x2139, 0x2139),
    (0x2190, 0x21FF),
    (0x2300, 0x23FF),
    (0x2460, 0x24FF),
    (0x25AA, 0x25AB),
    (0x25B6, 0x25B6),
    (0x25C0, 0x25C0),
    (0x25FB, 0x25FE),
    (0x2600, 0x27BF),
    (0x2934, 0x2935),
    (0x2B00, 0x2BFF),
    (0x3030, 0x3030),
    (0x303D, 0x303D),
    (0x3297, 0x3297),
    (0x3299, 0x3299),
    (0x1F000, 0x1FAFF),
    (0xFE00, 0xFE0F),
    (0x200D, 0x200D),
    (0x20E3, 0x20E3),
    (0xE0020, 0xE007F),
];

/// Whether every character is emoji (`all_emoji?`): empty is false.
pub fn all_emoji(text: &str) -> bool {
    !text.is_empty()
        && text.chars().all(|c| {
            let code = c as u32;
            PICTOGRAPHIC_RANGES
                .iter()
                .any(|&(lo, hi)| code >= lo && code <= hi)
        })
}

// --- attachments -------------------------------------------------------------

/// `Message::THUMBNAIL_MAX_WIDTH` / `THUMBNAIL_MAX_HEIGHT`.
const THUMBNAIL_MAX_WIDTH: f64 = 1200.0;
const THUMBNAIL_MAX_HEIGHT: f64 = 800.0;

/// `variable_content_types` (bmp, ico and psd removed by
/// `config/initializers/vips.rb`): these render lightboxed.
pub const VARIABLE_CONTENT_TYPES: &[&str] = &[
    "image/png",
    "image/gif",
    "image/jpeg",
    "image/tiff",
    "image/webp",
    "image/avif",
    "image/heic",
    "image/heif",
];

/// `config.active_storage.serve_as_binary_content_types`: served as
/// `application/octet-stream` (XSS hardening on serve).
const SERVE_AS_BINARY: &[&str] = &[
    "text/html",
    "image/svg+xml",
    "application/postscript",
    "application/x-shockwave-flash",
    "text/xml",
    "application/xml",
    "application/xhtml+xml",
    "application/mathml+xml",
    "text/cache-manifest",
];

/// `blob.content_type_for_serving`: binary-forced types serve as
/// octet-stream, everything else as stored.
pub fn content_type_for_serving(content_type: &str) -> &str {
    if SERVE_AS_BINARY.contains(&content_type) {
        "application/octet-stream"
    } else {
        content_type
    }
}

/// `variable?`: a lightboxed image preview.
pub fn is_variable(content_type: &str) -> bool {
    VARIABLE_CONTENT_TYPES.contains(&content_type)
}

/// A Ruby Integer-or-Float dimension (metadata values load as either).
#[derive(Clone, Copy)]
pub enum RubyNumber {
    Int(i64),
    Float(f64),
}

impl RubyNumber {
    fn to_f(self) -> f64 {
        match self {
            RubyNumber::Int(value) => value as f64,
            RubyNumber::Float(value) => value,
        }
    }

    /// `number / 2`: integer division for integers.
    fn half(self) -> RubyNumber {
        match self {
            RubyNumber::Int(value) => RubyNumber::Int(value.div_euclid(2)),
            RubyNumber::Float(value) => RubyNumber::Float(value / 2.0),
        }
    }
}

impl std::fmt::Display for RubyNumber {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RubyNumber::Int(value) => write!(f, "{value}"),
            RubyNumber::Float(value) => f.write_str(&ruby_float(*value)),
        }
    }
}

/// `Float#to_s`.
fn ruby_float(value: f64) -> String {
    if value.is_nan() {
        return "NaN".into();
    }
    if value.is_infinite() {
        return if value > 0.0 {
            "Infinity".into()
        } else {
            "-Infinity".into()
        };
    }
    let magnitude = value.abs();
    if magnitude != 0.0 && !(1e-4..1e16).contains(&magnitude) {
        // Rust: "1.5e16", Ruby: "1.5e+16"; Rust: "1e16", Ruby: "1.0e+16".
        let formatted = format!("{value:e}");
        let (mantissa, exponent) = formatted.split_once('e').unwrap();
        let mantissa = if mantissa.contains('.') {
            mantissa.to_string()
        } else {
            format!("{mantissa}.0")
        };
        let exponent: i32 = exponent.parse().unwrap();
        let sign = if exponent < 0 { '-' } else { '+' };
        return format!("{mantissa}e{sign}{:02}", exponent.abs());
    }
    let formatted = format!("{value}");
    if formatted.contains('.') {
        formatted
    } else {
        format!("{formatted}.0")
    }
}

/// What `Messages::AttachmentPresentation` renders for one blob.
pub struct AttachmentRef<'a> {
    pub filename: &'a str,
    pub content_type: &'a str,
    /// Inline (view) URL.
    pub blob_path: &'a str,
    /// `?disposition=attachment` URL.
    pub download_path: &'a str,
    /// Preview image URL (worker thumb once §22 records one, else the
    /// blob itself); video poster likewise.
    pub thumb_url: &'a str,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Asset URLs for the file-link row icons.
    pub file_icon: &'a str,
    pub download_icon: &'a str,
    pub share_icon: &'a str,
}

/// `Messages::AttachmentPresentation#render`: video plays inline,
/// variable images lightbox, everything else is a file link. Audio
/// has no player upstream — it renders as a file link too.
pub fn attachment_html(att: &AttachmentRef<'_>) -> String {
    if att.content_type.starts_with("video/") {
        video_preview(att)
    } else if is_variable(att.content_type) {
        lightboxed_image_preview(att)
    } else {
        file_link(att)
    }
}

fn video_preview(att: &AttachmentRef<'_>) -> String {
    let video = format!(
        r#"<video src="{}" poster="{}" controls="controls" preload="none" width="100%" height="100%" class="message__attachment"></video>"#,
        escape(att.blob_path),
        escape(att.thumb_url)
    );
    inline_media_dimension_constraints(preview_dimensions(att), &video)
}

fn lightboxed_image_preview(att: &AttachmentRef<'_>) -> String {
    let dimensions = preview_dimensions(att);
    let size = match dimensions {
        Some((width, height)) => format!(r#" width="{width}" height="{height}""#),
        None => String::new(),
    };
    let image = format!(
        r#"<img{size} class="message__attachment" loading="lazy" src="{}" />"#,
        escape(att.thumb_url)
    );
    let link = format!(
        r#"<a class="flex" href="{}">{image}</a>"#,
        escape(att.blob_path)
    );
    inline_media_dimension_constraints(dimensions, &link)
}

fn inline_media_dimension_constraints(
    dimensions: Option<(RubyNumber, RubyNumber)>,
    content: &str,
) -> String {
    match dimensions {
        Some((width, height)) => {
            let aspect_ratio = RubyNumber::Float(width.to_f() / height.to_f());
            format!(
                r#"<div class="max-inline-size center flex overflow-clip" style="width: {}px; aspect-ratio: {aspect_ratio};">{content}</div>"#,
                width.half()
            )
        }
        None => format!(r#"<div class="max-inline-size center overflow-clip">{content}</div>"#),
    }
}

/// `preview_dimensions`: the metadata size, scaled down to fit the thumbnail bounds.
fn preview_dimensions(att: &AttachmentRef<'_>) -> Option<(RubyNumber, RubyNumber)> {
    let (width, height) = (att.width?, att.height?);
    let (width, height) = (width as f64, height as f64);
    if width <= THUMBNAIL_MAX_WIDTH && height <= THUMBNAIL_MAX_HEIGHT {
        Some((
            RubyNumber::Int(width as i64),
            RubyNumber::Int(height as i64),
        ))
    } else {
        let width_factor = THUMBNAIL_MAX_WIDTH / width;
        let height_factor = THUMBNAIL_MAX_HEIGHT / height;
        let scale = width_factor.min(height_factor);
        Some((
            RubyNumber::Float(width * scale),
            RubyNumber::Float(height * scale),
        ))
    }
}

/// `render_link`: file icon, name, download link and share button, with no whitespace between.
fn file_link(att: &AttachmentRef<'_>) -> String {
    format!(
        concat!(
            r#"<div class="message__file-link flex-inline flex--align-center gap-double">"#,
            r#"<img class="colorize--black" aria-hidden="true" src="{}" width="22" height="22" />"#,
            r#"<span>{}</span>"#,
            r#"<a class="btn message__action-btn hide-in-ios-pwa" style="--width: auto;" href="{}">"#,
            r#"<img aria-hidden="true" src="{}" width="20" height="20" />"#,
            r#"<span class="for-screen-reader">Download {}</span></a>"#,
            r#"</div>"#
        ),
        escape(att.file_icon),
        escape(att.filename),
        escape(att.download_path),
        escape(att.download_icon),
        escape(att.filename),
    )
}

// --- mentions and embeds --------------------------------------------------------

/// A resolved mention target: live identity for `span.mention`.
#[derive(Clone)]
pub struct MentionUser {
    pub id: i64,
    pub name: String,
    pub title: String,
    pub avatar_url: String,
}

const MENTION_CONTENT_TYPE: &str = "application/vnd.topcamp.mention";
const EMBED_CONTENT_TYPE: &str = "application/vnd.actiontext.opengraph-embed";

#[derive(Clone, Copy, PartialEq, Eq)]
enum RichNode {
    Mention,
    Embed,
}

struct FoundNode {
    start: usize,
    end: usize,
    kind: RichNode,
    attrs: Vec<(String, Option<String>)>,
    inner: String,
}

fn node_attr(node: &FoundNode, name: &str) -> Option<String> {
    node.attrs
        .iter()
        .find(|(key, _)| key == name)
        .and_then(|(_, value)| value.clone())
}

/// Attributes of one opening tag (names lowercased).
fn tag_attributes(tag: &str) -> Vec<(String, Option<String>)> {
    let inner = tag.strip_prefix('<').unwrap_or(tag);
    let name_end = inner
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(inner.len());
    let mut attrs = Vec::new();
    let mut rest = &inner[name_end..];
    while let Some(((name, value), used)) = parse_attribute(rest) {
        if used == 0 {
            break;
        }
        attrs.push((name, value));
        rest = &rest[used..];
    }
    attrs
}

/// Byte span + inner HTML of the element opened at `start` (which is
/// at its `<`), honoring same-name nesting.
fn element_span(html: &str, start: usize, name: &str) -> Option<(usize, String)> {
    let open_end = tag_end(&html[start..])?;
    let lower = html.to_ascii_lowercase();
    let mut depth = 1;
    let mut offset = start + open_end;
    let mut close_start = offset;
    while depth > 0 {
        let next = lower[offset..].find('<')?;
        let abs = offset + next;
        let end = tag_end(&html[abs..])?;
        let tag = &lower[abs..abs + end];
        let tag_inner = &tag[1..tag.len() - 1];
        let closing = tag_inner.starts_with('/');
        let tag_body = if closing { &tag_inner[1..] } else { tag_inner };
        let tag_name_end = tag_body
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(tag_body.len());
        if tag_body[..tag_name_end] == *name {
            if closing {
                depth -= 1;
                if depth == 0 {
                    close_start = abs;
                }
            } else if !tag_inner.ends_with('/') {
                depth += 1;
            }
        }
        offset = abs + end;
    }
    let inner_start = start + open_end;
    Some((offset, html[inner_start..close_start].to_string()))
}

/// Mention + embed nodes in document order (top-level scan; nested
/// rich nodes inside a mention caption are not re-entered).
fn rich_nodes(html: &str) -> Vec<FoundNode> {
    let mut nodes = Vec::new();
    let lower = html.to_ascii_lowercase();
    let mut i = 0;
    while i < html.len() {
        let Some(lt) = lower[i..].find('<') else {
            break;
        };
        let abs = i + lt;
        let Some(end) = tag_end(&html[abs..]) else {
            break;
        };
        let tag = &html[abs..abs + end];
        let tag_inner = &tag[1..tag.len() - 1];
        if tag_inner.starts_with('/') || tag_inner.starts_with('!') || tag_inner.starts_with('?') {
            i = abs + end;
            continue;
        }
        let name_end = tag_inner
            .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
            .unwrap_or(tag_inner.len());
        let name = tag_inner[..name_end].to_ascii_lowercase();
        let kind = if name == "span"
            || name == "action-text-attachment"
            || name == "actiontext-opengraph-embed"
        {
            let attrs = tag_attributes(tag);
            let content_type = attrs
                .iter()
                .find(|(key, _)| key == "content-type")
                .and_then(|(_, value)| value.clone())
                .unwrap_or_default();
            let has_mention_class = name == "span"
                && attrs.iter().any(|(key, value)| {
                    key == "class"
                        && value
                            .as_deref()
                            .unwrap_or("")
                            .split_whitespace()
                            .any(|token| token == "mention")
                });
            if content_type.contains(EMBED_CONTENT_TYPE) || name == "actiontext-opengraph-embed" {
                Some(RichNode::Embed)
            } else if content_type.contains(MENTION_CONTENT_TYPE) || has_mention_class {
                Some(RichNode::Mention)
            } else {
                None
            }
        } else {
            None
        };
        if let Some(kind) = kind
            && let Some((close, inner)) = element_span(html, abs, &name)
        {
            nodes.push(FoundNode {
                start: abs,
                end: close,
                kind,
                attrs: tag_attributes(tag),
                inner,
            });
            i = close;
        } else {
            i = abs + end;
        }
    }
    nodes
}

/// Verified user ids referenced by mention nodes (deduped, in order).
pub fn mentioned_user_ids(html: &str) -> Vec<i64> {
    let mut ids = Vec::new();
    for node in rich_nodes(html) {
        if node.kind != RichNode::Mention {
            continue;
        }
        if let Some(sgid) = node_attr(&node, "sgid")
            && let Some(id) = crate::users::verify_mention_sgid(&sgid)
            && !ids.contains(&id)
        {
            ids.push(id);
        }
    }
    ids
}

/// `truncateString` (embed titles/descriptions): UTF-16 units like
/// JS `string.length`, omission `"…"`.
fn truncate_ellipsis(text: &str, length: usize) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    if units.len() <= length {
        return text.to_string();
    }
    let mut keep = length.saturating_sub(1);
    // Never split a surrogate pair (JS `slice` would keep the lone
    // half, but a replacement char reads worse than one less char).
    while keep > 0 && (0xd800..0xdc00).contains(&units[keep - 1]) {
        keep -= 1;
    }
    format!("{}\u{2026}", String::from_utf16_lossy(&units[..keep]))
}

fn attr_first(node: &FoundNode, names: &[&str]) -> String {
    names
        .iter()
        .filter_map(|name| node_attr(node, name))
        .find(|value| !value.trim().is_empty())
        .unwrap_or_default()
}

/// `users/_mention` with live identity.
fn render_mention(sgid: &str, user: &MentionUser) -> String {
    format!(
        "<span class=\"mention\" sgid=\"{}\"><a title=\"{}\" class=\"btn avatar\" href=\"/users/{}\"><img aria-hidden=\"true\" width=\"48\" height=\"48\" src=\"{}\" /></a> {}</span>",
        escape(sgid),
        escape(&user.title),
        user.id,
        escape(&user.avatar_url),
        escape(&user.name),
    )
}

/// First `href`/`src` in the node's inner HTML (bare embed
/// elements carry rendered markup instead of attributes).
fn inner_link(node: &FoundNode, tag: &str, attr: &str) -> String {
    let lower = node.inner.to_ascii_lowercase();
    let mut i = 0;
    while i < node.inner.len() {
        let needle = format!("<{tag}");
        let Some(found) = lower[i..].match_indices(&needle).find_map(|(at, _)| {
            node.inner[i + at + needle.len()..]
                .chars()
                .next()
                .filter(|c| c.is_whitespace() || *c == '>' || *c == '/')
                .map(|_| i + at)
        }) else {
            break;
        };
        let abs = found;
        let Some(end) = tag_end(&node.inner[abs..]) else {
            break;
        };
        for (name, value) in tag_attributes(&node.inner[abs..abs + end]) {
            if name == attr
                && let Some(value) = value.filter(|text| !text.trim().is_empty())
            {
                return value;
            }
        }
        i = abs + end;
    }
    String::new()
}

/// `action_text/attachables/_opengraph_embed` from node attributes
/// (bare elements harvest the same fields from their inner HTML).
fn render_embed(node: &FoundNode) -> String {
    let mut page = attr_first(node, &["href", "link", "url"]);
    let mut title = attr_first(node, &["filename", "title"]);
    let mut description = attr_first(node, &["description"]);
    let mut image = attr_first(node, &["image"]);
    if page.is_empty() && title.is_empty() && description.is_empty() && image.is_empty() {
        page = inner_link(node, "a", "href");
        image = inner_link(node, "img", "src");
        // Title = first link's text, description = whatever follows it.
        let lower = node.inner.to_ascii_lowercase();
        let link_at = lower.match_indices("<a").find_map(|(at, _)| {
            node.inner[at + 2..]
                .chars()
                .next()
                .filter(|c| c.is_whitespace() || *c == '>' || *c == '/')
                .map(|_| at)
        });
        if let Some(lt) = link_at
            && let Some((end, inner)) = element_span(&node.inner, lt, "a")
        {
            title = plain_text(&inner).trim().to_string();
            description = plain_text(&node.inner[end..]).trim().to_string();
        }
    }
    if image.is_empty() && !attr_first(node, &["href", "link"]).is_empty() {
        // Partial-shaped node: `url` names the image then.
        image = attr_first(node, &["url"]);
    }
    let title = truncate_ellipsis(&title, 280);
    let description = truncate_ellipsis(&description, 560);
    // Upstream `#isTwitterAvatar`: the image lives under the
    // profile-images prefix (an explicit node flag wins too).
    let twitter = node_attr(node, "twitter-avatar").as_deref() == Some("true")
        || node_attr(node, "twitter_avatar").as_deref() == Some("true")
        || (!image.is_empty() && image.starts_with("https://pbs.twimg.com/profile_images"));
    let title_html = if page.is_empty() {
        escape(&title)
    } else {
        format!(
            "<a href=\"{}\" rel=\"noreferrer\" target=\"_blank\">{}</a>",
            escape(&page),
            escape(&title)
        )
    };
    let image_html = if image.is_empty() {
        String::new()
    } else {
        format!(
            "<div class=\"og-embed__image\"><img src=\"{}\" class=\"image center\" alt=\"\" /></div>",
            escape(&image)
        )
    };
    format!(
        "<figure class=\"attachment attachment--content attachment--og\"><actiontext-opengraph-embed><div class=\"og-embed gap{}\"><div class=\"og-embed__content\"><div class=\"og-embed__title\">{}</div><div class=\"og-embed__description\">{}</div></div>{}</div></actiontext-opengraph-embed></figure>",
        if twitter {
            " og-embed--twitter-avatar"
        } else {
            ""
        },
        title_html,
        escape(&description),
        image_html,
    )
}

/// Normalize a URL the way `RemoveSoloUnfurledLinkText` does: map
/// x.com → twitter.com, drop the query.
fn normalize_tweet_url(url: &str) -> String {
    let trimmed = url.trim();
    let no_query = trimmed.split('?').next().unwrap_or(trimmed);
    if let Some(rest) = no_query.strip_prefix("http://")
        && let Some(host_end) = rest.find('/')
    {
        let (host, path) = rest.split_at(host_end);
        if host.eq_ignore_ascii_case("x.com") {
            return format!("http://twitter.com{path}");
        }
    }
    if let Some(rest) = no_query.strip_prefix("https://")
        && let Some(host_end) = rest.find('/')
    {
        let (host, path) = rest.split_at(host_end);
        if host.eq_ignore_ascii_case("x.com") {
            return format!("https://twitter.com{path}");
        }
    }
    no_query.to_string()
}

/// `present` with live mentions + embed cards. Mention nodes resolve
/// against `mentions` (verified sgids only — anything else unwraps
/// to plain text so stored markup can never impersonate); embed
/// attachments render as cards. A lone embed whose URL is the whole
/// body drops the link text, like upstream.
pub fn present_rich(
    stored: &str,
    mentions: &std::collections::HashMap<i64, MentionUser>,
) -> String {
    let nodes = rich_nodes(stored);
    if nodes.is_empty() {
        return present(stored);
    }
    // Text outside the rich nodes (for the solo-unfurl rule).
    let mut outside = String::new();
    let mut cursor = 0;
    for node in &nodes {
        outside.push_str(&plain_text(&stored[cursor..node.start]));
        cursor = node.end;
    }
    outside.push_str(&plain_text(&stored[cursor..]));
    let solo_embed_href = if nodes.len() == 1 && nodes[0].kind == RichNode::Embed {
        let href = attr_first(&nodes[0], &["href", "link", "url"]);
        (!href.is_empty()
            && (outside.trim().is_empty()
                || normalize_tweet_url(&outside) == normalize_tweet_url(&href)))
        .then_some(href)
    } else {
        None
    };
    let mut staged = String::with_capacity(stored.len());
    let mut replacements: Vec<(String, String)> = Vec::new();
    let mut cursor = 0;
    for (index, node) in nodes.iter().enumerate() {
        if solo_embed_href.is_none() {
            staged.push_str(&stored[cursor..node.start]);
        }
        let token = format!("\u{e000}rich:{index}\u{e001}");
        let replacement = match node.kind {
            RichNode::Mention => node_attr(node, "sgid")
                .and_then(|sgid| {
                    crate::users::verify_mention_sgid(&sgid)
                        .and_then(|id| mentions.get(&id).map(|user| render_mention(&sgid, user)))
                })
                .unwrap_or_else(|| escape(&plain_text(&node.inner))),
            RichNode::Embed => render_embed(node),
        };
        replacements.push((token.clone(), replacement));
        staged.push_str(&token);
        cursor = node.end;
    }
    if solo_embed_href.is_none() {
        staged.push_str(&stored[cursor..]);
    }
    // The tokens are non-blank text, so `present` never blanks here.
    let mut rendered = present(&staged);
    for (token, html) in replacements {
        rendered = rendered.replace(&token, &html);
    }
    rendered
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_covers_five_chars() {
        assert_eq!(escape("&<>\"'"), "&amp;&lt;&gt;&quot;&#39;");
    }

    #[test]
    fn canonicalize_wraps_lines() {
        assert_eq!(canonicalize_plain(""), "");
        assert_eq!(canonicalize_plain("  \n "), "");
        assert_eq!(canonicalize_plain("hi"), "<div>hi</div>");
        assert_eq!(
            canonicalize_plain("a\n\nb"),
            "<div>a</div><div><br></div><div>b</div>"
        );
        assert_eq!(
            canonicalize_plain("<b>bold</b> & free"),
            "<div>&lt;b&gt;bold&lt;/b&gt; &amp; free</div>"
        );
    }

    #[test]
    fn present_matches_golden_shape() {
        assert_eq!(present(""), "");
        assert_eq!(present("<div>   </div>"), "");
        assert_eq!(
            present("<div>Pizza party in HQ</div>"),
            "<div class=\"lexxy-content\">\n  Pizza party in HQ\n</div>\n"
        );
    }

    #[test]
    fn sanitize_unwraps_unknown_drops_script() {
        assert_eq!(
            sanitize("<div>a<foo>bar</foo><script>evil()</script>c</div>"),
            "<div>abarc</div>"
        );
        assert_eq!(
            sanitize("<a href=\"javascript:alert(1)\">x</a>"),
            "<a>x</a>"
        );
        assert_eq!(
            sanitize("<a href=\"https://x.test/?a=1&amp;b=2\" title=\"t\">x</a>"),
            "<a href=\"https://x.test/?a=1&amp;b=2\" title=\"t\">x</a>"
        );
        assert_eq!(sanitize("<img src=x onerror=alert(1)>"), "<img src=\"x\">");
        assert_eq!(sanitize("a < b"), "a &lt; b");
    }

    #[test]
    fn plain_text_blocks_newline() {
        assert_eq!(plain_text("<div>a</div><div>b<br>c</div>"), "a\nb\nc");
        assert_eq!(plain_text("<b>x</b> &amp; y"), "x & y");
        assert_eq!(plain_text("<script>no</script>yes"), "yes");
    }

    #[test]
    fn autolink_urls_and_emails() {
        assert_eq!(
            auto_link("see https://x.test/a(b)."),
            "see <a target=\"_blank\" href=\"https://x.test/a(b)\">https://x.test/a(b)</a>."
        );
        assert_eq!(
            auto_link("go to www.x.test/a, ok?"),
            "go to <a target=\"_blank\" href=\"http://www.x.test/a\">www.x.test/a</a>, ok?"
        );
        assert_eq!(
            auto_link("mail a+b@x.test!"),
            "mail <a target=\"_blank\" href=\"mailto:a%2Bb@x.test\">a+b@x.test</a>!"
        );
        assert_eq!(
            auto_link("<a href=\"https://x.test\">https://x.test</a>"),
            "<a href=\"https://x.test\">https://x.test</a>"
        );
    }

    #[test]
    fn emoji_matches_ruby() {
        assert!(all_emoji("👍"));
        assert!(all_emoji("❤️"));
        assert!(all_emoji("🎉🔥"));
        assert!(!all_emoji("hi 👍"));
        assert!(!all_emoji(""));
    }

    fn attachment(content_type: &'static str) -> AttachmentRef<'static> {
        AttachmentRef {
            filename: "photo.png",
            content_type,
            blob_path: "/rooms/1/messages/2/attachment",
            download_path: "/rooms/1/messages/2/attachment?disposition=attachment",
            thumb_url: "/rooms/1/messages/2/attachment",
            width: Some(2400),
            height: Some(1600),
            file_icon: "/assets/common-file-text.svg",
            download_icon: "/assets/download.svg",
            share_icon: "/assets/share.svg",
        }
    }

    #[test]
    fn image_previews_link_without_lightbox_hooks() {
        let html = attachment_html(&attachment("image/png"));
        assert!(
            html.contains(r#"<a class="flex" href="/rooms/1/messages/2/attachment">"#),
            "{html}"
        );
        assert!(!html.contains("data-lightbox-target"), "{html}");
        assert!(!html.contains("lightbox#open"), "{html}");
        // 2400x1600 scales to 1200x800; width/2 for the wrapper.
        assert!(html.contains(r#"width="1200.0" height="800.0""#), "{html}");
        assert!(
            html.contains(r#"style="width: 600.0px; aspect-ratio: 1.5;""#),
            "{html}"
        );
    }

    #[test]
    fn small_image_keeps_integer_dimensions() {
        let mut att = attachment("image/jpeg");
        att.width = Some(100);
        att.height = Some(50);
        let html = attachment_html(&att);
        assert!(html.contains(r#"width="100" height="50""#), "{html}");
        assert!(
            html.contains(r#"style="width: 50px; aspect-ratio: 2.0;""#),
            "{html}"
        );
    }

    #[test]
    fn video_plays_inline_with_poster() {
        let html = attachment_html(&attachment("video/mp4"));
        assert!(html.contains(r#"<video src=""#), "{html}");
        assert!(
            html.contains(r#"controls="controls" preload="none""#),
            "{html}"
        );
    }

    #[test]
    fn audio_and_files_render_a_file_link() {
        for content_type in [
            "audio/mpeg",
            "application/pdf",
            "image/svg+xml",
            "text/plain",
        ] {
            let html = attachment_html(&attachment(content_type));
            assert!(
                html.contains("message__file-link"),
                "{content_type}: {html}"
            );
            assert!(
                html.contains("?disposition=attachment"),
                "{content_type}: {html}"
            );
            assert!(!html.contains("web-share"), "{content_type}: {html}");
            assert!(!html.contains("data-controller="), "{content_type}: {html}");
        }
    }

    #[test]
    fn serving_forces_binary_for_active_content() {
        assert_eq!(
            content_type_for_serving("image/svg+xml"),
            "application/octet-stream"
        );
        assert_eq!(
            content_type_for_serving("text/html"),
            "application/octet-stream"
        );
        assert_eq!(content_type_for_serving("image/png"), "image/png");
    }

    fn mention_map() -> std::collections::HashMap<i64, MentionUser> {
        let _ = crate::users::init_secret(b"test-secret".to_vec());
        std::collections::HashMap::from([(
            7,
            MentionUser {
                id: 7,
                name: "Moe <x>".to_string(),
                title: "Moe".to_string(),
                avatar_url: "/users/t/avatar?v=1".to_string(),
            },
        )])
    }

    #[test]
    fn mention_span_renders_live_identity() {
        let map = mention_map();
        let sgid = crate::users::mention_sgid(7);
        let html = present_rich(
            &format!("<div>hi <span class=\"mention\" sgid=\"{sgid}\">stale</span></div>"),
            &map,
        );
        assert!(html.contains("<span class=\"mention\""), "{html}");
        assert!(html.contains("Moe &lt;x&gt;"), "{html}");
        assert!(!html.contains("stale"), "{html}");
    }

    #[test]
    fn forged_mention_unwraps_to_text() {
        let map = mention_map();
        let html = present_rich(
            "<div>hi <span class=\"mention\" sgid=\"User/7.forged\"><a href=\"/evil\">Evil</a></span></div>",
            &map,
        );
        assert!(!html.contains("class=\"mention\""), "{html}");
        assert!(!html.contains("/evil"), "{html}");
        assert!(html.contains("Evil"), "{html}");
    }

    #[test]
    fn mentioned_user_ids_verifies_sgids() {
        let _ = crate::users::init_secret(b"test-secret".to_vec());
        let sgid = crate::users::mention_sgid(9);
        let html = format!(
            "<span class=\"mention\" sgid=\"{sgid}\">x</span><span class=\"mention\" sgid=\"User/9.nope\">y</span>"
        );
        assert_eq!(mentioned_user_ids(&html), vec![9]);
    }

    #[test]
    fn embed_attachment_renders_card() {
        let map = mention_map();
        let html = present_rich(
            "<div><action-text-attachment content-type=\"application/vnd.actiontext.opengraph-embed\" href=\"https://example.com/a\" filename=\"A title\" description=\"Some words\"></action-text-attachment></div>",
            &map,
        );
        assert!(html.contains("attachment--og"), "{html}");
        assert!(html.contains("A title"), "{html}");
        assert!(html.contains("rel=\"noreferrer\""), "{html}");
    }

    #[test]
    fn solo_unfurl_drops_link_text() {
        let map = mention_map();
        let html = present_rich(
            "<div>https://example.com/a<action-text-attachment content-type=\"application/vnd.actiontext.opengraph-embed\" href=\"https://example.com/a\" filename=\"A\" description=\"d\"></action-text-attachment></div>",
            &map,
        );
        assert!(html.contains("attachment--og"), "{html}");
        assert!(!html.contains(">https://example.com/a<"), "{html}");
    }

    #[test]
    fn truncate_ellipsis_counts_chars() {
        assert_eq!(truncate_ellipsis("abcd", 10), "abcd");
        assert_eq!(truncate_ellipsis("abcdef", 5), "abcd\u{2026}");
    }
}
