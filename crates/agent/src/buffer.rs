use std::path::Path;

use shared::FrameData;
use tokio_rusqlite::Connection;
use tracing::{debug, info};

// ---------------------------------------------------------------------------
// Error
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub enum BufferError {
    Sqlite(rusqlite::Error),
    Async(tokio_rusqlite::Error),
}

impl std::fmt::Display for BufferError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BufferError::Sqlite(e) => write!(f, "SQLite error: {}", e),
            BufferError::Async(e) => write!(f, "Async SQLite error: {}", e),
        }
    }
}

impl From<rusqlite::Error> for BufferError {
    fn from(e: rusqlite::Error) -> Self {
        BufferError::Sqlite(e)
    }
}

impl From<tokio_rusqlite::Error> for BufferError {
    fn from(e: tokio_rusqlite::Error) -> Self {
        BufferError::Async(e)
    }
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct BufferedFrame {
    pub id: i64,
    pub camera_id: String,
    pub timestamp: String,
    pub jpeg_bytes: Vec<u8>,
    pub retry_count: i32,
}

// ---------------------------------------------------------------------------
// FrameBuffer
// ---------------------------------------------------------------------------

pub struct FrameBuffer {
    conn: Connection,
}

impl FrameBuffer {
    pub async fn open(db_path: &Path) -> Result<Self, BufferError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).ok();
        }

        let conn = Connection::open(db_path).await?;

        conn.call(|conn| {
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS pending_frames (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    camera_id TEXT NOT NULL,
                    timestamp TEXT NOT NULL,
                    jpeg_bytes BLOB NOT NULL,
                    created_at TEXT NOT NULL DEFAULT (datetime('now')),
                    status TEXT NOT NULL DEFAULT 'pending',
                    retry_count INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX IF NOT EXISTS idx_status ON pending_frames(status);",
            )?;
            Ok(())
        })
        .await?;

        debug!("Frame buffer opened: {}", db_path.display());
        Ok(Self { conn })
    }

    /// Insert a captured frame into the local buffer.
    pub async fn enqueue(&self, frame: &FrameData) -> Result<i64, BufferError> {
        let camera_id = frame.camera_id.clone();
        let timestamp = frame.timestamp.to_rfc3339();
        let jpeg_bytes = frame.jpeg_bytes.clone();

        let id = self
            .conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO pending_frames (camera_id, timestamp, jpeg_bytes) VALUES (?1, ?2, ?3)",
                    rusqlite::params![camera_id, timestamp, jpeg_bytes],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await?;

        debug!(id, "Frame enqueued to local buffer");
        Ok(id)
    }

    /// Fetch up to `limit` pending frames (oldest first).
    pub async fn peek_pending(&self, limit: usize) -> Result<Vec<BufferedFrame>, BufferError> {
        let limit = limit as i64;
        let rows = self
            .conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, camera_id, timestamp, jpeg_bytes, retry_count
                     FROM pending_frames
                     WHERE status = 'pending'
                     ORDER BY id ASC
                     LIMIT ?1",
                )?;
                let frames = stmt
                    .query_map([limit], |row| {
                        Ok(BufferedFrame {
                            id: row.get(0)?,
                            camera_id: row.get(1)?,
                            timestamp: row.get(2)?,
                            jpeg_bytes: row.get(3)?,
                            retry_count: row.get(4)?,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(frames)
            })
            .await?;
        Ok(rows)
    }

    /// Mark a frame as successfully uploaded.
    pub async fn mark_done(&self, id: i64) -> Result<(), BufferError> {
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE pending_frames SET status = 'done' WHERE id = ?1",
                    [id],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Increment retry count for a failed upload attempt.
    pub async fn mark_failed(&self, id: i64) -> Result<(), BufferError> {
        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE pending_frames SET retry_count = retry_count + 1 WHERE id = ?1",
                    [id],
                )?;
                Ok(())
            })
            .await?;
        Ok(())
    }

    /// Number of frames still pending upload.
    pub async fn pending_count(&self) -> Result<usize, BufferError> {
        let count = self
            .conn
            .call(|conn| {
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM pending_frames WHERE status = 'pending'",
                    [],
                    |row| row.get(0),
                )?;
                Ok(count as usize)
            })
            .await?;
        Ok(count)
    }

    /// Delete completed frames and frames older than `max_age_hours`.
    pub async fn cleanup_old(&self, max_age_hours: u64) -> Result<usize, BufferError> {
        let hours = max_age_hours as i64;
        let deleted = self
            .conn
            .call(move |conn| {
                let n = conn.execute(
                    "DELETE FROM pending_frames
                     WHERE status = 'done'
                        OR created_at < datetime('now', '-' || ?1 || ' hours')",
                    [hours],
                )?;
                Ok(n)
            })
            .await?;
        if deleted > 0 {
            info!(deleted, "Cleaned up old buffer entries");
        }
        Ok(deleted)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Utc;
    use shared::Resolution;
    use tempfile::NamedTempFile;

    fn test_frame(camera_id: &str) -> FrameData {
        FrameData {
            camera_id: camera_id.to_string(),
            timestamp: Utc::now(),
            jpeg_bytes: vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10],
            resolution: Resolution {
                width: 640,
                height: 480,
            },
        }
    }

    #[tokio::test]
    async fn test_enqueue_and_peek() {
        let tmp = NamedTempFile::new().unwrap();
        let buf = FrameBuffer::open(tmp.path()).await.unwrap();

        let id = buf.enqueue(&test_frame("cam-1")).await.unwrap();
        assert!(id > 0);

        let pending = buf.peek_pending(10).await.unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].camera_id, "cam-1");
        assert_eq!(pending[0].jpeg_bytes, vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10]);
    }

    #[tokio::test]
    async fn test_mark_done_removes_from_pending() {
        let tmp = NamedTempFile::new().unwrap();
        let buf = FrameBuffer::open(tmp.path()).await.unwrap();

        let id = buf.enqueue(&test_frame("cam-1")).await.unwrap();
        buf.mark_done(id).await.unwrap();

        let pending = buf.peek_pending(10).await.unwrap();
        assert_eq!(pending.len(), 0);
    }

    #[tokio::test]
    async fn test_mark_failed_increments_retry() {
        let tmp = NamedTempFile::new().unwrap();
        let buf = FrameBuffer::open(tmp.path()).await.unwrap();

        let id = buf.enqueue(&test_frame("cam-1")).await.unwrap();
        buf.mark_failed(id).await.unwrap();
        buf.mark_failed(id).await.unwrap();

        let pending = buf.peek_pending(10).await.unwrap();
        assert_eq!(pending[0].retry_count, 2);
    }

    #[tokio::test]
    async fn test_pending_count() {
        let tmp = NamedTempFile::new().unwrap();
        let buf = FrameBuffer::open(tmp.path()).await.unwrap();

        assert_eq!(buf.pending_count().await.unwrap(), 0);

        buf.enqueue(&test_frame("cam-1")).await.unwrap();
        buf.enqueue(&test_frame("cam-2")).await.unwrap();
        assert_eq!(buf.pending_count().await.unwrap(), 2);

        let pending = buf.peek_pending(1).await.unwrap();
        buf.mark_done(pending[0].id).await.unwrap();
        assert_eq!(buf.pending_count().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn test_cleanup_done_entries() {
        let tmp = NamedTempFile::new().unwrap();
        let buf = FrameBuffer::open(tmp.path()).await.unwrap();

        let id = buf.enqueue(&test_frame("cam-1")).await.unwrap();
        buf.mark_done(id).await.unwrap();

        // Also enqueue one still pending.
        buf.enqueue(&test_frame("cam-2")).await.unwrap();

        let deleted = buf.cleanup_old(24).await.unwrap();
        assert_eq!(deleted, 1); // only the done entry

        assert_eq!(buf.pending_count().await.unwrap(), 1);
    }
}
