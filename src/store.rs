use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use rusqlite::{Connection, MAIN_DB, OptionalExtension as _, params};
use sha2::{Digest, Sha256};

use crate::extract::chunk::chunk;
use crate::extract::{Page, PageSource};

/// The largest file that the store accepts: 100 MB.
pub const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// The analysis search takes at most this number of passages from one evidence file before it
/// fills the rest of its limit by rank.
pub const PASSAGES_PER_EVIDENCE: usize = 2;

/// The store moves file content in blocks of this size, and reports progress after each block.
pub const BLOCK_SIZE: usize = 256 * 1024;

/// Version 1 had one `files` table. Version 2 has cases, evidence, pages, chunks, and search.
/// Version 3 adds hearings and their transcript segments. Version 4 adds the suggestions of
/// the analysis. No release has version 3 yet, so its segment columns can still grow:
/// [`Store::migrate`] adds the new columns to a database that a development build made.
const SCHEMA_VERSION: i64 = 4;

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

const SCHEMA_V3: &str = "
    CREATE TABLE hearings (
        id         INTEGER PRIMARY KEY,
        case_id    INTEGER NOT NULL REFERENCES cases(id),
        title      TEXT    NOT NULL,
        started_at INTEGER, -- Unix seconds, set when the recording starts
        ended_at   INTEGER, -- Unix seconds
        audio_path TEXT
    );
    CREATE INDEX hearings_case ON hearings(case_id);
    CREATE TABLE segments (
        id         INTEGER PRIMARY KEY,
        hearing_id INTEGER NOT NULL REFERENCES hearings(id),
        t_start_ms INTEGER NOT NULL, -- Position in the hearing audio
        t_end_ms   INTEGER NOT NULL,
        text       TEXT    NOT NULL,
        speaker    TEXT    NOT NULL CHECK (speaker IN ('accused', 'other')),
        final      INTEGER NOT NULL DEFAULT 0,
        live_text  TEXT,    -- The live text, after a refined text replaced it
        revision   INTEGER NOT NULL DEFAULT 0, -- Grows by 1 at each change of the text
        failed     INTEGER NOT NULL DEFAULT 0  -- 1: the speech in this time range has no text
    );
    CREATE INDEX segments_hearing ON segments(hearing_id);
";

const SCHEMA_V4: &str = "
    CREATE TABLE suggestions (
        id                INTEGER PRIMARY KEY,
        hearing_id        INTEGER NOT NULL REFERENCES hearings(id),
        segment_id        INTEGER NOT NULL REFERENCES segments(id),
        chunk_id          INTEGER NOT NULL REFERENCES chunks(id),
        claim             TEXT    NOT NULL,
        evidence_quote    TEXT    NOT NULL,
        claim_quote       TEXT,
        kind              TEXT
            CHECK (kind IN ('date', 'amount', 'identity', 'location', 'sequence', 'other')),
        level             TEXT    NOT NULL CHECK (level IN ('conflict', 'check')),
        nli_contradiction REAL    NOT NULL,
        explanation       TEXT,
        status            TEXT    NOT NULL DEFAULT 'open'
            CHECK (status IN ('open', 'confirmed', 'dismissed')),
        created_at        INTEGER NOT NULL -- Unix seconds
    );
    CREATE INDEX suggestions_hearing ON suggestions(hearing_id);
    CREATE INDEX suggestions_chunk ON suggestions(chunk_id);
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

/// One hearing of a case. The times are local, as `YYYY-MM-DD HH:MM`.
#[derive(Clone, Debug, PartialEq)]
pub struct Hearing {
    pub id: i64,
    pub title: String,
    /// `None` until the recording starts.
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    /// The WAV file of the hearing audio.
    pub audio_path: Option<PathBuf>,
}

/// Who speaks in a segment. The user sets it; there is no automatic speaker detection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Speaker {
    Accused,
    Other,
}

impl Speaker {
    fn as_str(self) -> &'static str {
        match self {
            Self::Accused => "accused",
            Self::Other => "other",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "accused" => Self::Accused,
            _ => Self::Other,
        }
    }
}

/// One transcribed part of a hearing: speech between two pauses.
#[derive(Clone, Debug, PartialEq)]
pub struct Segment {
    pub id: i64,
    pub t_start_ms: i64,
    pub t_end_ms: i64,
    pub text: String,
    pub speaker: Speaker,
    /// False while the speech continues and the text can change.
    pub is_final: bool,
    /// Grows by 1 at each change of the text, for example after the refine pass.
    pub revision: i64,
    /// True when the transcription of this speech failed: the segment has no text, but it keeps
    /// its time range, so that the transcript shows the gap.
    pub failed: bool,
}

/// How sure the analysis is that a statement conflicts with the evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionLevel {
    /// The NLI model and the language model agree that the passage contradicts the statement.
    Conflict,
    /// The passage can contradict the statement. A person must read it.
    Check,
}

impl SuggestionLevel {
    fn as_str(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::Check => "check",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "conflict" => Self::Conflict,
            _ => Self::Check,
        }
    }
}

/// What the statement and the evidence do not agree on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionKind {
    Date,
    Amount,
    Identity,
    Location,
    Sequence,
    Other,
}

impl SuggestionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Date => "date",
            Self::Amount => "amount",
            Self::Identity => "identity",
            Self::Location => "location",
            Self::Sequence => "sequence",
            Self::Other => "other",
        }
    }

    /// Returns `None` for a value that is not a kind.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "date" => Self::Date,
            "amount" => Self::Amount,
            "identity" => Self::Identity,
            "location" => Self::Location,
            "sequence" => Self::Sequence,
            "other" => Self::Other,
            _ => return None,
        })
    }
}

/// The decision of the lawyer about a suggestion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SuggestionStatus {
    Open,
    Confirmed,
    Dismissed,
}

impl SuggestionStatus {
    fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Confirmed => "confirmed",
            Self::Dismissed => "dismissed",
        }
    }

    fn parse(value: &str) -> Self {
        match value {
            "confirmed" => Self::Confirmed,
            "dismissed" => Self::Dismissed,
            _ => Self::Open,
        }
    }
}

/// A suggestion before it is stored.
#[derive(Clone, Debug, PartialEq)]
pub struct NewSuggestion {
    pub hearing_id: i64,
    pub segment_id: i64,
    pub chunk_id: i64,
    /// One statement of the accused, as a standalone sentence.
    pub claim: String,
    /// The part of the claim that conflicts, if the language model found it.
    pub claim_quote: Option<String>,
    /// The exact passage of the evidence.
    pub evidence_quote: String,
    pub kind: Option<SuggestionKind>,
    pub level: SuggestionLevel,
    /// The probability of contradiction from the NLI model, from 0 to 1.
    pub nli_contradiction: f64,
    pub explanation: Option<String>,
}

/// A stored suggestion, with the source of its evidence passage.
#[derive(Clone, Debug, PartialEq)]
pub struct Suggestion {
    pub id: i64,
    pub hearing_id: i64,
    pub segment_id: i64,
    pub chunk_id: i64,
    pub evidence_id: i64,
    pub evidence_name: String,
    pub page: u32,
    pub claim: String,
    pub claim_quote: Option<String>,
    pub evidence_quote: String,
    pub kind: Option<SuggestionKind>,
    pub level: SuggestionLevel,
    pub nli_contradiction: f64,
    pub explanation: Option<String>,
    pub status: SuggestionStatus,
}

/// The open suggestions of a hearing, by level.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SuggestionCounts {
    pub conflict: u32,
    pub check: u32,
}

/// One chunk of evidence text, with its file and page.
#[derive(Clone, Debug, PartialEq)]
pub struct ChunkSource {
    pub id: i64,
    pub evidence_id: i64,
    pub evidence_name: String,
    pub page: u32,
    pub text: String,
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

impl Evidence {
    /// The location of a page of this file, for the user: "Página n" or "Sección n".
    pub fn locator_label(&self, page: u32) -> String {
        locator_label(&self.mime, page)
    }
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
    // The list and the analysis use the hits in rank order. The value is for diagnostics.
    #[allow(dead_code)]
    pub rank: f64,
}

/// The search hits of one evidence file, for the search list.
#[derive(Clone, Debug)]
pub struct SearchGroup {
    pub evidence_id: i64,
    pub evidence_name: String,
    pub mime: String,
    /// The best hits of the file, best first. The first hit is the best hit.
    pub hits: Vec<SearchHit>,
    /// The number of chunks of the file that match. It can be more than `hits.len()`.
    pub total: usize,
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

const SELECT_SUGGESTION: &str = "SELECT s.id, s.hearing_id, s.segment_id, s.chunk_id, \
     c.evidence_id, e.name, c.page, s.claim, s.claim_quote, s.evidence_quote, s.kind, s.level, \
     s.nli_contradiction, s.explanation, s.status \
     FROM suggestions s JOIN chunks c ON c.id = s.chunk_id JOIN evidence e ON e.id = c.evidence_id";

const SELECT_SEGMENT: &str =
    "SELECT id, t_start_ms, t_end_ms, text, speaker, final, revision, failed FROM segments";

const SELECT_HEARING: &str = "SELECT id, title, \
     strftime('%Y-%m-%d %H:%M', started_at, 'unixepoch', 'localtime'), \
     strftime('%Y-%m-%d %H:%M', ended_at, 'unixepoch', 'localtime'), \
     audio_path FROM hearings";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::migrate(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::migrate(Connection::open_in_memory()?)
    }

    /// Runs SQL on the database, so that a test can make a later write fail.
    #[cfg(test)]
    pub fn execute_test_sql(&self, sql: &str) -> rusqlite::Result<()> {
        self.conn.execute_batch(sql)
    }

    fn migrate(mut conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < SCHEMA_VERSION {
            let tx = conn.transaction()?;
            if version < 2 {
                // No data of version 1 is in use, so the migration drops the `files` table.
                tx.execute_batch(SCHEMA_V2)?;
            }
            if version < 3 {
                tx.execute_batch(SCHEMA_V3)?;
            }
            tx.execute_batch(SCHEMA_V4)?;
            tx.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))?;
            tx.commit()?;
        }
        add_segment_columns(&conn)?;
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

    /// Full-text search in the evidence of one case, grouped by evidence file. The file with the
    /// best hit is first. Each group has at most `hits_per_group` hits, best first, and the
    /// number of all its hits, so that one file with many hits cannot hide the other files.
    /// Accents and case do not matter, and each word also matches as a prefix. Any user input
    /// is safe.
    pub fn search_groups(
        &self,
        case_id: i64,
        query: &str,
        hits_per_group: usize,
    ) -> rusqlite::Result<Vec<SearchGroup>> {
        let words: Vec<&str> = query.split_whitespace().collect();
        let Some(query) = fts_query(&words, " ") else {
            return Ok(Vec::new());
        };
        let mut groups: Vec<SearchGroup> = Vec::new();
        let mut statement = self.conn.prepare(
            "WITH hits AS (
                 SELECT c.evidence_id AS evidence_id, c.page AS page, c.id AS chunk_id,
                        snippet(chunks_fts, 0, '', '', '…', 16) AS snippet,
                        bm25(chunks_fts) AS rank
                 FROM chunks_fts
                 JOIN chunks c ON c.id = chunks_fts.rowid
                 JOIN evidence e ON e.id = c.evidence_id
                 WHERE chunks_fts MATCH ?1 AND e.case_id = ?2
             ), numbered AS (
                 SELECT *,
                        row_number() OVER (PARTITION BY evidence_id ORDER BY rank, chunk_id) AS n,
                        count(*) OVER (PARTITION BY evidence_id) AS total,
                        min(rank) OVER (PARTITION BY evidence_id) AS best
                 FROM hits
             )
             SELECT h.evidence_id, e.name, e.mime, h.page, h.chunk_id, h.snippet, h.rank, h.total
             FROM numbered h JOIN evidence e ON e.id = h.evidence_id
             WHERE h.n <= ?3
             ORDER BY h.best, h.evidence_id, h.n",
        )?;
        let mut rows = statement.query(params![query, case_id, hits_per_group as i64])?;
        while let Some(row) = rows.next()? {
            let hit = SearchHit {
                evidence_id: row.get(0)?,
                evidence_name: row.get(1)?,
                page: row.get(3)?,
                chunk_id: row.get(4)?,
                snippet: row.get(5)?,
                rank: row.get(6)?,
            };
            match groups.last_mut() {
                Some(group) if group.evidence_id == hit.evidence_id => group.hits.push(hit),
                _ => groups.push(SearchGroup {
                    evidence_id: hit.evidence_id,
                    evidence_name: hit.evidence_name.clone(),
                    mime: row.get(2)?,
                    total: row.get::<_, i64>(7)? as usize,
                    hits: vec![hit],
                }),
            }
        }
        Ok(groups)
    }

    /// Full-text search in the evidence of one case, one chunk for each hit, best match first.
    /// Accents and case do not matter, and each word also matches as a prefix. Any user input
    /// is safe. The tests use it to find a chunk; the search list uses [`Self::search_groups`].
    #[cfg(test)]
    pub fn search(
        &self,
        case_id: i64,
        query: &str,
        limit: usize,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        let words: Vec<&str> = query.split_whitespace().collect();
        self.run_search(case_id, fts_query(&words, " "), limit, limit)
    }

    /// Full-text search for chunks that have at least one of the words, best match first. The
    /// BM25 rank puts chunks with more of the words, and with rare words, first. Each word is a
    /// prefix term, and a word with spaces or punctuation (for example a date) is a phrase.
    /// The first hits take at most [`PASSAGES_PER_EVIDENCE`] chunks from one file; the rest of
    /// the limit fills by rank. Thus one file with many hits cannot fill all the candidates.
    pub fn search_any(
        &self,
        case_id: i64,
        words: &[String],
        limit: usize,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        let words: Vec<&str> = words.iter().map(String::as_str).collect();
        self.run_search(
            case_id,
            fts_query(&words, " OR "),
            limit,
            PASSAGES_PER_EVIDENCE,
        )
    }

    /// The best `limit` hits. The hits with a rank of at most `per_evidence` in their file come
    /// first, by rank; the other hits fill the rest of the limit, by rank.
    fn run_search(
        &self,
        case_id: i64,
        query: Option<String>,
        limit: usize,
        per_evidence: usize,
    ) -> rusqlite::Result<Vec<SearchHit>> {
        let Some(query) = query else {
            return Ok(Vec::new());
        };
        self.conn
            .prepare(
                "WITH hits AS (
                     SELECT e.id AS evidence_id, e.name AS name, c.page AS page,
                            c.id AS chunk_id, snippet(chunks_fts, 0, '', '', '…', 16) AS snippet,
                            bm25(chunks_fts) AS rank
                     FROM chunks_fts
                     JOIN chunks c ON c.id = chunks_fts.rowid
                     JOIN evidence e ON e.id = c.evidence_id
                     WHERE chunks_fts MATCH ?1 AND e.case_id = ?2
                 ), numbered AS (
                     SELECT *,
                            row_number() OVER (PARTITION BY evidence_id ORDER BY rank, chunk_id)
                                AS n
                     FROM hits
                 )
                 SELECT evidence_id, name, page, chunk_id, snippet, rank FROM numbered
                 ORDER BY n > ?4, rank, chunk_id
                 LIMIT ?3",
            )?
            .query_map(
                params![query, case_id, limit as i64, per_evidence as i64],
                |row| {
                    Ok(SearchHit {
                        evidence_id: row.get(0)?,
                        evidence_name: row.get(1)?,
                        page: row.get(2)?,
                        chunk_id: row.get(3)?,
                        snippet: row.get(4)?,
                        rank: row.get(5)?,
                    })
                },
            )?
            .collect()
    }

    /// Deletes the evidence with its pages and chunks.
    pub fn delete(&self, id: i64) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        delete_analysis(&tx, id)?;
        tx.execute("DELETE FROM evidence WHERE id = ?1", [id])?;
        tx.commit()
    }

    /// Today in local time, as `DD/MM/YYYY`.
    pub fn today(&self) -> rusqlite::Result<String> {
        self.conn.query_row(
            "SELECT strftime('%d/%m/%Y', 'now', 'localtime')",
            [],
            |row| row.get(0),
        )
    }

    pub fn create_hearing(&self, case_id: i64, title: &str) -> rusqlite::Result<Hearing> {
        self.conn.execute(
            "INSERT INTO hearings (case_id, title) VALUES (?1, ?2)",
            params![case_id, title],
        )?;
        self.hearing(self.conn.last_insert_rowid())
    }

    pub fn hearing(&self, id: i64) -> rusqlite::Result<Hearing> {
        self.conn.query_row(
            &format!("{SELECT_HEARING} WHERE id = ?1"),
            [id],
            row_to_hearing,
        )
    }

    /// Returns the hearings of a case, newest first.
    pub fn hearings(&self, case_id: i64) -> rusqlite::Result<Vec<Hearing>> {
        self.conn
            .prepare(&format!(
                "{SELECT_HEARING} WHERE case_id = ?1 ORDER BY id DESC"
            ))?
            .query_map([case_id], row_to_hearing)?
            .collect()
    }

    /// Records the start of the recording and the path of the audio file.
    pub fn start_hearing(&self, id: i64, audio_path: &Path) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE hearings SET started_at = unixepoch(), audio_path = ?1 WHERE id = ?2",
            params![audio_path.to_string_lossy(), id],
        )?;
        Ok(())
    }

    pub fn end_hearing(&self, id: i64) -> rusqlite::Result<()> {
        let changed = self.conn.execute(
            "UPDATE hearings SET ended_at = unixepoch() WHERE id = ?1",
            [id],
        )?;
        if changed == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    /// Returns the id and the name of the case of a hearing.
    pub fn hearing_case(&self, hearing_id: i64) -> rusqlite::Result<(i64, String)> {
        self.conn.query_row(
            "SELECT c.id, c.name FROM hearings h JOIN cases c ON c.id = h.case_id WHERE h.id = ?1",
            [hearing_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
    }

    /// Deletes the hearing with its segments and suggestions, then its audio file.
    pub fn delete_hearing(&self, id: i64) -> rusqlite::Result<()> {
        let audio_path = self.hearing(id)?.audio_path;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM suggestions WHERE hearing_id = ?1", [id])?;
        tx.execute("DELETE FROM segments WHERE hearing_id = ?1", [id])?;
        tx.execute("DELETE FROM hearings WHERE id = ?1", [id])?;
        tx.commit()?;
        // The rows are gone, so a file that stays is only wasted space: ignore the error.
        if let Some(path) = audio_path {
            fs::remove_file(path).ok();
        }
        Ok(())
    }

    /// Adds an open segment with no text. Returns its id.
    pub fn insert_segment(
        &self,
        hearing_id: i64,
        t_start_ms: i64,
        speaker: Speaker,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO segments (hearing_id, t_start_ms, t_end_ms, text, speaker, final)
             VALUES (?1, ?2, ?2, '', ?3, 0)",
            params![hearing_id, t_start_ms, speaker.as_str()],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Sets the final text and the end of a segment.
    pub fn finish_segment(&self, id: i64, text: &str, t_end_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE segments SET text = ?1, t_end_ms = ?2, final = 1 WHERE id = ?3",
            params![text, t_end_ms, id],
        )?;
        Ok(())
    }

    /// Replaces the text of a segment, for example with a more accurate transcription. The
    /// first text stays as the live text, and the revision grows by 1. The same text again
    /// changes nothing.
    pub fn set_segment_text(&self, id: i64, text: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE segments SET live_text = coalesce(live_text, text), text = ?1,
             revision = revision + 1 WHERE id = ?2 AND text <> ?1",
            params![text, id],
        )?;
        Ok(())
    }

    /// The text of the live transcription, if a later text replaced it.
    #[cfg(test)]
    pub fn live_text(&self, id: i64) -> rusqlite::Result<Option<String>> {
        self.conn.query_row(
            "SELECT live_text FROM segments WHERE id = ?1",
            [id],
            |row| row.get(0),
        )
    }

    /// Ends a segment whose transcription failed. It keeps its time range and has no text.
    pub fn fail_segment(&self, id: i64, t_end_ms: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE segments SET text = '', t_end_ms = ?1, final = 1, failed = 1 WHERE id = ?2",
            params![t_end_ms, id],
        )?;
        Ok(())
    }

    pub fn set_speaker(&self, id: i64, speaker: Speaker) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE segments SET speaker = ?1 WHERE id = ?2",
            params![speaker.as_str(), id],
        )?;
        Ok(())
    }

    pub fn delete_segment(&self, id: i64) -> rusqlite::Result<()> {
        self.conn
            .execute("DELETE FROM segments WHERE id = ?1", [id])?;
        Ok(())
    }

    /// Returns the segments of a hearing in time order.
    pub fn segments(&self, hearing_id: i64) -> rusqlite::Result<Vec<Segment>> {
        self.conn
            .prepare(&format!(
                "{SELECT_SEGMENT} WHERE hearing_id = ?1 ORDER BY t_start_ms, id"
            ))?
            .query_map([hearing_id], row_to_segment)?
            .collect()
    }

    pub fn segment(&self, id: i64) -> rusqlite::Result<Segment> {
        self.conn.query_row(
            &format!("{SELECT_SEGMENT} WHERE id = ?1"),
            [id],
            row_to_segment,
        )
    }

    /// The text of the last final segment of an other speaker before `t_start_ms`: usually the
    /// question that the accused answers.
    pub fn question_before(
        &self,
        hearing_id: i64,
        t_start_ms: i64,
    ) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT text FROM segments
                 WHERE hearing_id = ?1 AND t_start_ms < ?2 AND speaker = 'other' AND final = 1
                   AND failed = 0
                 ORDER BY t_start_ms DESC, id DESC LIMIT 1",
                params![hearing_id, t_start_ms],
                |row| row.get(0),
            )
            .optional()
    }

    /// One chunk of evidence text, with its file and page.
    pub fn chunk(&self, id: i64) -> rusqlite::Result<ChunkSource> {
        self.conn.query_row(
            "SELECT c.id, c.evidence_id, e.name, c.page, c.text
             FROM chunks c JOIN evidence e ON e.id = c.evidence_id WHERE c.id = ?1",
            [id],
            |row| {
                Ok(ChunkSource {
                    id: row.get(0)?,
                    evidence_id: row.get(1)?,
                    evidence_name: row.get(2)?,
                    page: row.get(3)?,
                    text: row.get(4)?,
                })
            },
        )
    }

    /// Stores a new open suggestion. Returns it with the source of its evidence passage.
    pub fn insert_suggestion(&self, new: &NewSuggestion) -> rusqlite::Result<Suggestion> {
        self.conn.execute(
            "INSERT INTO suggestions (hearing_id, segment_id, chunk_id, claim, evidence_quote,
                 claim_quote, kind, level, nli_contradiction, explanation, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, unixepoch())",
            params![
                new.hearing_id,
                new.segment_id,
                new.chunk_id,
                new.claim,
                new.evidence_quote,
                new.claim_quote,
                new.kind.map(SuggestionKind::as_str),
                new.level.as_str(),
                new.nli_contradiction,
                new.explanation,
            ],
        )?;
        self.conn.query_row(
            &format!("{SELECT_SUGGESTION} WHERE s.id = ?1"),
            [self.conn.last_insert_rowid()],
            row_to_suggestion,
        )
    }

    /// The suggestions of a hearing: open first, then newest first.
    pub fn suggestions(&self, hearing_id: i64) -> rusqlite::Result<Vec<Suggestion>> {
        self.conn
            .prepare(&format!(
                "{SELECT_SUGGESTION} WHERE s.hearing_id = ?1
                 ORDER BY s.status = 'open' DESC, s.created_at DESC, s.id DESC"
            ))?
            .query_map([hearing_id], row_to_suggestion)?
            .collect()
    }

    /// Deletes the suggestions of a segment. Returns the number of deleted suggestions.
    pub fn delete_segment_suggestions(&self, segment_id: i64) -> rusqlite::Result<usize> {
        self.conn.execute(
            "DELETE FROM suggestions WHERE segment_id = ?1",
            [segment_id],
        )
    }

    pub fn set_suggestion_status(&self, id: i64, status: SuggestionStatus) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE suggestions SET status = ?1 WHERE id = ?2",
            params![status.as_str(), id],
        )?;
        Ok(())
    }

    pub fn suggestion_counts(&self, hearing_id: i64) -> rusqlite::Result<SuggestionCounts> {
        self.conn.query_row(
            "SELECT count(*) FILTER (WHERE level = 'conflict'),
                    count(*) FILTER (WHERE level = 'check')
             FROM suggestions WHERE hearing_id = ?1 AND status = 'open'",
            [hearing_id],
            |row| {
                Ok(SuggestionCounts {
                    conflict: row.get(0)?,
                    check: row.get(1)?,
                })
            },
        )
    }
}

/// Adds the segment columns that a version 3 database from an earlier development build does
/// not have. A database without the `segments` table is not changed.
fn add_segment_columns(conn: &Connection) -> rusqlite::Result<()> {
    let columns: Vec<String> = conn
        .prepare("SELECT name FROM pragma_table_info('segments')")?
        .query_map([], |row| row.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if columns.is_empty() {
        return Ok(());
    }
    for (name, definition) in [
        ("live_text", "TEXT"),
        ("revision", "INTEGER NOT NULL DEFAULT 0"),
        ("failed", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !columns.iter().any(|column| column == name) {
            conn.execute_batch(&format!(
                "ALTER TABLE segments ADD COLUMN {name} {definition};"
            ))?;
        }
    }
    Ok(())
}

fn delete_analysis(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    // A suggestion without its evidence passage has no source, so it goes with the chunks.
    conn.execute(
        "DELETE FROM suggestions WHERE chunk_id IN (SELECT id FROM chunks WHERE evidence_id = ?1)",
        [id],
    )?;
    conn.execute("DELETE FROM chunks WHERE evidence_id = ?1", [id])?;
    conn.execute("DELETE FROM evidence_pages WHERE evidence_id = ?1", [id])?;
    Ok(())
}

/// Converts words to an FTS5 query: each word becomes a quoted prefix term, so that quotes and
/// operators in the input have no special meaning. The terms are joined with `joiner`: a
/// space (all terms must match) or `" OR "`. Returns `None` if no word has a letter or a digit.
fn fts_query(words: &[&str], joiner: &str) -> Option<String> {
    let terms: Vec<String> = words
        .iter()
        .map(|word| word.replace('"', ""))
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .map(|word| format!("\"{word}\"*"))
        .collect();
    (!terms.is_empty()).then(|| terms.join(joiner))
}

/// True if the stored pages of a file of this type are physical pages: a PDF or an image. A DOCX
/// or text file is stored as text sections, not as printed pages.
pub fn has_physical_pages(mime: &str) -> bool {
    mime == "application/pdf" || mime.starts_with("image/")
}

/// The location of a page in an evidence file, for the user: "Página n" or "Sección n".
pub fn locator_label(mime: &str, page: u32) -> String {
    if has_physical_pages(mime) {
        format!("Página {page}")
    } else {
        format!("Sección {page}")
    }
}

/// The last part of `path`, used as the display name of the file.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn row_to_segment(row: &rusqlite::Row) -> rusqlite::Result<Segment> {
    Ok(Segment {
        id: row.get(0)?,
        t_start_ms: row.get(1)?,
        t_end_ms: row.get(2)?,
        text: row.get(3)?,
        speaker: Speaker::parse(&row.get::<_, String>(4)?),
        is_final: row.get(5)?,
        revision: row.get(6)?,
        failed: row.get(7)?,
    })
}

fn row_to_suggestion(row: &rusqlite::Row) -> rusqlite::Result<Suggestion> {
    Ok(Suggestion {
        id: row.get(0)?,
        hearing_id: row.get(1)?,
        segment_id: row.get(2)?,
        chunk_id: row.get(3)?,
        evidence_id: row.get(4)?,
        evidence_name: row.get(5)?,
        page: row.get(6)?,
        claim: row.get(7)?,
        claim_quote: row.get(8)?,
        evidence_quote: row.get(9)?,
        kind: row
            .get::<_, Option<String>>(10)?
            .as_deref()
            .and_then(SuggestionKind::parse),
        level: SuggestionLevel::parse(&row.get::<_, String>(11)?),
        nli_contradiction: row.get(12)?,
        explanation: row.get(13)?,
        status: SuggestionStatus::parse(&row.get::<_, String>(14)?),
    })
}

fn row_to_hearing(row: &rusqlite::Row) -> rusqlite::Result<Hearing> {
    Ok(Hearing {
        id: row.get(0)?,
        title: row.get(1)?,
        started_at: row.get(2)?,
        ended_at: row.get(3)?,
        audio_path: row.get::<_, Option<String>>(4)?.map(PathBuf::from),
    })
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

    use super::{
        AnalysisStatus, BLOCK_SIZE, ImportError, MAX_FILE_SIZE, NewSuggestion, SCHEMA_V2, Speaker,
        Store, SuggestionCounts, SuggestionKind, SuggestionLevel, SuggestionStatus, locator_label,
    };
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

    /// A page of about 250 words with `word` once, so that one hit has a low BM25 score.
    fn long_page_with(word: &str) -> String {
        format!(
            "{} {word}.",
            "La audiencia continuó sin novedad en la sala. ".repeat(30)
        )
    }

    #[test]
    fn grouped_search_reaches_every_file_when_one_file_has_many_hits() {
        let (store, case) = store_with_case();
        let large = import(&store, case, &temp_file("grande.txt", b"x"));
        store
            .save_analysis(large, &[text_page(1, &"El testigo habló. ".repeat(20_000))])
            .unwrap();
        let mut small = Vec::new();
        for n in 0..6 {
            let id = import(&store, case, &temp_file(&format!("acta{n}.txt"), b"x"));
            store
                .save_analysis(id, &[text_page(1, &long_page_with("testigo"))])
                .unwrap();
            small.push(id);
        }

        let groups = store.search_groups(case, "testigo", 3).unwrap();

        let ids: Vec<i64> = groups.iter().map(|group| group.evidence_id).collect();
        assert_eq!(ids.len(), 7);
        assert_eq!(ids[0], large);
        for id in &small {
            assert!(ids.contains(id), "{id} is missing");
        }
        assert_eq!(groups[0].evidence_name, "grande.txt");
        assert_eq!(groups[0].mime, "text/plain");
        assert_eq!(groups[0].hits.len(), 3);
        assert!(groups[0].total > 50, "{}", groups[0].total);
        assert!(groups[0].hits.windows(2).all(|w| w[0].rank <= w[1].rank));
        for group in &groups[1..] {
            assert_eq!((group.total, group.hits.len()), (1, 1));
            assert!(groups[0].hits[0].rank <= group.hits[0].rank);
        }
        assert!(store.search_groups(case, "", 3).unwrap().is_empty());
    }

    #[test]
    fn pdf_and_image_locators_are_pages_and_other_locators_are_sections() {
        let docx = "application/vnd.openxmlformats-officedocument.wordprocessingml.document";
        assert_eq!(locator_label("application/pdf", 3), "Página 3");
        assert_eq!(locator_label("image/png", 1), "Página 1");
        assert_eq!(locator_label("image/jpeg", 1), "Página 1");
        assert_eq!(locator_label(docx, 1), "Sección 1");
        assert_eq!(locator_label("text/plain", 2), "Sección 2");
        assert_eq!(locator_label("application/octet-stream", 1), "Sección 1");

        let (store, case) = store_with_case();
        let id = import(&store, case, &temp_file("acta.docx", b"x"));
        assert_eq!(store.evidence(id).unwrap().locator_label(1), "Sección 1");
    }

    #[test]
    fn search_any_takes_two_passages_from_one_file_before_it_fills_by_rank() {
        let (store, case) = store_with_case();
        let large = import(&store, case, &temp_file("grande.txt", b"x"));
        store
            .save_analysis(large, &[text_page(1, &"El testigo habló. ".repeat(20_000))])
            .unwrap();
        let small = import(&store, case, &temp_file("acta.txt", b"x"));
        store
            .save_analysis(small, &[text_page(1, &long_page_with("testigo"))])
            .unwrap();
        let words = ["testigo".to_owned()];

        let hits = store.search_any(case, &words, 5).unwrap();

        let ids: Vec<i64> = hits.iter().map(|hit| hit.evidence_id).collect();
        assert_eq!(ids, [large, large, small, large, large]);
        assert!(hits[..2].iter().all(|hit| hit.rank <= hits[2].rank));
        assert!(hits[3..].iter().all(|hit| hit.rank <= hits[2].rank));
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
    fn a_version_1_database_is_migrated_to_the_current_version() {
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
        assert_eq!(version, 4);
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

    #[test]
    fn a_version_2_database_keeps_its_evidence_and_gets_hearings() {
        let dir = std::env::temp_dir().join(format!("nicolaw-v2-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v2.db");
        let _ = fs::remove_file(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(super::SCHEMA_V2).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 2;
             INSERT INTO cases VALUES (1, 'Caso', 0);
             INSERT INTO evidence (id, case_id, name, mime, size, sha256, added_at, content)
             VALUES (1, 1, 'acta.txt', 'text/plain', 1, '', 0, x'41');",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&db).unwrap();

        assert_eq!(store.list(1).unwrap()[0].name, "acta.txt");
        let hearing = store.create_hearing(1, "Audiencia").unwrap();
        assert_eq!(store.hearings(1).unwrap()[0].id, hearing.id);
    }

    #[test]
    fn a_version_3_database_keeps_its_hearings_and_gets_suggestions() {
        let dir = std::env::temp_dir().join(format!("nicolaw-v3-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("v3.db");
        let _ = fs::remove_file(&db);
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(super::SCHEMA_V2).unwrap();
        conn.execute_batch(super::SCHEMA_V3).unwrap();
        conn.execute_batch(
            "PRAGMA user_version = 3;
             INSERT INTO cases VALUES (1, 'Caso', 0);
             INSERT INTO hearings (id, case_id, title) VALUES (1, 1, 'Audiencia');",
        )
        .unwrap();
        drop(conn);

        let store = Store::open(&db).unwrap();

        assert_eq!(store.hearings(1).unwrap()[0].title, "Audiencia");
        assert!(store.suggestions(1).unwrap().is_empty());
    }

    #[test]
    fn hearings_are_created_listed_newest_first_and_per_case() {
        let (store, case) = store_with_case();
        let other = store.create_case("Otro caso").unwrap().id;

        let first = store.create_hearing(case, "Primera audiencia").unwrap();
        let second = store.create_hearing(case, "Segunda audiencia").unwrap();
        store
            .create_hearing(other, "Audiencia de otro caso")
            .unwrap();

        let hearings = store.hearings(case).unwrap();
        let titles: Vec<&str> = hearings.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, ["Segunda audiencia", "Primera audiencia"]);
        assert_eq!(hearings[1].id, first.id);
        assert_eq!(hearings[0].id, second.id);
        assert!(hearings[0].started_at.is_none());
        assert!(hearings[0].ended_at.is_none());
        assert!(hearings[0].audio_path.is_none());
    }

    #[test]
    fn a_hearing_records_its_start_audio_path_and_end() {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;

        store
            .start_hearing(id, Path::new("/tmp/hearings/1.wav"))
            .unwrap();
        let started = store.hearings(case).unwrap().remove(0);
        store.end_hearing(id).unwrap();
        let ended = store.hearings(case).unwrap().remove(0);

        assert!(started.started_at.is_some());
        assert!(started.ended_at.is_none());
        assert_eq!(
            started.audio_path.as_deref(),
            Some(Path::new("/tmp/hearings/1.wav"))
        );
        assert!(ended.ended_at.is_some());
    }

    #[test]
    fn segments_go_from_open_to_final_in_time_order() {
        let (store, case) = store_with_case();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;

        let late = store
            .insert_segment(hearing, 5_000, Speaker::Other)
            .unwrap();
        let early = store
            .insert_segment(hearing, 1_000, Speaker::Accused)
            .unwrap();
        let open = store.segments(hearing).unwrap();
        store
            .finish_segment(early, "Yo no estaba allí.", 3_500)
            .unwrap();
        store.set_segment_text(early, "Yo no estaba ahí.").unwrap();
        store.set_speaker(late, Speaker::Accused).unwrap();
        let segments = store.segments(hearing).unwrap();

        assert_eq!(open.len(), 2);
        assert!(
            open.iter()
                .all(|segment| !segment.is_final && segment.text.is_empty())
        );
        assert_eq!(segments[0].id, early);
        assert_eq!(segments[0].t_start_ms, 1_000);
        assert_eq!(segments[0].t_end_ms, 3_500);
        assert_eq!(segments[0].text, "Yo no estaba ahí.");
        assert_eq!(segments[0].speaker, Speaker::Accused);
        assert!(segments[0].is_final);
        assert_eq!(segments[1].id, late);
        assert_eq!(segments[1].speaker, Speaker::Accused);
        assert!(!segments[1].is_final);
    }

    #[test]
    fn a_refined_text_keeps_the_live_text_and_increments_the_revision() {
        let (store, case) = store_with_case();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let id = store.insert_segment(hearing, 0, Speaker::Other).unwrap();
        store
            .finish_segment(id, "yo no estaba alli", 2_000)
            .unwrap();
        let live = store.segments(hearing).unwrap()[0].revision;

        store.set_segment_text(id, "Yo no estaba allí.").unwrap();
        store.set_segment_text(id, "Yo no estaba allí.").unwrap();
        store.set_segment_text(id, "Yo no estaba ahí.").unwrap();

        let segment = &store.segments(hearing).unwrap()[0];
        assert_eq!(live, 0);
        assert_eq!(segment.text, "Yo no estaba ahí.");
        assert_eq!(segment.revision, 2);
        assert_eq!(
            store.live_text(id).unwrap().as_deref(),
            Some("yo no estaba alli")
        );
    }

    #[test]
    fn a_failed_segment_stays_as_a_final_marker_without_text() {
        let (store, case) = store_with_case();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let id = store
            .insert_segment(hearing, 1_000, Speaker::Accused)
            .unwrap();

        store.fail_segment(id, 4_000).unwrap();

        let segment = &store.segments(hearing).unwrap()[0];
        assert!(segment.is_final && segment.failed);
        assert_eq!((segment.t_start_ms, segment.t_end_ms), (1_000, 4_000));
        assert!(segment.text.is_empty());
    }

    #[test]
    fn a_database_from_an_earlier_version_3_gets_the_new_segment_columns() {
        let path = temp_file("early-v3.db", b"");
        fs::remove_file(&path).unwrap();
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(&format!(
                "{SCHEMA_V2} CREATE TABLE hearings (id INTEGER PRIMARY KEY, case_id INTEGER \
                 NOT NULL, title TEXT NOT NULL, started_at INTEGER, ended_at INTEGER, \
                 audio_path TEXT); CREATE TABLE segments (id INTEGER PRIMARY KEY, hearing_id \
                 INTEGER NOT NULL, t_start_ms INTEGER NOT NULL, t_end_ms INTEGER NOT NULL, text \
                 TEXT NOT NULL, speaker TEXT NOT NULL, final INTEGER NOT NULL DEFAULT 0); \
                 PRAGMA user_version = 3;"
            ))
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        let case = store.create_case("Caso").unwrap().id;
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let id = store.insert_segment(hearing, 0, Speaker::Other).unwrap();
        store.fail_segment(id, 10).unwrap();

        assert!(store.segments(hearing).unwrap()[0].failed);
        assert_eq!(store.segments(hearing).unwrap()[0].revision, 0);
    }

    #[test]
    fn the_case_of_a_hearing_has_its_id_and_name() {
        let (store, case) = store_with_case();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;

        assert_eq!(
            store.hearing_case(hearing).unwrap(),
            (case, "Caso de prueba".to_owned())
        );
    }

    #[test]
    fn end_hearing_of_a_missing_hearing_is_an_error() {
        let (store, _) = store_with_case();

        assert!(store.end_hearing(99).is_err());
    }

    #[test]
    fn an_empty_segment_can_be_deleted() {
        let (store, case) = store_with_case();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let id = store.insert_segment(hearing, 0, Speaker::Other).unwrap();

        store.delete_segment(id).unwrap();

        assert!(store.segments(hearing).unwrap().is_empty());
    }

    #[test]
    fn delete_hearing_removes_its_segments_and_its_audio_file() {
        let (store, case) = store_with_case();
        let audio = temp_file("hearing.wav", b"RIFF");
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        let kept = store.create_hearing(case, "Otra audiencia").unwrap().id;
        store.start_hearing(id, &audio).unwrap();
        store.insert_segment(id, 0, Speaker::Other).unwrap();
        store.insert_segment(kept, 0, Speaker::Other).unwrap();

        store.delete_hearing(id).unwrap();

        let hearings = store.hearings(case).unwrap();
        assert_eq!(hearings.len(), 1);
        assert_eq!(hearings[0].id, kept);
        assert!(store.segments(id).unwrap().is_empty());
        assert_eq!(store.segments(kept).unwrap().len(), 1);
        assert!(!audio.exists());
    }

    #[test]
    fn delete_hearing_without_an_audio_file_succeeds() {
        let (store, case) = store_with_case();
        let id = store.create_hearing(case, "Audiencia").unwrap().id;
        store
            .start_hearing(id, Path::new("/nonexistent/nicolaw/1.wav"))
            .unwrap();

        store.delete_hearing(id).unwrap();

        assert!(store.hearings(case).unwrap().is_empty());
    }

    #[test]
    fn today_is_the_local_date_as_day_month_year() {
        let store = Store::open_in_memory().unwrap();

        let today = store.today().unwrap();

        let parts: Vec<&str> = today.split('/').collect();
        assert_eq!(
            parts.iter().map(|part| part.len()).collect::<Vec<_>>(),
            [2, 2, 4]
        );
        assert!(
            parts
                .iter()
                .all(|part| part.chars().all(|c| c.is_ascii_digit()))
        );
    }

    /// A store with one case, one analyzed evidence file, and one hearing with one accused
    /// segment. Returns the store, the hearing id, the segment id, and the chunk id.
    fn store_with_hearing_and_evidence() -> (Store, i64, i64, i64, i64) {
        let (store, case) = store_with_case();
        let evidence = import(&store, case, &temp_file("acta.txt", b"x"));
        store
            .save_analysis(
                evidence,
                &[text_page(1, "Primera página."), text_page(2, DECLARATION)],
            )
            .unwrap();
        let chunk = store.search(case, "declaracion", 1).unwrap()[0].chunk_id;
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let segment = store
            .insert_segment(hearing, 1_000, Speaker::Accused)
            .unwrap();
        store
            .finish_segment(segment, "Yo estaba en Medellín el 12 de marzo.", 4_000)
            .unwrap();
        (store, hearing, segment, chunk, evidence)
    }

    fn suggestion(
        hearing_id: i64,
        segment_id: i64,
        chunk_id: i64,
        level: SuggestionLevel,
    ) -> NewSuggestion {
        NewSuggestion {
            hearing_id,
            segment_id,
            chunk_id,
            claim: "El acusado estaba en Medellín el 12 de marzo.".into(),
            claim_quote: Some("en Medellín".into()),
            evidence_quote: "rendida el 12 de marzo de 2024 en Bogotá".into(),
            kind: Some(SuggestionKind::Location),
            level,
            nli_contradiction: 0.93,
            explanation: Some("La prueba ubica la declaración en Bogotá.".into()),
        }
    }

    #[test]
    fn a_suggestion_is_stored_with_its_source_file_and_page() {
        let (store, hearing, segment, chunk, evidence) = store_with_hearing_and_evidence();

        let stored = store
            .insert_suggestion(&suggestion(
                hearing,
                segment,
                chunk,
                SuggestionLevel::Conflict,
            ))
            .unwrap();

        assert_eq!(stored.hearing_id, hearing);
        assert_eq!(stored.segment_id, segment);
        assert_eq!(stored.chunk_id, chunk);
        assert_eq!(stored.evidence_id, evidence);
        assert_eq!(stored.evidence_name, "acta.txt");
        assert_eq!(stored.page, 2);
        assert_eq!(
            stored.claim,
            "El acusado estaba en Medellín el 12 de marzo."
        );
        assert_eq!(stored.claim_quote.as_deref(), Some("en Medellín"));
        assert_eq!(
            stored.evidence_quote,
            "rendida el 12 de marzo de 2024 en Bogotá"
        );
        assert_eq!(stored.kind, Some(SuggestionKind::Location));
        assert_eq!(stored.level, SuggestionLevel::Conflict);
        assert_eq!(stored.nli_contradiction, 0.93);
        assert_eq!(
            stored.explanation.as_deref(),
            Some("La prueba ubica la declaración en Bogotá.")
        );
        assert_eq!(stored.status, SuggestionStatus::Open);
        assert_eq!(store.suggestions(hearing).unwrap()[0].id, stored.id);
    }

    #[test]
    fn a_suggestion_can_have_no_quote_kind_or_explanation() {
        let (store, hearing, segment, chunk, _) = store_with_hearing_and_evidence();
        let mut new = suggestion(hearing, segment, chunk, SuggestionLevel::Check);
        new.claim_quote = None;
        new.kind = None;
        new.explanation = None;

        let stored = store.insert_suggestion(&new).unwrap();

        assert_eq!(stored.claim_quote, None);
        assert_eq!(stored.kind, None);
        assert_eq!(stored.explanation, None);
        assert_eq!(stored.level, SuggestionLevel::Check);
    }

    #[test]
    fn suggestions_list_open_first_then_newest_first() {
        let (store, hearing, segment, chunk, _) = store_with_hearing_and_evidence();
        let first = store
            .insert_suggestion(&suggestion(hearing, segment, chunk, SuggestionLevel::Check))
            .unwrap()
            .id;
        let second = store
            .insert_suggestion(&suggestion(
                hearing,
                segment,
                chunk,
                SuggestionLevel::Conflict,
            ))
            .unwrap()
            .id;
        let third = store
            .insert_suggestion(&suggestion(hearing, segment, chunk, SuggestionLevel::Check))
            .unwrap()
            .id;

        store
            .set_suggestion_status(third, SuggestionStatus::Dismissed)
            .unwrap();

        let ids: Vec<i64> = store
            .suggestions(hearing)
            .unwrap()
            .iter()
            .map(|suggestion| suggestion.id)
            .collect();
        assert_eq!(ids, [second, first, third]);
        assert_eq!(
            store.suggestions(hearing).unwrap()[2].status,
            SuggestionStatus::Dismissed
        );
    }

    #[test]
    fn counts_have_the_open_conflicts_and_the_open_checks() {
        let (store, hearing, segment, chunk, _) = store_with_hearing_and_evidence();
        let add = |level| {
            store
                .insert_suggestion(&suggestion(hearing, segment, chunk, level))
                .unwrap()
                .id
        };
        add(SuggestionLevel::Conflict);
        let confirmed = add(SuggestionLevel::Conflict);
        add(SuggestionLevel::Check);
        add(SuggestionLevel::Check);
        let dismissed = add(SuggestionLevel::Check);
        store
            .set_suggestion_status(confirmed, SuggestionStatus::Confirmed)
            .unwrap();
        store
            .set_suggestion_status(dismissed, SuggestionStatus::Dismissed)
            .unwrap();

        assert_eq!(
            store.suggestion_counts(hearing).unwrap(),
            SuggestionCounts {
                conflict: 1,
                check: 2
            }
        );
    }

    #[test]
    fn delete_hearing_removes_its_suggestions() {
        let (store, hearing, segment, chunk, _) = store_with_hearing_and_evidence();
        store
            .insert_suggestion(&suggestion(
                hearing,
                segment,
                chunk,
                SuggestionLevel::Conflict,
            ))
            .unwrap();

        store.delete_hearing(hearing).unwrap();

        assert!(store.suggestions(hearing).unwrap().is_empty());
        assert_eq!(
            store.suggestion_counts(hearing).unwrap(),
            SuggestionCounts::default()
        );
    }

    #[test]
    fn delete_evidence_removes_the_suggestions_of_its_chunks() {
        let (store, hearing, segment, chunk, evidence) = store_with_hearing_and_evidence();
        store
            .insert_suggestion(&suggestion(hearing, segment, chunk, SuggestionLevel::Check))
            .unwrap();

        store.delete(evidence).unwrap();

        assert!(store.suggestions(hearing).unwrap().is_empty());
    }

    #[test]
    fn a_chunk_gives_its_text_and_its_source() {
        let (store, _, _, chunk, evidence) = store_with_hearing_and_evidence();

        let source = store.chunk(chunk).unwrap();

        assert_eq!(source.id, chunk);
        assert_eq!(source.evidence_id, evidence);
        assert_eq!(source.evidence_name, "acta.txt");
        assert_eq!(source.page, 2);
        assert_eq!(source.text, DECLARATION);
    }

    #[test]
    fn a_segment_is_read_by_id_with_the_question_before_it() {
        let (store, case) = store_with_case();
        let hearing = store.create_hearing(case, "Audiencia").unwrap().id;
        let add = |t: i64, speaker, text: &str| {
            let id = store.insert_segment(hearing, t, speaker).unwrap();
            store.finish_segment(id, text, t + 500).unwrap();
            id
        };
        add(0, Speaker::Other, "¿Dónde estaba usted?");
        add(1_000, Speaker::Other, "¿Dónde estaba el 12 de marzo?");
        add(2_000, Speaker::Accused, "No recuerdo.");
        let answer = add(3_000, Speaker::Accused, "Estaba en Medellín.");
        let first = store.insert_segment(hearing, 0, Speaker::Accused).unwrap();

        let segment = store.segment(answer).unwrap();

        assert_eq!(segment.text, "Estaba en Medellín.");
        assert_eq!(segment.speaker, Speaker::Accused);
        assert_eq!(
            store.question_before(hearing, 3_000).unwrap().as_deref(),
            Some("¿Dónde estaba el 12 de marzo?")
        );
        assert_eq!(store.question_before(hearing, 0).unwrap(), None);
        assert!(store.segment(first + 100).is_err());
    }

    #[test]
    fn search_any_finds_chunks_with_some_of_the_words_best_first() {
        let (store, case) = store_with_case();
        let evidence = import(&store, case, &temp_file("acta.txt", b"x"));
        store
            .save_analysis(
                evidence,
                &[
                    text_page(1, "El testigo vio un carro rojo."),
                    text_page(2, DECLARATION),
                    text_page(3, "El acusado vive en Bogotá desde marzo."),
                ],
            )
            .unwrap();
        let words = ["acusado", "12", "marzo", "medellín", "\"OR", "*"].map(String::from);

        let hits = store.search_any(case, &words, 20).unwrap();

        let pages: Vec<u32> = hits.iter().map(|hit| hit.page).collect();
        assert_eq!(pages.len(), 2);
        assert_eq!(pages[0], 2, "the chunk with more words is first");
        assert!(store.search_any(case, &[], 20).unwrap().is_empty());
    }
}
