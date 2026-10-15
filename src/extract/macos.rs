//! PDF text (PDFKit) and optical character recognition (Vision). macOS only.

use objc2::AnyThread as _;
use objc2::rc::autoreleasepool;
use objc2_foundation::{NSArray, NSData, NSDictionary, NSSize, NSString};
use objc2_pdf_kit::{PDFDisplayBox, PDFDocument};
use objc2_vision::{
    VNImageRequestHandler, VNRecognizeTextRequest, VNRequest, VNRequestTextRecognitionLevel,
};

use super::{ExtractError, Page, PageSource};

/// A page without text is drawn at this scale before OCR, so that small text is legible.
const OCR_SCALE: f64 = 2.0;

/// The text of each page. A page without text goes through OCR.
pub fn pdf(bytes: &[u8]) -> Result<Vec<Page>, ExtractError> {
    autoreleasepool(|_| {
        let data = NSData::with_bytes(bytes);
        // SAFETY: `data` is a valid NSData. PDFKit returns nil for data that is not a PDF.
        let document = unsafe { PDFDocument::initWithData(PDFDocument::alloc(), &data) }
            .ok_or(ExtractError::Unreadable)?;
        // SAFETY: plain property reads on a valid document.
        if unsafe { document.isLocked() } {
            return Err(ExtractError::Locked);
        }
        let count = unsafe { document.pageCount() };
        let mut pages = Vec::with_capacity(count);
        for index in 0..count {
            // SAFETY: `index` is less than the page count.
            let Some(page) = (unsafe { document.pageAtIndex(index) }) else {
                return Err(ExtractError::Unreadable);
            };
            let number = index as u32 + 1;
            // SAFETY: plain property read on a valid page.
            let text = unsafe { page.string() }
                .map(|text| text.to_string())
                .unwrap_or_default();
            if !text.trim().is_empty() {
                pages.push(Page {
                    number,
                    text,
                    source: PageSource::Text,
                });
                continue;
            }
            // SAFETY: the page is valid, and the box is a defined PDFDisplayBox value.
            let bounds = unsafe { page.boundsForBox(PDFDisplayBox::MediaBox) };
            let size = NSSize::new(
                bounds.size.width * OCR_SCALE,
                bounds.size.height * OCR_SCALE,
            );
            let image = unsafe { page.thumbnailOfSize_forBox(size, PDFDisplayBox::MediaBox) };
            let tiff = image.TIFFRepresentation().ok_or(ExtractError::Ocr)?;
            pages.push(Page {
                number,
                text: recognize(&tiff)?,
                source: PageSource::Ocr,
            });
        }
        Ok(pages)
    })
}

/// The text in an image file (PNG, JPEG, HEIC, TIFF, WebP), as one page.
pub fn image(bytes: &[u8]) -> Result<Vec<Page>, ExtractError> {
    autoreleasepool(|_| {
        let text = recognize(&NSData::with_bytes(bytes))?;
        Ok(vec![Page {
            number: 1,
            text,
            source: PageSource::Ocr,
        }])
    })
}

/// Runs Vision text recognition on encoded image data. Lines are joined with newlines, top to
/// bottom, then left to right.
fn recognize(data: &NSData) -> Result<String, ExtractError> {
    let handler = VNImageRequestHandler::initWithData_options(
        VNImageRequestHandler::alloc(),
        data,
        &NSDictionary::new(),
    );
    let request = VNRecognizeTextRequest::new();
    request.setRecognitionLevel(VNRequestTextRecognitionLevel::Accurate);
    request.setRecognitionLanguages(&NSArray::from_retained_slice(&[
        NSString::from_str("es-ES"),
        NSString::from_str("en-US"),
    ]));
    request.setUsesLanguageCorrection(true);
    let base: &VNRequest = &request;
    handler
        .performRequests_error(&NSArray::from_slice(&[base]))
        .map_err(|_| ExtractError::Ocr)?;

    let Some(observations) = request.results() else {
        return Ok(String::new());
    };
    let mut lines: Vec<(i64, f64, String)> = observations
        .iter()
        .filter_map(|observation| {
            let text = observation
                .topCandidates(1)
                .firstObject()?
                .string()
                .to_string();
            // SAFETY: plain property read. The origin is at the lower-left corner, and the
            // values are normalized to the image size.
            let bounds = unsafe { observation.boundingBox() };
            let top = bounds.origin.y + bounds.size.height;
            // Lines whose tops are within 1% of the image height are on the same row.
            let row = ((1.0 - top) * 100.0).round() as i64;
            Some((row, bounds.origin.x, text))
        })
        .collect();
    lines.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.total_cmp(&b.1)));
    Ok(lines
        .into_iter()
        .map(|(_, _, text)| text)
        .collect::<Vec<_>>()
        .join("\n"))
}
