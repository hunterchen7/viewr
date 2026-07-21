//! XMP sidecar reading and merge-preserving rating writes.
//!
//! Lightroom reads `xmp:Rating` (namespace `http://ns.adobe.com/xap/1.0/`)
//! from `.xmp` sidecars on import. It writes scalar properties as
//! ATTRIBUTES on `rdf:Description`; other tools (darktable) use the
//! ELEMENT form. We read both. Existing rating attributes use a byte-range
//! splice so every non-value byte passes through untouched. Element updates
//! and new-property injection use an XML event fallback that preserves
//! semantic content but can change the document's lexical representation.

use std::ops::Range;
use std::path::Path;

use quick_xml::NsReader;
use quick_xml::Writer;
use quick_xml::events::{BytesCData, BytesEnd, BytesStart, BytesText, Event};
use quick_xml::name::{PrefixDeclaration, QName, ResolveResult};

use crate::atomic_write;

#[derive(Debug, thiserror::Error)]
/// Errors produced while updating or writing an XMP rating.
pub enum XmpError {
    /// The sidecar could not be read, created, or atomically replaced.
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    /// Existing sidecar XML could not be safely interpreted or updated.
    #[error("xml: {0}")]
    Xml(String),
}

const XMP_NAMESPACE: &[u8] = b"http://ns.adobe.com/xap/1.0/";
const XMP_NAMESPACE_STR: &str = "http://ns.adobe.com/xap/1.0/";
const RDF_NAMESPACE: &[u8] = b"http://www.w3.org/1999/02/22-rdf-syntax-ns#";

fn is_xmp_rating(namespace: &ResolveResult<'_>, local_name: &[u8]) -> bool {
    local_name == b"Rating"
        && matches!(namespace, ResolveResult::Bound(uri) if is_xmp_namespace(uri.as_ref()))
}

fn is_xmp_namespace(namespace: &[u8]) -> bool {
    is_namespace(namespace, XMP_NAMESPACE)
}

fn is_namespace(namespace: &[u8], expected: &[u8]) -> bool {
    if namespace == expected {
        return true;
    }
    namespace.contains(&b'&')
        && std::str::from_utf8(namespace)
            .ok()
            .and_then(|namespace| quick_xml::escape::unescape(namespace).ok())
            .is_some_and(|namespace| namespace.as_bytes() == expected)
}

fn is_xmp_rating_element(reader: &NsReader<&[u8]>, name: QName<'_>) -> bool {
    if name.local_name().as_ref() != b"Rating" {
        return false;
    }
    let (namespace, local_name) = reader.resolve_element(name);
    is_xmp_rating(&namespace, local_name.as_ref())
}

fn is_xmp_rating_attribute(reader: &NsReader<&[u8]>, name: QName<'_>) -> bool {
    if name.local_name().as_ref() != b"Rating" {
        return false;
    }
    let (namespace, local_name) = reader.resolve_attribute(name);
    is_xmp_rating(&namespace, local_name.as_ref())
}

fn is_rdf_description(reader: &NsReader<&[u8]>, name: QName<'_>) -> bool {
    if name.local_name().as_ref() != b"Description" {
        return false;
    }
    let (namespace, local_name) = reader.resolve_element(name);
    local_name.as_ref() == b"Description"
        && matches!(namespace, ResolveResult::Bound(uri) if is_namespace(uri.as_ref(), RDF_NAMESPACE))
}

/// Read the rating from a sidecar in attribute or element form.
///
/// Returns `None` when the file cannot be read as UTF-8, the XML is rejected,
/// or no numeric rating exists in an RDF `Description`. A zero value means
/// unrated. If the document contains several semantic ratings, the first one
/// in document order wins.
pub fn read_rating(path: &Path) -> Option<u8> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_rating(&content)
}

/// Parse an XMP rating from a UTF-8 XML document.
///
/// Accepts an Adobe XMP `Rating` attribute on an RDF `Description`, or a
/// direct `Rating` child element of that description. Returns `None` for a
/// rejected document or missing/non-numeric rating. Negative numeric values
/// become zero; positive values are converted to `u8` using Rust's saturating
/// float-to-integer conversion. If several semantic ratings exist, the first
/// one in document order wins.
pub fn parse_rating(xml: &str) -> Option<u8> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut rating = None;
    let mut element_stack: Vec<bool> = Vec::new();
    let mut captures: Vec<RatingCapture> = Vec::new();
    loop {
        match reader.read_event().ok()? {
            Event::Eof => {
                if !element_stack.is_empty() || !captures.is_empty() {
                    return None;
                }
                return rating;
            }
            Event::Start(e) => {
                let depth = element_stack.len();
                let is_description = is_rdf_description(&reader, e.name());
                let is_rating_element = element_stack.last().copied().unwrap_or(false)
                    && is_xmp_rating_element(&reader, e.name());
                if is_description {
                    if let Some(value) = parse_rating_attribute(&reader, &e).ok()? {
                        rating.get_or_insert(value);
                    }
                } else if !e.attributes_raw().is_empty() {
                    validate_attributes(&e).ok()?;
                }
                if is_rating_element {
                    captures.push(RatingCapture {
                        depth,
                        content: String::new(),
                        saw_cdata: false,
                    });
                }
                element_stack.push(is_description);
            }
            Event::Empty(e) => {
                let is_description = is_rdf_description(&reader, e.name());
                if is_description {
                    if let Some(value) = parse_rating_attribute(&reader, &e).ok()? {
                        rating.get_or_insert(value);
                    }
                } else if !e.attributes_raw().is_empty() {
                    validate_attributes(&e).ok()?;
                }
                // A self-closing rating element has no scalar value. Do not
                // enter capture state that could consume a later text node.
            }
            Event::Text(text) => {
                if let Some(capture) = captures.last_mut()
                    && element_stack.len() == capture.depth + 1
                {
                    capture.content.push_str(&text.unescape().ok()?);
                }
            }
            Event::CData(cdata) => {
                if let Some(capture) = captures.last_mut()
                    && element_stack.len() == capture.depth + 1
                {
                    capture
                        .content
                        .push_str(std::str::from_utf8(cdata.as_ref()).ok()?);
                    capture.saw_cdata = true;
                }
            }
            Event::End(e) => {
                let depth = element_stack.len().checked_sub(1)?;
                if is_xmp_rating_element(&reader, e.name())
                    && captures
                        .last()
                        .is_some_and(|capture| capture.depth == depth)
                {
                    let capture = captures.pop()?;
                    if let Some(value) = parse_rating_value(&capture.content) {
                        rating.get_or_insert(value);
                    }
                }
                element_stack.pop()?;
            }
            _ => {}
        }
    }
}

fn parse_rating_attribute(reader: &NsReader<&[u8]>, e: &BytesStart<'_>) -> Result<Option<u8>, ()> {
    let mut found_rating = false;
    let mut rating = None;
    for attr in e.attributes() {
        let attr = attr.map_err(|_| ())?;
        if !is_xmp_rating_attribute(reader, attr.key) {
            continue;
        }
        if found_rating {
            return Err(());
        }
        found_rating = true;
        let value = attr.unescape_value().map_err(|_| ())?;
        rating = parse_rating_value(&value);
    }
    Ok(rating)
}

struct RatingCapture {
    depth: usize,
    content: String,
    saw_cdata: bool,
}

fn parse_rating_value(value: &str) -> Option<u8> {
    value
        .trim()
        .parse::<f32>()
        .ok()
        .map(|rating| rating.max(0.0) as u8)
}

/// Write `rating` into an XMP sidecar.
///
/// Creates a minimal sidecar if the file does not exist. For an existing
/// rating attribute, every byte outside the rating values is preserved.
/// Updating an element-form rating or injecting a new property preserves the
/// XML's semantic content but can reserialize its lexical representation.
/// The destination is replaced atomically only after the update succeeds.
///
/// # Errors
///
/// Returns [`XmpError::Io`] when the sidecar cannot be read or replaced.
/// Returns [`XmpError::Xml`] when existing XML is malformed or has no RDF
/// `Description` that can own the rating property.
pub fn write_rating(path: &Path, rating: u8) -> Result<(), XmpError> {
    let output = match std::fs::read_to_string(path) {
        Ok(existing) => update_rating_xml(&existing, rating)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => new_sidecar(rating),
        Err(e) => return Err(e.into()),
    };
    atomic_write::replace_durable(path, output.as_bytes()).map_err(Into::into)
}

/// Update the semantic `xmp:Rating` property inside existing sidecar XML.
///
/// Existing rating attributes are changed with byte-range splices that
/// preserve every other input byte. Element updates and new-property
/// injection preserve semantic content but can reserialize XML.
///
/// # Errors
///
/// Returns [`XmpError::Xml`] when `xml` is malformed or contains no RDF
/// `Description` that can own the rating property.
pub fn update_rating_xml(xml: &str, rating: u8) -> Result<String, XmpError> {
    if let Some(updated) = update_rating_attributes(xml, rating)? {
        return Ok(updated);
    }

    update_rating_xml_fallback(xml, rating)
}

/// Scan the complete document and replace existing rating attribute values
/// without owning and rewriting every XML event. Returns `None` when an XMP
/// rating element is also present or no semantic rating attribute was found;
/// those less common shapes use the general event-rewrite path below.
fn update_rating_attributes(xml: &str, rating: u8) -> Result<Option<String>, XmpError> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut ranges = Vec::new();
    let mut element_stack: Vec<bool> = Vec::new();
    let mut saw_rating_element = false;

    loop {
        match reader
            .read_event()
            .map_err(|error| XmpError::Xml(error.to_string()))?
        {
            Event::Eof => break,
            Event::Start(e) => {
                let is_description = is_rdf_description(&reader, e.name());
                saw_rating_element |= element_stack.last().copied().unwrap_or(false)
                    && is_xmp_rating_element(&reader, e.name());
                if is_description {
                    collect_rating_attribute_ranges(&reader, &e, xml, &mut ranges)?;
                } else {
                    validate_attributes(&e)?;
                }
                element_stack.push(is_description);
            }
            Event::Empty(e) => {
                let is_description = is_rdf_description(&reader, e.name());
                saw_rating_element |= element_stack.last().copied().unwrap_or(false)
                    && is_xmp_rating_element(&reader, e.name());
                if is_description {
                    collect_rating_attribute_ranges(&reader, &e, xml, &mut ranges)?;
                } else {
                    validate_attributes(&e)?;
                }
            }
            Event::End(_) => {
                element_stack
                    .pop()
                    .ok_or_else(|| XmpError::Xml("unexpected closing element".into()))?;
            }
            _ => {}
        }
    }

    if !element_stack.is_empty() {
        return Err(XmpError::Xml("unexpected end of document".into()));
    }

    if saw_rating_element || ranges.is_empty() {
        return Ok(None);
    }

    let value = rating.to_string();
    let mut updated = String::with_capacity(xml.len() + value.len() * ranges.len());
    let mut cursor = 0;
    for range in ranges {
        if range.start < cursor {
            return Err(XmpError::Xml("overlapping rating attributes".into()));
        }
        updated.push_str(
            xml.get(cursor..range.start)
                .ok_or_else(|| XmpError::Xml("invalid rating attribute boundary".into()))?,
        );
        updated.push_str(&value);
        cursor = range.end;
    }
    updated.push_str(
        xml.get(cursor..)
            .ok_or_else(|| XmpError::Xml("invalid rating attribute boundary".into()))?,
    );
    Ok(Some(updated))
}

fn collect_rating_attribute_ranges(
    reader: &NsReader<&[u8]>,
    e: &BytesStart<'_>,
    xml: &str,
    ranges: &mut Vec<Range<usize>>,
) -> Result<(), XmpError> {
    let mut found_rating = false;
    for attr in e.attributes() {
        let attr = attr.map_err(|error| XmpError::Xml(error.to_string()))?;
        if !is_xmp_rating_attribute(reader, attr.key) {
            continue;
        }
        if found_rating {
            return Err(XmpError::Xml(
                "duplicate XMP rating attributes on one element".into(),
            ));
        }
        found_rating = true;
        ranges.push(input_range(xml, attr.value.as_ref())?);
    }
    Ok(())
}

fn input_range(input: &str, value: &[u8]) -> Result<Range<usize>, XmpError> {
    let input_start = input.as_ptr() as usize;
    let start = (value.as_ptr() as usize)
        .checked_sub(input_start)
        .ok_or_else(|| XmpError::Xml("rating attribute is outside the input".into()))?;
    let end = start
        .checked_add(value.len())
        .ok_or_else(|| XmpError::Xml("rating attribute range overflow".into()))?;
    if input.as_bytes().get(start..end) != Some(value) {
        return Err(XmpError::Xml(
            "rating attribute is outside the input".into(),
        ));
    }
    Ok(start..end)
}

fn update_rating_xml_fallback(xml: &str, rating: u8) -> Result<String, XmpError> {
    let mut reader = NsReader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let mut wrote = false;
    let mut element_stack: Vec<bool> = Vec::new();
    let mut rating_elements: Vec<RatingCapture> = Vec::new();
    let mut injection: Option<InjectionTarget> = None;
    let mut events: Vec<Event<'static>> = Vec::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| XmpError::Xml(e.to_string()))?
        {
            Event::Eof => break,
            Event::Start(e) => {
                let depth = element_stack.len();
                let is_description = is_rdf_description(&reader, e.name());
                let is_rating = element_stack.last().copied().unwrap_or(false)
                    && is_xmp_rating_element(&reader, e.name());
                let rewritten = if is_description {
                    if injection.is_none() {
                        injection = Some(injection_target(&reader, events.len()));
                    }
                    rewrite_attrs(&reader, &e, rating, &mut wrote)?
                } else {
                    validate_attributes(&e)?;
                    e.into_owned()
                };
                events.push(Event::Start(rewritten));
                if is_rating {
                    wrote = true;
                    rating_elements.push(RatingCapture {
                        depth,
                        content: String::new(),
                        saw_cdata: false,
                    });
                }
                element_stack.push(is_description);
            }
            Event::Empty(e) => {
                let is_description = is_rdf_description(&reader, e.name());
                let is_rating = element_stack.last().copied().unwrap_or(false)
                    && is_xmp_rating_element(&reader, e.name());
                let rewritten = if is_description {
                    if injection.is_none() {
                        injection = Some(injection_target(&reader, events.len()));
                    }
                    rewrite_attrs(&reader, &e, rating, &mut wrote)?
                } else {
                    validate_attributes(&e)?;
                    e.into_owned()
                };
                if is_rating {
                    wrote = true;
                    let name = String::from_utf8(rewritten.name().as_ref().to_vec())
                        .map_err(|error| XmpError::Xml(error.to_string()))?;
                    events.push(Event::Start(rewritten));
                    events.push(Event::Text(
                        BytesText::new(&rating.to_string()).into_owned(),
                    ));
                    events.push(Event::End(BytesEnd::new(name)));
                } else {
                    events.push(Event::Empty(rewritten));
                }
            }
            Event::Text(text) => {
                if !is_direct_rating_content(&rating_elements, element_stack.len()) {
                    events.push(Event::Text(text.into_owned()));
                }
            }
            Event::CData(cdata) => {
                if is_direct_rating_content(&rating_elements, element_stack.len()) {
                    if let Some(capture) = rating_elements.last_mut() {
                        capture.saw_cdata = true;
                    }
                } else {
                    events.push(Event::CData(cdata.into_owned()));
                }
            }
            Event::End(e) => {
                let depth = element_stack
                    .len()
                    .checked_sub(1)
                    .ok_or_else(|| XmpError::Xml("unexpected closing element".into()))?;
                if is_xmp_rating_element(&reader, e.name())
                    && rating_elements
                        .last()
                        .is_some_and(|capture| capture.depth == depth)
                {
                    let capture = rating_elements
                        .pop()
                        .expect("matching rating capture must exist");
                    let value = rating.to_string();
                    if capture.saw_cdata {
                        events.push(Event::CData(BytesCData::new(value)));
                    } else {
                        events.push(Event::Text(BytesText::new(&value).into_owned()));
                    }
                }
                events.push(Event::End(e.into_owned()));
                element_stack.pop();
            }
            other => events.push(other.into_owned()),
        }
    }

    if !element_stack.is_empty() {
        return Err(XmpError::Xml("unexpected end of document".into()));
    }

    // Neither form present: inject the attribute onto the first RDF Description.
    // Existing XML without an RDF subject cannot persist a semantic XMP rating;
    // returning success would leave the caller's file silently unchanged.
    if !wrote {
        let target = injection
            .ok_or_else(|| XmpError::Xml("no RDF Description available for XMP rating".into()))?;
        let (Event::Start(e) | Event::Empty(e)) = events[target.position].clone() else {
            unreachable!()
        };
        let mut new_e = e.clone();
        let rating_value = rating.to_string();
        new_e.push_attribute((target.rating_name.as_str(), rating_value.as_str()));
        if let Some(namespace_name) = &target.namespace_name {
            new_e.push_attribute((namespace_name.as_str(), XMP_NAMESPACE_STR));
        }
        events[target.position] = match &events[target.position] {
            Event::Empty(_) => Event::Empty(new_e),
            _ => Event::Start(new_e),
        };
    }

    for event in events {
        writer
            .write_event(event)
            .map_err(|e| XmpError::Xml(e.to_string()))?;
    }
    String::from_utf8(writer.into_inner()).map_err(|e| XmpError::Xml(e.to_string()))
}

fn rewrite_attrs(
    reader: &NsReader<&[u8]>,
    e: &BytesStart<'_>,
    rating: u8,
    wrote: &mut bool,
) -> Result<BytesStart<'static>, XmpError> {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut new_e = BytesStart::new(name);
    let mut found_rating = false;
    for attr in e.attributes() {
        let attr = attr.map_err(|error| XmpError::Xml(error.to_string()))?;
        if is_xmp_rating_attribute(reader, attr.key) {
            if found_rating {
                return Err(XmpError::Xml(
                    "duplicate XMP rating attributes on one element".into(),
                ));
            }
            found_rating = true;
            *wrote = true;
            new_e.push_attribute((
                String::from_utf8_lossy(attr.key.as_ref()).as_ref(),
                rating.to_string().as_str(),
            ));
        } else {
            new_e.push_attribute(attr);
        }
    }
    Ok(new_e.into_owned())
}

fn validate_attributes(e: &BytesStart<'_>) -> Result<(), XmpError> {
    for attr in e.attributes() {
        attr.map_err(|error| XmpError::Xml(error.to_string()))?;
    }
    Ok(())
}

fn is_direct_rating_content(rating_elements: &[RatingCapture], depth: usize) -> bool {
    rating_elements
        .last()
        .is_some_and(|capture| depth == capture.depth + 1)
}

struct InjectionTarget {
    position: usize,
    rating_name: String,
    namespace_name: Option<String>,
}

fn injection_target(reader: &NsReader<&[u8]>, position: usize) -> InjectionTarget {
    let mut active_prefixes = Vec::new();
    for (declaration, namespace) in reader.prefixes() {
        let PrefixDeclaration::Named(prefix) = declaration else {
            continue;
        };
        active_prefixes.push(prefix.to_vec());
        if is_xmp_namespace(namespace.as_ref())
            && let Ok(prefix) = std::str::from_utf8(prefix)
        {
            return InjectionTarget {
                position,
                rating_name: format!("{prefix}:Rating"),
                namespace_name: None,
            };
        }
    }

    let prefix = (0usize..)
        .map(|suffix| {
            if suffix == 0 {
                "xmp".to_string()
            } else {
                format!("viewrXmp{suffix}")
            }
        })
        .find(|candidate| {
            !active_prefixes
                .iter()
                .any(|prefix| prefix.as_slice() == candidate.as_bytes())
        })
        .expect("an unused namespace prefix must exist");
    InjectionTarget {
        position,
        rating_name: format!("{prefix}:Rating"),
        namespace_name: Some(format!("xmlns:{prefix}")),
    }
}

fn new_sidecar(rating: u8) -> String {
    let bom = '\u{FEFF}';
    format!(
        r#"<?xpacket begin="{bom}" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="viewr">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
   xmp:Rating="{rating}"/>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped like a real Lightroom sidecar: attribute form + crs data.
    const LR_STYLE: &str = r#"<?xpacket begin="" id="W5M0MpCehiHzreSzNTczkc9d"?>
<x:xmpmeta xmlns:x="adobe:ns:meta/" x:xmptk="Adobe XMP Core 7.0-c000">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about=""
    xmlns:xmp="http://ns.adobe.com/xap/1.0/"
    xmlns:crs="http://ns.adobe.com/camera-raw-settings/1.0/"
   xmp:Rating="2"
   crs:Exposure2012="+0.35"
   crs:Contrast2012="0">
   <crs:ToneCurvePV2012>
    <rdf:Seq>
     <rdf:li>0, 0</rdf:li>
     <rdf:li>255, 255</rdf:li>
    </rdf:Seq>
   </crs:ToneCurvePV2012>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>
<?xpacket end="w"?>"#;

    /// Element form (darktable-style).
    const ELEMENT_STYLE: &str = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:xmp="http://ns.adobe.com/xap/1.0/">
   <xmp:Rating>4</xmp:Rating>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;

    #[test]
    fn parses_attribute_form() {
        assert_eq!(parse_rating(LR_STYLE), Some(2));
    }

    #[test]
    fn parses_element_form() {
        assert_eq!(parse_rating(ELEMENT_STYLE), Some(4));
    }

    #[test]
    fn namespace_aliases_work_for_attribute_and_element_forms() {
        let attribute = r#"<rdf:Description
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:alias="http://ns.adobe.com/xap/1.0/"
            alias:Rating="3" keep="yes"/>"#;
        let element = r#"<rdf:Description
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:photo="http://ns.adobe.com/xap/1.0/">
            <photo:Rating>4</photo:Rating>
        </rdf:Description>"#;

        assert_eq!(parse_rating(attribute), Some(3));
        assert_eq!(parse_rating(element), Some(4));

        let attribute = update_rating_xml(attribute, 5).unwrap();
        let element = update_rating_xml(element, 2).unwrap();
        assert!(attribute.contains(r#"alias:Rating="5""#));
        assert!(attribute.contains(r#"keep="yes""#));
        assert!(element.contains("<photo:Rating>2</photo:Rating>"));
        assert_eq!(parse_rating(&attribute), Some(5));
        assert_eq!(parse_rating(&element), Some(2));
    }

    #[test]
    fn namespace_scopes_shadow_and_restore_rating_aliases() {
        let xml = r#"<root xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:p="http://ns.adobe.com/xap/1.0/">
            <group xmlns:p="urn:not-xmp">
                <rdf:Description p:Rating="5"/>
            </group>
            <rdf:Description p:Rating="3"/>
        </root>"#;

        assert_eq!(parse_rating(xml), Some(3));
        let updated = update_rating_xml(xml, 4).unwrap();
        assert!(updated.contains(r#"<rdf:Description p:Rating="5"/>"#));
        assert!(updated.contains(r#"<rdf:Description p:Rating="4"/>"#));
    }

    #[test]
    fn default_rdf_namespace_is_recognized_but_does_not_bind_attributes() {
        let attribute = r#"<Description
            xmlns="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:xmp="http://ns.adobe.com/xap/1.0/"
            xmp:Rating="4"/>"#;
        let element = r#"<Description
            xmlns="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:xmp="http://ns.adobe.com/xap/1.0/">
            <xmp:Rating>4</xmp:Rating>
        </Description>"#;
        let unqualified_attribute = r#"<Description
            xmlns="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            Rating="4"/>"#;

        assert_eq!(parse_rating(attribute), Some(4));
        assert_eq!(parse_rating(element), Some(4));
        assert_eq!(parse_rating(unqualified_attribute), None);
        assert_eq!(
            parse_rating(&update_rating_xml(attribute, 2).unwrap()),
            Some(2)
        );
        assert_eq!(
            parse_rating(&update_rating_xml(element, 2).unwrap()),
            Some(2)
        );

        let injected = update_rating_xml(unqualified_attribute, 3).unwrap();
        assert!(injected.contains(r#"Rating="4""#));
        assert_eq!(parse_rating(&injected), Some(3));
    }

    #[test]
    fn undeclared_rating_prefix_is_not_xmp() {
        let xml = r#"<rdf:Description
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            unknown:Rating="4"/>"#;

        assert_eq!(parse_rating(xml), None);
        let updated = update_rating_xml(xml, 2).unwrap();
        assert!(updated.contains(r#"unknown:Rating="4""#));
        assert_eq!(parse_rating(&updated), Some(2));
    }

    #[test]
    fn escaped_namespace_uri_still_resolves_to_xmp() {
        let xml = r#"<rdf:Description
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:p="http://ns.adobe.com/xap/1.&#48;/"
            p:Rating="4"/>"#;

        assert_eq!(parse_rating(xml), Some(4));
        let updated = update_rating_xml(xml, 1).unwrap();
        assert!(updated.contains(r#"p:Rating="1""#));
        assert_eq!(parse_rating(&updated), Some(1));
    }

    #[test]
    fn malformed_non_description_and_invalid_namespace_bindings_fail_closed() {
        let malformed = r#"<root broken
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description
            xmlns:p="http://ns.adobe.com/xap/1.0/" p:Rating="4"/></root>"#;
        let duplicate_namespace = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:p="urn:one" xmlns:p="urn:two">
            <rdf:Description xmlns:x="http://ns.adobe.com/xap/1.0/" x:Rating="4"/>
        </root>"#;
        let invalid_xml_binding = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:xml="urn:not-xml">
            <rdf:Description xmlns:x="http://ns.adobe.com/xap/1.0/" x:Rating="4"/>
        </root>"#;
        let invalid_xmlns_binding = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:xmlns="urn:not-xmlns">
            <rdf:Description xmlns:x="http://ns.adobe.com/xap/1.0/" x:Rating="4"/>
        </root>"#;

        for xml in [
            malformed,
            duplicate_namespace,
            invalid_xml_binding,
            invalid_xmlns_binding,
        ] {
            assert_eq!(parse_rating(xml), None);
            assert!(matches!(update_rating_xml(xml, 2), Err(XmpError::Xml(_))));
        }
    }

    #[test]
    fn parse_validates_the_document_tail_before_returning_a_rating() {
        let truncated = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:xmp="http://ns.adobe.com/xap/1.0/">
            <rdf:Description xmp:Rating="2"/><broken>"#;
        let mismatched = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:xmp="http://ns.adobe.com/xap/1.0/">
            <rdf:Description xmp:Rating="2"></root>"#;

        for xml in [truncated, mismatched] {
            assert_eq!(parse_rating(xml), None);
            assert!(matches!(update_rating_xml(xml, 4), Err(XmpError::Xml(_))));
        }
    }

    #[test]
    fn non_rdf_description_is_not_a_rating_subject() {
        let xml = r#"<root
            xmlns:fake="urn:not-rdf"
            xmlns:xmp="http://ns.adobe.com/xap/1.0/">
            <fake:Description xmp:Rating="2" keep="yes"/>
        </root>"#;

        assert_eq!(parse_rating(xml), None);
        assert!(matches!(update_rating_xml(xml, 4), Err(XmpError::Xml(_))));
    }

    #[test]
    fn rating_elements_must_be_direct_rdf_description_properties() {
        let xml = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:xmp="http://ns.adobe.com/xap/1.0/">
            <xmp:Rating>1</xmp:Rating>
            <rdf:Description>
                <wrapper><xmp:Rating>2</xmp:Rating></wrapper>
            </rdf:Description>
        </root>"#;

        assert_eq!(parse_rating(xml), None);
        let updated = update_rating_xml(xml, 4).unwrap();
        assert!(updated.contains("<xmp:Rating>1</xmp:Rating>"));
        assert!(updated.contains("<wrapper><xmp:Rating>2</xmp:Rating></wrapper>"));
        assert_eq!(parse_rating(&updated), Some(4));
    }

    #[test]
    fn spaced_single_quoted_rating_splice_preserves_every_other_byte() {
        let xml = r#"<rdf:Description  xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'  xmlns:xmp='http://ns.adobe.com/xap/1.0/'  xmp:Rating = '2'  keep = 'A&amp;B'/>"#;
        let expected = r#"<rdf:Description  xmlns:rdf='http://www.w3.org/1999/02/22-rdf-syntax-ns#'  xmlns:xmp='http://ns.adobe.com/xap/1.0/'  xmp:Rating = '5'  keep = 'A&amp;B'/>"#;

        assert_eq!(update_rating_xml(xml, 5).unwrap(), expected);
        assert_eq!(parse_rating(expected), Some(5));
    }

    #[test]
    fn empty_rating_element_does_not_capture_later_text_and_updates_in_place() {
        let sidecars = [
            r#"<rdf:Description
                xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                xmlns:p="http://ns.adobe.com/xap/1.0/">
                <p:Rating/><unrelated>5</unrelated>
            </rdf:Description>"#,
            r#"<rdf:Description
                xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                xmlns:p="http://ns.adobe.com/xap/1.0/">
                <p:Rating></p:Rating><unrelated>5</unrelated>
            </rdf:Description>"#,
        ];

        for xml in sidecars {
            assert_eq!(parse_rating(xml), None);
            let updated = update_rating_xml(xml, 3).unwrap();
            assert!(updated.contains("<p:Rating>3</p:Rating>"));
            assert!(!updated.contains("<p:Rating/"));
            assert!(!updated.contains(":Rating=\""));
            assert_eq!(parse_rating(&updated), Some(3));
        }
    }

    #[test]
    fn cdata_rating_is_replaced_as_one_scalar_value() {
        let xml = r#"<rdf:Description
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:p="http://ns.adobe.com/xap/1.0/">
            <p:Rating> <![CDATA[4]]> </p:Rating>
        </rdf:Description>"#;

        assert_eq!(parse_rating(xml), Some(4));
        let updated = update_rating_xml(xml, 1).unwrap();
        assert!(updated.contains("<p:Rating><![CDATA[1]]></p:Rating>"));
        assert!(!updated.contains("<![CDATA[4]]>"));
        assert_eq!(parse_rating(&updated), Some(1));
    }

    #[test]
    fn attribute_update_touches_only_the_rating() {
        let updated = update_rating_xml(LR_STYLE, 5).unwrap();
        assert!(updated.contains(r#"xmp:Rating="5""#));
        assert!(updated.contains(r#"crs:Exposure2012="+0.35""#));
        assert!(updated.contains("ToneCurvePV2012"));
        assert!(updated.contains("<?xpacket begin="));
        assert!(updated.contains(r#"<?xpacket end="w"?>"#));
        // Idempotent on re-application.
        assert_eq!(update_rating_xml(&updated, 5).unwrap(), updated);
    }

    #[test]
    fn element_update_replaces_text() {
        let updated = update_rating_xml(ELEMENT_STYLE, 1).unwrap();
        assert!(updated.contains("<xmp:Rating>1</xmp:Rating>"));
    }

    #[test]
    fn injects_attribute_when_absent() {
        let bare = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/">
 <rdf:RDF xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#">
  <rdf:Description rdf:about="" xmlns:dc="http://purl.org/dc/elements/1.1/">
   <dc:subject><rdf:Bag><rdf:li>bird</rdf:li></rdf:Bag></dc:subject>
  </rdf:Description>
 </rdf:RDF>
</x:xmpmeta>"#;
        let updated = update_rating_xml(bare, 3).unwrap();
        assert!(updated.contains(r#"xmp:Rating="3""#));
        assert!(updated.contains("xmlns:xmp=\"http://ns.adobe.com/xap/1.0/\""));
        assert!(updated.contains("bird"));
    }

    #[test]
    fn injection_reuses_an_existing_xmp_namespace_alias() {
        let bare = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"
            xmlns:photo="http://ns.adobe.com/xap/1.0/">
            <rdf:Description
                xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
                keep="yes"/>
        </x:xmpmeta>"#;

        let updated = update_rating_xml(bare, 4).unwrap();

        assert!(updated.contains(r#"photo:Rating="4""#));
        assert!(!updated.contains("xmlns:xmp="));
        assert!(updated.contains(r#"keep="yes""#));
        assert_eq!(parse_rating(&updated), Some(4));
    }

    #[test]
    fn fresh_sidecar_is_parseable_and_carries_rating() {
        let fresh = new_sidecar(4);
        assert_eq!(parse_rating(&fresh), Some(4));
    }

    #[test]
    fn file_update_preserves_develop_settings_and_replaces_rating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.xmp");
        std::fs::write(&path, LR_STYLE).unwrap();

        write_rating(&path, 5).unwrap();

        let updated = std::fs::read_to_string(&path).unwrap();
        assert_eq!(read_rating(&path), Some(5));
        assert!(updated.contains(r#"crs:Exposure2012="+0.35""#));
        assert!(updated.contains("<crs:ToneCurvePV2012>"));
        assert!(updated.contains("<rdf:li>255, 255</rdf:li>"));
        assert!(!path.with_extension("xmp.tmp").exists());
    }

    #[test]
    fn malformed_sidecar_is_preserved_when_update_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.xmp");
        let malformed = br#"<rdf:Description xmp:Rating="2"#;
        std::fs::write(&path, malformed).unwrap();

        let error = write_rating(&path, 4).unwrap_err();

        assert!(matches!(error, XmpError::Xml(_)));
        assert_eq!(std::fs::read(&path).unwrap(), malformed);
        assert!(!path.with_extension("xmp.tmp").exists());
    }

    #[test]
    fn well_formed_sidecar_without_rdf_description_is_preserved() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("photo.xmp");
        let original = br#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><empty/></x:xmpmeta>"#;
        std::fs::write(&path, original).unwrap();

        let error = write_rating(&path, 4).unwrap_err();

        assert!(matches!(error, XmpError::Xml(_)));
        assert_eq!(std::fs::read(&path).unwrap(), original);
        assert!(!path.with_extension("xmp.tmp").exists());
    }

    #[test]
    fn update_rejects_malformed_and_semantically_duplicate_attributes() {
        let malformed = r#"<rdf:Description
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:p="http://ns.adobe.com/xap/1.0/"
            p:Rating="2" broken/>"#;
        let duplicate = r#"<rdf:Description
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:a="http://ns.adobe.com/xap/1.0/"
            xmlns:b="http://ns.adobe.com/xap/1.0/"
            a:Rating="2" b:Rating="3"/>"#;

        assert!(matches!(
            update_rating_xml(malformed, 4),
            Err(XmpError::Xml(_))
        ));
        assert!(matches!(
            update_rating_xml(duplicate, 4),
            Err(XmpError::Xml(_))
        ));
        assert_eq!(parse_rating(malformed), None);
        assert_eq!(parse_rating(duplicate), None);
    }

    #[test]
    fn update_rejects_well_formed_xml_without_an_rdf_description() {
        let original = r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><empty/></x:xmpmeta>"#;

        let error = update_rating_xml(original, 4).unwrap_err();

        assert!(matches!(error, XmpError::Xml(_)));
        assert_eq!(
            original,
            r#"<x:xmpmeta xmlns:x="adobe:ns:meta/"><empty/></x:xmpmeta>"#
        );
    }

    #[test]
    fn update_rejects_truncated_attribute_and_element_documents() {
        let attribute = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"><rdf:Description
            xmlns:p="http://ns.adobe.com/xap/1.0/" p:Rating="2">"#;
        let element = r#"<root
            xmlns:rdf="http://www.w3.org/1999/02/22-rdf-syntax-ns#"
            xmlns:p="http://ns.adobe.com/xap/1.0/">
            <rdf:Description><p:Rating>2"#;

        assert!(matches!(
            update_rating_xml(attribute, 4),
            Err(XmpError::Xml(_))
        ));
        assert!(matches!(
            update_rating_xml(element, 4),
            Err(XmpError::Xml(_))
        ));
    }
}
