//! Standalone image syntax, references, and source resolution.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD_NO_PAD as BASE64_STANDARD_NO_PAD;
use gpui::{Image, ImageFormat, SharedUri};
use url::Url;

use crate::net;

/// Active fenced code block while scanning for image reference definitions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct FenceInfo {
    ch: char,
    len: usize,
}

/// HTML block start that suppresses reference-definition scanning.
enum HtmlBlockStart {
    /// HTML comment beginning with `<!--`.
    Comment,
    /// HTML tag block whose closing behavior depends on the tag.
    Tag {
        name: String,
        self_closing: bool,
        closes_same_line: bool,
    },
}

/// Parsed standalone image expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageSyntax {
    pub(crate) alt: String,
    pub(crate) target: ImageTarget,
}

/// Inline image/text segment used only by native table-cell rendering.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TableCellInlineImageSegment {
    Text(String),
    Image {
        markdown: String,
        syntax: ImageSyntax,
    },
}

/// Image target form before reference resolution.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageTarget {
    /// Direct image target from `![alt](src "title")`.
    Direct { src: String, title: Option<String> },
    /// Reference image target from `![alt][label]`.
    Reference { label: String },
}

/// Global reference definition for a reference-style image label.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageReferenceDefinition {
    pub(crate) src: String,
    pub(crate) title: Option<String>,
}

pub(crate) type ImageReferenceDefinitions = HashMap<String, ImageReferenceDefinition>;

/// Resolved image target ready for path or URL loading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedImageTarget {
    pub(crate) src: String,
    pub(crate) title: Option<String>,
}

/// Concrete image source after local-path or remote-URL classification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ImageResolvedSource {
    /// Filesystem path resolved relative to the current document, when possible.
    Local(PathBuf),
    /// HTTP(S) image URL handled by GPUI's HTTP client.
    Remote(SharedUri),
    /// Bytes carried inline by a `data:` URI. Needs no IO at all, so it renders
    /// straight from memory.
    Inline(Arc<Image>),
    /// A source recognized as a `data:` URI but unusable — malformed payload,
    /// non-image media type, or past [`MAX_INLINE_IMAGE_BYTES`]. Distinct from
    /// `Local` so the placeholder never claims a filesystem path built out of a
    /// multi-kilobyte base64 blob.
    Unusable,
}

/// Ceiling on decoded `data:` URI payloads.
///
/// A `data:` image is re-decoded on each render rather than memoized, which is
/// fine for the icons and badges people actually inline (GPUI already pays an
/// O(n) content hash per frame to key its own cache). This cap keeps a
/// pathologically large inline blob from turning that into per-frame work.
const MAX_INLINE_IMAGE_BYTES: usize = 4 * 1024 * 1024;

impl ImageSyntax {
    pub(crate) fn resolve_target(
        &self,
        reference_definitions: &ImageReferenceDefinitions,
    ) -> Option<ResolvedImageTarget> {
        match &self.target {
            ImageTarget::Direct { src, title } => Some(ResolvedImageTarget {
                src: src.clone(),
                title: title.clone(),
            }),
            ImageTarget::Reference { label } => {
                let definition = reference_definitions.get(label)?;
                Some(ResolvedImageTarget {
                    src: definition.src.clone(),
                    title: definition.title.clone(),
                })
            }
        }
    }
}

pub(crate) fn resolve_image_source(source: &str, base_dir: Option<&Path>) -> ImageResolvedSource {
    let source = source.trim();

    if let Some(payload) = source.strip_prefix("data:") {
        return decode_data_uri(payload);
    }

    if net::is_remote_image_source(source) {
        return ImageResolvedSource::Remote(SharedUri::from(source.to_string()));
    }

    // Protocol-relative URLs inherit the page scheme on the web; there is no
    // page here, so assume the secure one rather than reading `//host/x.png` as
    // a filesystem path.
    if let Some(authority) = source.strip_prefix("//")
        && !authority.is_empty()
    {
        return ImageResolvedSource::Remote(SharedUri::from(format!("https://{authority}")));
    }

    if let Some(path) = file_url_to_path(source) {
        return ImageResolvedSource::Local(path);
    }

    let decoded = percent_decode_path(source);
    let path = Path::new(&decoded);
    if path.is_absolute() {
        return ImageResolvedSource::Local(path.to_path_buf());
    }

    let resolved = base_dir
        .map(|dir| dir.join(path))
        .unwrap_or_else(|| path.to_path_buf());
    ImageResolvedSource::Local(resolved)
}

/// Turns a `file://` URL into a local path, including percent-decoding.
///
/// Anything that is not a `file:` URL returns `None` so the caller falls
/// through to ordinary relative-path handling.
fn file_url_to_path(source: &str) -> Option<PathBuf> {
    if !source.starts_with("file:") {
        return None;
    }
    Url::parse(source).ok()?.to_file_path().ok()
}

/// Decodes the payload of a `data:` URI into renderable image bytes.
///
/// Handles both the base64 form (`data:image/png;base64,…`) and the plain form
/// (`data:image/svg+xml,<svg …>`), the latter being how inline SVG is usually
/// written. Returns [`ImageResolvedSource::Unusable`] rather than a bogus path
/// when the payload cannot be used.
fn decode_data_uri(payload: &str) -> ImageResolvedSource {
    let Some((metadata, data)) = payload.split_once(',') else {
        return ImageResolvedSource::Unusable;
    };

    let mut parameters = metadata.split(';').map(str::trim);
    let media_type = parameters.next().unwrap_or_default().to_ascii_lowercase();
    let is_base64 = parameters.any(|parameter| parameter.eq_ignore_ascii_case("base64"));

    let Some(format) = data_uri_image_format(&media_type) else {
        return ImageResolvedSource::Unusable;
    };

    let bytes = if is_base64 {
        // Whitespace is legal in a base64 data URI after line wrapping, and the
        // standard alphabet's padding is often omitted, so tolerate both.
        let compact = data
            .chars()
            .filter(|ch| !ch.is_ascii_whitespace())
            .collect::<String>();
        match BASE64_STANDARD_NO_PAD.decode(compact.trim_end_matches('=')) {
            Ok(bytes) => bytes,
            Err(_) => return ImageResolvedSource::Unusable,
        }
    } else {
        percent_decode_bytes(data)
    };

    if bytes.is_empty() || bytes.len() > MAX_INLINE_IMAGE_BYTES {
        return ImageResolvedSource::Unusable;
    }

    ImageResolvedSource::Inline(Arc::new(Image::from_bytes(format, bytes)))
}

/// Maps a `data:` URI media type onto the GPUI image format that decodes it.
///
/// Limited to what `gpui::ImageFormat` can express, so e.g. an `image/x-icon`
/// payload is reported unusable and renders as a placeholder rather than being
/// mislabeled as some other format.
fn data_uri_image_format(media_type: &str) -> Option<ImageFormat> {
    match media_type {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" | "image/jpg" => Some(ImageFormat::Jpeg),
        "image/gif" => Some(ImageFormat::Gif),
        "image/webp" => Some(ImageFormat::Webp),
        "image/bmp" => Some(ImageFormat::Bmp),
        "image/tiff" => Some(ImageFormat::Tiff),
        "image/svg+xml" => Some(ImageFormat::Svg),
        _ => None,
    }
}

/// Percent-decodes a destination for filesystem use.
///
/// Markdown destinations are URL-ish, so `my%20image.png` means `my image.png`.
/// This is deliberately pure — no existence probing — because it runs on the
/// render path. The trade-off is a real file whose name literally contains a
/// valid escape (`report%20v2.png` on disk) would be looked up decoded; that is
/// rare, and CommonMark says the encoded reading is the correct one.
fn percent_decode_path(source: &str) -> String {
    String::from_utf8(percent_decode_bytes(source)).unwrap_or_else(|_| source.to_string())
}

fn percent_decode_bytes(source: &str) -> Vec<u8> {
    let bytes = source.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let Some(high) = (bytes[index + 1] as char).to_digit(16)
            && let Some(low) = (bytes[index + 2] as char).to_digit(16)
        {
            output.push((high * 16 + low) as u8);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    output
}

pub(crate) fn parse_standalone_image(markdown: &str) -> Option<ImageSyntax> {
    if markdown.contains('\n') || markdown.contains('\r') {
        return None;
    }
    let markdown = markdown.trim();
    if markdown.is_empty() {
        return None;
    }
    if !markdown.starts_with("![") {
        return None;
    }

    let bytes = markdown.as_bytes();
    let mut alt_end = None;
    for index in 2..bytes.len() {
        if bytes[index] == b']' && !is_escaped(markdown, index) {
            alt_end = Some(index);
            break;
        }
    }
    let alt_end = alt_end?;

    let alt = unescape_ascii_punctuation(&markdown[2..alt_end]);
    match bytes.get(alt_end + 1) {
        Some(b'(') => {
            let close_paren = find_matching_target_close_paren(markdown, alt_end + 1)?;
            if close_paren != markdown.len() - 1 {
                return None;
            }
            let inner = &markdown[alt_end + 2..close_paren];
            let (src, title) = parse_image_target(inner)?;
            Some(ImageSyntax {
                alt,
                target: ImageTarget::Direct { src, title },
            })
        }
        Some(b'[') => {
            let close_bracket = find_unescaped_char(markdown, alt_end + 2, b']')?;
            if close_bracket != markdown.len() - 1 {
                return None;
            }
            let raw_label = &markdown[alt_end + 2..close_bracket];
            let label_source = if raw_label.is_empty() {
                alt.as_str()
            } else {
                raw_label
            };
            let label = normalize_reference_label(label_source)?;
            Some(ImageSyntax {
                alt,
                target: ImageTarget::Reference { label },
            })
        }
        None => {
            let label = normalize_reference_label(&alt)?;
            Some(ImageSyntax {
                alt,
                target: ImageTarget::Reference { label },
            })
        }
        _ => None,
    }
}

pub(crate) fn parse_table_cell_inline_images(markdown: &str) -> Vec<TableCellInlineImageSegment> {
    let mut segments = Vec::new();
    let mut text_start = 0usize;
    let mut cursor = 0usize;
    let mut found_image = false;

    while cursor < markdown.len() {
        if markdown[cursor..].starts_with("![")
            && !is_escaped(markdown, cursor)
            && let Some((image_markdown, syntax, end)) = parse_inline_image_at(markdown, cursor)
        {
            if is_link_wrapped_inline_image(markdown, cursor, end) {
                cursor += markdown[cursor..]
                    .chars()
                    .next()
                    .map(char::len_utf8)
                    .unwrap_or(1);
                continue;
            }

            if text_start < cursor {
                segments.push(TableCellInlineImageSegment::Text(
                    markdown[text_start..cursor].to_string(),
                ));
            }
            segments.push(TableCellInlineImageSegment::Image {
                markdown: image_markdown,
                syntax,
            });
            found_image = true;
            cursor = end;
            text_start = cursor;
            continue;
        }

        cursor += markdown[cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }

    if text_start < markdown.len() {
        segments.push(TableCellInlineImageSegment::Text(
            markdown[text_start..].to_string(),
        ));
    }

    if found_image {
        segments
    } else {
        vec![TableCellInlineImageSegment::Text(markdown.to_string())]
    }
}

/// Parses one Markdown image starting at `start`, returning its raw source, the
/// parsed syntax, and the byte offset just past it.
///
/// Shared by table-cell inline image scanning and by the inline text tree, which
/// renders `![alt](src)` as a widget inside otherwise-editable text.
pub(crate) fn parse_inline_image_at(
    markdown: &str,
    start: usize,
) -> Option<(String, ImageSyntax, usize)> {
    if !markdown[start..].starts_with("![") {
        return None;
    }

    let alt_end = find_unescaped_char(markdown, start + 2, b']')?;
    let alt = unescape_ascii_punctuation(&markdown[start + 2..alt_end]);
    let next = markdown.as_bytes().get(alt_end + 1).copied();

    match next {
        Some(b'(') => {
            // Depth- and title-aware, so `![a](pic(1).png "A ) title")` closes on
            // the right paren instead of the first one.
            let close = find_matching_target_close_paren(markdown, alt_end + 1)?;
            let inner = &markdown[alt_end + 2..close];
            let (src, title) = parse_image_target(inner)?;
            let end = close + 1;
            Some((
                markdown[start..end].to_string(),
                ImageSyntax {
                    alt,
                    target: ImageTarget::Direct { src, title },
                },
                end,
            ))
        }
        Some(b'[') => {
            let close = find_unescaped_char(markdown, alt_end + 2, b']')?;
            let raw_label = &markdown[alt_end + 2..close];
            let label_source = if raw_label.is_empty() {
                alt.as_str()
            } else {
                raw_label
            };
            let label = normalize_reference_label(label_source)?;
            let end = close + 1;
            Some((
                markdown[start..end].to_string(),
                ImageSyntax {
                    alt,
                    target: ImageTarget::Reference { label },
                },
                end,
            ))
        }
        _ => {
            let label = normalize_reference_label(&alt)?;
            let end = alt_end + 1;
            Some((
                markdown[start..end].to_string(),
                ImageSyntax {
                    alt,
                    target: ImageTarget::Reference { label },
                },
                end,
            ))
        }
    }
}

fn is_link_wrapped_inline_image(markdown: &str, start: usize, end: usize) -> bool {
    let mut cursor = 0usize;
    let mut open_label = None;
    while cursor < start {
        let byte = markdown.as_bytes()[cursor];
        if byte == b'[' && !is_escaped(markdown, cursor) {
            open_label = Some(cursor);
        } else if byte == b']' && !is_escaped(markdown, cursor) {
            open_label = None;
        }
        cursor += markdown[cursor..]
            .chars()
            .next()
            .map(char::len_utf8)
            .unwrap_or(1);
    }

    open_label.is_some() && markdown[end..].starts_with("](")
}

pub(crate) fn parse_image_reference_definitions(markdown: &str) -> ImageReferenceDefinitions {
    let lines = markdown.split('\n').collect::<Vec<_>>();
    let normalized_lines = lines
        .iter()
        .map(|line| strip_reference_scan_container_prefixes(line).to_string())
        .collect::<Vec<_>>();
    let normalized_refs = normalized_lines
        .iter()
        .map(|line| line.as_str())
        .collect::<Vec<_>>();
    let mut definitions = ImageReferenceDefinitions::new();
    let mut index = 0usize;
    let mut active_fence = None;
    let mut active_html_tag: Option<String> = None;
    let mut active_html_comment = false;

    while index < lines.len() {
        let line = normalized_refs[index];

        if let Some(fence) = active_fence {
            if is_reference_scan_closing_fence(line, fence) {
                active_fence = None;
            }
            index += 1;
            continue;
        }

        if active_html_comment {
            if line.contains("-->") || line.trim().is_empty() {
                active_html_comment = false;
            }
            index += 1;
            continue;
        }

        if let Some(tag_name) = active_html_tag.clone() {
            if line.trim().is_empty()
                || parse_reference_scan_html_close_tag_name(line).as_deref() == Some(&tag_name)
            {
                active_html_tag = None;
            }
            index += 1;
            continue;
        }

        if let Some(fence) = parse_reference_scan_opening_fence(line) {
            if !is_reference_scan_closing_fence(line, fence) {
                active_fence = Some(fence);
            }
            index += 1;
            continue;
        }

        if let Some(html_start) = parse_reference_scan_html_block_start(line) {
            match html_start {
                HtmlBlockStart::Comment => {
                    if !line.contains("-->") {
                        active_html_comment = true;
                    }
                }
                HtmlBlockStart::Tag {
                    name,
                    self_closing,
                    closes_same_line,
                } => {
                    if !self_closing && !closes_same_line {
                        active_html_tag = Some(name);
                    }
                }
            }
            index += 1;
            continue;
        }

        let Some((label, definition, consumed)) =
            parse_image_reference_definition(&normalized_refs, index)
        else {
            index += 1;
            continue;
        };

        definitions.entry(label).or_insert(definition);
        index += consumed;
    }

    definitions
}

pub(crate) fn normalize_reference_label(label: &str) -> Option<String> {
    // Single-pass concat: walk the words once, push to the output with a
    // leading separator on all but the first. Avoids the intermediate
    // Vec<&str> allocation that split_whitespace().collect::<Vec<_>>()
    // produces before .join("") copies again.
    let mut normalized = String::with_capacity(label.len());
    for word in label.split_whitespace() {
        if !normalized.is_empty() {
            normalized.push(' ');
        }
        normalized.push_str(word);
    }
    if normalized.is_empty() {
        None
    } else {
        Some(normalized.to_lowercase())
    }
}

fn parse_image_reference_definition(
    lines: &[&str],
    start: usize,
) -> Option<(String, ImageReferenceDefinition, usize)> {
    let line = lines.get(start)?;
    let trimmed_end = line.trim_end();
    let leading_spaces = trimmed_end.bytes().take_while(|b| *b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }

    let rest = &trimmed_end[leading_spaces..];
    if !rest.starts_with('[') {
        return None;
    }

    let label_end = find_unescaped_char(rest, 1, b']')?;
    if rest.as_bytes().get(label_end + 1) != Some(&b':') {
        return None;
    }

    let label = normalize_reference_label(&rest[1..label_end])?;
    let mut target = rest[label_end + 2..].trim_start().to_string();
    let mut consumed = 1usize;

    if let Some(next_line) = lines.get(start + 1)
        && is_reference_definition_title_continuation(next_line)
    {
        if !target.is_empty() {
            target.push(' ');
        }
        target.push_str(next_line.trim());
        consumed += 1;
    }

    let (src, title) = parse_image_target(&target)?;
    Some((label, ImageReferenceDefinition { src, title }, consumed))
}

fn strip_reference_scan_container_prefixes(mut line: &str) -> &str {
    loop {
        let original = line;
        if let Some(rest) = strip_reference_scan_quote_prefix(line) {
            line = rest;
            continue;
        }
        if let Some(rest) = strip_reference_scan_list_marker(line) {
            line = rest;
            continue;
        }
        if line == original {
            return line;
        }
    }
}

fn strip_reference_scan_quote_prefix(line: &str) -> Option<&str> {
    let leading_spaces = line.bytes().take_while(|b| *b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }

    let rest = &line[leading_spaces..];
    if !rest.starts_with('>') {
        return None;
    }

    Some(rest[1..].strip_prefix(' ').unwrap_or(&rest[1..]))
}

fn strip_reference_scan_list_marker(line: &str) -> Option<&str> {
    let indent_bytes = line
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .map(char::len_utf8)
        .sum::<usize>();
    let rest = &line[indent_bytes..];

    if let Some(marker) = rest.chars().next()
        && matches!(marker, '-' | '*' | '+')
    {
        let after_marker = &rest[marker.len_utf8()..];
        return after_marker
            .strip_prefix(' ')
            .or_else(|| after_marker.strip_prefix('\t'));
    }

    let digit_len = rest.bytes().take_while(|b| b.is_ascii_digit()).count();
    if !(1..=9).contains(&digit_len) {
        return None;
    }

    let marker = *rest.as_bytes().get(digit_len)?;
    if !matches!(marker, b'.' | b')') {
        return None;
    }

    let separator = *rest.as_bytes().get(digit_len + 1)?;
    if !matches!(separator, b' ' | b'\t') {
        return None;
    }

    Some(&rest[digit_len + 2..])
}

fn parse_reference_scan_opening_fence(line: &str) -> Option<FenceInfo> {
    let trimmed = line.trim_end();
    let ch = trimmed.chars().next()?;
    if !matches!(ch, '`' | '~') {
        return None;
    }
    let len = trimmed.chars().take_while(|current| *current == ch).count();
    let rest = &trimmed[ch.len_utf8() * len..];
    if ch == '`' && rest.contains('`') {
        return None;
    }
    (len >= 3).then_some(FenceInfo { ch, len })
}

fn is_reference_scan_closing_fence(line: &str, opener: FenceInfo) -> bool {
    let trimmed = line.trim_end();
    if !trimmed.starts_with(opener.ch) {
        return false;
    }

    let run_len = trimmed
        .chars()
        .take_while(|current| *current == opener.ch)
        .count();
    run_len == opener.len && trimmed[opener.ch.len_utf8() * run_len..].trim().is_empty()
}

fn parse_reference_scan_html_block_start(line: &str) -> Option<HtmlBlockStart> {
    let rest = line.trim_start().trim_end();
    if rest.starts_with("<!--") {
        return Some(HtmlBlockStart::Comment);
    }

    let tagged = rest.strip_prefix('<')?;
    if tagged.starts_with('/') {
        return None;
    }

    let name_len = tagged
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .count();
    if name_len == 0 {
        return None;
    }

    let name = &tagged[..name_len];
    let suffix = &tagged[name_len..];
    let next = suffix.chars().next()?;
    if !matches!(next, '>' | ' ' | '\t' | '/') {
        return None;
    }

    Some(HtmlBlockStart::Tag {
        name: name.to_string(),
        self_closing: rest.ends_with("/>"),
        closes_same_line: rest.contains(&format!("</{name}>")),
    })
}

fn parse_reference_scan_html_close_tag_name(line: &str) -> Option<String> {
    let rest = line.trim_start().trim_end();
    let tagged = rest.strip_prefix("</")?;
    let name_len = tagged
        .chars()
        .take_while(|ch| ch.is_ascii_alphanumeric() || *ch == '-')
        .count();
    if name_len == 0 {
        return None;
    }

    let name = &tagged[..name_len];
    let suffix = &tagged[name_len..];
    let next = suffix.chars().next()?;
    if !matches!(next, '>' | ' ' | '\t') {
        return None;
    }

    Some(name.to_string())
}

fn parse_image_target(inner: &str) -> Option<(String, Option<String>)> {
    if inner.is_empty() {
        return None;
    }

    // All three CommonMark title forms: `"…"`, `'…'`, and `(…)`. Reference
    // definitions already accepted all three, so recognizing only `"…"` here
    // made the two parsers disagree about what counts as a title.
    let close = inner.len() - 1;
    if let Some(open_delimiter) = title_delimiter_pair(inner.as_bytes()[close])
        && !is_escaped(inner, close)
        && let Some(open) = find_open_title_delimiter(inner, close, open_delimiter)
    {
        let src = inner[..open].trim_end();
        let title = inner[open + 1..close].to_string();
        if !src.is_empty() {
            return Some((normalize_image_source(src), Some(title)));
        }
    }

    Some((normalize_image_source(inner), None))
}

fn is_reference_definition_title_continuation(line: &str) -> bool {
    let indent_bytes = line
        .chars()
        .take_while(|ch| matches!(ch, ' ' | '\t'))
        .map(char::len_utf8)
        .sum::<usize>();
    if indent_bytes == 0 {
        return false;
    }

    let trimmed = line[indent_bytes..].trim();
    (trimmed.starts_with('"') && trimmed.ends_with('"'))
        || (trimmed.starts_with('\'') && trimmed.ends_with('\''))
        || (trimmed.starts_with('(') && trimmed.ends_with(')'))
}

/// Finds the opening delimiter of a trailing title, scanning back from its
/// closing delimiter at `close`.
///
/// CommonMark allows three title forms — `"…"`, `'…'`, and `(…)` — and requires
/// whitespace between the destination and the title, which is what keeps a
/// bare `'` or `(` inside a filename from being read as a title opener.
fn find_open_title_delimiter(input: &str, close: usize, open: u8) -> Option<usize> {
    let bytes = input.as_bytes();
    (0..close).rev().find(|&index| {
        bytes[index] == open
            && !is_escaped(input, index)
            && index > 0
            && bytes[index - 1].is_ascii_whitespace()
    })
}

/// Closing title delimiter mapped to the opener it pairs with.
fn title_delimiter_pair(close: u8) -> Option<u8> {
    match close {
        b'"' => Some(b'"'),
        b'\'' => Some(b'\''),
        b')' => Some(b'('),
        _ => None,
    }
}

fn normalize_image_source(source: &str) -> String {
    let source = unescape_ascii_punctuation(source);
    // `<…>` is CommonMark's way to wrap a destination that contains spaces, so
    // the brackets come off unconditionally. Requiring the contents to parse as
    // a URI defeated the one case the syntax exists for: `<my image.png>` is
    // not a valid URI, which is exactly why the author bracketed it.
    if source.len() >= 2 && source.starts_with('<') && source.ends_with('>') {
        return source[1..source.len() - 1].to_string();
    }
    source
}

fn unescape_ascii_punctuation(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && chars.peek().is_some_and(|next| next.is_ascii_punctuation()) {
            output.push(chars.next().expect("peeked punctuation must exist"));
        } else {
            output.push(ch);
        }
    }
    output
}

fn find_unescaped_char(input: &str, start: usize, target: u8) -> Option<usize> {
    let bytes = input.as_bytes();
    (start..bytes.len()).find(|&index| bytes[index] == target && !is_escaped(input, index))
}

/// Finds the `)` that closes the image target opened at `open_paren`
/// (the index of the `(` itself), respecting escapes, balanced nested
/// parentheses inside the source (`![a](foo(bar).png)`), and `)` inside
/// a quoted title (`![a](pic.png "a ) title")`).
fn find_matching_target_close_paren(input: &str, open_paren: usize) -> Option<usize> {
    let bytes = input.as_bytes();
    let mut depth = 1usize;
    let mut index = open_paren + 1;
    while index < bytes.len() {
        match bytes[index] {
            // Only a quote that opens a *title* may hide a `)` from the depth
            // count, and CommonMark requires whitespace before a title. Without
            // that guard an apostrophe in a filename — `![a](it's.png)` — looks
            // like an unterminated title and the whole image fails to parse.
            quote @ (b'"' | b'\'')
                if !is_escaped(input, index)
                    && index > open_paren + 1
                    && bytes[index - 1].is_ascii_whitespace() =>
            {
                index = find_unescaped_char(input, index + 1, quote)?;
            }
            b'(' if !is_escaped(input, index) => depth += 1,
            b')' if !is_escaped(input, index) => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
        index += 1;
    }
    None
}

fn is_escaped(input: &str, index: usize) -> bool {
    if index == 0 {
        return false;
    }

    let bytes = input.as_bytes();
    let mut backslashes = 0usize;
    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        if bytes[cursor] == b'\\' {
            backslashes += 1;
        } else {
            break;
        }
    }
    backslashes % 2 == 1
}

#[cfg(test)]
mod tests {
    use super::{
        ImageReferenceDefinition, ImageResolvedSource, ImageSyntax, ImageTarget,
        TableCellInlineImageSegment, normalize_reference_label, parse_image_reference_definitions,
        parse_standalone_image, parse_table_cell_inline_images, resolve_image_source,
    };
    use std::path::Path;

    #[test]
    fn parses_standalone_image_without_title() {
        let parsed = parse_standalone_image("![alt](./img.png)").expect("image syntax");
        assert_eq!(parsed.alt, "alt");
        assert_eq!(
            parsed.target,
            ImageTarget::Direct {
                src: "./img.png".to_string(),
                title: None,
            }
        );
    }

    #[test]
    fn parses_standalone_image_with_surrounding_whitespace() {
        let three_space =
            parse_standalone_image("   ![alt](https://example.com/a.png)").expect("image syntax");
        assert_eq!(three_space.alt, "alt");
        assert_eq!(
            three_space.target,
            ImageTarget::Direct {
                src: "https://example.com/a.png".to_string(),
                title: None,
            }
        );

        let deeply_indented =
            parse_standalone_image("        ![alt](https://example.com/a.png)   ")
                .expect("image syntax");
        assert_eq!(deeply_indented, three_space);
        assert!(parse_standalone_image("   text ![alt](x)").is_none());
        assert!(parse_standalone_image("   ![alt](x)\n").is_none());
    }

    #[test]
    fn parses_image_target_with_escaped_punctuation_in_source() {
        let parsed = parse_standalone_image("![alt](https://example.com/typera\\_picgo/img.png)")
            .expect("image syntax");
        assert_eq!(
            parsed.target,
            ImageTarget::Direct {
                src: "https://example.com/typera_picgo/img.png".to_string(),
                title: None,
            }
        );
    }

    #[test]
    fn parses_standalone_image_with_underscores_in_alt_and_source() {
        let parsed = parse_standalone_image(
            "![1.1_进制转换例子](./NetworkEngineerSummer.assets/1.1_进制转换例子.jpg)",
        )
        .expect("image syntax");

        assert_eq!(parsed.alt, "1.1_进制转换例子");
        assert_eq!(
            parsed.target,
            ImageTarget::Direct {
                src: "./NetworkEngineerSummer.assets/1.1_进制转换例子.jpg".to_string(),
                title: None,
            }
        );
    }

    #[test]
    fn parses_standalone_image_with_title() {
        let parsed =
            parse_standalone_image("![alt](./img.png \"caption text\")").expect("image syntax");
        assert_eq!(parsed.alt, "alt");
        assert_eq!(
            parsed.target,
            ImageTarget::Direct {
                src: "./img.png".to_string(),
                title: Some("caption text".to_string()),
            }
        );
    }

    #[test]
    fn parses_reference_style_standalone_image() {
        let parsed =
            parse_standalone_image("![reference image][ref-image]").expect("reference image");
        assert_eq!(parsed.alt, "reference image");
        assert_eq!(
            parsed.target,
            ImageTarget::Reference {
                label: "ref-image".to_string(),
            }
        );
    }

    #[test]
    fn parses_collapsed_reference_style_standalone_image() {
        let parsed =
            parse_standalone_image("![collapsed image][]").expect("collapsed reference image");
        assert_eq!(parsed.alt, "collapsed image");
        assert_eq!(
            parsed.target,
            ImageTarget::Reference {
                label: "collapsed image".to_string(),
            }
        );
    }

    #[test]
    fn parses_shortcut_reference_style_standalone_image() {
        let parsed = parse_standalone_image("![shortcut image]").expect("shortcut reference image");
        assert_eq!(parsed.alt, "shortcut image");
        assert_eq!(
            parsed.target,
            ImageTarget::Reference {
                label: "shortcut image".to_string(),
            }
        );
    }

    #[test]
    fn rejects_mixed_or_wrapped_image_syntax() {
        assert!(parse_standalone_image("text ![alt](./img.png)").is_none());
        assert!(parse_standalone_image("[![alt](./img.png)](https://example.com)").is_none());
        assert!(parse_standalone_image("![][]").is_none());
        assert!(parse_standalone_image("![]").is_none());
    }

    #[test]
    fn rejects_three_badge_line_as_single_image() {
        // real-world readme line: three separate badge images on one line
        // must not collapse into a single image with a garbage src.
        let line = concat!(
            "![JetBrains Plugins](https://img.shields.io/jetbrains/plugin/v/18717-smithy?style=for-the-badge) ",
            "![JetBrains plugins](https://img.shields.io/jetbrains/plugin/d/18717-smithy?style=for-the-badge) ",
            "![License](https://img.shields.io/github/license/iancaffey/smithy-intellij-plugin?style=for-the-badge)",
        );
        assert!(parse_standalone_image(line).is_none());
    }

    #[test]
    fn rejects_two_image_line_as_single_image() {
        assert!(parse_standalone_image("![a](x.png) ![b](y.png)").is_none());
    }

    #[test]
    fn parses_balanced_parens_in_image_source() {
        let parsed = parse_standalone_image("![a](foo(bar).png)").expect("image syntax");
        assert_eq!(parsed.alt, "a");
        assert_eq!(
            parsed.target,
            ImageTarget::Direct {
                src: "foo(bar).png".to_string(),
                title: None,
            }
        );
    }

    #[test]
    fn parses_title_containing_close_paren() {
        let parsed =
            parse_standalone_image("![a](pic.png \"a ) title\")").expect("image syntax");
        assert_eq!(parsed.alt, "a");
        assert_eq!(
            parsed.target,
            ImageTarget::Direct {
                src: "pic.png".to_string(),
                title: Some("a ) title".to_string()),
            }
        );
    }

    #[test]
    fn parses_table_cell_inline_image_segments() {
        let segments = parse_table_cell_inline_images("image ![alt](https://example.com/x.png)");
        assert_eq!(
            segments,
            vec![
                TableCellInlineImageSegment::Text("image ".to_string()),
                TableCellInlineImageSegment::Image {
                    markdown: "![alt](https://example.com/x.png)".to_string(),
                    syntax: ImageSyntax {
                        alt: "alt".to_string(),
                        target: ImageTarget::Direct {
                            src: "https://example.com/x.png".to_string(),
                            title: None,
                        },
                    },
                },
            ]
        );
    }

    #[test]
    fn parses_multiple_table_cell_inline_images() {
        let segments = parse_table_cell_inline_images("![a](x.png) and ![b](y.png)");
        assert_eq!(segments.len(), 3);
        assert!(matches!(
            &segments[0],
            TableCellInlineImageSegment::Image { syntax, .. } if syntax.alt == "a"
        ));
        assert_eq!(
            segments[1],
            TableCellInlineImageSegment::Text(" and ".to_string())
        );
        assert!(matches!(
            &segments[2],
            TableCellInlineImageSegment::Image { syntax, .. } if syntax.alt == "b"
        ));
    }

    #[test]
    fn table_cell_inline_image_segments_keep_escaped_wrapped_and_broken_text() {
        assert_eq!(
            parse_table_cell_inline_images(r"\![alt](x.png)"),
            vec![TableCellInlineImageSegment::Text(
                r"\![alt](x.png)".to_string()
            )]
        );
        assert_eq!(
            parse_table_cell_inline_images("[![alt](x.png)](https://example.com)"),
            vec![TableCellInlineImageSegment::Text(
                "[![alt](x.png)](https://example.com)".to_string()
            )]
        );
        assert_eq!(
            parse_table_cell_inline_images("broken ![alt](x.png"),
            vec![TableCellInlineImageSegment::Text(
                "broken ![alt](x.png".to_string()
            )]
        );
    }

    #[test]
    fn table_cell_inline_reference_images_resolve() {
        let definitions = parse_image_reference_definitions(
            "[ref]: ./ref.png\n[collapsed]: ./collapsed.png\n[shortcut]: ./shortcut.png",
        );
        let segments = parse_table_cell_inline_images("![full][ref] ![collapsed][] ![shortcut]");
        let resolved = segments
            .iter()
            .filter_map(|segment| match segment {
                TableCellInlineImageSegment::Image { syntax, .. } => {
                    syntax.resolve_target(&definitions).map(|target| target.src)
                }
                TableCellInlineImageSegment::Text(_) => None,
            })
            .collect::<Vec<_>>();

        assert_eq!(
            resolved,
            vec!["./ref.png", "./collapsed.png", "./shortcut.png"]
        );
    }

    #[test]
    fn parses_image_reference_definitions_with_title_and_first_wins() {
        let definitions = parse_image_reference_definitions(
            "[Ref Image]: ./first.png \"Caption\"\n[ref image]: ./second.png".trim(),
        );
        assert_eq!(
            definitions.get("ref image"),
            Some(&ImageReferenceDefinition {
                src: "./first.png".to_string(),
                title: Some("Caption".to_string()),
            })
        );
    }

    #[test]
    fn normalizes_reference_labels_case_and_whitespace_insensitively() {
        assert_eq!(
            normalize_reference_label("  Ref\t Image  "),
            Some("ref image".to_string())
        );
    }

    #[test]
    fn resolves_reference_targets() {
        let syntax = ImageSyntax {
            alt: "alt".to_string(),
            target: ImageTarget::Reference {
                label: "ref-image".to_string(),
            },
        };
        let definitions = parse_image_reference_definitions("[ref-image]: ./img.png \"Caption\"");
        let resolved = syntax
            .resolve_target(&definitions)
            .expect("resolved target");
        assert_eq!(resolved.src, "./img.png");
        assert_eq!(resolved.title.as_deref(), Some("Caption"));
    }

    #[test]
    fn resolves_collapsed_and_shortcut_reference_images() {
        let definitions = parse_image_reference_definitions(
            "[collapsed image]: ./collapsed.png\n[shortcut image]: ./shortcut.png",
        );

        let collapsed = parse_standalone_image("![collapsed image][]")
            .expect("collapsed reference image")
            .resolve_target(&definitions)
            .expect("resolved collapsed image");
        assert_eq!(collapsed.src, "./collapsed.png");

        let shortcut = parse_standalone_image("![shortcut image]")
            .expect("shortcut reference image")
            .resolve_target(&definitions)
            .expect("resolved shortcut image");
        assert_eq!(shortcut.src, "./shortcut.png");
    }

    #[test]
    fn unresolved_reference_target_returns_none() {
        let syntax = ImageSyntax {
            alt: "alt".to_string(),
            target: ImageTarget::Reference {
                label: "missing".to_string(),
            },
        };
        assert!(
            syntax
                .resolve_target(&parse_image_reference_definitions("[ref]: ./img.png"))
                .is_none()
        );
    }

    #[test]
    fn resolves_relative_and_remote_sources() {
        let local = resolve_image_source("images/pic.png", Some(Path::new("D:/docs")));
        assert_eq!(
            local,
            ImageResolvedSource::Local(Path::new("D:/docs").join("images/pic.png"))
        );

        let remote = resolve_image_source("https://example.com/img.gif", None);
        match remote {
            ImageResolvedSource::Remote(uri) => {
                assert_eq!(uri.to_string(), "https://example.com/img.gif");
            }
            other => panic!("expected remote source, got {other:?}"),
        }
    }

    #[test]
    fn parses_container_scoped_reference_definitions_in_source_order() {
        let definitions = parse_image_reference_definitions(
            [
                "> [quoted ref]: ./quoted.png \"Quoted\"",
                "- [list ref]: ./list.png",
                "1) [ordered ref]: ./ordered.png",
                "> > [quoted ref]: ./ignored.png",
            ]
            .join("\n")
            .as_str(),
        );

        assert_eq!(
            definitions.get("quoted ref"),
            Some(&ImageReferenceDefinition {
                src: "./quoted.png".to_string(),
                title: Some("Quoted".to_string()),
            })
        );
        assert_eq!(
            definitions.get("list ref"),
            Some(&ImageReferenceDefinition {
                src: "./list.png".to_string(),
                title: None,
            })
        );
        assert_eq!(
            definitions.get("ordered ref"),
            Some(&ImageReferenceDefinition {
                src: "./ordered.png".to_string(),
                title: None,
            })
        );
    }

    #[test]
    fn ignores_reference_definitions_inside_code_fences_and_html_blocks() {
        let definitions = parse_image_reference_definitions(
            [
                "> ```md",
                "> [code ref]: ./ignored-code.png",
                "> ```",
                "",
                "<div>",
                "[html ref]: ./ignored-html.png",
                "</div>",
                "",
                "> [live ref]: ./real.png",
            ]
            .join("\n")
            .as_str(),
        );

        assert!(!definitions.contains_key("code ref"));
        assert!(!definitions.contains_key("html ref"));
        assert_eq!(
            definitions.get("live ref"),
            Some(&ImageReferenceDefinition {
                src: "./real.png".to_string(),
                title: None,
            })
        );
    }

    #[test]
    fn parses_all_three_commonmark_title_forms() {
        for (markdown, expected_title) in [
            ("![a](pic.png \"double\")", "double"),
            ("![a](pic.png 'single')", "single"),
            ("![a](pic.png (paren))", "paren"),
        ] {
            let syntax = parse_standalone_image(markdown).expect(markdown);
            let ImageTarget::Direct { src, title } = syntax.target else {
                panic!("{markdown} should be a direct target");
            };
            assert_eq!(src, "pic.png", "{markdown}");
            assert_eq!(title.as_deref(), Some(expected_title), "{markdown}");
        }
    }

    #[test]
    fn a_quote_inside_a_destination_is_not_a_title() {
        // no whitespace before the delimiter, so it belongs to the filename
        let syntax = parse_standalone_image("![a](it's.png)").expect("image");
        let ImageTarget::Direct { src, title } = syntax.target else {
            panic!("direct target");
        };
        assert_eq!(src, "it's.png");
        assert_eq!(title, None);
    }

    #[test]
    fn angle_bracketed_destination_with_spaces_is_unwrapped() {
        let syntax = parse_standalone_image("![a](<my image.png>)").expect("image");
        let ImageTarget::Direct { src, .. } = syntax.target else {
            panic!("direct target");
        };
        assert_eq!(src, "my image.png");
    }

    #[test]
    fn resolves_base64_data_uri_to_inline_bytes() {
        // 1x1 transparent png
        let src = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAAC0lEQVR42mNkAAIAAAoAAv/lxKUAAAAASUVORK5CYII=";
        let ImageResolvedSource::Inline(image) = resolve_image_source(src, None) else {
            panic!("expected inline bytes");
        };
        assert_eq!(image.format, gpui::ImageFormat::Png);
        assert_eq!(&image.bytes[1..4], b"PNG");
    }

    #[test]
    fn resolves_plain_svg_data_uri_without_base64() {
        let src = "data:image/svg+xml,%3Csvg%20xmlns%3D%22http%3A%2F%2Fwww.w3.org%2F2000%2Fsvg%22%2F%3E";
        let ImageResolvedSource::Inline(image) = resolve_image_source(src, None) else {
            panic!("expected inline bytes");
        };
        assert_eq!(image.format, gpui::ImageFormat::Svg);
        assert_eq!(
            String::from_utf8(image.bytes.clone()).expect("utf8"),
            "<svg xmlns=\"http://www.w3.org/2000/svg\"/>"
        );
    }

    #[test]
    fn unusable_data_uris_do_not_become_filesystem_paths() {
        for src in [
            "data:image/png;base64,!!!not base64!!!",
            "data:text/plain;base64,aGVsbG8=",
            "data:image/x-icon;base64,AAABAA==",
            "data:image/png;base64,",
            "data:nocomma",
        ] {
            assert_eq!(
                resolve_image_source(src, Some(Path::new("/docs"))),
                ImageResolvedSource::Unusable,
                "{src}"
            );
        }
    }

    #[test]
    fn resolves_percent_encoded_and_file_url_and_protocol_relative_sources() {
        assert_eq!(
            resolve_image_source("my%20image.png", Some(Path::new("/docs"))),
            ImageResolvedSource::Local(Path::new("/docs/my image.png").to_path_buf())
        );
        assert_eq!(
            resolve_image_source("file:///abs/my%20pic.png", None),
            ImageResolvedSource::Local(Path::new("/abs/my pic.png").to_path_buf())
        );
        assert_eq!(
            resolve_image_source("//example.com/img.png", None),
            ImageResolvedSource::Remote(gpui::SharedUri::from(
                "https://example.com/img.png".to_string()
            ))
        );
    }

    #[test]
    fn plain_relative_and_remote_sources_are_unchanged() {
        assert_eq!(
            resolve_image_source("./pic.png", Some(Path::new("/docs"))),
            ImageResolvedSource::Local(Path::new("/docs/./pic.png").to_path_buf())
        );
        assert_eq!(
            resolve_image_source("https://example.com/x.png", None),
            ImageResolvedSource::Remote(gpui::SharedUri::from(
                "https://example.com/x.png".to_string()
            ))
        );
    }
}
