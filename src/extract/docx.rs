//! Text of a Word document (DOCX): the `w:t` runs of `word/document.xml`, with any prefix.

use std::io::{Cursor, Read};

use quick_xml::NsReader;
use quick_xml::events::Event;
use quick_xml::name::{Namespace, ResolveResult};

use super::ExtractError;

/// The text of the document. Each paragraph is one line.
pub fn text(bytes: &[u8]) -> Result<String, ExtractError> {
    text_with_limit(bytes, MAX_DOCUMENT_SIZE)
}

/// The WordprocessingML namespace.
const WORD: &[u8] = b"http://schemas.openxmlformats.org/wordprocessingml/2006/main";

/// The maximum uncompressed size of `word/document.xml`.
const MAX_DOCUMENT_SIZE: u64 = 256 * 1024 * 1024;

fn text_with_limit(bytes: &[u8], limit: u64) -> Result<String, ExtractError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| ExtractError::Unreadable)?;
    let document = archive
        .by_name("word/document.xml")
        .map_err(|_| ExtractError::Unreadable)?;
    if document.size() > limit {
        return Err(ExtractError::Unreadable);
    }
    // The size in the ZIP header can be false. Read one byte more than the limit to find out.
    let mut xml = String::new();
    document
        .take(limit + 1)
        .read_to_string(&mut xml)
        .map_err(|_| ExtractError::Unreadable)?;
    if xml.len() as u64 > limit {
        return Err(ExtractError::Unreadable);
    }

    let mut reader = NsReader::from_str(&xml);
    let mut paragraphs = Vec::new();
    let mut paragraph = String::new();
    let mut in_text = false;
    loop {
        let (namespace, event) = reader
            .read_resolved_event()
            .map_err(|_| ExtractError::Unreadable)?;
        // Compare local names, so that any prefix of the WordprocessingML namespace matches.
        let word = matches!(namespace, ResolveResult::Bound(Namespace(WORD)));
        match event {
            Event::Start(tag) if word && tag.local_name().as_ref() == b"t" => in_text = true,
            Event::End(tag) if word && tag.local_name().as_ref() == b"t" => in_text = false,
            Event::End(tag) if word && tag.local_name().as_ref() == b"p" => {
                paragraphs.push(std::mem::take(&mut paragraph));
            }
            Event::Empty(tag) if word && tag.local_name().as_ref() == b"tab" => {
                paragraph.push('\t')
            }
            Event::Empty(tag) if word && tag.local_name().as_ref() == b"br" => paragraph.push('\n'),
            Event::Text(text) if in_text => {
                paragraph.push_str(&text.decode().map_err(|_| ExtractError::Unreadable)?);
            }
            Event::GeneralRef(reference) if in_text => {
                let resolved = match reference.resolve_char_ref() {
                    Ok(Some(char)) => Some(char),
                    _ => match &*reference {
                        b"amp" => Some('&'),
                        b"lt" => Some('<'),
                        b"gt" => Some('>'),
                        b"quot" => Some('"'),
                        b"apos" => Some('\''),
                        _ => None,
                    },
                };
                paragraph.extend(resolved);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !paragraph.is_empty() {
        paragraphs.push(paragraph);
    }
    Ok(paragraphs.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::io::{Cursor, Write};

    use zip::write::SimpleFileOptions;

    use super::{ExtractError, text, text_with_limit};

    /// A DOCX file with only `word/document.xml`.
    fn docx(xml: &str) -> Vec<u8> {
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("word/document.xml", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(xml.as_bytes()).unwrap();
        writer.finish().unwrap().into_inner()
    }

    const W: &str = "http://schemas.openxmlformats.org/wordprocessingml/2006/main";

    #[test]
    fn a_document_larger_than_the_limit_is_unreadable() {
        let xml = format!(r#"<w:document xmlns:w="{W}"><w:body/></w:document>"#);
        let bytes = docx(&xml);

        assert_eq!(
            text_with_limit(&bytes, xml.len() as u64 - 1),
            Err(ExtractError::Unreadable)
        );
        assert_eq!(text_with_limit(&bytes, xml.len() as u64), Ok(String::new()));
    }

    #[test]
    fn another_namespace_prefix_gives_the_same_text() {
        let xml = format!(
            r#"<x:document xmlns:x="{W}"><x:body><x:p><x:r><x:t>Hola</x:t></x:r></x:p><x:p><x:r><x:t>mundo</x:t></x:r></x:p></x:body></x:document>"#
        );

        assert_eq!(text(&docx(&xml)).unwrap(), "Hola\nmundo");
    }

    #[test]
    fn the_default_namespace_gives_the_same_text() {
        let xml = format!(
            r#"<document xmlns="{W}"><body><p><r><t>Hola</t><tab/><t>mundo</t></r></p></body></document>"#
        );

        assert_eq!(text(&docx(&xml)).unwrap(), "Hola\tmundo");
    }

    #[test]
    fn elements_of_another_namespace_are_not_text() {
        let xml = format!(
            r#"<w:document xmlns:w="{W}" xmlns:o="urn:other"><w:body><w:p><w:r><o:t>no</o:t><w:t>sí</w:t></w:r></w:p></w:body></w:document>"#
        );

        assert_eq!(text(&docx(&xml)).unwrap(), "sí");
    }
}
