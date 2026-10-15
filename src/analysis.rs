//! Analysis of one evidence file: text extraction, then storage of pages and chunks.

use std::sync::Mutex;

use crate::extract::extract;
use crate::store::{AnalysisStatus, Store};

/// Extracts the text of the evidence and stores the result. Returns the new status: `ready`, or
/// `not_analyzed` with a note when the file has no text that Nicolaw can read.
///
/// The store is not locked during the extraction, because OCR can take many seconds.
/// Do not call this function on the UI thread.
pub fn run(store: &Mutex<Store>, id: i64) -> rusqlite::Result<AnalysisStatus> {
    let (entry, content) = {
        let store = store.lock().unwrap();
        (store.evidence(id)?, store.content(id, |_, _| {})?)
    };
    let pages = extract(&entry.name, &entry.mime, &content);
    let store = store.lock().unwrap();
    match pages {
        Ok(pages) => {
            store.save_analysis(id, &pages)?;
            Ok(AnalysisStatus::Ready)
        }
        Err(error) => {
            store.set_status(id, AnalysisStatus::NotAnalyzed, Some(error.note()))?;
            Ok(AnalysisStatus::NotAnalyzed)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::Mutex;

    use super::run;
    use crate::store::{AnalysisStatus, Store};

    fn store_with_file(name: &str, content: &[u8]) -> (Mutex<Store>, i64, i64) {
        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso").unwrap().id;
        let dir = std::env::temp_dir().join(format!("nicolaw-analysis-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        let id = store.import(case, &path, |_, _| {}).unwrap().id;
        (Mutex::new(store), case, id)
    }

    #[test]
    fn a_text_file_becomes_ready_and_searchable() {
        let text = "La declaración del acusado fue rendida en Bogotá.";
        let (store, case, id) = store_with_file("acta.txt", text.as_bytes());

        assert_eq!(run(&store, id).unwrap(), AnalysisStatus::Ready);

        let store = store.lock().unwrap();
        assert_eq!(store.evidence(id).unwrap().status, AnalysisStatus::Ready);
        assert_eq!(store.pages(id).unwrap()[0].text, text);
        assert_eq!(store.search(case, "declaracion", 5).unwrap().len(), 1);
    }

    #[test]
    fn an_unsupported_file_is_not_analyzed_with_a_note() {
        let (store, _, id) = store_with_file("data.bin", &[0, 1, 2]);

        assert_eq!(run(&store, id).unwrap(), AnalysisStatus::NotAnalyzed);

        let entry = store.lock().unwrap().evidence(id).unwrap();
        assert_eq!(entry.status, AnalysisStatus::NotAnalyzed);
        assert_eq!(
            entry.note.as_deref(),
            Some("Tipo de archivo sin análisis de texto")
        );
    }
}
