use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use rusqlite::{Connection, MAIN_DB, OptionalExtension as _, params};
use sha2::{Digest, Sha256};

use crate::extract::chunk::chunk;
use crate::extract::{Page, PageSource};

/// The largest file that the store accepts: 100 MB.
pub const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// The store moves file content in blocks of this size, and reports progress after each block.
pub const BLOCK_SIZE: usize = 256 * 1024;

/// Version 1 had one `files` table. Version 2 has cases, evidence, pages, chunks, and search.
const SCHEMA_VERSION: i64 = 2;

const SCHEMA_V2: &str = "
    DROP TABLE IF EXISTS files;
    CREATE TABLE cases (
        id         INTEGER PRIMARY KEY,
        name       TEXT    NOT NULL,
        created_at INTEGER NOT NULL -- Unix seconds
    );
    CREATE TABLE evidence (
        id              INTEGER PRIMARY KEY,
        case_id         INTEGER NOT NULL REFERENCES cases(id),
        name            TEXT    NOT NULL,
        mime            TEXT    NOT NULL,
        size            INTEGER NOT NULL,
        sha256          TEXT    NOT NULL,
        added_at        INTEGER NOT NULL, -- Unix seconds
        content         BLOB    NOT NULL,
        analysis_status TEXT    NOT NULL DEFAULT 'pending'
            CHECK (analysis_status IN ('pending', 'analyzing', 'ready', 'not_analyzed')),
        analysis_note   TEXT
    );
    CREATE INDEX evidence_case ON evidence(case_id);
    CREATE TABLE evidence_pages (
        evidence_id INTEGER NOT NULL REFERENCES evidence(id),
        page        INTEGER NOT NULL,
        text        TEXT    NOT NULL,
        source      TEXT    NOT NULL CHECK (source IN ('text', 'ocr')),
        PRIMARY KEY (evidence_id, page)
    );
    CREATE TABLE chunks (
        id          INTEGER PRIMARY KEY,
        evidence_id INTEGER NOT NULL REFERENCES evidence(id),
        page        INTEGER NOT NULL,
        idx         INTEGER NOT NULL,
        text        TEXT    NOT NULL
    );
    CREATE INDEX chunks_evidence ON chunks(evidence_id);
    CREATE VIRTUAL TABLE chunks_fts USING fts5(
        text,
        content = 'chunks',
        content_rowid = 'id',
        tokenize = 'unicode61 remove_diacritics 2'
    );
    CREATE TRIGGER chunks_ai AFTER INSERT ON chunks BEGIN
        INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
    END;
    CREATE TRIGGER chunks_ad AFTER DELETE ON chunks BEGIN
        INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
    END;
    CREATE TRIGGER chunks_au AFTER UPDATE ON chunks BEGIN
        INSERT INTO chunks_fts(chunks_fts, rowid, text) VALUES ('delete', old.id, old.text);
        INSERT INTO chunks_fts(rowid, text) VALUES (new.id, new.text);
    END;
    CREATE TABLE settings (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
    );
";

#[derive(Clone, Debug, PartialEq)]
pub struct Case {
    pub id: i64,
    pub name: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnalysisStatus {
    Pending,
    Analyzing,
    Ready,
    NotAnalyzed,
}

impl AnalysisStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Analyzing => "analyzing",
            Self::Ready => "ready",
            Self::NotAnalyzed => "not_analyzed",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "analyzing" => Self::Analyzing,
            "ready" => Self::Ready,
            "not_analyzed" => Self::NotAnalyzed,
            _ => Self::Pending,
        }
    }
}

/// One evidence file, without its content.
#[derive(Clone, Debug)]
pub struct Evidence {
    pub id: i64,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Lowercase hexadecimal SHA-256 of the content.
    pub sha256: String,
    /// Local time, as `YYYY-MM-DD HH:MM`.
    pub added_at: String,
    pub status: AnalysisStatus,
    pub note: Option<String>,
    /// Number of pages with extracted text.
    pub pages: u32,
}

/// One search result: a chunk of evidence text that matches the query.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub evidence_id: i64,
    pub evidence_name: String,
    pub page: u32,
    pub chunk_id: i64,
    pub snippet: String,
    /// BM25 rank. A lower value is a better match.
    // The list shows hits in rank order. The rank value is for the retrieval in layer 3.
    #[allow(dead_code)]
    pub rank: f64,
}

#[derive(Debug)]
pub enum ImportError {
    TooLarge,
    Read,
    /// The file changed size during the import.
    Changed,
    Storage,
}

impl From<rusqlite::Error> for ImportError {
    fn from(_: rusqlite::Error) -> Self {
        Self::Storage
    }
}

pub struct Store {
    conn: Connection,
}

const SELECT_EVIDENCE: &str = "SELECT e.id, e.name, e.mime, e.size, e.sha256, \
     strftime('%Y-%m-%d %H:%M', e.added_at, 'unixepoch', 'localtime'), \
     e.analysis_status, e.analysis_note, \
     (SELECT count(*) FROM evidence_pages p WHERE p.evidence_id = e.id) \
     FROM evidence e";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::migrate(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::migrate(Connection::open_in_memory()?)
    }

    fn migrate(mut conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < SCHEMA_VERSION {
            // No data of version 1 is in use, so the migration drops the `files` table.
            let tx = conn.transaction()?;
            tx.execute_batch(SCHEMA_V2)?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    pub fn create_case(&self, name: &str) -> rusqlite::Result<Case> {
        self.conn.execute(
            "INSERT INTO cases (name, created_at) VALUES (?1, unixepoch())",
            [name],
        )?;
        Ok(Case {
            id: self.conn.last_insert_rowid(),
            name: name.to_owned(),
        })
    }

    /// Returns all cases, newest first.
    pub fn cases(&self) -> rusqlite::Result<Vec<Case>> {
        self.conn
            .prepare("SELECT id, name FROM cases ORDER BY created_at DESC, id DESC")?
            .query_map([], |row| {
                Ok(Case {
                    id: row.get(0)?,
                    name: row.get(1)?,
                })
            })?
            .collect()
    }

    pub fn setting(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
                row.get(0)
            })
            .optional()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        Ok(())
    }

    /// Reads the file at `path` and stores a copy of it in the case, with the status `pending`.
    /// Calls `progress` with (bytes done, bytes total) after each block.
    /// If the import fails, the store keeps no row for the file.
    pub fn import(
        &self,
        case_id: i64,
        path: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<Evidence, ImportError> {
        let size = fs::metadata(path).map_err(|_| ImportError::Read)?.len();
        if size > MAX_FILE_SIZE {
            return Err(ImportError::TooLarge);
        }
        let mut file = File::open(path).map_err(|_| ImportError::Read)?.take(size);
        let name = file_name(path);
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();

        // Dropping the transaction without a commit removes the row.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO evidence (case_id, name, mime, size, sha256, added_at, content)
             VALUES (?1, ?2, ?3, ?4, '', unixepoch(), zeroblob(?4))",
            params![case_id, name, mime, size as i64],
        )?;
        let id = tx.last_insert_rowid();
        let mut hasher = Sha256::new();
        {
            let mut blob = tx.blob_open(MAIN_DB, "evidence", "content", id, false)?;
            let mut buffer = vec![0; BLOCK_SIZE];
            let mut done = 0;
            // One pass for an empty file, so that the caller gets (0, 0).
            loop {
                let read = file.read(&mut buffer).map_err(|_| ImportError::Read)?;
                if read == 0 && done < size {
                    // The file became shorter after the metadata read.
                    return Err(ImportError::Read);
                }
                blob.write_all(&buffer[..read])
                    .map_err(|_| ImportError::Storage)?;
                hasher.update(&buffer[..read]);
                done += read as u64;
                progress(done, size);
                if done == size {
                    break;
                }
            }
        }
        // The file became longer after the metadata read, so the copy is not complete.
        let mut file = file.into_inner();
        let more = file.read(&mut [0]).map_err(|_| ImportError::Read)?;
        let len = file.metadata().map_err(|_| ImportError::Read)?.len();
        if more != 0 || len != size {
            return Err(ImportError::Changed);
        }
        let sha256: String = hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        tx.execute(
            "UPDATE evidence SET sha256 = ?1 WHERE id = ?2",
            params![sha256, id],
        )?;
        let entry = tx.query_row(
            &format!("{SELECT_EVIDENCE} WHERE e.id = ?1"),
            [id],
            row_to_evidence,
        )?;
        tx.commit()?;
        Ok(entry)
    }

    /// Returns the evidence of a case, newest first. The content is not read.
    pub fn list(&self, case_id: i64) -> rusqlite::Result<Vec<Evidence>> {
        self.conn
            .prepare(&format!(
                "{SELECT_EVIDENCE} WHERE e.case_id = ?1 ORDER BY e.added_at DESC, e.id DESC"
            ))?
            .query_map([case_id], row_to_evidence)?
            .collect()
    }

    pub fn evidence(&self, id: i64) -> rusqlite::Result<Evidence> {
        self.conn.query_row(
            &format!("{SELECT_EVIDENCE} WHERE e.id = ?1"),
            [id],
            row_to_evidence,
        )
    }

    /// Reads the content of a file.
    /// Calls `progress` with (bytes done, bytes total) after each block.
    pub fn content(
        &self,
        id: i64,
        mut progress: impl FnMut(u64, u64),
    ) -> rusqlite::Result<Vec<u8>> {
        let mut blob = self
            .conn
            .blob_open(MAIN_DB, "evidence", "content", id, true)?;
        let size = blob.len();
        let mut content = vec![0; size];
        let mut done = 0;
        loop {
            let end = (done + BLOCK_SIZE).min(size);
            blob.read_exact(&mut content[done..end])
                .map_err(|_| rusqlite::Error::BlobSizeError)?;
            done = end;
            progress(done as u64, size as u64);
            if done == size {
                return Ok(content);
            }
        }
    }

    pub fn set_status(
        &self,
        id: i64,
        status: AnalysisStatus,
        note: Option<&str>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE evidence SET analysis_status = ?1, analysis_note = ?2 WHERE id = ?3",
            params![status.as_str(), note, id],
        )?;
        Ok(())
    }

    /// Evidence with the status `pending` or `analyzing`, oldest first. After a restart, this
    /// evidence needs analysis again.
    pub fn unfinished_analysis(&self) -> rusqlite::Result<Vec<i64>> {
        self.conn
            .prepare(
                "SELECT id FROM evidence
                 WHERE analysis_status IN ('pending', 'analyzing') ORDER BY id",
            )?
            .query_map([], |row| row.get(0))?
            .collect()
    }

    /// Replaces the pages and chunks of the evidence, and sets the status `ready`.
    pub fn save_analysis(&self, id: i64, pages: &[Page]) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_analysis(&tx, id)?;
        for page in pages {
            tx.execute(
                "INSERT INTO evidence_pages (evidence_id, page, text, source)
                 VALUES (?1, ?2, ?3, ?4)",
                params![id, page.number, page.text, page.source.as_str()],
            )?;
            for chunk in chunk(page.number, &page.text) {
                tx.execute(
                    "INSERT INTO chunks (evidence_id, page, idx, text) VALUES (?1, ?2, ?3, ?4)",
                    params![id, chunk.page, chunk.idx, chunk.text],
                )?;
            }
        }
        tx.execute(
            "UPDATE evidence SET analysis_status = 'ready', analysis_note = NULL WHERE id = ?1",
            [id],
        )?;
        tx.commit()
    }

    /// The extracted pages of the evidence, in page order.
    pub fn pages(&self, id: i64) -> rusqlite::Result<Vec<Page>> {
        self.conn
            .prepare(
                "SELECT page, text, source FROM evidence_pages WHERE evidence_id = ?1 ORDER BY page",
            )?
            .query_map([id], |row| {
                Ok(Page {
                    number: row.get(0)?,
                    text: row.get(1)?,
                    source: PageSource::parse(&row.get::<_, String>(2)?),
                })
            })?
            .collect()
    }

    /// Full-text search in the evidence of one case, best match first. Accents and case do not
    /// matter, and each word also matches as a prefix. Any user input is safe.
    pub fn search(
        &self,
        case_id: i64,
        query: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        let Some(query) = fts_query(query) else {
            return Ok(Vec::new());
        };
        self.conn
            .prepare(
                "SELECT e.id, e.name, c.page, c.id,
                        snippet(chunks_fts, 0, '', '', '…', 16), bm25(chunks_fts)
                 FROM chunks_fts
                 JOIN chunks c ON c.id = chunks_fts.rowid
                 JOIN evidence e ON e.id = c.evidence_id
                 WHERE chunks_fts MATCH ?1 AND e.case_id = ?2
                 ORDER BY bm25(chunks_fts)
                 LIMIT ?3",
            )?
            .query_map(params![query, case_id, limit as i64], |row| {
                Ok(SearchHit {
                    evidence_id: row.get(0)?,
                    evidence_name: row.get(1)?,
                    page: row.get(2)?,
                    chunk_id: row.get(3)?,
                    snippet: row.get(4)?,
                    rank: row.get(5)?,
                })
            })?
            .collect()
    }

    /// Deletes the evidence with its pages and chunks.
    pub fn delete(&self, id: i64) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_analysis(&tx, id)?;
        tx.execute("DELETE FROM evidence WHERE id = ?1", [id])?;
        tx.commit()
    }
}

fn delete_analysis(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM chunks WHERE evidence_id = ?1", [id])?;
    conn.execute("DELETE FROM evidence_pages WHERE evidence_id = ?1", [id])?;
    Ok(())
}

/// Converts user input to an FTS5 query: each word becomes a quoted prefix term, so that
/// quotes and operators in the input have no special meaning. Returns `None` if no word
/// has a letter or a digit.
fn fts_query(query: &str) -> Option<String> {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(|word| word.replace('"', ""))
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .map(|word| format!("\"{word}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(" "))
}

/// The last part of `path`, used as the display name of the file.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn row_to_evidence(row: &rusqlite::Row) -> rusqlite::Result<Evidence> {
    Ok(Evidence {
        id: row.get(0)?,
        name: row.get(1)?,
        mime: row.get(2)?,
        size: row.get::<_, i64>(3)? as u64,
        sha256: row.get(4)?,
        added_at: row.get(5)?,
        status: AnalysisStatus::parse(&row.get::<_, String>(6)?),
        note: row.get(7)?,
        pages: row.get(8)?,
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rusqlite::Connection;

    use super::{AnalysisStatus, BLOCK_SIZE, ImportError, MAX_FILE_SIZE, Store};
    use crate::extract::{Page, PageSource};

    /// A new file in its own folder, so that tests that run at the same time do not change the
    /// file of an other test during an import.
    fn temp_file(name: &str, content: &[u8]) -> PathBuf {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("nicolaw-test-{}-{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    /// A store with one case. Returns the store and the case id.
    fn store_with_case() -> (Store, i64) {
        let store = Store::open_in_memory().unwrap();
        let case = store.create_case("Caso de prueba").unwrap();
        (store, case.id)
    }

    fn import(store: &Store, case: i64, path: &Path) -> i64 {
        store.import(case, path, |_, _| {}).unwrap().id
    }

    fn text_page(number: u32, text: &str) -> Page {
        Page {
            number,
            text: text.into(),
            source: PageSource::Text,
        }
    }

    const DECLARATION: &str =
        "La declaración del acusado fue rendida el 12 de marzo de 2024 en Bogotá.";

    #[test]
    fn cases_are_created_and_listed_newest_first() {
        let store = Store::open_in_memory().unwrap();

        let first = store.create_case("Caso Pérez").unwrap();
        let second = store.create_case("Caso Gómez").unwrap();

        assert_eq!(first.name, "Caso Pérez");
        let names: Vec<_> = store
            .cases()
            .unwrap()
            .into_iter()
            .map(|case| (case.id, case.name))
            .collect();
        assert_eq!(
            names,
            [
                (second.id, "Caso Gómez".into()),
                (first.id, "Caso Pérez".into())
            ]
        );
    }

    #[test]
    fn settings_keep_the_last_value() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.setting("last_case").unwrap(), None);

        store.set_setting("last_case", "1").unwrap();
        store.set_setting("last_case", "2").unwrap();

        assert_eq!(store.setting("last_case").unwrap().as_deref(), Some("2"));
    }

    #[test]
    fn import_stores_name_type_size_hash_content_and_pending_status() {
        let (store, case) = store_with_case();
        let path = temp_file("notes.txt", b"hello");

        let entry = store.import(case, &path, |_, _| {}).unwrap();

        assert_eq!(entry.name, "notes.txt");
        assert_eq!(entry.mime, "text/plain");
        assert_eq!(entry.size, 5);
        // SHA-256 of "hello".
        assert_eq!(
            entry.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(entry.status, AnalysisStatus::Pending);
        assert_eq!(entry.note, None);
        assert_eq!(entry.pages, 0);
        assert_eq!(store.content(entry.id, |_, _| {}).unwrap(), b"hello");
    }

    #[test]
    fn unknown_type_is_octet_stream() {
        let (store, case) = store_with_case();
        let path = temp_file("data.unknownext", &[0, 1, 2]);

        let entry = store.import(case, &path, |_, _| {}).unwrap();

        assert_eq!(entry.mime, "application/octet-stream");
    }

    #[test]
    fn list_returns_the_evidence_of_one_case_newest_first() {
        let (store, case) = store_with_case();
        let other = store.create_case("Otro caso").unwrap().id;
        import(&store, case, &temp_file("a.txt", b"a"));
        import(&store, case, &temp_file("b.txt", b"b"));
        import(&store, other, &temp_file("c.txt", b"c"));

        let files = store.list(case).unwrap();

        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["b.txt", "a.txt"]);
        // Format: YYYY-MM-DD HH:MM
        assert_eq!(files[0].added_at.len(), 16);
        assert_eq!(&files[0].added_at[4..5], "-");
        assert_eq!(&files[0].added_at[13..14], ":");
        assert_eq!(store.list(other).unwrap().len(), 1);
    }

    #[test]
    fn status_changes_are_stored_with_the_note() {
        let (store, case) = store_with_case();
        let id = import(&store, case, &temp_file("x.bin", b"x"));

        store
            .set_status(id, AnalysisStatus::Analyzing, None)
            .unwrap();
        assert_eq!(
            store.list(case).unwrap()[0].status,
            AnalysisStatus::Analyzing
        );

        store
            .set_status(id, AnalysisStatus::NotAnalyzed, Some("Sin texto"))
            .unwrap();
        let entry = &store.list(case).unwrap()[0];
        assert_eq!(entry.status, AnalysisStatus::NotAnalyzed);
        assert_eq!(entry.note.as_deref(), Some("Sin texto"));
    }

    #[test]
    fn unfinished_analysis_lists_pending_and_analyzing_evidence() {
        let (store, case) = store_with_case();
        let pending = import(&store, case, &temp_file("p.txt", b"p"));
        let analyzing = import(&store, case, &temp_file("a.txt", b"a"));
        let ready = import(&store, case, &temp_file("r.txt", b"r"));
        store
            .set_status(analyzing, AnalysisStatus::Analyzing, None)
            .unwrap();
        store.save_analysis(ready, &[text_page(1, "r")]).unwrap();

        assert_eq!(store.unfinished_analysis().unwrap(), [pending, analyzing]);
    }

    #[test]
    fn saved_analysis_gives_pages_and_ready_status() {
        let (store, case) = store_with_case();
        let id = import(&store, case, &temp_file("doc.pdf", b"%PDF"));
        let pages = [
            text_page(1, DECLARATION),
            Page {
                number: 2,
                text: "Texto escaneado.".into(),
                source: PageSource::Ocr,
            },
        ];

        store.save_analysis(id, &pages).unwrap();

        assert_eq!(store.pages(id).unwrap(), pages);
        let entry = &store.list(case).unwrap()[0];
        assert_eq!(entry.status, AnalysisStatus::Ready);
        assert_eq!(entry.pages, 2);
    }

    #[test]
    fn search_ignores_accents_and_gives_the_page_and_a_snippet() {
        let (store, case) = store_with_case();
        let id = import(&store, case, &temp_file("acta.txt", b"x"));
        store
            .save_analysis(
                id,
                &[text_page(1, "Primera página."), text_page(2, DECLARATION)],
            )
            .unwrap();

        let hits = store.search(case, "declaracion", 10).unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].evidence_id, id);
        assert_eq!(hits[0].evidence_name, "acta.txt");
        assert_eq!(hits[0].page, 2);
        assert!(
            hits[0].snippet.contains("declaración"),
            "{}",
            hits[0].snippet
        );
        // Prefix match while the user types.
        assert_eq!(store.search(case, "BOGO", 10).unwrap().len(), 1);
        assert!(store.search(case, "tribunal", 10).unwrap().is_empty());
    }

    #[test]
    fn search_finds_evidence_of_the_case_only() {
        let (store, case) = store_with_case();
        let other = store.create_case("Otro caso").unwrap().id;
        let mine = import(&store, case, &temp_file("mine.txt", b"x"));
        let theirs = import(&store, other, &temp_file("theirs.txt", b"x"));
        store
            .save_analysis(mine, &[text_page(1, DECLARATION)])
            .unwrap();
        store
            .save_analysis(theirs, &[text_page(1, DECLARATION)])
            .unwrap();

        let hits = store.search(case, "acusado", 10).unwrap();

        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].evidence_id, mine);
    }

    #[test]
    fn search_accepts_any_user_input() {
        let (store, case) = store_with_case();
        let id = import(&store, case, &temp_file("acta.txt", b"x"));
        store
            .save_analysis(id, &[text_page(1, DECLARATION)])
            .unwrap();

        for query in [
            "\"acusado",
            "acusado\"",
            "acusado OR",
            "NOT acusado",
            "acusado AND (",
            "acu*sado",
            "text:acusado",
            "^acusado",
            "-acusado",
            "a\"b\"c",
            "",
            "   ",
            "*",
        ] {
            assert!(
                store.search(case, query, 10).is_ok(),
                "query failed: {query}"
            );
        }
        assert_eq!(store.search(case, "\"acusado\"", 10).unwrap().len(), 1);
        assert!(store.search(case, "", 10).unwrap().is_empty());
    }

    #[test]
    fn delete_removes_the_evidence_with_its_pages_and_chunks() {
        let (store, case) = store_with_case();
        let id = import(&store, case, &temp_file("gone.txt", b"x"));
        store
            .save_analysis(id, &[text_page(1, DECLARATION)])
            .unwrap();

        store.delete(id).unwrap();

        assert!(store.list(case).unwrap().is_empty());
        assert!(store.content(id, |_, _| {}).is_err());
        assert!(store.pages(id).unwrap().is_empty());
        assert!(store.search(case, "acusado", 10).unwrap().is_empty());
    }

    #[test]
    fn file_larger_than_limit_is_rejected() {
        let (store, case) = store_with_case();
        let path = temp_file("big.bin", b"");
        // A sparse file: the size is set without writing the bytes.
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_FILE_SIZE + 1)
            .unwrap();

        assert!(matches!(
            store.import(case, &path, |_, _| {}),
            Err(ImportError::TooLarge)
        ));
        assert!(store.list(case).unwrap().is_empty());
    }

    #[test]
    fn a_file_that_grows_during_import_is_rejected() {
        let (store, case) = store_with_case();
        let path = temp_file("grows.txt", b"abc");

        let result = store.import(case, &path, |done, total| {
            if done == total {
                let mut file = fs::File::options().append(true).open(&path).unwrap();
                std::io::Write::write_all(&mut file, b"d").unwrap();
            }
        });

        assert!(matches!(result, Err(ImportError::Changed)));
        assert!(store.list(case).unwrap().is_empty());
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let (store, case) = store_with_case();
        let path = std::env::temp_dir().join("nicolaw-missing-file.txt");

        assert!(matches!(
            store.import(case, &path, |_, _| {}),
            Err(ImportError::Read)
        ));
    }

    #[test]
    fn data_stays_after_reopen() {
        let dir = std::env::temp_dir().join(format!("nicolaw-db-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let _ = fs::remove_file(&db);

        let store = Store::open(&db).unwrap();
        let case = store.create_case("Caso").unwrap().id;
        import(&store, case, &temp_file("keep.txt", b"keep"));
        drop(store);
        let files = Store::open(&db).unwrap().list(case).unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "keep.txt");
    }

    #[test]
    fn a_version_1_database_is_migrated_to_version_2() {
        let dir = std::env::temp_dir().join(format!("nicolaw-v1-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v1.db");
        let _ = fs::remove_file(&db);
        Connection::open(&db)
            .unwrap()
            .execute_batch(
                "CREATE TABLE files (id INTEGER PRIMARY KEY, name TEXT NOT NULL,
                 mime TEXT NOT NULL, size INTEGER NOT NULL, added_at INTEGER NOT NULL,
                 content BLOB NOT NULL);
                 INSERT INTO files VALUES (1, 'old.txt', 'text/plain', 1, 0, x'41');",
            )
            .unwrap();

        let store = Store::open(&db).unwrap();
        let case = store.create_case("Caso").unwrap().id;
        import(&store, case, &temp_file("new.txt", b"new"));
        drop(store);

        let conn = Connection::open(&db).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        let old_table: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name = 'files'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(old_table, 0);
    }

    /// Records each progress call.
    fn record(calls: &mut Vec<(u64, u64)>) -> impl FnMut(u64, u64) + '_ {
        |done, total| calls.push((done, total))
    }

    #[test]
    fn import_reports_progress_for_each_block() {
        let (store, case) = store_with_case();
        let data: Vec<u8> = (0..BLOCK_SIZE * 2 + 1000)
            .map(|i| (i % 251) as u8)
            .collect();
        let path = temp_file("large.bin", &data);
        let mut calls = Vec::new();

        let entry = store.import(case, &path, record(&mut calls)).unwrap();

        let total = data.len() as u64;
        let block = BLOCK_SIZE as u64;
        assert_eq!(calls, [(block, total), (2 * block, total), (total, total)]);
        assert_eq!(store.content(entry.id, |_, _| {}).unwrap(), data);
    }

    #[test]
    fn import_of_an_empty_file_reports_zero_of_zero() {
        let (store, case) = store_with_case();
        let mut calls = Vec::new();

        let entry = store
            .import(case, &temp_file("empty.txt", b""), record(&mut calls))
            .unwrap();

        assert_eq!(calls, [(0, 0)]);
        assert_eq!(entry.size, 0);
        // SHA-256 of no bytes.
        assert_eq!(
            entry.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert!(store.content(entry.id, |_, _| {}).unwrap().is_empty());
    }

    #[test]
    fn failed_read_after_the_metadata_leaves_no_row() {
        let (store, case) = store_with_case();
        // A directory has metadata with a size, but a read from it fails.
        let dir = std::env::temp_dir().join(format!("nicolaw-dir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inside.txt"), b"x").unwrap();

        assert!(matches!(
            store.import(case, &dir, |_, _| {}),
            Err(ImportError::Read)
        ));
        assert!(store.list(case).unwrap().is_empty());
    }

    #[test]
    fn content_reports_progress_and_returns_all_bytes() {
        let (store, case) = store_with_case();
        let data: Vec<u8> = (0..BLOCK_SIZE + 10).map(|i| (i % 7) as u8).collect();
        let id = import(&store, case, &temp_file("read.bin", &data));
        let mut calls = Vec::new();

        let content = store.content(id, record(&mut calls)).unwrap();

        let total = data.len() as u64;
        assert_eq!(calls, [(BLOCK_SIZE as u64, total), (total, total)]);
        assert_eq!(content, data);
    }
}
