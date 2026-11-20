use super::{Line, Result, Word, clock};
use roxmltree::{Document, Node, ParsingOptions};

const TTML_NAMESPACE: &str = "http://www.w3.org/ns/ttml";
const METADATA_NAMESPACE: &str = "http://www.w3.org/ns/ttml#metadata";

fn ttml_tag(node: Node<'_, '_>, name: &str) -> bool {
    node.has_tag_name(name)
        && (matches!(node.tag_name().namespace(), None | Some(TTML_NAMESPACE))
            || node.tag_name().namespace() == node.document().root_element().tag_name().namespace())
}

fn non_lyric_content(node: Node<'_, '_>) -> bool {
    ttml_tag(node, "head")
        || ttml_tag(node, "metadata")
        || node.tag_name().namespace() == Some(METADATA_NAMESPACE)
        || node
            .tag_name()
            .namespace()
            .and_then(|namespace| namespace.strip_suffix("#metadata"))
            .is_some_and(|namespace| {
                Some(namespace) == node.document().root_element().tag_name().namespace()
            })
}

fn time(text: &str, frame_rate: f64, tick_rate: f64) -> Result<u64> {
    if text.contains(':') {
        return clock(text).ok_or("Unsupported TTML clock time");
    }
    for (suffix, scale) in [
        ("ms", 1.0),
        ("h", 3_600_000.0),
        ("m", 60_000.0),
        ("s", 1000.0),
        ("f", 1000.0 / frame_rate),
        ("t", 1000.0 / tick_rate),
    ] {
        if let Some(value) = text
            .strip_suffix(suffix)
            .and_then(|n| n.parse::<f64>().ok())
        {
            let ms = value * scale;
            if ms.is_finite() && (0.0..=86_400_000.0).contains(&ms) {
                return Ok(ms.round() as u64);
            }
        }
    }
    Err("Unsupported TTML time")
}
fn interval(node: Node<'_, '_>, frame: f64, tick: f64) -> Result<(u64, Option<u64>)> {
    let mut start = 0u64;
    let mut end: Option<u64> = None;
    let mut ancestors: Vec<_> = node.ancestors().filter(|n| n.is_element()).collect();
    if ancestors.len() > 64 {
        return Err("TTML nesting limit");
    }
    ancestors.reverse();
    for part in ancestors {
        if part.attribute("timeContainer").is_some_and(|s| s != "par") {
            return Err("Sequential TTML containers are unsupported");
        }
        let parent_start = start;
        start = start
            .checked_add(
                part.attribute("begin")
                    .map(|s| time(s, frame, tick))
                    .transpose()?
                    .unwrap_or(0),
            )
            .ok_or("Invalid TTML interval")?;
        let own_end = part
            .attribute("end")
            .map(|s| time(s, frame, tick).map(|t| parent_start + t))
            .transpose()?;
        let duration_end = part
            .attribute("dur")
            .map(|s| time(s, frame, tick).map(|t| start + t))
            .transpose()?;
        end = [end, own_end, duration_end].into_iter().flatten().min();
    }
    if start > 86_400_000 || end.is_some_and(|end| end < start) {
        return Err("Invalid TTML interval");
    }
    Ok((start, end))
}
pub(super) fn ttml(text: &str, duration: u64) -> Result<Vec<Line>> {
    parse(text, duration, false)
}

/// AMLL explicitly uses song-absolute times, unlike W3C TTML parent-relative times.
/// This dialect is selected by the host, never guessed from numeric timestamps.
pub(super) fn amll(text: &str, duration: u64) -> Result<Vec<Line>> {
    parse(text, duration, true)
}

fn absolute_time(text: &str) -> Result<u64> {
    if text.ends_with(['f', 't']) {
        return Err("Frame and tick clocks are unsupported in absolute lyrics");
    }
    if let Ok(seconds) = text.parse::<f64>() {
        let ms = seconds * 1000.0;
        if ms.is_finite() && (0.0..=86_400_000.0).contains(&ms) {
            return Ok(ms.round() as u64);
        }
        return Err("Invalid absolute lyrics time");
    }
    time(text, 30.0, 1.0)
}

fn absolute_interval<'a, 'input>(
    node: Node<'a, 'input>,
    enclosing_lyric: Option<Node<'a, 'input>>,
) -> Result<(u64, Option<u64>)> {
    let mut ancestors: Vec<_> = node.ancestors().filter(|n| n.is_element()).collect();
    if ancestors.len() > 64 {
        return Err("TTML nesting limit");
    }
    ancestors.reverse();
    let (mut start, mut end) = (0, None);
    for part in ancestors {
        // Backing vocals share the section, but can start before or end after the lead.
        if Some(part) == enclosing_lyric {
            continue;
        }
        if part.attribute("timeContainer").is_some_and(|s| s != "par") {
            return Err("Sequential TTML containers are unsupported");
        }
        let own_start = part
            .attribute("begin")
            .map(absolute_time)
            .transpose()?
            .unwrap_or(start);
        let own_end = part.attribute("end").map(absolute_time).transpose()?;
        let duration_end = part
            .attribute("dur")
            .map(absolute_time)
            .transpose()?
            .map(|d| own_start + d);
        let own_end = [own_end, duration_end].into_iter().flatten().min().or(end);
        if own_start < start
            || own_end.is_some_and(|e| e < own_start || e > 86_400_000)
            || end.is_some_and(|parent| own_start > parent || own_end.is_some_and(|e| e > parent))
        {
            return Err("Absolute lyrics exceed their parent interval");
        }
        (start, end) = (own_start, own_end);
    }
    Ok((start, end))
}

fn auxiliary(node: Node<'_, '_>) -> bool {
    matches!(role(node), Some("x-translation" | "x-roman"))
}

fn backing(node: Node<'_, '_>) -> bool {
    ttml_tag(node, "span") && role(node) == Some("x-bg")
}

fn role<'a>(node: Node<'a, '_>) -> Option<&'a str> {
    node.attribute((METADATA_NAMESPACE, "role"))
}

fn parse(text: &str, duration: u64, absolute: bool) -> Result<Vec<Line>> {
    let document = Document::parse_with_options(
        text,
        ParsingOptions {
            allow_dtd: false,
            nodes_limit: 50000,
            ..Default::default()
        },
    )
    .map_err(|_| "Invalid TTML document")?;
    let root = document.root_element();
    if !ttml_tag(root, "tt") {
        return Err("Expected a TTML document");
    }
    let body = root.children().find(|&n| ttml_tag(n, "body"));
    let lyrics_root = if absolute {
        let body = body.ok_or("Missing lyrics body")?;
        // The optional body duration bounds the lyrics, which may end before the audio.
        let (_, body_end) = absolute_interval(body, None)?;
        if duration == 0 || body_end.is_some_and(|end| end > duration) {
            return Err("Lyrics body exceeds recording duration");
        }
        if root.descendants().any(|n| {
            n.attributes().any(|a| a.name() == "ruby") && !n.ancestors().any(non_lyric_content)
        }) {
            return Err("Ruby lyrics are unsupported");
        }
        body
    } else {
        // Standard TTML renders the body. Keep supporting bodyless local files.
        body.unwrap_or(root)
    };
    let attr = |name| {
        root.attributes()
            .find(|a| a.name() == name)
            .map(|a| a.value())
    };
    if attr("timeBase").is_some_and(|s| s != "media") {
        return Err("Only media time TTML is supported");
    }
    let frame: f64 = attr("frameRate")
        .unwrap_or("30")
        .parse()
        .map_err(|_| "Invalid frame rate")?;
    let tick: f64 = attr("tickRate")
        .unwrap_or("1")
        .parse()
        .map_err(|_| "Invalid tick rate")?;
    if !frame.is_finite()
        || frame <= 0.0
        || !tick.is_finite()
        || tick <= 0.0
        || attr("frameRateMultiplier").is_some()
    {
        return Err("Unsupported TTML timing parameters");
    }
    let mut lines = vec![];
    let bounds = |node| {
        if absolute {
            absolute_interval(node, None)
        } else {
            interval(node, frame, tick)
        }
    };
    for p in lyrics_root.descendants().filter(|&n| {
        ttml_tag(n, "p")
            && !n
                .ancestors()
                .take_while(|parent| *parent != lyrics_root)
                .any(non_lyric_content)
    }) {
        let (start, end) = bounds(p)?;
        if absolute && (start >= duration || end.is_some_and(|end| end > duration)) {
            return Err("Lyrics cue exceeds recording duration");
        }
        let end = end.or((duration > start).then_some(duration));
        let (text, words) = row_content(p, absolute, bounds)?;
        let mut background = Vec::new();
        if absolute {
            for node in p.descendants().skip(1).filter(|&node| backing(node)) {
                let ancestors = node.ancestors().skip(1).take_while(|n| *n != p);
                if ancestors
                    .clone()
                    .any(|n| non_lyric_content(n) || auxiliary(n))
                {
                    continue;
                }
                if ancestors.clone().any(backing) || background.len() >= 8 {
                    return Err("Unsupported background vocal nesting or count");
                }
                let backing_bounds = |node| absolute_interval(node, Some(p));
                let (start, end) = backing_bounds(node)?;
                if start >= duration || end.is_some_and(|end| end > duration) {
                    return Err("Background vocal exceeds recording duration");
                }
                let (text, words) = row_content(node, true, backing_bounds)?;
                if text.trim().is_empty() {
                    continue;
                }
                let explicit_end =
                    node.attribute("end").is_some() || node.attribute("dur").is_some();
                let start = node
                    .attribute("begin")
                    .map(|_| start)
                    .or_else(|| words.first().map(|w| w.start_ms));
                let end = end
                    .filter(|_| explicit_end)
                    .or_else(|| words.iter().map(|w| w.end_ms).max());
                let interval = start.zip(end);
                background.push(Line {
                    text,
                    start_ms: interval.map(|(start, _)| start),
                    end_ms: interval.map(|(_, end)| end),
                    words,
                    background: vec![],
                });
            }
        }
        lines.push(Line {
            text,
            start_ms: Some(start),
            end_ms: end,
            words,
            background,
        });
    }
    Ok(lines)
}

fn row_content<'a, 'input>(
    row: Node<'a, 'input>,
    absolute: bool,
    bounds: impl Fn(Node<'a, 'input>) -> Result<(u64, Option<u64>)>,
) -> Result<(String, Vec<Word>)> {
    let mut content = String::new();
    let mut words: Vec<Word> = vec![];
    let mut untimed_text = false;
    let mut last_break_end = 0;
    for node in row.descendants().skip(1) {
        let line_break = ttml_tag(node, "br");
        if !line_break && !node.is_text() {
            continue;
        }
        if node
            .ancestors()
            .take_while(|n| *n != row)
            .any(|n| non_lyric_content(n) || absolute && (auxiliary(n) || backing(n)))
        {
            continue;
        }
        if line_break {
            content.push('\n');
            last_break_end = content.len();
            if let Some(previous) = words.last_mut() {
                previous.text.push('\n');
            }
            continue;
        }
        let raw = node.text().unwrap_or("");
        let preserve = node.ancestors().any(|n| {
            n.attribute(("http://www.w3.org/XML/1998/namespace", "space")) == Some("preserve")
        });
        let mut part: String = if preserve {
            raw.into()
        } else {
            let mut s = String::new();
            for c in raw.chars() {
                if c.is_whitespace() {
                    if !s.ends_with(' ') {
                        s.push(' ');
                    }
                } else {
                    s.push(c);
                }
            }
            s
        };
        if !preserve && (content.is_empty() || content.ends_with([' ', '\n'])) {
            part = part.trim_start_matches(' ').into();
        }
        if part.is_empty() {
            continue;
        }
        let span = node
            .ancestors()
            .take_while(|n| *n != row)
            .find(|&n| ttml_tag(n, "span") && n.attribute("begin").is_some());
        let span = if let Some(span) = span
            && absolute
            && part.trim().is_empty()
            && layout_separator(span, &bounds)?
        {
            None
        } else {
            span
        };
        content.push_str(&part);
        if let Some(span) = span {
            let (word_start, word_end) = bounds(span)?;
            let word_end = word_end.ok_or("TTML word needs an end time")?;
            if let Some(previous) = words
                .last_mut()
                .filter(|w| w.start_ms == word_start && w.end_ms == word_end)
            {
                previous.text.push_str(&part);
            } else {
                // Layout whitespace before the first timed span has no cue of its own.
                let text = if words.is_empty() && !untimed_text {
                    content.clone()
                } else {
                    part
                };
                words.push(Word {
                    text,
                    start_ms: word_start,
                    end_ms: word_end,
                    end_estimated: span.attribute("end").is_none()
                        && span.attribute("dur").is_none(),
                });
            }
        } else if part.trim().is_empty() {
            if let Some(previous) = words.last_mut() {
                previous.text.push_str(&part);
            }
        } else {
            untimed_text = true;
        }
    }
    if row.attribute(("http://www.w3.org/XML/1998/namespace", "space")) != Some("preserve") {
        // Keep explicit trailing breaks while trimming ordinary trailing whitespace.
        let end = content.trim_end().len().max(last_break_end);
        let mut trimmed = content.len() - end;
        content.truncate(end);
        if !untimed_text {
            for word in words.iter_mut().rev() {
                if trimmed == 0 {
                    break;
                }
                let bytes = trimmed.min(word.text.len());
                word.text.truncate(word.text.len() - bytes);
                trimmed -= bytes;
            }
        }
    }
    // Preserve complete text if an import mixes timed spans and untimed vocals.
    if untimed_text || words.iter().map(|w| w.text.as_str()).collect::<String>() != content {
        words.clear();
    }
    Ok((content, words))
}

fn layout_separator<'a, 'input>(
    span: Node<'a, 'input>,
    bounds: &impl Fn(Node<'a, 'input>) -> Result<(u64, Option<u64>)>,
) -> Result<bool> {
    let (Some(begin), Some(end)) = (span.attribute("begin"), span.attribute("end")) else {
        return Ok(false);
    };
    let start = absolute_time(begin)?;
    if start != absolute_time(end)?
        || span.attribute("dur").is_some()
        || span
            .attribute("timeContainer")
            .is_some_and(|container| container != "par")
    {
        return Ok(false);
    }
    // AMLL explicitly permits 0/0 space spans inside later lyric intervals.
    if start == 0 {
        bounds(span.parent_element().ok_or("Invalid lyric separator")?)?;
    } else {
        bounds(span)?;
    }
    Ok(true)
}
