use gpui_kit::ImageFormat;

/// The text preview shows at most this number of bytes.
pub const TEXT_PREVIEW_LIMIT: usize = 64 * 1024;

#[derive(Clone, Debug, PartialEq)]
pub enum Preview {
    Image(ImageFormat),
    Text(String),
    None,
}

/// Selects how to preview a file from its MIME type and content.
pub fn classify(mime: &str, content: &[u8]) -> Preview {
    if let Some(format) = ImageFormat::from_mime_type(mime) {
        return Preview::Image(format);
    }
    let head = &content[..content.len().min(TEXT_PREVIEW_LIMIT)];
    let text = match std::str::from_utf8(head) {
        Ok(text) => text,
        // The limit cut a character in two: keep the complete part.
        Err(error) if error.error_len().is_none() => {
            std::str::from_utf8(&head[..error.valid_up_to()]).unwrap()
        }
        Err(_) => return Preview::None,
    };
    if text.contains('\0') {
        return Preview::None;
    }
    Preview::Text(text.to_owned())
}

pub fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;
    match bytes {
        b if b < KB => format!("{b} B"),
        b if b < MB => format!("{} KB", (b as f64 / KB as f64).round()),
        b if b < GB => format!("{:.1} MB", b as f64 / MB as f64),
        b => format!("{:.1} GB", b as f64 / GB as f64),
    }
}

/// A short type label for the file list, for example "PDF".
pub fn type_label(name: &str) -> String {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() && !extension.is_empty() => {
            extension.to_uppercase()
        }
        _ => "Archivo".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supported_image_types_are_images() {
        assert_eq!(
            classify("image/png", b"\x89PNG"),
            Preview::Image(ImageFormat::Png)
        );
        assert_eq!(
            classify("image/jpeg", b""),
            Preview::Image(ImageFormat::Jpeg)
        );
        assert_eq!(
            classify("image/svg+xml", b"<svg/>"),
            Preview::Image(ImageFormat::Svg)
        );
    }

    #[test]
    fn utf8_content_is_text() {
        assert_eq!(
            classify("text/plain", "héllo".as_bytes()),
            Preview::Text("héllo".into())
        );
        assert_eq!(
            classify("application/json", b"{}"),
            Preview::Text("{}".into())
        );
    }

    #[test]
    fn binary_content_has_no_preview() {
        assert_eq!(
            classify("application/pdf", &[0x25, 0xff, 0xfe]),
            Preview::None
        );
        assert_eq!(classify("application/octet-stream", b"a\0b"), Preview::None);
    }

    #[test]
    fn long_text_is_cut_at_a_char_boundary() {
        // "é" has 2 bytes. After "a", the limit falls in the middle of an "é".
        let text = format!("a{}", "é".repeat(TEXT_PREVIEW_LIMIT));
        let Preview::Text(preview) = classify("text/plain", text.as_bytes()) else {
            panic!("expected a text preview");
        };
        assert_eq!(preview.len(), TEXT_PREVIEW_LIMIT - 1);
        assert!(text.starts_with(&preview));
    }

    #[test]
    fn sizes_use_the_largest_fitting_unit() {
        assert_eq!(format_size(0), "0 B");
        assert_eq!(format_size(1023), "1023 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(310 * 1024), "310 KB");
        assert_eq!(format_size(2_516_582), "2.4 MB");
        assert_eq!(format_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn type_label_is_the_upper_case_extension() {
        assert_eq!(type_label("report.pdf"), "PDF");
        assert_eq!(type_label("archive.tar.gz"), "GZ");
        assert_eq!(type_label("Makefile"), "Archivo");
    }
}
