//! Bounded, platform-independent lyrics normalization. No network, audio or clock owner.
use serde::{Deserialize, Serialize};
const MAX_TEXT: usize = 512 * 1024;
const MAX_TIME: u64 = 86_400_000;
const MAX_LINES: usize = 3000;
type Result<T> = std::result::Result<T, &'static str>;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Word {
    pub text: String,
    pub start_ms: u64,
    pub end_ms: u64,
    #[serde(default)]
    pub end_estimated: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Line {
    pub text: String,
    pub start_ms: Option<u64>,
    pub end_ms: Option<u64>,
    #[serde(default)]
    pub words: Vec<Word>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub background: Vec<Line>,
}
impl Line {
    fn untimed(text: &str) -> Self {
        Self {
            text: text.into(),
            start_ms: None,
            end_ms: None,
            words: vec![],
            background: vec![],
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Document {
    pub lines: Vec<Line>,
    pub instrumental: bool,
    pub precision: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    format: String,
    text: String,
    duration_ms: u64,
}
/// Stateless entry point used by host workers. Errors never contain the supplied lyrics.
pub fn parse_json(bytes: &[u8]) -> Result<Vec<u8>> {
    let request: Request = serde_json::from_slice(bytes).map_err(|_| "Invalid lyrics request")?;
    let document = parse(&request.format, &request.text, request.duration_ms)?;
    serde_json::to_vec(&document).map_err(|_| "Lyrics could not be encoded")
}
pub fn parse(format: &str, text: &str, duration: u64) -> Result<Document> {
    if text.is_empty() || text.len() > MAX_TEXT || duration > MAX_TIME || text.contains('\0') {
        return Err("Lyrics exceed supported limits");
    }
    let (lines, instrumental) = match format {
        "lrc" => (lrc(text)?, false),
        "ttml" => (ttml(text, duration)?, false),
        "ttml-amll" => (ttml::amll(text, duration)?, false),
        "lyricsfile" | "yaml" => lyricsfile(text)?,
        "plain" | "txt" => (text.lines().map(Line::untimed).collect(), false),
        _ => return Err("Unsupported lyrics format"),
    };
    normalize(lines, instrumental, duration, format == "ttml-amll")
}
fn normalize(
    mut lines: Vec<Line>,
    instrumental: bool,
    duration: u64,
    overlapping_words: bool,
) -> Result<Document> {
    fn all_lines(lines: &[Line]) -> impl Iterator<Item = &Line> {
        lines
            .iter()
            .flat_map(|line| std::iter::once(line).chain(&line.background))
    }
    let (mut text_size, mut line_count, mut word_count) = (0, 0, 0);
    for line in all_lines(&lines) {
        line_count += 1;
        word_count += line.words.len();
        if line_count > MAX_LINES || word_count > 30000 {
            return Err("Too many lyrics cues");
        }
        text_size += line.text.len() + line.words.iter().map(|w| w.text.len()).sum::<usize>();
        if line.words.len() > 1000
            || line.text.len() > 8192
            || line.words.iter().any(|w| w.text.len() > 2048)
            || text_size > MAX_TEXT * 2
        {
            return Err("Lyrics exceed supported limits");
        }
        if line.start_ms.is_some_and(|t| t > MAX_TIME) || line.end_ms.is_some_and(|t| t > MAX_TIME)
        {
            return Err("Invalid lyrics timing");
        }
        if !line.words.is_empty()
            && line
                .words
                .iter()
                .map(|w| w.text.as_str())
                .collect::<String>()
                != line.text
        {
            return Err("Word timing does not match line text");
        }
    }
    if lines.iter().any(|l| l.start_ms.is_some()) {
        if lines.iter().any(|l| l.start_ms.is_none()) {
            return Err("Mixed timed and untimed lines");
        }
        lines.sort_by_key(|l| l.start_ms);
        for i in 0..lines.len() {
            let next = lines.get(i + 1).and_then(|l| l.start_ms);
            let line = &mut lines[i];
            let start = line.start_ms.unwrap();
            if line.end_ms.is_none() {
                line.end_ms = next.or((duration > start).then_some(duration));
            }
            validate_intervals(line, start, overlapping_words)?;
            for backing in &line.background {
                match (backing.start_ms, backing.end_ms) {
                    (Some(start), Some(end)) if end <= duration && start < end => {
                        validate_intervals(backing, start, overlapping_words)?;
                    }
                    (None, None) if backing.words.is_empty() => {}
                    _ => return Err("Invalid background lyric interval"),
                }
            }
        }
    }
    let has_text = all_lines(&lines).any(|line| !line.text.trim().is_empty());
    if instrumental && has_text {
        return Err("Instrumental lyrics contain text");
    }
    let precision = if all_lines(&lines).any(|l| !l.words.is_empty()) {
        "word"
    } else if lines.iter().any(|l| l.start_ms.is_some()) {
        "line"
    } else {
        "plain"
    };
    if !instrumental && !has_text {
        return Err("Lyrics contain no text");
    }
    Ok(Document {
        lines,
        instrumental,
        precision: precision.into(),
    })
}
fn validate_intervals(line: &Line, start: u64, overlapping_words: bool) -> Result<()> {
    if line.end_ms.is_some_and(|end| end < start) {
        return Err("Invalid lyrics interval");
    }
    let mut previous = start;
    for word in &line.words {
        if word.start_ms < previous
            || word.end_ms <= word.start_ms
            || word.end_ms > MAX_TIME
            || line.end_ms.is_some_and(|end| word.end_ms > end)
        {
            return Err("Invalid word interval");
        }
        previous = if overlapping_words {
            word.start_ms
        } else {
            word.end_ms
        };
    }
    Ok(())
}
fn clock(text: &str) -> Option<u64> {
    let parts: Vec<_> = text.split(':').collect();
    if !(2..=3).contains(&parts.len()) {
        return None;
    }
    let mut seconds = 0.0;
    for (i, part) in parts.iter().enumerate() {
        let value: f64 = part.parse().ok()?;
        if !value.is_finite()
            || value < 0.0
            || (i > 0 && value >= 60.0)
            || (i + 1 < parts.len() && value.fract() != 0.0)
        {
            return None;
        }
        seconds = seconds * 60.0 + value;
    }
    (seconds <= 86400.0).then_some((seconds * 1000.0).round() as u64)
}
fn lrc_word_tags(text: &str) -> impl Iterator<Item = usize> + '_ {
    text.match_indices('<').filter_map(|(index, _)| {
        let candidate = &text[index + 1..];
        let suffix = candidate.trim_start_matches(|character: char| character.is_ascii_digit());
        (suffix.len() < candidate.len() && suffix.starts_with(':')).then_some(index)
    })
}
fn shifted_lrc_time(time: u64, offset: i64) -> u64 {
    (time as i64 + offset).max(0) as u64
}
fn lrc_line(text: &str, start_ms: u64, offset: i64, repeated: bool) -> Line {
    let mut tags = lrc_word_tags(text).peekable();
    let mut content = String::from(&text[..tags.peek().copied().unwrap_or(text.len())]);
    // A shared set of absolute word stamps cannot describe every repeated line.
    let mut has_word_timing = !repeated && content.is_empty();
    let mut previous: Option<(u64, &str)> = None;
    let mut words = vec![];
    while let Some(open) = tags.next() {
        let next = tags.peek().copied().unwrap_or(text.len());
        let tag_and_text = &text[open + 1..next];
        let Some((tag, sung)) = tag_and_text.split_once('>') else {
            has_word_timing = false;
            // An incomplete numeric stamp at the end carries no sung text.
            if !tag_and_text
                .chars()
                .all(|c| c.is_ascii_digit() || matches!(c, ':' | '.'))
            {
                content.push_str(&text[open..next]);
            }
            continue;
        };
        content.push_str(sung);
        let Some(start) = clock(tag).map(|time| shifted_lrc_time(time, offset)) else {
            has_word_timing = false;
            continue;
        };
        has_word_timing &= (start_ms..=MAX_TIME).contains(&start);
        if let Some((old, text)) = previous.replace((start, sung)) {
            has_word_timing &= start >= old;
            if !text.is_empty() {
                has_word_timing &= start > old;
                if has_word_timing {
                    words.push(Word {
                        text: text.into(),
                        start_ms: old,
                        end_ms: start,
                        end_estimated: true,
                    });
                }
            }
        }
    }
    // Keep the explicit line cue when its last word has no end, or when a
    // damaged word tag prevents safe karaoke timing for the whole line.
    if !has_word_timing || previous.is_some_and(|(_, text)| !text.is_empty()) {
        words.clear();
    }
    Line {
        text: content,
        start_ms: Some(start_ms),
        end_ms: words.last().map(|word| word.end_ms),
        words,
        background: vec![],
    }
}
fn lrc_metadata(tag: &str) -> bool {
    tag.split_once(':').is_some_and(|(name, _)| {
        name == "#"
            || (name.starts_with(|c: char| c.is_ascii_alphabetic())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_')))
    })
}
fn lrc(text: &str) -> Result<Vec<Line>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut result = vec![];
    let mut untimed = vec![];
    let mut timed_text_size = 0;
    let mut offset: i64 = 0;
    for raw in text.lines() {
        if let Some(value) = raw
            .trim()
            .strip_prefix("[offset:")
            .and_then(|s| s.strip_suffix(']'))
        {
            offset = value.parse().map_err(|_| "Invalid LRC offset")?;
            if offset.unsigned_abs() > MAX_TIME {
                return Err("Invalid LRC offset");
            }
        }
    }
    for raw in text.lines() {
        let mut rest = raw.trim();
        let mut times = vec![];
        while rest.starts_with('[') {
            let Some(end) = rest.find(']') else {
                return Err("Unclosed LRC timestamp");
            };
            let stamp = &rest[1..end];
            if let Some(time) = clock(stamp) {
                times.push(time);
            } else if stamp.contains(':')
                && stamp.chars().next().is_some_and(|c| c.is_ascii_digit())
            {
                return Err("Invalid LRC timestamp");
            } else if !lrc_metadata(stamp) {
                break;
            }
            rest = &rest[end + 1..];
        }
        let rest = rest.trim();
        if times.is_empty() {
            if !rest.is_empty() || raw.trim().is_empty() {
                untimed.push(rest);
            }
            continue;
        }
        if result.len() + times.len() > MAX_LINES {
            return Err("Too many lyrics cues");
        }
        let repeated = times.len() > 1;
        let line = lrc_line(rest, shifted_lrc_time(times[0], offset), offset, repeated);
        timed_text_size += line.text.len() * times.len();
        if timed_text_size > MAX_TEXT * 2 {
            return Err("Lyrics exceed supported limits");
        }
        if repeated {
            result.extend(times.into_iter().map(|time| Line {
                start_ms: Some(shifted_lrc_time(time, offset)),
                ..line.clone()
            }));
        } else {
            result.push(line);
        }
    }
    if result.is_empty() {
        if untimed.len() > MAX_LINES {
            return Err("Too many lyrics cues");
        }
        if untimed
            .iter()
            .any(|line| lrc_word_tags(line).next().is_some())
        {
            return Err("Enhanced LRC needs line timestamps");
        }
        return Ok(untimed.into_iter().map(Line::untimed).collect());
    }
    Ok(result)
}
#[derive(Deserialize)]
struct File {
    version: String,
    #[serde(default)]
    metadata: Metadata,
    #[serde(default)]
    lines: Vec<FileLine>,
    plain: Option<String>,
}
#[derive(Default, Deserialize)]
struct Metadata {
    #[serde(default)]
    instrumental: bool,
}
#[derive(Deserialize)]
struct FileLine {
    text: String,
    start_ms: Option<u64>,
    end_ms: Option<u64>,
    #[serde(default)]
    words: Vec<FileWord>,
}
#[derive(Deserialize)]
struct FileWord {
    text: String,
    start_ms: u64,
    end_ms: Option<u64>,
}
fn has_yaml_references(text: &str) -> bool {
    let mut quote = None;
    let mut escaped = false;
    let mut previous = ' ';
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if q == '"' && !escaped && c == '\\' {
                escaped = true;
                continue;
            }
            if c == q && !escaped {
                if q == '\'' && chars.peek() == Some(&'\'') {
                    chars.next();
                } else {
                    quote = None;
                }
            }
            escaped = false;
        } else {
            let boundary = previous.is_whitespace() || ":,[{-".contains(previous);
            if boundary && c == '#' {
                for comment in chars.by_ref() {
                    if comment == '\n' {
                        break;
                    }
                }
                previous = ' ';
                continue;
            }
            if boundary && (c == '\'' || c == '"') {
                quote = Some(c);
            } else if boundary && "&*!".contains(c) {
                return true;
            }
        }
        previous = c;
    }
    false
}
fn lyricsfile(text: &str) -> Result<(Vec<Line>, bool)> {
    // Disallow aliases/tags before deserialization to bound expansion independently of YAML's parser.
    if has_yaml_references(text) {
        return Err("YAML aliases and tags are unsupported");
    }
    let file: File = serde_yaml_ng::from_str(text).map_err(|_| "Invalid Lyricsfile YAML")?;
    if file.version != "1.0" {
        return Err("Unsupported Lyricsfile version");
    }
    if file.lines.is_empty() && !file.metadata.instrumental {
        return Ok((
            file.plain
                .unwrap_or_default()
                .lines()
                .map(|s| Line {
                    text: s.into(),
                    start_ms: None,
                    end_ms: None,
                    words: vec![],
                    background: vec![],
                })
                .collect(),
            false,
        ));
    }
    let mut lines = vec![];
    for line in file.lines {
        let mut words = vec![];
        for (i, word) in line.words.iter().enumerate() {
            let end = word
                .end_ms
                .or_else(|| line.words.get(i + 1).map(|w| w.start_ms))
                .or(line.end_ms)
                .ok_or("Missing word end")?;
            words.push(Word {
                text: word.text.clone(),
                start_ms: word.start_ms,
                end_ms: end,
                end_estimated: word.end_ms.is_none(),
            });
        }
        lines.push(Line {
            text: line.text,
            start_ms: line.start_ms,
            end_ms: line.end_ms,
            words,
            background: vec![],
        });
    }
    Ok((lines, file.metadata.instrumental))
}
mod ttml;
use ttml::ttml;
#[cfg(test)]
mod tests;
