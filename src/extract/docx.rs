//! Text of a Word document (DOCX): the `w:t` runs of `word/document.xml`.

use std::io::{Cursor, Read};

use quick_xml::Reader;
use quick_xml::events::Event;

use super::ExtractError;

/// The text of the document. Each paragraph is one line.
pub fn text(bytes: &[u8]) -> Result<String, ExtractError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| ExtractError::Unreadable)?;
    let mut xml = String::new();
    archive
        .by_name("word/document.xml")
        .map_err(|_| ExtractError::Unreadable)?
        .read_to_string(&mut xml)
        .map_err(|_| ExtractError::Unreadable)?;

    let mut reader = Reader::from_str(&xml);
    let mut paragraphs = Vec::new();
    let mut paragraph = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event().map_err(|_| ExtractError::Unreadable)? {
            Event::Start(tag) if tag.name().as_ref() == b"w:t" => in_text = true,
            Event::End(tag) if tag.name().as_ref() == b"w:t" => in_text = false,
            Event::End(tag) if tag.name().as_ref() == b"w:p" => {
                paragraphs.push(std::mem::take(&mut paragraph));
            }
            Event::Empty(tag) if tag.name().as_ref() == b"w:tab" => paragraph.push('\t'),
            Event::Empty(tag) if tag.name().as_ref() == b"w:br" => paragraph.push('\n'),
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
