use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;

use rusqlite::{Connection, MAIN_DB, params};

/// The largest file that the store accepts: 100 MB.
pub const MAX_FILE_SIZE: u64 = 100 * 1024 * 1024;

/// The store moves file content in blocks of this size, and reports progress after each block.
pub const BLOCK_SIZE: usize = 256 * 1024;

/// One stored file, without its content.
#[derive(Clone, Debug)]
pub struct FileEntry {
    pub id: i64,
    pub name: String,
    pub mime: String,
    pub size: u64,
    /// Local time, as `YYYY-MM-DD HH:MM`.
    pub added_at: String,
}

#[derive(Debug)]
pub enum ImportError {
    TooLarge,
    Read,
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

const SELECT_ENTRY: &str = "SELECT id, name, mime, size, \
     strftime('%Y-%m-%d %H:%M', added_at, 'unixepoch', 'localtime') FROM files";

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        Self::migrate(Connection::open(path)?)
    }

    #[cfg(test)]
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::migrate(Connection::open_in_memory()?)
    }

    fn migrate(conn: Connection) -> rusqlite::Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS files (
                id       INTEGER PRIMARY KEY,
                name     TEXT    NOT NULL,
                mime     TEXT    NOT NULL,
                size     INTEGER NOT NULL,
                added_at INTEGER NOT NULL, -- Unix seconds
                content  BLOB    NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    /// Reads the file at `path` and stores a copy of it.
    /// Calls `progress` with (bytes done, bytes total) after each block.
    /// If the import fails, the store keeps no row for the file.
    pub fn import(
        &self,
        path: &Path,
        mut progress: impl FnMut(u64, u64),
    ) -> Result<FileEntry, ImportError> {
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
            "INSERT INTO files (name, mime, size, added_at, content)
             VALUES (?1, ?2, ?3, unixepoch(), zeroblob(?3))",
            params![name, mime, size as i64],
        )?;
        let id = tx.last_insert_rowid();
        {
            let mut blob = tx.blob_open(MAIN_DB, "files", "content", id, false)?;
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
                done += read as u64;
                progress(done, size);
                if done == size {
                    break;
                }
            }
        }
        let entry = tx.query_row(&format!("{SELECT_ENTRY} WHERE id = ?1"), [id], row_to_entry)?;
        tx.commit()?;
        Ok(entry)
    }

    /// Returns all files, newest first. The content is not read.
    pub fn list(&self) -> rusqlite::Result<Vec<FileEntry>> {
        self.conn
            .prepare(&format!("{SELECT_ENTRY} ORDER BY added_at DESC, id DESC"))?
            .query_map([], row_to_entry)?
            .collect()
    }

    /// Reads the content of a file.
    /// Calls `progress` with (bytes done, bytes total) after each block.
    pub fn content(
        &self,
        id: i64,
        mut progress: impl FnMut(u64, u64),
    ) -> rusqlite::Result<Vec<u8>> {
        let mut blob = self.conn.blob_open(MAIN_DB, "files", "content", id, true)?;
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

    pub fn delete(&self, id: i64) -> rusqlite::Result<()> {
        self.conn.execute("DELETE FROM files WHERE id = ?1", [id])?;
        Ok(())
    }
}

/// The last part of `path`, used as the display name of the file.
pub fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<FileEntry> {
    Ok(FileEntry {
        id: row.get(0)?,
        name: row.get(1)?,
        mime: row.get(2)?,
        size: row.get::<_, i64>(3)? as u64,
        added_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_file(name: &str, content: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("nicolaw-test-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn import_stores_name_type_size_and_content() {
        let store = Store::open_in_memory().unwrap();
        let path = temp_file("notes.txt", b"hello");

        let entry = store.import(&path, |_, _| {}).unwrap();

        assert_eq!(entry.name, "notes.txt");
        assert_eq!(entry.mime, "text/plain");
        assert_eq!(entry.size, 5);
        assert_eq!(store.content(entry.id, |_, _| {}).unwrap(), b"hello");
    }

    #[test]
    fn unknown_type_is_octet_stream() {
        let store = Store::open_in_memory().unwrap();
        let path = temp_file("data.unknownext", &[0, 1, 2]);

        let entry = store.import(&path, |_, _| {}).unwrap();

        assert_eq!(entry.mime, "application/octet-stream");
    }

    #[test]
    fn list_returns_newest_first_with_formatted_date() {
        let store = Store::open_in_memory().unwrap();
        store.import(&temp_file("a.txt", b"a"), |_, _| {}).unwrap();
        store.import(&temp_file("b.txt", b"b"), |_, _| {}).unwrap();

        let files = store.list().unwrap();

        let names: Vec<_> = files.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["b.txt", "a.txt"]);
        // Format: YYYY-MM-DD HH:MM
        assert_eq!(files[0].added_at.len(), 16);
        assert_eq!(&files[0].added_at[4..5], "-");
        assert_eq!(&files[0].added_at[13..14], ":");
    }

    #[test]
    fn delete_removes_the_file() {
        let store = Store::open_in_memory().unwrap();
        let entry = store
            .import(&temp_file("gone.txt", b"x"), |_, _| {})
            .unwrap();

        store.delete(entry.id).unwrap();

        assert!(store.list().unwrap().is_empty());
        assert!(store.content(entry.id, |_, _| {}).is_err());
    }

    #[test]
    fn file_larger_than_limit_is_rejected() {
        let store = Store::open_in_memory().unwrap();
        let path = temp_file("big.bin", b"");
        // A sparse file: the size is set without writing the bytes.
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(MAX_FILE_SIZE + 1)
            .unwrap();

        assert!(matches!(
            store.import(&path, |_, _| {}),
            Err(ImportError::TooLarge)
        ));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn missing_file_is_a_read_error() {
        let store = Store::open_in_memory().unwrap();
        let path = std::env::temp_dir().join("nicolaw-missing-file.txt");

        assert!(matches!(
            store.import(&path, |_, _| {}),
            Err(ImportError::Read)
        ));
    }

    #[test]
    fn data_stays_after_reopen() {
        let dir = std::env::temp_dir().join(format!("nicolaw-db-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("test.db");
        let _ = fs::remove_file(&db);

        Store::open(&db)
            .unwrap()
            .import(&temp_file("keep.txt", b"keep"), |_, _| {})
            .unwrap();
        let files = Store::open(&db).unwrap().list().unwrap();

        assert_eq!(files.len(), 1);
        assert_eq!(files[0].name, "keep.txt");
    }

    /// Records each progress call.
    fn record(calls: &mut Vec<(u64, u64)>) -> impl FnMut(u64, u64) + '_ {
        |done, total| calls.push((done, total))
    }

    #[test]
    fn import_reports_progress_for_each_block() {
        let store = Store::open_in_memory().unwrap();
        let data: Vec<u8> = (0..BLOCK_SIZE * 2 + 1000)
            .map(|i| (i % 251) as u8)
            .collect();
        let path = temp_file("large.bin", &data);
        let mut calls = Vec::new();

        let entry = store.import(&path, record(&mut calls)).unwrap();

        let total = data.len() as u64;
        let block = BLOCK_SIZE as u64;
        assert_eq!(calls, [(block, total), (2 * block, total), (total, total)]);
        assert_eq!(store.content(entry.id, |_, _| {}).unwrap(), data);
    }

    #[test]
    fn import_of_an_empty_file_reports_zero_of_zero() {
        let store = Store::open_in_memory().unwrap();
        let mut calls = Vec::new();

        let entry = store
            .import(&temp_file("empty.txt", b""), record(&mut calls))
            .unwrap();

        assert_eq!(calls, [(0, 0)]);
        assert_eq!(entry.size, 0);
        assert!(store.content(entry.id, |_, _| {}).unwrap().is_empty());
    }

    #[test]
    fn failed_read_after_the_metadata_leaves_no_row() {
        let store = Store::open_in_memory().unwrap();
        // A directory has metadata with a size, but a read from it fails.
        let dir = std::env::temp_dir().join(format!("nicolaw-dir-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("inside.txt"), b"x").unwrap();

        assert!(matches!(
            store.import(&dir, |_, _| {}),
            Err(ImportError::Read)
        ));
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn content_reports_progress_and_returns_all_bytes() {
        let store = Store::open_in_memory().unwrap();
        let data: Vec<u8> = (0..BLOCK_SIZE + 10).map(|i| (i % 7) as u8).collect();
        let entry = store
            .import(&temp_file("read.bin", &data), |_, _| {})
            .unwrap();
        let mut calls = Vec::new();

        let content = store.content(entry.id, record(&mut calls)).unwrap();

        let total = data.len() as u64;
        assert_eq!(calls, [(BLOCK_SIZE as u64, total), (total, total)]);
        assert_eq!(content, data);
    }
}
