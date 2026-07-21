//! XMP sidecar reading and merge-preserving rating writes.
//!
//! Lightroom reads `xmp:Rating` (namespace `http://ns.adobe.com/xap/1.0/`)
//! from `.xmp` sidecars on import. It writes scalar properties as
//! ATTRIBUTES on `rdf:Description`; other tools (darktable) use the
//! ELEMENT form. We read both and, when updating, rewrite only the
//! rating token — every other byte of an existing sidecar passes through
//! untouched, so Lightroom develop settings/keywords survive.

use std::io::Write as _;
use std::path::Path;

use quick_xml::Reader;
use quick_xml::Writer;
use quick_xml::events::{BytesStart, Event};

#[derive(Debug, thiserror::Error)]
pub enum XmpError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("xml: {0}")]
    Xml(String),
}

fn is_rating_name(name: &[u8]) -> bool {
    name == b"xmp:Rating"
}

fn is_description(name: &[u8]) -> bool {
    name == b"rdf:Description" || name.ends_with(b":Description")
}

/// Read the rating from a sidecar (attribute or element form).
/// Returns None if the file or the property is absent. 0 ⇒ unrated.
pub fn read_rating(path: &Path) -> Option<u8> {
    let content = std::fs::read_to_string(path).ok()?;
    parse_rating(&content)
}

pub fn parse_rating(xml: &str) -> Option<u8> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut in_rating_element = false;
    loop {
        match reader.read_event().ok()? {
            Event::Eof => return None,
            Event::Start(e) | Event::Empty(e) => {
                if is_rating_name(e.name().as_ref()) {
                    in_rating_element = true;
                } else if is_description(e.name().as_ref()) {
                    for attr in e.attributes().flatten() {
                        if is_rating_name(attr.key.as_ref())
                            && let Ok(v) = attr.unescape_value()
                            && let Ok(n) = v.trim().parse::<f32>()
                        {
                            return Some(n.max(0.0) as u8);
                        }
                    }
                }
            }
            Event::Text(t) if in_rating_element => {
                if let Ok(v) = t.unescape()
                    && let Ok(n) = v.trim().parse::<f32>()
                {
                    return Some(n.max(0.0) as u8);
                }
            }
            Event::End(e) if is_rating_name(e.name().as_ref()) => {
                in_rating_element = false;
            }
            _ => {}
        }
    }
}

/// Write `rating` into the sidecar, preserving all other content.
/// Creates a minimal sidecar if none exists. Atomic (tmp + rename).
pub fn write_rating(path: &Path, rating: u8) -> Result<(), XmpError> {
    let output = match std::fs::read_to_string(path) {
        Ok(existing) => update_rating_xml(&existing, rating)?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => new_sidecar(rating),
        Err(e) => return Err(e.into()),
    };
    let tmp = path.with_extension("xmp.tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(output.as_bytes())?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Rewrite only the xmp:Rating token inside existing sidecar XML.
pub fn update_rating_xml(xml: &str, rating: u8) -> Result<String, XmpError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut writer = Writer::new(Vec::new());
    let mut wrote = false;
    let mut replace_next_text = false;
    let mut first_description_pos: Option<usize> = None;
    let mut events: Vec<Event<'static>> = Vec::new();

    loop {
        match reader
            .read_event()
            .map_err(|e| XmpError::Xml(e.to_string()))?
        {
            Event::Eof => break,
            Event::Start(e) => {
                let is_desc = is_description(e.name().as_ref());
                let is_rating = is_rating_name(e.name().as_ref());
                let rewritten = if is_desc {
                    if first_description_pos.is_none() {
                        first_description_pos = Some(events.len());
                    }
                    rewrite_attrs(&e, rating, &mut wrote)
                } else {
                    e.into_owned()
                };
                if is_rating {
                    replace_next_text = true;
                }
                events.push(Event::Start(rewritten));
            }
            Event::Empty(e) => {
                let rewritten = if is_description(e.name().as_ref()) {
                    if first_description_pos.is_none() {
                        first_description_pos = Some(events.len());
                    }
                    rewrite_attrs(&e, rating, &mut wrote)
                } else {
                    e.into_owned()
                };
                events.push(Event::Empty(rewritten));
            }
            Event::Text(t) if replace_next_text => {
                replace_next_text = false;
                wrote = true;
                let text = rating.to_string();
                events.push(Event::Text(
                    quick_xml::events::BytesText::new(&text).into_owned(),
                ));
                let _ = t;
            }
            other => events.push(other.into_owned()),
        }
    }

    // Neither form present: inject the attribute onto the first Description.
    if !wrote && let Some(pos) = first_description_pos {
        let (Event::Start(e) | Event::Empty(e)) = events[pos].clone() else {
            unreachable!()
        };
        let mut new_e = e.clone();
        new_e.push_attribute(("xmp:Rating", rating.to_string().as_str()));
        // Ensure the xmp namespace exists on this element or assume the
        // document declares it (Lightroom sidecars always do).
        let has_ns = e
            .attributes()
            .flatten()
            .any(|a| a.key.as_ref() == b"xmlns:xmp")
            || xml.contains("xmlns:xmp");
        if !has_ns {
            new_e.push_attribute(("xmlns:xmp", "http://ns.adobe.com/xap/1.0/"));
        }
        events[pos] = match &events[pos] {
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

fn rewrite_attrs(e: &BytesStart<'_>, rating: u8, wrote: &mut bool) -> BytesStart<'static> {
    let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
    let mut new_e = BytesStart::new(name);
    for attr in e.attributes().flatten() {
        if is_rating_name(attr.key.as_ref()) {
            *wrote = true;
            new_e.push_attribute((
                String::from_utf8_lossy(attr.key.as_ref()).as_ref(),
                rating.to_string().as_str(),
            ));
        } else {
            new_e.push_attribute(attr);
        }
    }
    new_e.into_owned()
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
}
