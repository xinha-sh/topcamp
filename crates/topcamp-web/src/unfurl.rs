//! `UnfurlLinksController` + `Opengraph::*`: `POST /unfurl_link`
//! fetches a URL's OpenGraph tags (`title`/`url`/`image`/
//! `description`) as JSON, or answers 204.
//!
//! This mirrors upstream `crates/topcamp/src/integrations/opengraph/*`
//! (itself a port of the Rails reference), and follows the golden
//! fixtures in upstream `testdata/opengraph_cases.json` /
//! `opengraph_expected.json` case by case. Two byte-level details are
//! deliberately not reproduced: the JSON key order (upstream emits
//! first-assigned order; we emit alphabetical) and Rails' `\uXXXX`
//! string escapes. The composer parses the JSON, so only the parsed
//! values are user-visible, and those match.
//!
//! Approximations, all without golden coverage: tag-name parsing in
//! `strip_tags` is HTML5-flavoured rather than html5ever exactly;
//! entities without `;` never decode there (only `;`-terminated ones
//! do, like the meta scan); and URL hier-part characters follow the
//! WHATWG parser (`url`) rather than RFC 3986, so a vanishingly rare
//! advisory URL may fetch here where the reference 204s.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::time::Duration;

use tokio::sync::Semaphore;
use topcoat::{
    Result,
    context::Cx,
    router::{
        Body,
        content::Json,
        error::{forbidden, see_other},
        response::{IntoResponse, Response},
        route, to_bytes,
    },
};

/// Documents are capped at 5MB, like upstream's `MAX_BODY_SIZE`.
const MAX_BODY_SIZE: usize = 5 * 1024 * 1024;
/// Up to 10 responses per fetch: 9 redirects at most, like upstream.
const MAX_REDIRECTS: usize = 10;
/// One unfurl's whole budget, redirects and image check included.
const UNFURL_DEADLINE: Duration = Duration::from_secs(10);
/// Unfurls in flight at once; more wait their turn, within deadline.
const MAX_CONCURRENT_UNFURLS: usize = 16;
/// `Net::HTTP`'s default user agent, like upstream sends.
const USER_AGENT: &str = "Ruby";
/// The `Accept-Encoding` `Net::HTTP` sends; gzip/deflate inflate on read.
const ACCEPT_ENCODING: &str = "gzip;q=1.0,deflate;q=0.6,identity;q=0.3";
/// Documents must be exactly this (params stripped, case kept).
const ALLOWED_DOCUMENT_CONTENT_TYPE: &str = "text/html";
/// Images survive only with one of these HEAD content types.
const ALLOWED_IMAGE_CONTENT_TYPES: [&str; 4] =
    ["image/jpeg", "image/png", "image/gif", "image/webp"];
/// Hosts whose pages are read through fxtwitter instead.
const TWITTER_HOSTS: [&str; 4] = ["twitter.com", "www.twitter.com", "x.com", "www.x.com"];

/// Where the reference raises (a 500): a `mailto:` URL whose `to`
/// part fails `URI::MailTo` validation, or a tweet whose fxtwitter
/// page can't be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Raised(&'static str);

/// `Opengraph::Metadata::ATTRIBUTES`, sliced from each page.
const ATTRIBUTES: [&str; 4] = ["title", "url", "image", "description"];

/// libxml2's `html40EntitiesTable`, via upstream `integrations/opengraph/entities.rs`:
/// the HTML 4.01 named character references plus `apos` (case-sensitive, need `;`).
pub const ENTITIES: &[(&str, u32)] = &[
    ("AElig", 198),
    ("Aacute", 193),
    ("Acirc", 194),
    ("Agrave", 192),
    ("Alpha", 913),
    ("Aring", 197),
    ("Atilde", 195),
    ("Auml", 196),
    ("Beta", 914),
    ("Ccedil", 199),
    ("Chi", 935),
    ("Dagger", 8225),
    ("Delta", 916),
    ("ETH", 208),
    ("Eacute", 201),
    ("Ecirc", 202),
    ("Egrave", 200),
    ("Epsilon", 917),
    ("Eta", 919),
    ("Euml", 203),
    ("Gamma", 915),
    ("Iacute", 205),
    ("Icirc", 206),
    ("Igrave", 204),
    ("Iota", 921),
    ("Iuml", 207),
    ("Kappa", 922),
    ("Lambda", 923),
    ("Mu", 924),
    ("Ntilde", 209),
    ("Nu", 925),
    ("OElig", 338),
    ("Oacute", 211),
    ("Ocirc", 212),
    ("Ograve", 210),
    ("Omega", 937),
    ("Omicron", 927),
    ("Oslash", 216),
    ("Otilde", 213),
    ("Ouml", 214),
    ("Phi", 934),
    ("Pi", 928),
    ("Prime", 8243),
    ("Psi", 936),
    ("Rho", 929),
    ("Scaron", 352),
    ("Sigma", 931),
    ("THORN", 222),
    ("Tau", 932),
    ("Theta", 920),
    ("Uacute", 218),
    ("Ucirc", 219),
    ("Ugrave", 217),
    ("Upsilon", 933),
    ("Uuml", 220),
    ("Xi", 926),
    ("Yacute", 221),
    ("Yuml", 376),
    ("Zeta", 918),
    ("aacute", 225),
    ("acirc", 226),
    ("acute", 180),
    ("aelig", 230),
    ("agrave", 224),
    ("alefsym", 8501),
    ("alpha", 945),
    ("amp", 38),
    ("and", 8743),
    ("ang", 8736),
    ("apos", 39),
    ("aring", 229),
    ("asymp", 8776),
    ("atilde", 227),
    ("auml", 228),
    ("bdquo", 8222),
    ("beta", 946),
    ("brvbar", 166),
    ("bull", 8226),
    ("cap", 8745),
    ("ccedil", 231),
    ("cedil", 184),
    ("cent", 162),
    ("chi", 967),
    ("circ", 710),
    ("clubs", 9827),
    ("cong", 8773),
    ("copy", 169),
    ("crarr", 8629),
    ("cup", 8746),
    ("curren", 164),
    ("dArr", 8659),
    ("dagger", 8224),
    ("darr", 8595),
    ("deg", 176),
    ("delta", 948),
    ("diams", 9830),
    ("divide", 247),
    ("eacute", 233),
    ("ecirc", 234),
    ("egrave", 232),
    ("empty", 8709),
    ("emsp", 8195),
    ("ensp", 8194),
    ("epsilon", 949),
    ("equiv", 8801),
    ("eta", 951),
    ("eth", 240),
    ("euml", 235),
    ("euro", 8364),
    ("exist", 8707),
    ("fnof", 402),
    ("forall", 8704),
    ("frac12", 189),
    ("frac14", 188),
    ("frac34", 190),
    ("frasl", 8260),
    ("gamma", 947),
    ("ge", 8805),
    ("gt", 62),
    ("hArr", 8660),
    ("harr", 8596),
    ("hearts", 9829),
    ("hellip", 8230),
    ("iacute", 237),
    ("icirc", 238),
    ("iexcl", 161),
    ("igrave", 236),
    ("image", 8465),
    ("infin", 8734),
    ("int", 8747),
    ("iota", 953),
    ("iquest", 191),
    ("isin", 8712),
    ("iuml", 239),
    ("kappa", 954),
    ("lArr", 8656),
    ("lambda", 955),
    ("lang", 9001),
    ("laquo", 171),
    ("larr", 8592),
    ("lceil", 8968),
    ("ldquo", 8220),
    ("le", 8804),
    ("lfloor", 8970),
    ("lowast", 8727),
    ("loz", 9674),
    ("lrm", 8206),
    ("lsaquo", 8249),
    ("lsquo", 8216),
    ("lt", 60),
    ("macr", 175),
    ("mdash", 8212),
    ("micro", 181),
    ("middot", 183),
    ("minus", 8722),
    ("mu", 956),
    ("nabla", 8711),
    ("nbsp", 160),
    ("ndash", 8211),
    ("ne", 8800),
    ("ni", 8715),
    ("not", 172),
    ("notin", 8713),
    ("nsub", 8836),
    ("ntilde", 241),
    ("nu", 957),
    ("oacute", 243),
    ("ocirc", 244),
    ("oelig", 339),
    ("ograve", 242),
    ("oline", 8254),
    ("omega", 969),
    ("omicron", 959),
    ("oplus", 8853),
    ("or", 8744),
    ("ordf", 170),
    ("ordm", 186),
    ("oslash", 248),
    ("otilde", 245),
    ("otimes", 8855),
    ("ouml", 246),
    ("para", 182),
    ("part", 8706),
    ("permil", 8240),
    ("perp", 8869),
    ("phi", 966),
    ("pi", 960),
    ("piv", 982),
    ("plusmn", 177),
    ("pound", 163),
    ("prime", 8242),
    ("prod", 8719),
    ("prop", 8733),
    ("psi", 968),
    ("quot", 34),
    ("rArr", 8658),
    ("radic", 8730),
    ("rang", 9002),
    ("raquo", 187),
    ("rarr", 8594),
    ("rceil", 8969),
    ("rdquo", 8221),
    ("real", 8476),
    ("reg", 174),
    ("rfloor", 8971),
    ("rho", 961),
    ("rlm", 8207),
    ("rsaquo", 8250),
    ("rsquo", 8217),
    ("sbquo", 8218),
    ("scaron", 353),
    ("sdot", 8901),
    ("sect", 167),
    ("shy", 173),
    ("sigma", 963),
    ("sigmaf", 962),
    ("sim", 8764),
    ("spades", 9824),
    ("sub", 8834),
    ("sube", 8838),
    ("sum", 8721),
    ("sup", 8835),
    ("sup1", 185),
    ("sup2", 178),
    ("sup3", 179),
    ("supe", 8839),
    ("szlig", 223),
    ("tau", 964),
    ("there4", 8756),
    ("theta", 952),
    ("thetasym", 977),
    ("thinsp", 8201),
    ("thorn", 254),
    ("tilde", 732),
    ("times", 215),
    ("trade", 8482),
    ("uArr", 8657),
    ("uacute", 250),
    ("uarr", 8593),
    ("ucirc", 251),
    ("ugrave", 249),
    ("uml", 168),
    ("upsih", 978),
    ("upsilon", 965),
    ("uuml", 252),
    ("weierp", 8472),
    ("xi", 958),
    ("yacute", 253),
    ("yen", 165),
    ("yuml", 255),
    ("zeta", 950),
    ("zwj", 8205),
    ("zwnj", 8204),
];

/// `htmlParseCharRef` result: the decoded char (if it is a valid
/// XML character) and the position past the reference.
fn char_ref_at(bytes: &[u8], mut pos: usize) -> (Option<char>, usize) {
    debug_assert_eq!(bytes[pos], b'&');
    debug_assert_eq!(bytes[pos + 1], b'#');
    let hex = matches!(bytes.get(pos + 2), Some(b'x' | b'X'));
    pos += if hex { 3 } else { 2 };
    let radix = if hex { 16 } else { 10 };
    let mut value: u32 = 0;
    while let Some(c) = bytes.get(pos) {
        if *c == b';' {
            pos += 1;
            break;
        }
        let Some(digit) = char::from(*c).to_digit(radix) else {
            break;
        };
        if value < 0x110000 {
            value = value * radix + digit;
        }
        pos += 1;
    }
    let is_char =
        matches!(value, 0x9 | 0xA | 0xD | 0x20..=0xD7FF | 0xE000..=0xFFFD | 0x10000..=0x10FFFF);
    (is_char.then(|| char::from_u32(value)).flatten(), pos)
}

/// `htmlParseEntityRef`: a known name followed by `;` decodes;
/// anything else stays as written. Returns the text and the new pos.
fn entity_ref_at(bytes: &[u8], mut pos: usize) -> (String, usize) {
    debug_assert_eq!(bytes[pos], b'&');
    pos += 1;
    let start = pos;
    if bytes
        .get(pos)
        .is_some_and(|c| c.is_ascii_alphabetic() || matches!(c, b'_' | b':'))
    {
        while bytes
            .get(pos)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b':' | b'.' | b'-'))
        {
            pos += 1;
        }
    }
    let name = &bytes[start..pos];
    let name = std::str::from_utf8(name).unwrap_or("");
    if !name.is_empty()
        && bytes.get(pos) == Some(&b';')
        && let Ok(index) = ENTITIES.binary_search_by(|(n, _)| (*n).cmp(name))
    {
        pos += 1;
        return (
            char::from_u32(ENTITIES[index].1)
                .map(String::from)
                .unwrap_or_default(),
            pos,
        );
    }
    (format!("&{name}"), pos)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Element {
    attributes: Vec<(String, String)>,
}

impl Element {
    fn attr(&self, name: &str) -> Option<&str> {
        self.attributes
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }

    fn has_attr(&self, name: &str) -> bool {
        self.attributes.iter().any(|(n, _)| n == name)
    }
}

/// libxml2 reading a UTF-8 buffer: valid sequences decode, any other
/// byte is taken as Latin-1.
fn decode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    let mut rest = bytes;
    while !rest.is_empty() {
        match std::str::from_utf8(rest) {
            Ok(valid) => {
                out.push_str(valid);
                break;
            }
            Err(error) => {
                let (valid, after) = rest.split_at(error.valid_up_to());
                out.push_str(std::str::from_utf8(valid).expect("validated"));
                out.push(after[0] as char);
                rest = &after[1..];
            }
        }
    }
    out
}

/// How many attributes of one tag are kept; the rest are parsed and dropped.
const MAX_ATTRIBUTES: usize = 256;

/// The `<meta>` elements of the document, in document order.
fn meta_elements(html: &str) -> Vec<Element> {
    // A NUL ends libxml2's input.
    let html = &html[..html.find('\0').unwrap_or(html.len())];
    let mut scanner = Scanner {
        bytes: html.as_bytes(),
        pos: 0,
    };
    let mut metas = Vec::new();
    while let Some(c) = scanner.peek(0) {
        if c != b'<' {
            scanner.pos += 1;
            continue;
        }
        match scanner.peek(1) {
            Some(b'/') => scanner.end_tag(),
            Some(b'!') => scanner.markup_declaration(),
            Some(b'?') => scanner.skip_past(b'>'),
            Some(c) if c.is_ascii_alphabetic() => {
                let (name, element, self_closing) = scanner.start_tag();
                if name == "meta" {
                    metas.push(element);
                } else if (name == "script" || name == "style") && !self_closing {
                    scanner.raw_text(&name);
                }
            }
            _ => scanner.pos += 1,
        }
    }
    metas
}

struct Scanner<'a> {
    bytes: &'a [u8],
    pos: usize,
}

fn is_blank_byte(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r')
}

impl Scanner<'_> {
    fn peek(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.pos + ahead).copied()
    }

    fn starts_with_ignore_case(&self, text: &str) -> bool {
        self.bytes
            .get(self.pos..self.pos + text.len())
            .is_some_and(|b| b.eq_ignore_ascii_case(text.as_bytes()))
    }

    /// The text from `start` to here, which begins and ends at ASCII bytes.
    fn text_from(&self, start: usize) -> &str {
        std::str::from_utf8(&self.bytes[start..self.pos]).expect("split at ASCII")
    }

    fn skip_blanks(&mut self) {
        while self.peek(0).is_some_and(is_blank_byte) {
            self.pos += 1;
        }
    }

    /// Moves past the next `c` (or to the end).
    fn skip_past(&mut self, c: u8) {
        while let Some(next) = self.peek(0) {
            self.pos += 1;
            if next == c {
                break;
            }
        }
    }

    /// `htmlParseHTMLName`: `[A-Za-z_:.][A-Za-z0-9:_.-]*`, lowercased.
    fn html_name(&mut self) -> Option<String> {
        let first = self.peek(0)?;
        if !(first.is_ascii_alphabetic() || matches!(first, b'_' | b':' | b'.')) {
            return None;
        }
        let start = self.pos;
        while self
            .peek(0)
            .is_some_and(|c| c.is_ascii_alphanumeric() || matches!(c, b':' | b'-' | b'_' | b'.'))
        {
            self.pos += 1;
        }
        Some(self.text_from(start).to_ascii_lowercase())
    }

    fn end_tag(&mut self) {
        self.pos += 2;
        if self.html_name().is_some() {
            self.skip_past(b'>');
        }
    }

    /// `<!--…-->` (with `<!-->` and `<!--->` closing at once, and `--!>`
    /// accepted), `<!DOCTYPE…>`, and any other `<!…>` skipped as a bogus comment.
    fn markup_declaration(&mut self) {
        if self.peek(2) == Some(b'-') && self.peek(3) == Some(b'-') {
            self.pos += 4;
            if self.peek(0) == Some(b'>') {
                self.pos += 1;
                return;
            }
            if self.peek(0) == Some(b'-') && self.peek(1) == Some(b'>') {
                self.pos += 2;
                return;
            }
            while self.pos < self.bytes.len() {
                if self.starts_with_ignore_case("-->") {
                    self.pos += 3;
                    return;
                }
                if self.starts_with_ignore_case("--!>") {
                    self.pos += 4;
                    return;
                }
                self.pos += 1;
            }
        } else {
            self.skip_past(b'>');
        }
    }

    /// `htmlParseStartTag`: the name, the element, and whether it ended with `/>`.
    fn start_tag(&mut self) -> (String, Element, bool) {
        self.pos += 1;
        let name = self.html_name().unwrap_or_default();
        let mut attributes: Vec<(String, String)> = Vec::new();
        self.skip_blanks();
        loop {
            match self.peek(0) {
                None => break,
                Some(b'>') => break,
                Some(b'/') if self.peek(1) == Some(b'>') => break,
                _ => {}
            }
            match self.html_name() {
                Some(attribute) => {
                    self.skip_blanks();
                    let value = if self.peek(0) == Some(b'=') {
                        self.pos += 1;
                        self.skip_blanks();
                        self.attribute_value()
                    } else {
                        String::new()
                    };
                    if attributes.len() < MAX_ATTRIBUTES
                        && !attributes.iter().any(|(n, _)| *n == attribute)
                    {
                        attributes.push((attribute, value));
                    }
                }
                None => {
                    // Dump the bogus attribute string up to the next blank or the end of the tag.
                    while let Some(c) = self.peek(0) {
                        if is_blank_byte(c)
                            || c == b'>'
                            || (c == b'/' && self.peek(1) == Some(b'>'))
                        {
                            break;
                        }
                        self.pos += 1;
                    }
                }
            }
            self.skip_blanks();
        }
        let self_closing = self.peek(0) == Some(b'/');
        if self_closing {
            self.pos += 2;
        } else if self.peek(0) == Some(b'>') {
            self.pos += 1;
        }
        (name, Element { attributes }, self_closing)
    }

    /// `htmlParseAttValue`
    fn attribute_value(&mut self) -> String {
        match self.peek(0) {
            Some(quote @ (b'"' | b'\'')) => {
                self.pos += 1;
                let value = self.attribute_text(Some(quote));
                if self.peek(0) == Some(quote) {
                    self.pos += 1;
                }
                value
            }
            _ => self.attribute_text(None),
        }
    }

    /// `htmlParseHTMLAttribute`: up to the quote, or (unquoted) a blank or `>`.
    fn attribute_text(&mut self, stop: Option<u8>) -> String {
        let ends_text = |c: u8| {
            c == b'&' || Some(c) == stop || (stop.is_none() && (c == b'>' || is_blank_byte(c)))
        };
        let mut out = String::new();
        let mut truncated = false;
        loop {
            let start = self.pos;
            while self.peek(0).is_some_and(|c| !ends_text(c)) {
                self.pos += 1;
            }
            if !truncated {
                out.push_str(self.text_from(start));
            }
            if self.peek(0) != Some(b'&') {
                break;
            }
            let decoded = if self.peek(1) == Some(b'#') {
                let (parsed, pos) = char_ref_at(self.bytes, self.pos);
                self.pos = pos;
                parsed.map(|c| c.to_string())
            } else {
                let (parsed, pos) = entity_ref_at(self.bytes, self.pos);
                self.pos = pos;
                Some(parsed)
            };
            match decoded {
                Some(text) if !truncated => out.push_str(&text),
                Some(_) => {}
                // An invalid reference cuts the value short, as the NUL
                // it produces ends libxml2's C string.
                None => truncated = true,
            }
        }
        out
    }

    /// `htmlParseScript`: everything up to `</name` (any case) is text.
    fn raw_text(&mut self, name: &str) {
        let end = format!("</{name}");
        while self.pos < self.bytes.len() && !self.starts_with_ignore_case(&end) {
            self.pos += 1;
        }
    }
}

/// `Nokogiri::HTML4::Document#meta_encoding`: the first `meta[@charset]`,
/// else the charset in the first `http-equiv="Content-Type"` meta with a `content`.
fn meta_encoding(metas: &[Element]) -> Option<String> {
    if let Some(meta) = metas.iter().find(|m| m.has_attr("charset")) {
        return meta.attr("charset").map(str::to_string);
    }
    let meta = metas.iter().find(|m| {
        m.has_attr("content")
            && m.attr("http-equiv")
                .is_some_and(|v| v.eq_ignore_ascii_case("content-type"))
    })?;
    charset_in(meta.attr("content")?)
}

/// `content[/charset\s*=\s*([\w-]+)/i, 1]`, ASCII-only.
fn charset_in(content: &str) -> Option<String> {
    let bytes = content.as_bytes();
    let mut i = 0;
    while i + 7 <= bytes.len() {
        if bytes[i..i + 7].eq_ignore_ascii_case(b"charset") {
            let mut j = i + 7;
            while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                j += 1;
            }
            if bytes.get(j) == Some(&b'=') {
                j += 1;
                while j < bytes.len() && bytes[j].is_ascii_whitespace() {
                    j += 1;
                }
                let start = j;
                while j < bytes.len()
                    && (bytes[j].is_ascii_alphanumeric() || bytes[j] == b'_' || bytes[j] == b'-')
                {
                    j += 1;
                }
                if j > start {
                    return Some(content[start..j].to_string());
                }
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    None
}

/// `//*/meta[starts-with(@property, "og:") or starts-with(@name, "og:")]`
fn is_opengraph_tag(meta: &Element) -> bool {
    meta.attr("property").is_some_and(|p| p.starts_with("og:"))
        || meta.attr("name").is_some_and(|n| n.starts_with("og:"))
}

/// `String#blank?`: empty or only (Unicode) whitespace.
fn is_blank(s: &str) -> bool {
    s.chars().all(char::is_whitespace)
}

/// `opengraph_attributes`: from each `meta` whose `property` or `name`
/// starts with `og:`, the key is that attribute (`property` when
/// present) with every `og:` removed, and the value its non-blank
/// `content`. Later tags win. Without a meta charset, non-ASCII
/// characters are dropped.
fn opengraph_attributes(body: &[u8]) -> Vec<(&'static str, String)> {
    let html = decode(body);
    let metas = meta_elements(&html);
    let meta_encoding = meta_encoding(&metas);

    // Only the `ATTRIBUTES` keys are sliced out, so only they are kept.
    let mut found: [Option<String>; ATTRIBUTES.len()] = Default::default();
    for meta in metas.iter().filter(|m| is_opengraph_tag(m)) {
        let key = if meta.has_attr("property") {
            "property"
        } else {
            "name"
        };
        let name = meta.attr(key).unwrap_or("").replace("og:", "");
        let Some(index) = ATTRIBUTES.iter().position(|a| *a == name) else {
            continue;
        };
        let Some(content) = meta.attr("content").filter(|c| !is_blank(c)) else {
            continue;
        };
        let content = if meta_encoding.is_some() {
            content.to_string()
        } else {
            content.chars().filter(|c| c.is_ascii()).collect()
        };
        found[index] = Some(content);
    }

    ATTRIBUTES
        .into_iter()
        .zip(found)
        .filter_map(|(key, value)| Some((key, value?)))
        .collect()
}

/// `strip_tags` (Rails' full sanitizer) then `sanitize` (its
/// allowlist one), for og titles and descriptions: markup goes, text
/// stays (script/style content included), and the survivors are
/// re-escaped for HTML.
fn clean_text(value: &str) -> String {
    escape_text(&strip_tags(value))
}

/// The text of the fragment: tags and comments are skipped
/// (quote-aware), raw-text elements keep their content literally,
/// and terminated references decode. Approximates upstream's
/// html5ever pass; see the module docs.
fn strip_tags(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut out = String::new();
    let mut pos = 0;
    let mut text_since = 0;
    macro_rules! flush {
        ($end:expr) => {
            out.push_str(&html[text_since..$end]);
        };
    }
    while pos < bytes.len() {
        let c = bytes[pos];
        if c != b'<' {
            if c == b'&' {
                flush!(pos);
                if bytes.get(pos + 1) == Some(&b'#') {
                    let (parsed, next) = char_ref_at(bytes, pos);
                    // HTML5 replaces an invalid reference instead of
                    // truncating the value like libxml2 does.
                    out.push(parsed.unwrap_or('\u{FFFD}'));
                    pos = next;
                } else {
                    let (parsed, next) = entity_ref_at(bytes, pos);
                    out.push_str(&parsed);
                    pos = next;
                }
                text_since = pos;
                continue;
            }
            pos += 1;
            continue;
        }
        let Some(next) = bytes.get(pos + 1) else {
            pos += 1;
            continue;
        };
        if *next == b'!' {
            flush!(pos);
            pos = skip_markup_declaration(bytes, pos);
            text_since = pos;
            continue;
        }
        if *next == b'?' {
            flush!(pos);
            pos = skip_past_byte(bytes, pos, b'>');
            text_since = pos;
            continue;
        }
        if *next == b'/' {
            // An end tag (or literal `</` when no name follows).
            let after = pos + 2;
            if bytes.get(after).is_some_and(|c| c.is_ascii_alphabetic()) {
                flush!(pos);
                pos = skip_past_byte(bytes, pos, b'>');
                text_since = pos;
            } else {
                pos += 2;
            }
            continue;
        }
        if !next.is_ascii_alphabetic() {
            // A literal `<`.
            pos += 1;
            continue;
        }
        // A start tag: read its name, then skip it quote-aware.
        let mut end = pos + 1;
        while end < bytes.len()
            && (bytes[end].is_ascii_alphanumeric()
                || matches!(bytes[end], b':' | b'-' | b'_' | b'.'))
        {
            end += 1;
        }
        let name = html[pos + 1..end].to_ascii_lowercase();
        flush!(pos);
        pos = skip_tag(bytes, pos);
        text_since = pos;
        if matches!(
            name.as_str(),
            "script" | "style" | "textarea" | "title" | "iframe" | "noframes" | "noembed" | "xmp"
        ) {
            let close = format!("</{name}");
            let mut body_end = pos;
            if name == "textarea" && bytes.get(pos) == Some(&b'\n') {
                body_end += 1;
                pos += 1;
            }
            while body_end < bytes.len()
                && !bytes[body_end..]
                    .get(..close.len())
                    .is_some_and(|head| head.eq_ignore_ascii_case(close.as_bytes()))
            {
                body_end += 1;
            }
            let body = &html[pos..body_end];
            if name == "script" || name == "style" {
                out.push_str(body);
            } else {
                out.push_str(&decode_text_refs(body));
            }
            pos = body_end;
            text_since = pos;
        } else if name == "plaintext" {
            out.push_str(&html[pos..]);
            return out;
        }
    }
    out.push_str(&html[text_since..pos]);
    out
}

/// Decode terminated references in plain text (script/style keep theirs raw).
fn decode_text_refs(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::new();
    let mut pos = 0;
    let mut since = 0;
    while pos < bytes.len() {
        if bytes[pos] != b'&' {
            pos += 1;
            continue;
        }
        out.push_str(&text[since..pos]);
        if bytes.get(pos + 1) == Some(&b'#') {
            let (parsed, next) = char_ref_at(bytes, pos);
            out.push(parsed.unwrap_or('\u{FFFD}'));
            pos = next;
        } else {
            let (parsed, next) = entity_ref_at(bytes, pos);
            out.push_str(&parsed);
            pos = next;
        }
        since = pos;
    }
    out.push_str(&text[since..pos]);
    out
}

/// Skip `<!--…-->`, `<!DOCTYPE…>`, or any other `<!…>` bogus comment.
fn skip_markup_declaration(bytes: &[u8], pos: usize) -> usize {
    if bytes.get(pos + 2) == Some(&b'-') && bytes.get(pos + 3) == Some(&b'-') {
        let mut i = pos + 4;
        if bytes.get(i) == Some(&b'>') {
            return i + 1;
        }
        if bytes.get(i) == Some(&b'-') && bytes.get(i + 1) == Some(&b'>') {
            return i + 2;
        }
        while i < bytes.len() {
            if bytes[i..].starts_with(b"-->") {
                return i + 3;
            }
            if bytes[i..].starts_with(b"--!>") {
                return i + 4;
            }
            i += 1;
        }
        bytes.len()
    } else {
        skip_past_byte(bytes, pos, b'>')
    }
}

fn skip_past_byte(bytes: &[u8], mut pos: usize, stop: u8) -> usize {
    while pos < bytes.len() {
        pos += 1;
        if bytes[pos - 1] == stop {
            break;
        }
    }
    pos
}

/// Skip a start tag quote-aware; an unterminated tag is dropped.
fn skip_tag(bytes: &[u8], mut pos: usize) -> usize {
    debug_assert_eq!(bytes[pos], b'<');
    pos += 1;
    let mut quote = None;
    while pos < bytes.len() {
        let c = bytes[pos];
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if c == b'"' || c == b'\'' {
            quote = Some(c);
        } else if c == b'>' {
            return pos + 1;
        }
        pos += 1;
    }
    pos
}

/// `to_html` text serialization: `&`, `<`, `>` and non-breaking spaces escape.
fn escape_text(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '\u{a0}' => out.push_str("&nbsp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            _ => out.push(c),
        }
    }
    out
}

// --- Private network guard (RestrictedHTTP + Surfguard, default policy) ---

type V4Range = (u32, u8);
type V6Range = (u128, u8);

const fn v4(a: u8, b: u8, c: u8, d: u8, prefix: u8) -> V4Range {
    (u32::from_be_bytes([a, b, c, d]), prefix)
}

const fn v6(segments: [u16; 8], prefix: u8) -> V6Range {
    let mut value: u128 = 0;
    let mut i = 0;
    while i < 8 {
        value = (value << 16) | segments[i] as u128;
        i += 1;
    }
    (value, prefix)
}

/// `Surfguard::DISALLOWED_IPV4`
const DISALLOWED_IPV4: &[V4Range] = &[
    v4(0, 0, 0, 0, 8),
    v4(10, 0, 0, 0, 8),
    v4(100, 64, 0, 0, 10),
    v4(127, 0, 0, 0, 8),
    v4(168, 63, 129, 16, 32),
    v4(169, 254, 0, 0, 16),
    v4(172, 16, 0, 0, 12),
    v4(192, 0, 0, 0, 24),
    v4(192, 0, 2, 0, 24),
    v4(192, 88, 99, 0, 24),
    v4(192, 168, 0, 0, 16),
    v4(198, 18, 0, 0, 15),
    v4(198, 51, 100, 0, 24),
    v4(203, 0, 113, 0, 24),
    v4(224, 0, 0, 0, 4),
    v4(240, 0, 0, 0, 4),
];

/// `Surfguard::DISALLOWED_IPV6`
const DISALLOWED_IPV6: &[V6Range] = &[
    v6([0, 0, 0, 0, 0, 0, 0, 0], 128),
    v6([0x100, 0, 0, 0, 0, 0, 0, 0], 64),
    v6([0x100, 0, 0, 1, 0, 0, 0, 0], 64),
    v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 32),
    v6([0x2001, 2, 0, 0, 0, 0, 0, 0], 48),
    v6([0x2001, 0xdb8, 0, 0, 0, 0, 0, 0], 32),
    v6([0x2002, 0, 0, 0, 0, 0, 0, 0], 16),
    v6([0x3fff, 0, 0, 0, 0, 0, 0, 0], 20),
    v6([0x5f00, 0, 0, 0, 0, 0, 0, 0], 16),
    v6([0xfec0, 0, 0, 0, 0, 0, 0, 0], 10),
    v6([0xff00, 0, 0, 0, 0, 0, 0, 0], 8),
];

/// `Surfguard::IANA_ALLOCATED_IPV6_UNICAST`
const IANA_ALLOCATED_IPV6_UNICAST: &[V6Range] = &[
    v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x200, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x400, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x600, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x800, 0, 0, 0, 0, 0, 0], 22),
    v6([0x2001, 0xc00, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0xe00, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x1200, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x1400, 0, 0, 0, 0, 0, 0], 22),
    v6([0x2001, 0x1800, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x1a00, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x1c00, 0, 0, 0, 0, 0, 0], 22),
    v6([0x2001, 0x2000, 0, 0, 0, 0, 0, 0], 19),
    v6([0x2001, 0x4000, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x4200, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x4400, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x4600, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x4800, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x4a00, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x4c00, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2001, 0x5000, 0, 0, 0, 0, 0, 0], 20),
    v6([0x2001, 0x8000, 0, 0, 0, 0, 0, 0], 19),
    v6([0x2001, 0xa000, 0, 0, 0, 0, 0, 0], 20),
    v6([0x2001, 0xb000, 0, 0, 0, 0, 0, 0], 20),
    v6([0x2002, 0, 0, 0, 0, 0, 0, 0], 16),
    v6([0x2003, 0, 0, 0, 0, 0, 0, 0], 18),
    v6([0x2400, 0, 0, 0, 0, 0, 0, 0], 12),
    v6([0x2410, 0, 0, 0, 0, 0, 0, 0], 12),
    v6([0x2600, 0, 0, 0, 0, 0, 0, 0], 12),
    v6([0x2610, 0, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2620, 0, 0, 0, 0, 0, 0, 0], 23),
    v6([0x2630, 0, 0, 0, 0, 0, 0, 0], 12),
    v6([0x2800, 0, 0, 0, 0, 0, 0, 0], 12),
    v6([0x2a00, 0, 0, 0, 0, 0, 0, 0], 12),
    v6([0x2a10, 0, 0, 0, 0, 0, 0, 0], 12),
    v6([0x2c00, 0, 0, 0, 0, 0, 0, 0], 12),
];

/// `Surfguard::GLOBALLY_REACHABLE_IETF_ASSIGNMENTS`
const GLOBALLY_REACHABLE_IETF_ASSIGNMENTS: &[V6Range] = &[
    v6([0x2001, 3, 0, 0, 0, 0, 0, 0], 32),
    v6([0x2001, 4, 0x112, 0, 0, 0, 0, 0], 48),
];
const IETF_PROTOCOL_ASSIGNMENTS: V6Range = v6([0x2001, 0, 0, 0, 0, 0, 0, 0], 23);
const NAT64_WELL_KNOWN: V6Range = v6([0x64, 0xff9b, 0, 0, 0, 0, 0, 0], 96);
const NAT64_LOCAL_USE: V6Range = v6([0x64, 0xff9b, 1, 0, 0, 0, 0, 0], 48);
const IPV4_MAPPED: V6Range = v6([0, 0, 0, 0, 0, 0xffff, 0, 0], 96);
const IPV4_TRANSLATABLE: V6Range = v6([0, 0, 0, 0, 0xffff, 0, 0, 0], 96);
const IPV4_COMPATIBLE: V6Range = v6([0, 0, 0, 0, 0, 0, 0, 0], 96);
const UNIQUE_LOCAL: V6Range = v6([0xfc00, 0, 0, 0, 0, 0, 0, 0], 7);
const LINK_LOCAL_V6: V6Range = v6([0xfe80, 0, 0, 0, 0, 0, 0, 0], 10);

fn in_v4(ip: u32, (network, prefix): V4Range) -> bool {
    prefix == 0 || (ip ^ network) >> (32 - prefix as u32) == 0
}

fn in_v6(ip: u128, (network, prefix): V6Range) -> bool {
    prefix == 0 || (ip ^ network) >> (128 - prefix as u32) == 0
}

/// `Surfguard.blocked_address?`
pub(crate) fn blocked_address(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => disallowed_ipv4(u32::from(ip)),
        IpAddr::V6(ip) => {
            let ip = u128::from(ip);
            if in_v6(ip, IPV4_MAPPED) || in_v6(ip, IPV4_COMPATIBLE) || in_v6(ip, NAT64_LOCAL_USE) {
                true
            } else if in_v6(ip, NAT64_WELL_KNOWN) || in_v6(ip, IPV4_TRANSLATABLE) {
                disallowed_ipv4(ip as u32)
            } else {
                disallowed_ipv6(ip)
            }
        }
    }
}

fn disallowed_ipv4(ip: u32) -> bool {
    DISALLOWED_IPV4.iter().any(|range| in_v4(ip, *range))
}

fn disallowed_ipv6(ip: u128) -> bool {
    if GLOBALLY_REACHABLE_IETF_ASSIGNMENTS
        .iter()
        .any(|range| in_v6(ip, *range))
    {
        return false;
    }
    if in_v6(ip, UNIQUE_LOCAL)
        || ip == 1
        || in_v6(ip, LINK_LOCAL_V6)
        || in_v6(ip, IETF_PROTOCOL_ASSIGNMENTS)
    {
        return true;
    }
    if DISALLOWED_IPV6.iter().any(|range| in_v6(ip, *range)) {
        return true;
    }
    !IANA_ALLOCATED_IPV6_UNICAST
        .iter()
        .any(|range| in_v6(ip, *range))
}

const MAX_HOST_BYTES: usize = 255;
const MAX_ADDRESSES: usize = 256;

/// `PrivateNetworkGuard.resolve(hostname)`: the first public address.
/// Numeric hosts never reach DNS; every answer is classified and the
/// blocked ones dropped, IPv4 answers before IPv6 ones.
async fn guard_resolve(host: &str, port: u16) -> Option<IpAddr> {
    if !normal_host(host) {
        return None;
    }
    // Numeric forms are classified directly, so exotic spellings
    // can't depend on resolver quirks.
    if host.contains(':') {
        let ip: Ipv6Addr = host.parse().ok()?;
        return (!blocked_address(IpAddr::V6(ip))).then_some(IpAddr::V6(ip));
    }
    if legacy_ipv4_shape(host) {
        let ip = inet_aton(host)?;
        return (!blocked_address(IpAddr::V4(ip))).then_some(IpAddr::V4(ip));
    }
    let mut answers: Vec<IpAddr> = Vec::new();
    let resolved = tokio::net::lookup_host((host, port)).await.ok()?;
    for addr in resolved {
        let ip = addr.ip();
        if answers.len() >= MAX_ADDRESSES {
            return None;
        }
        if !answers.contains(&ip) {
            answers.push(ip);
        }
    }
    if answers.is_empty() {
        return None;
    }
    let (v4, v6): (Vec<IpAddr>, Vec<IpAddr>) = answers
        .into_iter()
        .filter(|ip| !blocked_address(*ip))
        .partition(IpAddr::is_ipv4);
    v4.into_iter().chain(v6).next()
}

/// `normalize_host` for a String host: ASCII, no NUL, not empty, at most 255 bytes, no zone.
fn normal_host(host: &str) -> bool {
    host.is_ascii()
        && !host.contains('\0')
        && !host.is_empty()
        && host.len() <= MAX_HOST_BYTES
        && !host.contains('%')
}

/// 1 to 4 dot-separated (empty parts ignored) decimal or 0x-hex numbers.
fn legacy_ipv4_shape(text: &str) -> bool {
    let parts: Vec<&str> = text.split('.').filter(|p| !p.is_empty()).collect();
    (1..=4).contains(&parts.len())
        && parts.iter().all(|part| {
            match part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
                Some(hex) => !hex.is_empty() && hex.bytes().all(|b| b.is_ascii_hexdigit()),
                None => part.bytes().all(|b| b.is_ascii_digit()),
            }
        })
}

/// glibc `__inet_aton_exact`: 1-4 parts in decimal, octal (leading 0)
/// or hex (0x); the last part fills the remaining bytes.
fn inet_aton(text: &str) -> Option<Ipv4Addr> {
    let parts: Vec<&str> = text.split('.').collect();
    if parts.is_empty() || parts.len() > 4 {
        return None;
    }
    let mut values = Vec::with_capacity(parts.len());
    for part in &parts {
        let (digits, radix) =
            if let Some(hex) = part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
                (hex, 16)
            } else if part.len() > 1 && part.starts_with('0') {
                (&part[1..], 8)
            } else {
                (*part, 10)
            };
        if part.is_empty() || !digits.chars().all(|c| c.is_digit(radix)) {
            return None;
        }
        let value = if digits.is_empty() {
            0
        } else {
            u64::from_str_radix(digits, radix).ok()?
        };
        if value > u32::MAX as u64 {
            return None;
        }
        values.push(value as u32);
    }
    let (last, leading) = values.split_last()?;
    if leading.iter().any(|v| *v > 0xff) {
        return None;
    }
    let remaining_bits = 32 - 8 * leading.len() as u32;
    if remaining_bits < 32 && *last >= 1 << remaining_bits {
        return None;
    }
    let mut address = *last;
    for (i, v) in leading.iter().enumerate() {
        address |= v << (24 - 8 * i as u32);
    }
    Some(Ipv4Addr::from(address))
}

const MEDIA_EXTENSIONS: &[&str] = &[
    "zip", "tar", "tar.gz", "tar.bz2", "tar.xz", "gz", "bz2", "rar", "7z", "dmg", "exe", "msi",
    "pkg", "deb", "iso", "jpg", "jpeg", "png", "gif", "bmp", "mp4", "mov", "avi", "mkv", "wmv",
    "flv", "heic", "heif", "mp3", "wav", "ogg", "aac", "wma", "webm", "ogv", "mpg", "mpeg",
];

fn is_word_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

/// `FILES_AND_MEDIA_URL_REGEX`:
/// `\bhttps?://\S+\.(?:…)\b`, case-sensitive.
fn media_url(url: &str) -> bool {
    let bytes = url.as_bytes();
    let mut schemes: Vec<(usize, usize)> = Vec::new();
    for scheme in ["https://", "http://"] {
        let mut from = 0;
        while let Some(pos) = url[from..].find(scheme) {
            schemes.push((from + pos, scheme.len()));
            from += pos + 1;
        }
    }
    schemes.sort();
    for (start, len) in schemes {
        if start > 0 && is_word_char(bytes[start - 1]) {
            continue;
        }
        let run_end = bytes[start + len..]
            .iter()
            .position(|b| b.is_ascii_whitespace())
            .map(|p| start + len + p)
            .unwrap_or(bytes.len());
        let run = &url[start + len..run_end];
        // `\S+` needs a char before the dot, and `\b` a non-word char (or the end) after.
        let mut i = 1;
        while i < run.len() {
            if run.as_bytes()[i] != b'.' {
                i += 1;
                continue;
            }
            let after = &run[i + 1..];
            for ext in MEDIA_EXTENSIONS {
                if let Some(rest) = after.strip_prefix(ext)
                    && (rest.is_empty() || !is_word_char(rest.as_bytes()[0]))
                {
                    return true;
                }
            }
            i += 1;
        }
    }
    false
}

/// `/\A(?:[^@,;]+@[^@,;]+(?:\z|[,;]))*\z/`: the `URI::MailTo` `to` check.
fn mailto_to_valid(to: &str) -> bool {
    let bytes = to.as_bytes();
    let mut i = 0;
    let special = |b: u8| matches!(b, b'@' | b',' | b';');
    while i < bytes.len() {
        let start = i;
        while i < bytes.len() && !special(bytes[i]) {
            i += 1;
        }
        if i == start || i >= bytes.len() || bytes[i] != b'@' {
            return false;
        }
        i += 1;
        let domain = i;
        while i < bytes.len() && !special(bytes[i]) {
            i += 1;
        }
        if i == domain {
            return false;
        }
        if i < bytes.len() {
            if bytes[i] == b'@' {
                return false;
            }
            i += 1;
        }
    }
    true
}

/// RFC 3986 character classes, mirroring `URI::RFC3986_Parser`.
fn is_unreserved_or_sub(b: u8) -> bool {
    matches!(b, b'!' | b'$' | b'&'..=b'.' | b'0'..=b'9' | b';' | b'=' | b'A'..=b'Z' | b'_' | b'a'..=b'z' | b'~')
}

fn pct_at(s: &[u8], i: usize) -> bool {
    i + 2 < s.len() && s[i] == b'%' && s[i + 1].is_ascii_hexdigit() && s[i + 2].is_ascii_hexdigit()
}

fn seg_char(b: u8) -> bool {
    is_unreserved_or_sub(b) || matches!(b, b':' | b'@' | b'/')
}

fn fragment_char(b: u8) -> bool {
    is_unreserved_or_sub(b) || matches!(b, b':' | b'@' | b'/' | b'?')
}

/// The hier-part is RFC 3986 segments (the mailto branch needs this;
/// fetched URLs go through the WHATWG parser instead — see module docs).
fn hier_valid(hier: &str) -> bool {
    let s = hier.as_bytes();
    let mut i = 0;
    while i < s.len() {
        if pct_at(s, i) {
            i += 3;
        } else if seg_char(s[i]) {
            i += 1;
        } else {
            return false;
        }
    }
    true
}

/// `URI::Generic#query=`: `%` followed by two non-hex characters rejects the URL.
fn query_escapes_invalid(url: &str) -> bool {
    let Some(qpos) = url.find('?') else {
        return false;
    };
    let after = &url[qpos + 1..];
    let end = after.find('#').unwrap_or(after.len());
    let cleaned: Vec<u8> = after[..end]
        .bytes()
        .filter(|b| !matches!(b, b'\t' | b'\r' | b'\n'))
        .collect();
    cleaned
        .windows(3)
        .any(|w| w[0] == b'%' && !w[1].is_ascii_hexdigit() && !w[2].is_ascii_hexdigit())
}

/// The fragment (after the first `#`) must be fragment characters.
fn fragment_invalid(url: &str) -> bool {
    let Some(fpos) = url.find('#') else {
        return false;
    };
    let bytes = &url.as_bytes()[fpos + 1..];
    let mut i = 0;
    while i < bytes.len() {
        if pct_at(bytes, i) {
            i += 3;
        } else if fragment_char(bytes[i]) {
            i += 1;
        } else {
            return true;
        }
    }
    false
}

fn strip_scheme<'a>(url: &'a str, scheme: &str) -> Option<&'a str> {
    let (head, rest) = url.split_once(':')?;
    let mut chars = head.bytes();
    if !chars.next()?.is_ascii_alphabetic() {
        return None;
    }
    if !head
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
    {
        return None;
    }
    head.eq_ignore_ascii_case(scheme).then_some(rest)
}

enum UrlVerdict {
    Valid(url::Url),
    Invalid,
    Raised,
}

/// The slice of `URI.parse` the unfurl depends on: non-ASCII, bad
/// query escapes, and bad fragments never parse, and a `mailto:`
/// URL with an invalid `to` part raises instead.
fn check_url(url: &str) -> UrlVerdict {
    if !url.is_ascii() || query_escapes_invalid(url) || fragment_invalid(url) {
        return UrlVerdict::Invalid;
    }
    if let Some(rest) = strip_scheme(url, "mailto") {
        let hier_end = rest.find(['?', '#']).unwrap_or(rest.len());
        let hier = &rest[..hier_end];
        if !hier_valid(hier) {
            return UrlVerdict::Invalid;
        }
        if hier.starts_with("//") {
            // No opaque part; a query salvages an empty `to`.
            let has_query = rest
                .find('?')
                .is_some_and(|q| rest.find('#').is_none_or(|f| q < f));
            return if has_query {
                UrlVerdict::Invalid
            } else {
                UrlVerdict::Raised
            };
        }
        return if mailto_to_valid(hier) {
            UrlVerdict::Invalid
        } else {
            UrlVerdict::Raised
        };
    }
    match url::Url::parse(url) {
        Ok(parsed) => UrlVerdict::Valid(parsed),
        Err(_) => UrlVerdict::Invalid,
    }
}

/// The raw host span (case preserved) of an `scheme://authority` URL.
fn raw_host_span(url: &str) -> Option<(usize, usize)> {
    let scheme_end = url.find("://")? + 3;
    let auth_end = url[scheme_end..]
        .find(['/', '?', '#'])
        .map(|p| scheme_end + p)
        .unwrap_or(url.len());
    let auth = &url[scheme_end..auth_end];
    let host_start = scheme_end + auth.rfind('@').map(|p| p + 1).unwrap_or(0);
    let host = &url[host_start..auth_end];
    if host.starts_with('[') {
        return None;
    }
    match host.rfind(':') {
        Some(c)
            if !host[c + 1..].is_empty() && host[c + 1..].bytes().all(|b| b.is_ascii_digit()) =>
        {
            Some((host_start, host_start + c))
        }
        Some(_) => None,
        None => Some((host_start, auth_end)),
    }
}

/// `tweet_url?` + `replace_twitter_domain_for_opengraph_support`:
/// a tweet URL reads through fxtwitter instead. The host match is
/// case-sensitive, like upstream.
fn tweet_rewrite(url: &str, parsed: &url::Url) -> Option<String> {
    let (start, end) = raw_host_span(url)?;
    if !TWITTER_HOSTS.contains(&&url[start..end]) {
        return None;
    }
    let path = parsed.path();
    if path == "/" || path.chars().all(char::is_whitespace) {
        return None;
    }
    Some(format!("{}fxtwitter.com{}", &url[..start], &url[end..]))
}

/// `Location#valid?` without fetching: http(s) plus a public address to pin.
async fn validate_location(url: &url::Url) -> Option<(url::Url, IpAddr)> {
    if url.scheme() != "http" && url.scheme() != "https" {
        return None;
    }
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    let ip = guard_resolve(host, port).await?;
    Some((url.clone(), ip))
}

/// `Location#valid?` for an og value: http(s) plus a public address.
async fn location_valid(url: &str) -> bool {
    if !url.is_ascii() || query_escapes_invalid(url) || fragment_invalid(url) {
        return false;
    }
    let Ok(parsed) = url::Url::parse(url) else {
        return false;
    };
    validate_location(&parsed).await.is_some()
}

/// `response[name]`: every value of the header, joined with ", ".
fn joined_header(
    headers: &reqwest::header::HeaderMap,
    name: reqwest::header::HeaderName,
) -> Option<String> {
    let values: Vec<String> = headers
        .get_all(name)
        .iter()
        .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
        .collect();
    if values.is_empty() {
        None
    } else {
        Some(values.join(", "))
    }
}

/// Ruby `String#strip`: surrounding whitespace and NULs.
fn ruby_strip(s: &str) -> String {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\0')
        .to_string()
}

/// `Net::HTTPHeader#content_type`: the media type before any `;`,
/// main and sub type each stripped (not downcased).
fn document_content_type(headers: &reqwest::header::HeaderMap) -> Option<String> {
    let header = joined_header(headers, reqwest::header::CONTENT_TYPE)?;
    let media = header.split(';').next().unwrap_or("");
    let mut parts = media.split('/');
    let main = ruby_strip(parts.next().unwrap_or(""));
    match parts.next() {
        Some(sub) => Some(format!("{main}/{}", ruby_strip(sub))),
        None => Some(main),
    }
}

/// `Net::HTTPHeader#content_length`: the first run of digits.
/// `None` is a missing header; a present-but-digitless one fails the fetch.
fn header_content_length(headers: &reqwest::header::HeaderMap) -> Option<Option<u64>> {
    let Some(header) = joined_header(headers, reqwest::header::CONTENT_LENGTH) else {
        return Some(None);
    };
    let digits: String = header
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    Some(Some(digits.parse().unwrap_or(u64::MAX)))
}

/// `Opengraph::Fetch`: one method against a pinned address, any 3xx
/// a redirect to an absolute http(s) URL re-resolved through the
/// guard, at most 10 responses.
async fn request(
    method: reqwest::Method,
    mut url: url::Url,
    mut ip: IpAddr,
) -> Option<reqwest::Response> {
    for _ in 0..MAX_REDIRECTS {
        let host = url.host_str()?.to_string();
        let port = url.port_or_known_default()?;
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .resolve(
                host.trim_start_matches('[').trim_end_matches(']'),
                std::net::SocketAddr::new(ip, port),
            )
            .connect_timeout(Duration::from_secs(5))
            .build()
            .ok()?;
        let response = client
            .request(method.clone(), url.clone())
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT_ENCODING, ACCEPT_ENCODING)
            .send()
            .await
            .ok()?;
        let status = response.status().as_u16();
        if (300..400).contains(&status) {
            let location = joined_header(response.headers(), reqwest::header::LOCATION)?;
            let next = url::Url::parse(&location).ok()?;
            if next.scheme() != "http" && next.scheme() != "https" {
                return None;
            }
            let host = next.host_str()?;
            let port = next.port_or_known_default()?;
            ip = guard_resolve(host, port).await?;
            url = next;
            continue;
        }
        return Some(response);
    }
    None
}

/// `fetch_document`: a 200 `text/html` of at most 5MB, by
/// `Content-Length` and by what's actually read (inflated).
async fn fetch_document(url: url::Url, ip: IpAddr) -> Option<Vec<u8>> {
    let response = request(reqwest::Method::GET, url, ip).await?;
    if response.status() != reqwest::StatusCode::OK {
        return None;
    }
    if document_content_type(response.headers()).as_deref() != Some(ALLOWED_DOCUMENT_CONTENT_TYPE) {
        return None;
    }
    if header_content_length(response.headers())?.is_some_and(|n| n > MAX_BODY_SIZE as u64) {
        return None;
    }
    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    use futures_util::StreamExt as _;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.ok()?;
        if body.len() + chunk.len() > MAX_BODY_SIZE {
            return None;
        }
        body.extend_from_slice(&chunk);
    }
    Some(body)
}

/// `fetch_content_type`: the final HEAD response's `Content-Type`, whatever its status.
async fn fetch_content_type(url: url::Url, ip: IpAddr) -> Option<String> {
    let response = request(reqwest::Method::HEAD, url, ip).await?;
    joined_header(response.headers(), reqwest::header::CONTENT_TYPE)
}

/// `valid_image_content_type`: kept only when a HEAD says JPEG, PNG, GIF or WebP.
async fn valid_image(image: Option<String>) -> Option<String> {
    let image = image.filter(|i| !is_blank(i))?;
    if !image.is_ascii() || query_escapes_invalid(&image) || fragment_invalid(&image) {
        tracing::warn!("Failed to fetch image content type: {image} (bad URI)");
        return None;
    }
    let parsed = url::Url::parse(&image).ok()?;
    let (_, ip) = validate_location(&parsed).await?;
    let content_type = fetch_content_type(parsed, ip).await?.to_lowercase();
    ALLOWED_IMAGE_CONTENT_TYPES
        .contains(&content_type.as_str())
        .then_some(image)
}

/// `Opengraph::Metadata.from_url` + `valid?`: the metadata as JSON,
/// 204 (`None`), or a raise — what the reference answers 500.
async fn unfurl(url_param: &str) -> Result<Option<serde_json::Value>, Raised> {
    static SLOTS: Semaphore = Semaphore::const_new(MAX_CONCURRENT_UNFURLS);
    let outcome = tokio::time::timeout(UNFURL_DEADLINE, async {
        let _slot = SLOTS.acquire().await.expect("never closed");
        unfurl_inner(url_param).await
    })
    .await;
    match outcome {
        Ok(result) => result,
        // One that runs out of time unfurls nothing.
        Err(_) => Ok(None),
    }
}

async fn unfurl_inner(url_param: &str) -> Result<Option<serde_json::Value>, Raised> {
    let parsed = match check_url(url_param) {
        UrlVerdict::Valid(parsed) => parsed,
        UrlVerdict::Invalid => return Ok(None),
        UrlVerdict::Raised => return Err(Raised("URI::InvalidComponentError")),
    };
    // Tweets read through fxtwitter; a tweet whose page can't be read raises.
    let tweet = tweet_rewrite(url_param, &parsed);
    let fetch_url = tweet.as_deref().unwrap_or(url_param);
    if media_url(fetch_url) {
        return if tweet.is_some() {
            Err(Raised("NoMethodError"))
        } else {
            Ok(None)
        };
    }
    let fetch_valid = match check_url(fetch_url) {
        UrlVerdict::Valid(parsed) => validate_location(&parsed).await,
        _ => None,
    };
    let Some((fetch_parsed, ip)) = fetch_valid else {
        return if tweet.is_some() {
            Err(Raised("NoMethodError"))
        } else {
            Ok(None)
        };
    };
    let Some(body) = fetch_document(fetch_parsed, ip).await else {
        return if tweet.is_some() {
            Err(Raised("NoMethodError"))
        } else {
            Ok(None)
        };
    };
    let found = opengraph_attributes(&body);
    let og = |key: &str| {
        found
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
    };
    // `valid_canonical_url`: the og url when it is a valid location, else the request URL.
    let page = match og("url") {
        Some(url) if location_valid(&url).await => url,
        _ => url_param.to_string(),
    };
    let image = valid_image(og("image")).await;
    let title = og("title").map(|t| clean_text(&t));
    let description = og("description").map(|d| clean_text(&d));
    let present = |value: &Option<String>| value.as_ref().is_some_and(|v| !is_blank(v));
    let mut valid = present(&title) && present(&Some(page.clone())) && present(&description);
    if let Some(image) = image.clone().filter(|i| !is_blank(i)) {
        valid &= location_valid(&image).await;
    }
    if !valid {
        return Ok(None);
    }
    Ok(Some(serde_json::json!({
        "title": title,
        "url": page,
        "image": image,
        "description": description,
        "context_for_validation": { "context": serde_json::Value::Null },
        "errors": serde_json::Value::Object(serde_json::Map::new()),
    })))
}

/// The `url` param after `params.require(:url)`: missing or blank is
/// the controller's 400, while a hash or array passes `require` but
/// isn't a string, so the metadata has no title and isn't valid.
enum UrlParam {
    Missing,
    Blank,
    Collection,
    Value(String),
}

fn url_param_json(parsed: &serde_json::Value) -> UrlParam {
    match parsed.get("url") {
        None | Some(serde_json::Value::Null) => UrlParam::Missing,
        Some(serde_json::Value::String(url)) if url.is_empty() => UrlParam::Blank,
        Some(serde_json::Value::String(url)) => UrlParam::Value(url.clone()),
        Some(_) => UrlParam::Collection,
    }
}

fn url_param_form(pairs: &[(String, String)]) -> UrlParam {
    if let Some((_, url)) = pairs.iter().find(|(key, _)| key == "url") {
        return if url.is_empty() {
            UrlParam::Blank
        } else {
            UrlParam::Value(url.clone())
        };
    }
    if pairs.iter().any(|(key, _)| key.starts_with("url[")) {
        UrlParam::Collection
    } else {
        UrlParam::Missing
    }
}

/// `POST /unfurl_link`: JSON metadata, 204, or 500 where the reference raises.
async fn create_unfurl(cx: &Cx, body: Body) -> Result<Response> {
    use topcoat::router::{error::bad_request, request};
    let Some(_) = crate::auth::current_user_or_deny_bot(cx).await? else {
        return see_other("/session/new").into_response(cx);
    };
    let headers = request::headers(cx);
    let content_type = headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let header_token = headers
        .get("x-csrf-token")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    let raw = to_bytes(body, 64 * 1024).await?;
    let (param, token) = if content_type.starts_with("application/json") {
        let parsed: serde_json::Value = serde_json::from_slice(&raw).unwrap_or_default();
        (url_param_json(&parsed), header_token)
    } else {
        let pairs = serde_urlencoded::from_bytes::<Vec<(String, String)>>(&raw).unwrap_or_default();
        let token = pairs
            .iter()
            .find(|(key, _)| key == "authenticity_token")
            .map(|(_, value)| value.clone())
            .unwrap_or(header_token);
        (url_param_form(&pairs), token)
    };
    if !crate::csrf::verify(cx, &token) {
        return Err(forbidden().into());
    }
    let url = match param {
        UrlParam::Missing | UrlParam::Blank => {
            return Err(bad_request("param is missing or the value is empty: url").into());
        }
        UrlParam::Collection => return no_content(cx),
        UrlParam::Value(url) => url,
    };
    match unfurl(&url).await {
        Ok(Some(metadata)) => Json(metadata).into_response(cx),
        Ok(None) => no_content(cx),
        Err(Raised(which)) => Err(topcoat::Error::msg(format!("unfurl raised {which}"))),
    }
}

fn no_content(_cx: &Cx) -> Result<Response> {
    Ok(Response::builder()
        .status(204)
        .body(Body::empty())
        .expect("204 builds"))
}

/// `POST /unfurl_link`.
#[route(POST "/unfurl_link")]
pub async fn create(cx: &Cx, body: Body) -> Result<Response> {
    create_unfurl(cx, body).await
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn title(html: &str) -> Option<String> {
        meta_elements(&format!("<meta charset=utf-8>{html}"))
            .into_iter()
            .rfind(|m| m.attr("property") == Some("og:title"))
            .and_then(|m| {
                m.attr("content")
                    .filter(|c| !c.is_empty())
                    .map(str::to_string)
            })
    }

    fn title_of(content: &str) -> Option<String> {
        title(&format!(
            "<meta property=\"og:title\" content=\"a{content}b\">"
        ))
    }

    /// Probed against the reference's Nokogiri 1.19.4.
    #[test]
    fn decodes_references_like_libxml2() {
        for (reference, expected) in [
            ("&apos;", "a'b"),
            ("&eacute", "a&eacuteb"),
            ("&eacute;x", "aéxb"),
            ("&#233", "aéb"),
            ("&#233x", "aéxb"),
            ("&#xE9", "a\u{0e9b}"),
            ("&#xe9;", "aéb"),
            ("&AMP;", "a&AMP;b"),
            ("&Eacute;", "aÉb"),
            ("&unknown;", "a&unknown;b"),
            ("& x", "a& xb"),
            ("&#65;&#x41;", "aAAb"),
            ("&#128;", "a\u{80}b"),
            ("&#150;", "a\u{96}b"),
            ("&#xD800;", "a"),
            ("&#1114112;", "a"),
            ("&lt", "a&ltb"),
            ("&amp;amp;", "a&amp;b"),
            ("&hellip;", "a…b"),
            ("&nbsp", "a&nbspb"),
            ("&#;", "a"),
            ("&#x;", "a"),
        ] {
            assert_eq!(
                title_of(reference).as_deref(),
                Some(expected),
                "{reference}"
            );
        }
    }

    #[test]
    fn tokenizes_like_libxml2() {
        let cases: &[(&str, Option<&str>)] = &[
            (
                "<script><meta property=\"og:title\" content=\"in script\"></script><meta property=\"og:title\" content=\"after\">",
                Some("after"),
            ),
            (
                "<style><meta property=\"og:title\" content=\"in style\"></style>",
                None,
            ),
            (
                "<textarea><meta property=\"og:title\" content=\"in textarea\"></textarea>",
                Some("in textarea"),
            ),
            (
                "<title><meta property=\"og:title\" content=\"in title\"></title>",
                Some("in title"),
            ),
            (
                "<noscript><meta property=\"og:title\" content=\"in noscript\"></noscript>",
                Some("in noscript"),
            ),
            (
                "<template><meta property=\"og:title\" content=\"in template\"></template>",
                Some("in template"),
            ),
            (
                "<svg><meta property=\"og:title\" content=\"in svg\"></svg>",
                Some("in svg"),
            ),
            (
                "<!-- <meta property=\"og:title\" content=\"comment\"> --><p>",
                None,
            ),
            (
                "<meta property=og:title content=unquoted>",
                Some("unquoted"),
            ),
            (
                "<meta property=\"og:title\" content=\"line1\r\nline2\">",
                Some("line1\r\nline2"),
            ),
            (
                "<meta property='og:title' content='single'>",
                Some("single"),
            ),
            (
                "<meta property = \"og:title\" content = \"spaced\">",
                Some("spaced"),
            ),
            (
                "<META PROPERTY=\"og:title\" CONTENT=\"upper\">",
                Some("upper"),
            ),
            (
                "<meta property=\"og:title\"content=\"nospace\">",
                Some("nospace"),
            ),
            ("<meta/property=\"og:title\"/content=\"slashes\">", None),
            (
                "<meta property=\"og:title\" content=\"<b>tag</b>\">",
                Some("<b>tag</b>"),
            ),
            ("<meta property=\"og:title\" content=\"a\">b\">", Some("a")),
            (
                "<meta property=\"og:title\" content=\"unterminated>",
                Some("unterminated>"),
            ),
            (
                "<meta property=\"og:title\" content=unq\"uoted>",
                Some("unq\"uoted"),
            ),
            ("<meta property=\"og:title\" content=a&amp;b>", Some("a&b")),
            (
                "<!--> <meta property=\"og:title\" content=\"after empty comment\"> -->",
                Some("after empty comment"),
            ),
            (
                "<!---> <meta property=\"og:title\" content=\"after dash comment\"> -->",
                Some("after dash comment"),
            ),
            (
                "<!DOCTYPE html><meta property=\"og:title\" content=\"doctype\">",
                Some("doctype"),
            ),
            (
                "<?xml version=\"1.0\"?><meta property=\"og:title\" content=\"pi\">",
                Some("pi"),
            ),
            (
                "<![CDATA[ <meta property=\"og:title\" content=\"cdata\"> ]]>",
                None,
            ),
            ("<p <meta property=\"og:title\" content=\"broken\">", None),
            ("< meta property=\"og:title\" content=\"space\">", None),
            (
                "<meta property=\"og:title\" content=\"tab\there\">",
                Some("tab\there"),
            ),
            ("<meta property=\"og:title\" content=\"\x00nul\">", None),
        ];
        for (html, expected) in cases {
            assert_eq!(title(html).as_deref(), *expected, "{html}");
        }
    }

    #[test]
    fn decodes_bytes_like_libxml2_reading_utf8() {
        assert_eq!(decode(b"caf\xc3\xa9 \xff x"), "café ÿ x");
        assert_eq!(decode(b"\xff caf\xc3\xa9 x"), "ÿ café x");
        assert_eq!(decode(b"\x93q\x94"), "\u{93}q\u{94}");
        assert_eq!(decode(b"\x82\xa0"), "\u{82}\u{a0}");
    }

    #[test]
    fn finds_the_meta_encoding_like_nokogiri() {
        let encoding = |html: &str| meta_encoding(&meta_elements(html));
        assert_eq!(
            encoding("<meta charset=\"iso-8859-1\">"),
            Some("iso-8859-1".into())
        );
        assert_eq!(encoding("<meta charset=\"\">"), Some("".into()));
        assert_eq!(
            encoding(
                "<meta http-equiv=\"content-type\" content=\"text/html; charset=iso-8859-1\">"
            ),
            Some("iso-8859-1".into())
        );
        assert_eq!(
            encoding(
                "<meta http-equiv=\"Content-Type\" content=\"text/html\"><meta http-equiv=\"Content-Type\" content=\"charset=utf-8\">"
            ),
            None
        );
        assert_eq!(
            encoding("<meta http-equiv=\"refresh\" content=\"charset=utf-8\">"),
            None
        );
        assert_eq!(encoding("<meta property=\"og:title\" content=\"x\">"), None);
    }

    /// Probed against the reference (`strip_tags` then `sanitize`).
    #[test]
    fn strips_tags_like_rails() {
        for (input, expected) in [
            ("Tom & Jerry", "Tom &amp; Jerry"),
            ("a < b", "a &lt; b"),
            ("x&nbsp;y", "x&nbsp;y"),
            ("\u{a0}nb", "&nbsp;nb"),
            ("Hey!<script>alert('hi')</script>", "Hey!alert('hi')"),
            ("<!-- c -->t", "t"),
            ("a &lt;b&gt; c", "a &lt;b&gt; c"),
            ("<p>one</p><p>two</p>", "onetwo"),
            ("\"q\" 'a'", "\"q\" 'a'"),
            ("<style>x</style>y", "xy"),
            ("&amp;amp;", "&amp;amp;"),
            ("<b>bold</b>", "bold"),
            ("</script><img src=a onerror=prompt(1)>", ""),
            (" sp  ", " sp  "),
            ("<textarea>t<b>x</b></textarea>", "t&lt;b&gt;x&lt;/b&gt;"),
        ] {
            assert_eq!(clean_text(input), expected, "{input}");
        }
    }

    #[test]
    fn classifies_addresses_like_surfguard() {
        let blocked = |ip: &str| blocked_address(ip.parse().unwrap());
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "100.64.0.1",
            "127.0.0.1",
            "168.63.129.16",
            "169.254.169.254",
            "172.16.0.0",
            "172.31.255.255",
            "192.0.0.8",
            "192.0.2.1",
            "192.88.99.1",
            "192.168.1.1",
            "198.18.0.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "240.0.0.1",
            "255.255.255.255",
            "::",
            "::1",
            "::ffff:192.168.1.1",
            "::ffff:8.8.8.8",
            "::8.8.8.8",
            "64:ff9b::a00:1",
            "64:ff9b:1::1",
            "::ffff:0:a00:1",
            "fc00::1",
            "fd00::1",
            "fe80::1",
            "fec0::1",
            "ff02::1",
            "2001::1",
            "2001:db8::1",
            "2002::1",
            "3fff::1",
            "5f00::1",
            "100::1",
            "2001:2::1",
            "4000::1",
            "2001:10::1",
        ] {
            assert!(blocked(ip), "{ip} should be blocked");
        }
        for ip in [
            "8.8.8.8",
            "1.1.1.1",
            "93.184.216.34",
            "142.250.185.206",
            "172.32.0.1",
            "100.128.0.1",
            "192.0.1.1",
            "2606:2800:220:1:248:1893:25c8:1946",
            "2a00:1450:4001:82a::200e",
            "2001:3::1",
            "2001:4:112::1",
            "64:ff9b::808:808",
            "::ffff:0:808:808",
            "2c0f:ffff::1",
        ] {
            assert!(!blocked(ip), "{ip} should be public");
        }
    }

    #[test]
    fn inet_aton_forms() {
        use std::net::Ipv4Addr;
        assert_eq!(inet_aton("127.1"), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(inet_aton("0x7f.1"), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(inet_aton("2130706433"), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(inet_aton("0177.0.0.01"), Some(Ipv4Addr::new(127, 0, 0, 1)));
        assert_eq!(inet_aton("10.0.258"), Some(Ipv4Addr::new(10, 0, 1, 2)));
        assert_eq!(inet_aton("09.1.1.1"), None);
        assert_eq!(inet_aton("256.1.1.1"), None);
        assert_eq!(inet_aton("1.2.3.4."), None);
        assert_eq!(inet_aton("www.example.com"), None);
    }

    #[test]
    fn media_regex_is_case_sensitive_with_word_boundaries() {
        assert!(media_url("http://www.example.com/video.mp4"));
        assert!(media_url("http://www.example.com/archive.tar.gzip"));
        assert!(media_url("https://example.com/a.png?x=1"));
        assert!(media_url("https://example.com/a.tar.gz"));
        assert!(!media_url("http://www.example.com/UPPER.MP4"));
        assert!(!media_url("HTTP://www.example.com/video.mp4"));
        assert!(!media_url("http://www.example.com/article"));
        assert!(!media_url("http://www.example.com/pngs"));
        assert!(!media_url("xhttp://www.example.com/video.mp4"));
        assert!(!media_url("http://.mp4"));
    }

    #[test]
    fn mailto_without_an_address_raises() {
        assert!(!mailto_to_valid("foo"));
        assert!(mailto_to_valid("a@b.com"));
        assert!(mailto_to_valid(""));
        assert!(mailto_to_valid("a@b.com,c@d.org;"));
        assert!(!mailto_to_valid("a@b@c.com"));
        assert!(matches!(check_url("mailto:foo"), UrlVerdict::Raised));
        assert!(matches!(check_url("mailto:a@b.com"), UrlVerdict::Invalid));
        assert!(matches!(check_url("mailto:"), UrlVerdict::Invalid));
        assert!(matches!(check_url("mailto:foo bar"), UrlVerdict::Invalid));
        assert!(matches!(check_url("mailto://host/x"), UrlVerdict::Raised));
    }

    #[test]
    fn unparsable_urls_are_invalid_not_raised() {
        assert!(matches!(check_url("httpfake"), UrlVerdict::Invalid));
        assert!(matches!(check_url(" foo"), UrlVerdict::Invalid));
        assert!(matches!(
            check_url("http://www.example.com/é"),
            UrlVerdict::Invalid
        ));
        assert!(matches!(
            check_url("http://www.example.com/?x=%zz"),
            UrlVerdict::Invalid
        ));
        assert!(matches!(
            check_url("http://www.example.com/?x=%41"),
            UrlVerdict::Valid(_)
        ));
        assert!(matches!(
            check_url("http://www.example.com/#a b"),
            UrlVerdict::Invalid
        ));
        assert!(matches!(
            check_url("http://www.example.com/#a?b"),
            UrlVerdict::Valid(_)
        ));
        assert!(matches!(
            check_url("ftp://example.com/x.png"),
            UrlVerdict::Valid(_)
        ));
    }

    #[test]
    fn tweets_rewrite_to_fxtwitter_case_sensitively() {
        let rewrite = |url: &str| {
            let UrlVerdict::Valid(parsed) = check_url(url) else {
                panic!("{url} should parse");
            };
            tweet_rewrite(url, &parsed)
        };
        assert_eq!(
            rewrite("http://twitter.com/dhh/status/1").as_deref(),
            Some("http://fxtwitter.com/dhh/status/1")
        );
        assert_eq!(
            rewrite("https://x.com/a/2?x=1#f").as_deref(),
            Some("https://fxtwitter.com/a/2?x=1#f")
        );
        assert_eq!(
            rewrite("http://www.x.com:8080/a").as_deref(),
            Some("http://fxtwitter.com:8080/a")
        );
        assert_eq!(rewrite("http://twitter.com/"), None);
        assert_eq!(rewrite("http://twitter.com"), None);
        assert_eq!(rewrite("http://TWITTER.COM/dhh/1"), None);
        assert_eq!(rewrite("http://example.com/dhh/1"), None);
    }

    #[test]
    fn document_content_type_strips_params_but_not_case() {
        use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderValue};
        let content_type = |value: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(CONTENT_TYPE, HeaderValue::from_str(value).unwrap());
            }
            document_content_type(&headers)
        };
        assert_eq!(
            content_type(Some("text/html")).as_deref(),
            Some("text/html")
        );
        assert_eq!(
            content_type(Some("text/html; charset=utf-8")).as_deref(),
            Some("text/html")
        );
        assert_eq!(
            content_type(Some("Text/HTML ; charset=utf-8")).as_deref(),
            Some("Text/HTML")
        );
        assert_eq!(content_type(None), None);
    }

    #[test]
    fn content_length_takes_the_first_digit_run() {
        use reqwest::header::{CONTENT_LENGTH, HeaderMap, HeaderValue};
        let length = |value: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(value) = value {
                headers.insert(CONTENT_LENGTH, HeaderValue::from_str(value).unwrap());
            }
            header_content_length(&headers)
        };
        assert_eq!(length(None), Some(None));
        assert_eq!(length(Some("123")), Some(Some(123)));
        assert_eq!(length(Some("xx12yy34")), Some(Some(12)));
        assert_eq!(length(Some("abc")), None);
        assert_eq!(
            length(Some("99999999999999999999999")),
            Some(Some(u64::MAX))
        );
    }

    #[test]
    fn og_slicing_follows_the_golden_routes() {
        let og = |body: &str| {
            opengraph_attributes(body.as_bytes())
                .into_iter()
                .collect::<HashMap<_, _>>()
        };
        // Later tags win; unknown attributes are sliced away.
        let found = og(
            "<html><head><meta property=\"og:image\" content=\"http://example.com/image.png\"><meta property=\"og:title\" content=\"First\"><meta property=\"og:description\" content=\"d\"><meta property=\"og:title\" content=\"Second\"><meta property=\"og:type\" content=\"article\"></head></html>",
        );
        assert_eq!(found.get("title").map(String::as_str), Some("Second"));
        assert!(!found.contains_key("type"));
        // `property` wins over `name`; the `og:` gate is case-sensitive.
        let found = og(
            "<html><head><meta property=\"description\" name=\"og:title\" content=\"Wrong\"><meta name=\"og:title\" content=\"Right\"><meta property=\"og:description\" content=\"d\"><meta property=\"OG:title\" content=\"Upper\"></head></html>",
        );
        assert_eq!(found.get("title").map(String::as_str), Some("Right"));
        assert_eq!(found.get("description").map(String::as_str), Some("d"));
        // `og:` is removed everywhere, and metas in the body count.
        let found = og(
            "<html><head><meta property=\"og:og:title\" content=\"Doubled\"><meta property=\"og:description\" content=\"d\"></head></html>",
        );
        assert_eq!(found.get("title").map(String::as_str), Some("Doubled"));
        let found = og(
            "<html><head></head><body><div><meta property=\"og:title\" content=\"In body\"><meta property=\"og:description\" content=\"d\"></div></body></html>",
        );
        assert_eq!(found.get("title").map(String::as_str), Some("In body"));
        // A tag gated by `name` still slices through `property`.
        let found = og(
            "<html><head><meta charset=\"utf-8\"><meta property=\"description\" name=\"og:title\" content=\"Wrong\"></head></html>",
        );
        assert_eq!(found.get("description").map(String::as_str), Some("Wrong"));
        assert!(!found.contains_key("title"));
        // Without a meta charset, non-ASCII is dropped.
        let found = og(
            "<html><head><meta property=\"og:title\" content=\"Café 😀 ok\"><meta property=\"og:description\" content=\"naïve &eacute; &#233; x\"></head></html>",
        );
        assert_eq!(found.get("title").map(String::as_str), Some("Caf  ok"));
        assert_eq!(
            found.get("description").map(String::as_str),
            Some("nave   x")
        );
        // With one, it survives.
        let found = og(
            "<html><head><meta charset=\"utf-8\"><meta property=\"og:title\" content=\"Café 😀 ok\"><meta property=\"og:description\" content=\"naïve &eacute; x\"></head></html>",
        );
        assert_eq!(found.get("title").map(String::as_str), Some("Café 😀 ok"));
        assert_eq!(
            found.get("description").map(String::as_str),
            Some("naïve é x")
        );
    }

    #[test]
    fn golden_titles_clean_end_to_end() {
        // `/entities`: decode, strip the nbsp without a charset, re-escape.
        let found = opengraph_attributes("<html><head><meta property=\"og:title\" content=\"Tom &amp; Jerry &lt;3 &nbsp;x\"><meta property=\"og:description\" content=\"a &gt; b \"q\" 'a'\"></head></html>".as_bytes());
        let get = |key: &str| {
            found
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| clean_text(v))
        };
        assert_eq!(get("title").as_deref(), Some("Tom &amp; Jerry &lt;3 x"));
        // The inner quote ends the value early, like upstream.
        assert_eq!(get("description").as_deref(), Some("a &gt; b "));
        // `/script` and `/encoded`: markup decodes, then strips away.
        assert_eq!(
            clean_text("Hey!<script>alert('hi')</script>"),
            "Hey!alert('hi')"
        );
        assert_eq!(
            clean_text("Hey!</script><img src=a onerror=prompt(1)>"),
            "Hey!"
        );
        // `/only-markup` strips to nothing, which fails validation.
        assert!(is_blank(&clean_text("<img src='x' onerror='alert(1)'/>")));
    }

    #[test]
    fn url_param_shapes_match_require() {
        assert!(matches!(
            url_param_json(&serde_json::json!({})),
            UrlParam::Missing
        ));
        assert!(matches!(
            url_param_json(&serde_json::json!({"url": null})),
            UrlParam::Missing
        ));
        assert!(matches!(
            url_param_json(&serde_json::json!({"url": ""})),
            UrlParam::Blank
        ));
        assert!(matches!(
            url_param_json(&serde_json::json!({"url": "http://example.com/"})),
            UrlParam::Value(_)
        ));
        assert!(matches!(
            url_param_json(&serde_json::json!({"url": {"a": "http://example.com/"}})),
            UrlParam::Collection
        ));
        let form = |pairs: &[(&str, &str)]| {
            url_param_form(
                &pairs
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect::<Vec<_>>(),
            )
        };
        assert!(matches!(form(&[]), UrlParam::Missing));
        assert!(matches!(form(&[("url", "")]), UrlParam::Blank));
        assert!(matches!(
            form(&[("url", "http://example.com/")]),
            UrlParam::Value(_)
        ));
        assert!(matches!(
            form(&[("url[a]", "http://example.com/")]),
            UrlParam::Collection
        ));
        assert!(matches!(
            form(&[("url[]", "http://example.com/")]),
            UrlParam::Collection
        ));
    }
}
