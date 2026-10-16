//! Text extraction from evidence files, and the split of the text into chunks for search.

pub mod chunk;
mod docx;
#[cfg(target_os = "macos")]
mod macos;

/// The text of one page of a file.
#[derive(Clone, Debug, PartialEq)]
pub struct Page {
    /// Page number, from 1.
    pub number: u32,
    pub text: String,
    pub source: PageSource,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSource {
    /// The text comes from the file.
    Text,
    /// The text comes from optical character recognition of an image.
    Ocr,
}

impl PageSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Ocr => "ocr",
        }
    }

    pub fn parse(value: &str) -> Self {
        match value {
            "ocr" => Self::Ocr,
            _ => Self::Text,
        }
    }
}

/// Why a file has no text. The evidence gets the status `not_analyzed` and the note.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExtractError {
    Unsupported,
    Unreadable,
    /// The PDF needs a password.
    Locked,
    /// Text recognition did not complete.
    Ocr,
    /// PDF and image analysis needs macOS.
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    Platform,
}

impl ExtractError {
    /// A note for the user, in Spanish.
    pub fn note(self) -> &'static str {
        match self {
            Self::Unsupported => "Tipo de archivo sin análisis de texto",
            Self::Unreadable => "Nicolaw no puede leer el texto de este archivo.",
            Self::Locked => "El PDF tiene contraseña. Nicolaw no puede leer el texto.",
            Self::Ocr => "El reconocimiento de texto falló.",
            Self::Platform => "El análisis de PDF e imágenes necesita macOS.",
        }
    }
}

const DOCX_MIME: &str = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

#[derive(Debug, PartialEq)]
enum Kind {
    Pdf,
    Image,
    Text,
    Docx,
    Other,
}

fn kind(name: &str, mime: &str) -> Kind {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_lowercase())
        .unwrap_or_default();
    match (mime, extension.as_str()) {
        ("application/pdf", _) | (_, "pdf") => Kind::Pdf,
        (DOCX_MIME, _) | (_, "docx") => Kind::Docx,
        (_, "txt" | "md" | "markdown" | "csv") => Kind::Text,
        (_, "png" | "jpg" | "jpeg" | "heic" | "heif" | "tif" | "tiff" | "webp")
        | (
            "image/png" | "image/jpeg" | "image/heic" | "image/heif" | "image/tiff" | "image/webp",
            _,
        ) => Kind::Image,
        _ if mime.starts_with("text/") => Kind::Text,
        _ => Kind::Other,
    }
}

/// Extracts the text of a file, page by page.
/// This function can run for a long time (OCR). Do not call it on the UI thread.
pub fn extract(name: &str, mime: &str, bytes: &[u8]) -> Result<Vec<Page>, ExtractError> {
    let one_page = |text: String| {
        vec![Page {
            number: 1,
            text,
            source: PageSource::Text,
        }]
    };
    match kind(name, mime) {
        Kind::Text => Ok(one_page(String::from_utf8_lossy(bytes).into_owned())),
        Kind::Docx => docx::text(bytes).map(one_page),
        #[cfg(target_os = "macos")]
        Kind::Pdf => macos::pdf(bytes),
        #[cfg(target_os = "macos")]
        Kind::Image => macos::image(bytes),
        #[cfg(not(target_os = "macos"))]
        Kind::Pdf | Kind::Image => Err(ExtractError::Platform),
        Kind::Other => Err(ExtractError::Unsupported),
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtractError, Kind, Page, PageSource, extract, kind};

    const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures");

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{FIXTURES}/{name}")).unwrap()
    }

    /// True if every word is in the text, without regard to case.
    fn contains_all(text: &str, words: &[&str]) -> bool {
        let text = text.to_lowercase();
        words.iter().all(|word| text.contains(word))
    }

    #[test]
    fn plain_text_is_one_text_page() {
        let pages = extract("sample.txt", "text/plain", &fixture("sample.txt")).unwrap();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].number, 1);
        assert_eq!(pages[0].source, PageSource::Text);
        assert!(pages[0].text.starts_with("La declaración del acusado"));
    }

    #[test]
    fn markdown_and_csv_are_text() {
        let pages = extract("notes.md", "text/markdown", "# Título\nTexto".as_bytes()).unwrap();
        assert_eq!(pages[0].text, "# Título\nTexto");
        let pages = extract("data.csv", "text/csv", b"a,b\n1,2").unwrap();
        assert_eq!(pages[0].text, "a,b\n1,2");
    }

    #[test]
    fn docx_paragraphs_are_lines_of_one_page() {
        let mime = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";

        let pages = extract("sample.docx", mime, &fixture("sample.docx")).unwrap();

        assert_eq!(
            pages,
            [Page {
                number: 1,
                text: "La declaración del acusado fue rendida el 12 de marzo de 2024 en Bogotá.\n\
                       El testigo afirmó que el vehículo era de color rojo."
                    .into(),
                source: PageSource::Text,
            }]
        );
    }

    #[test]
    fn a_damaged_docx_is_unreadable() {
        let mime = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        assert_eq!(
            extract("bad.docx", mime, b"not a zip"),
            Err(ExtractError::Unreadable)
        );
    }

    #[test]
    fn a_supported_image_mime_type_is_an_image_with_any_extension() {
        assert_eq!(kind("foto.jfif", "image/jpeg"), Kind::Image);
        assert_eq!(kind("foto.jpe", "image/jpeg"), Kind::Image);
        for mime in [
            "image/png",
            "image/tiff",
            "image/heic",
            "image/heif",
            "image/webp",
        ] {
            assert_eq!(kind("foto", mime), Kind::Image, "{mime}");
        }
        assert_eq!(kind("dibujo.svg", "image/svg+xml"), Kind::Other);
    }

    #[test]
    fn other_types_are_not_analyzed() {
        let error = extract("data.bin", "application/octet-stream", &[0, 1, 2]).unwrap_err();

        assert_eq!(error, ExtractError::Unsupported);
        assert_eq!(error.note(), "Tipo de archivo sin análisis de texto");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn text_pdf_gives_the_text_of_each_page() {
        let pages = extract("sample.pdf", "application/pdf", &fixture("sample.pdf")).unwrap();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].number, 1);
        assert_eq!(pages[0].source, PageSource::Text);
        assert!(contains_all(
            &pages[0].text,
            &["declaración", "acusado", "bogotá", "vehículo"]
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_damaged_pdf_is_unreadable() {
        assert_eq!(
            extract("bad.pdf", "application/pdf", b"not a pdf"),
            Err(ExtractError::Unreadable)
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn image_text_is_recognized() {
        let pages = extract("scanned.png", "image/png", &fixture("scanned.png")).unwrap();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].source, PageSource::Ocr);
        assert!(
            contains_all(&pages[0].text, &["acusado", "marzo", "testigo"]),
            "OCR text: {}",
            pages[0].text
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_pdf_page_without_text_is_recognized_from_its_image() {
        let pages = extract("scanned.pdf", "application/pdf", &fixture("scanned.pdf")).unwrap();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].source, PageSource::Ocr);
        assert!(
            contains_all(&pages[0].text, &["acusado", "marzo", "testigo"]),
            "OCR text: {}",
            pages[0].text
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn a_pdf_page_with_a_short_text_header_is_recognized_from_its_image() {
        // One page: the text layer has only the header "Acta 7", and the body is a scanned image.
        let pages = extract(
            "header-scan.pdf",
            "application/pdf",
            &fixture("header-scan.pdf"),
        )
        .unwrap();

        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].source, PageSource::Ocr);
        assert!(
            contains_all(&pages[0].text, &["acta", "acusado", "marzo", "testigo"]),
            "OCR text: {}",
            pages[0].text
        );
    }
}
