//! Hand-written parsers for Android shell/uiautomator output.
//!
//! These replace the former `regex` patterns (kept in the doc comments) so the
//! browser bundle does not carry a regex engine. They are target independent
//! and unit tested natively.

/// Regex `\w` (Unicode word character), close enough for identifiers in ADB output.
fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Decode `(?i)&(#x[0-9a-f]+|#\d+|amp|lt|gt|quot|apos);`. Named entities only
/// decode in lower case; invalid or NUL numeric references are dropped.
pub fn decode_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let tail = &rest[at + 1..];
        match entity(tail) {
            Some((len, replacement)) => {
                out.push_str(&replacement);
                rest = &tail[len..];
            }
            None => {
                out.push('&');
                rest = tail;
            }
        }
    }
    out.push_str(rest);
    out
}
/// Parse one entity body after `&`; returns the consumed length (including `;`).
fn entity(tail: &str) -> Option<(usize, String)> {
    let end = tail.find(';')?;
    let key = &tail[..end];
    let replacement = if let Some(hex) = key.strip_prefix("#x").or_else(|| key.strip_prefix("#X")) {
        if hex.is_empty() || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return None;
        }
        numeric(u32::from_str_radix(hex, 16).ok())
    } else if let Some(dec) = key.strip_prefix('#') {
        if dec.is_empty() || !dec.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        numeric(dec.parse::<u32>().ok())
    } else {
        match key {
            "amp" => "&".into(),
            "lt" => "<".into(),
            "gt" => ">".into(),
            "quot" => "\"".into(),
            "apos" => "'".into(),
            // Upper/mixed-case names were matched but left unchanged.
            _ if ["amp", "lt", "gt", "quot", "apos"].iter().any(|n| n.eq_ignore_ascii_case(key)) => {
                format!("&{key};")
            }
            _ => return None,
        }
    };
    Some((end + 1, replacement))
}
fn numeric(code: Option<u32>) -> String {
    code.and_then(char::from_u32).filter(|c| *c != '\0').map(String::from).unwrap_or_default()
}

/// Attribute bodies of `<node\s+([^<>]*)/?\s*>` elements, in document order.
pub fn node_bodies(xml: &str) -> impl Iterator<Item = &str> {
    let mut rest = xml;
    std::iter::from_fn(move || {
        loop {
            let at = rest.find("<node")?;
            let after = &rest[at + 5..];
            let body = after.trim_start();
            if body.len() == after.len() {
                // `\s+` requires at least one whitespace character.
                rest = after;
                continue;
            }
            match body.find(['<', '>']) {
                Some(end) if body.as_bytes()[end] == b'>' => {
                    rest = &body[end + 1..];
                    return Some(&body[..end]);
                }
                _ => rest = after,
            }
        }
    })
}

/// `([\w:-]+)="([^"]*)"` matches within one node body, as `(name, raw value)`.
pub fn attributes(body: &str) -> Vec<(&str, &str)> {
    let name_char = |c: char| word(c) || c == ':' || c == '-';
    let mut found = Vec::new();
    let mut index = 0;
    while index < body.len() {
        let rest = &body[index..];
        let Some(c) = rest.chars().next() else { break };
        if !name_char(c) {
            index += c.len_utf8();
            continue;
        }
        let name_len = rest.find(|c: char| !name_char(c)).unwrap_or(rest.len());
        let name = &rest[..name_len];
        let after = &rest[name_len..];
        if let Some(value) = after.strip_prefix("=\"")
            && let Some(close) = value.find('"')
        {
            found.push((name, &value[..close]));
            index += name_len + 2 + close + 1;
        } else {
            // Any suffix of this name run fails the same way.
            index += name_len;
        }
    }
    found
}

/// `^\[(\d+),(\d+)\]\[(\d+),(\d+)\]$`
pub fn bounds(value: &str) -> Option<[f64; 4]> {
    let inner = value.strip_prefix('[')?.strip_suffix(']')?;
    let (first, second) = inner.split_once("][")?;
    let pair = |s: &str| -> Option<(f64, f64)> {
        let (a, b) = s.split_once(',')?;
        Some((digits(a)?, digits(b)?))
    };
    let ((x, y), (right, bottom)) = (pair(first)?, pair(second)?);
    Some([x, y, right, bottom])
}
fn digits(s: &str) -> Option<f64> {
    (!s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse().unwrap_or(0.0))
}
/// Leading ASCII digits of `s` (at least one).
fn leading_number(s: &str) -> Option<(&str, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    (end > 0).then(|| s.split_at(end))
}

/// Viewport of display 0 from `dumpsys input`: `(width, height, orientation)`.
pub fn input_viewport(input: &str) -> Option<(u32, u32, u32)> {
    let line = input.lines().find(|s| {
        s.contains("Viewport") && display_zero(s) && s.contains("logicalFrame") && !s.contains("valid=false")
    })?;
    let (width, height) = logical_frame(line)?;
    let orientation = line
        .match_indices("orientation=")
        .find_map(|(at, key)| leading_number(&line[at + key.len()..]).map(|(n, _)| n.parse::<u32>().unwrap_or(0)))?;
    Some((width, height, orientation))
}
/// `displayId[=: ]+0\b`
fn display_zero(line: &str) -> bool {
    line.match_indices("displayId").any(|(at, key)| {
        let rest = &line[at + key.len()..];
        let value = rest.trim_start_matches(['=', ':', ' ']);
        value.len() < rest.len() && value.strip_prefix('0').is_some_and(|after| !after.chars().next().is_some_and(word))
    })
}
/// `logicalFrame=\[0,\s*0,\s*(\d+),\s*(\d+)\]`
fn logical_frame(line: &str) -> Option<(u32, u32)> {
    line.match_indices("logicalFrame=[0,").find_map(|(at, key)| {
        let rest = line[at + key.len()..].trim_start().strip_prefix("0,")?.trim_start();
        let (width, rest) = leading_number(rest)?;
        let (height, rest) = leading_number(rest.strip_prefix(',')?.trim_start())?;
        rest.starts_with(']').then(|| (width.parse().unwrap_or(0), height.parse().unwrap_or(0)))
    })
}

/// `^[A-Za-z][A-Za-z0-9_]*(?:\.[A-Za-z0-9_]+)+$`
pub fn package_id(app: &str) -> bool {
    let mut parts = app.split('.');
    let first = parts.next().unwrap_or_default();
    let segment = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    first.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
        && segment(first)
        && app.contains('.')
        && parts.all(segment)
}
/// `^[\w.$]+/[\w.$]+$`
pub fn component(value: &str) -> bool {
    let part = |s: &str| !s.is_empty() && s.chars().all(|c| word(c) || c == '.' || c == '$');
    value.split_once('/').is_some_and(|(a, b)| part(a) && part(b))
}
/// `^[\w.]+$`
pub fn loose_package(value: &str) -> bool {
    !value.is_empty() && value.chars().all(|c| word(c) || c == '.')
}
/// `(?i)Error:|Exception`
pub fn reports_error(output: &str) -> bool {
    let lower = output.to_lowercase();
    lower.contains("error:") || lower.contains("exception")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entities_decode_like_the_former_pattern() {
        assert_eq!(decode_xml("搜索 &amp; 设置"), "搜索 & 设置");
        assert_eq!(decode_xml("&lt;a&gt; &quot;q&quot; &apos;"), "<a> \"q\" '");
        assert_eq!(decode_xml("&#x4e2d;&#X4E2D;&#25991;"), "中中文");
        assert_eq!(decode_xml("&#0;&#x110000;x"), "x");
        assert_eq!(decode_xml("&AMP; &amp &nbsp; & &#12a; &#x;"), "&AMP; &amp &nbsp; & &#12a; &#x;");
        assert_eq!(decode_xml("&&amp;;"), "&&;");
    }

    #[test]
    fn hierarchy_nodes_and_attributes() {
        let xml = r#"<hierarchy><node text="a &amp; b" bounds="[0,0][1,2]"/><nodex a="1"/><node
            class="x" k:y-z="v" >"#;
        let bodies: Vec<_> = node_bodies(xml).collect();
        assert_eq!(bodies.len(), 2);
        assert_eq!(attributes(bodies[0]), vec![("text", "a &amp; b"), ("bounds", "[0,0][1,2]")]);
        assert_eq!(attributes(bodies[1]), vec![("class", "x"), ("k:y-z", "v")]);
        assert_eq!(attributes(r#"broken a ="1" b="2" c="open"#), vec![("b", "2")]);
        assert_eq!(node_bodies("<node a=\"1\" <node b=\"2\">").collect::<Vec<_>>(), vec!["b=\"2\""]);
        assert_eq!(bounds("[10,20][210,120]"), Some([10.0, 20.0, 210.0, 120.0]));
        for bad in ["[1,2][3]", "[1,2][3,4", "[a,2][3,4]", "[1,2] [3,4]", "[,2][3,4]"] {
            assert_eq!(bounds(bad), None, "{bad}");
        }
    }

    #[test]
    fn viewport_from_dumpsys_input() {
        let input = "  Viewport INTERNAL: displayId=0, uniqueId=local:1, port=1, orientation=1, logicalFrame=[0, 0, 2800, 1752], valid=true\n";
        assert_eq!(input_viewport(input), Some((2800, 1752, 1)));
        assert_eq!(input_viewport(&input.replace("displayId=0", "displayId=01")), None);
        assert_eq!(input_viewport(&input.replace("valid=true", "valid=false")), None);
        assert_eq!(input_viewport(&input.replace("orientation=1", "orientation=")), None);
        let second = "Viewport: displayId: 0 logicalFrame=[0,1,2,3] logicalFrame=[0,0,5,6] orientation=x orientation=3";
        assert_eq!(input_viewport(second), Some((5, 6, 3)));
    }

    #[test]
    fn identifiers_and_errors() {
        assert!(package_id("com.android.settings"));
        assert!(package_id("a.b_1"));
        for bad in ["settings", "1com.x", "com..x", "com.x.", "com.x-y", ".com.x"] {
            assert!(!package_id(bad), "{bad}");
        }
        assert!(component("com.android.settings/.Settings$Main"));
        assert!(!component("com.x/"));
        assert!(!component("com.x/a/b"));
        assert!(!component("com x/a"));
        assert!(loose_package("com.example_1"));
        assert!(!loose_package("com example"));
        assert!(!loose_package(""));
        assert!(reports_error("ERROR: no activity"));
        assert!(reports_error("java.lang.SecurityException"));
        assert!(!reports_error("Status: ok"));
    }
}
