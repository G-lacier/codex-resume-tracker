use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, ErrorCode, OptionalExtension, Row};

use crate::config::{SummaryInput, SummaryProvider};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SessionMetadata {
    pub thread_id: String,
    pub cwd: String,
    pub source: String,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub recency_at: Option<i64>,
    pub generated_title: Option<String>,
}

impl SessionMetadata {
    pub fn resume_command(&self) -> String {
        format!("codex resume {}", self.thread_id)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Session {
    pub thread_id: String,
    pub resume_command: String,
    pub cwd: String,
    pub source: String,
    pub model: Option<String>,
    pub model_provider: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
    pub recency_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub generated_title: Option<String>,
    pub generated_summary: Option<String>,
    pub manual_title: Option<String>,
    pub manual_summary: Option<String>,
    pub notes: String,
    pub pinned: bool,
    pub summary_provider: Option<String>,
    pub summary_cap: Option<String>,
    pub summary_status: String,
    pub summary_error: Option<String>,
    pub tags: Vec<String>,
}

impl Session {
    pub fn title(&self) -> &str {
        self.manual_title
            .as_deref()
            .or(self.generated_title.as_deref())
            .unwrap_or("Untitled Codex session")
    }

    pub fn summary(&self) -> &str {
        self.manual_summary
            .as_deref()
            .or(self.generated_summary.as_deref())
            .unwrap_or("")
    }

    pub fn searchable_text(&self) -> String {
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.title(),
            self.summary(),
            self.cwd,
            self.source,
            self.model.as_deref().unwrap_or(""),
            self.model_provider.as_deref().unwrap_or(""),
            self.notes,
            self.tags.join(" ")
        )
        .to_lowercase()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Job {
    pub thread_id: String,
    pub provider: String,
    pub cap: String,
    pub status: String,
    pub attempts: u32,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Database {
    path: PathBuf,
}

impl Database {
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create database directory {}", parent.display()))?;
        }
        let database = Self { path };
        database.migrate()?;
        Ok(database)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn connect(&self) -> Result<Connection> {
        let connection = Connection::open(&self.path)
            .with_context(|| format!("open SQLite database {}", self.path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        enable_wal(&connection)?;
        Ok(connection)
    }

    fn migrate(&self) -> Result<()> {
        let connection = self.connect()?;
        connection.execute_batch(
            r#"
            BEGIN IMMEDIATE;
            CREATE TABLE IF NOT EXISTS sessions (
                thread_id TEXT PRIMARY KEY NOT NULL,
                resume_command TEXT NOT NULL,
                cwd TEXT NOT NULL,
                source TEXT NOT NULL DEFAULT 'unknown',
                model TEXT,
                model_provider TEXT,
                created_at INTEGER,
                updated_at INTEGER,
                recency_at INTEGER,
                ended_at INTEGER,
                generated_title TEXT,
                generated_summary TEXT,
                manual_title TEXT,
                manual_summary TEXT,
                notes TEXT NOT NULL DEFAULT '',
                pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
                summary_provider TEXT,
                summary_cap TEXT,
                summary_status TEXT NOT NULL DEFAULT 'not_requested',
                summary_error TEXT,
                last_synced_at INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS tags (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL UNIQUE COLLATE NOCASE
            );

            CREATE TABLE IF NOT EXISTS session_tags (
                thread_id TEXT NOT NULL REFERENCES sessions(thread_id) ON DELETE CASCADE,
                tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
                PRIMARY KEY (thread_id, tag_id)
            );

            CREATE TABLE IF NOT EXISTS jobs (
                thread_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(thread_id) ON DELETE CASCADE,
                provider TEXT NOT NULL,
                cap TEXT NOT NULL,
                status TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 0,
                last_error TEXT,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS sessions_recency_idx
                ON sessions(pinned DESC, recency_at DESC, updated_at DESC);
            CREATE INDEX IF NOT EXISTS jobs_status_idx ON jobs(status, updated_at);
            PRAGMA user_version = 1;
            COMMIT;
            "#,
        )
        .with_context(|| format!("migrate SQLite database {}", self.path.display()))?;
        Ok(())
    }

    pub fn ingest_hook(&self, thread_id: &str, cwd: &str, ended_at: i64) -> Result<()> {
        let connection = self.connect()?;
        let resume_command = format!("codex resume {thread_id}");
        connection.execute(
            r#"
            INSERT INTO sessions (
                thread_id, resume_command, cwd, source, ended_at, summary_status, last_synced_at
            ) VALUES (?1, ?2, ?3, 'unknown', ?4, 'pending_metadata', ?4)
            ON CONFLICT(thread_id) DO UPDATE SET
                resume_command = excluded.resume_command,
                cwd = excluded.cwd,
                ended_at = excluded.ended_at,
                last_synced_at = excluded.last_synced_at
            "#,
            params![thread_id, resume_command, cwd, ended_at],
        )?;
        Ok(())
    }

    pub fn upsert_metadata(&self, metadata: &SessionMetadata) -> Result<bool> {
        let connection = self.connect()?;
        let existed = connection
            .query_row(
                "SELECT 1 FROM sessions WHERE thread_id = ?1",
                [metadata.thread_id.as_str()],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        let now = unix_now();
        connection.execute(
            r#"
            INSERT INTO sessions (
                thread_id, resume_command, cwd, source, model, model_provider,
                created_at, updated_at, recency_at, generated_title, last_synced_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
            ON CONFLICT(thread_id) DO UPDATE SET
                resume_command = excluded.resume_command,
                cwd = excluded.cwd,
                source = excluded.source,
                model = COALESCE(excluded.model, sessions.model),
                model_provider = COALESCE(excluded.model_provider, sessions.model_provider),
                created_at = COALESCE(excluded.created_at, sessions.created_at),
                updated_at = COALESCE(excluded.updated_at, sessions.updated_at),
                recency_at = COALESCE(excluded.recency_at, sessions.recency_at),
                generated_title = COALESCE(NULLIF(excluded.generated_title, ''), sessions.generated_title),
                last_synced_at = excluded.last_synced_at
            "#,
            params![
                metadata.thread_id,
                metadata.resume_command(),
                metadata.cwd,
                metadata.source,
                metadata.model,
                metadata.model_provider,
                metadata.created_at,
                metadata.updated_at,
                metadata.recency_at,
                metadata.generated_title,
                now,
            ],
        )?;
        Ok(!existed)
    }

    pub fn set_generated_summary(
        &self,
        thread_id: &str,
        title: &str,
        summary: &str,
        provider: SummaryProvider,
        cap: SummaryInput,
    ) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            r#"
            UPDATE sessions SET
                generated_title = ?2,
                generated_summary = ?3,
                summary_provider = ?4,
                summary_cap = ?5,
                summary_status = 'ready',
                summary_error = NULL,
                last_synced_at = ?6
            WHERE thread_id = ?1
            "#,
            params![
                thread_id,
                title,
                summary,
                provider.to_string(),
                cap.to_string(),
                unix_now(),
            ],
        )?;
        Ok(())
    }

    pub fn mark_summary_pending(&self, thread_id: &str) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE sessions SET summary_status = 'pending', summary_error = NULL WHERE thread_id = ?1",
            [thread_id],
        )?;
        Ok(())
    }

    pub fn mark_summary_error(&self, thread_id: &str, error: &str) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE sessions SET summary_status = 'error', summary_error = ?2 WHERE thread_id = ?1",
            params![thread_id, error],
        )?;
        Ok(())
    }

    pub fn mark_not_requested(&self, thread_id: &str) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE sessions SET summary_status = 'not_requested', summary_error = NULL WHERE thread_id = ?1",
            [thread_id],
        )?;
        Ok(())
    }

    pub fn set_manual_title(&self, thread_id: &str, value: Option<&str>) -> Result<()> {
        self.set_optional_field("manual_title", thread_id, value)
    }

    pub fn set_manual_summary(&self, thread_id: &str, value: Option<&str>) -> Result<()> {
        self.set_optional_field("manual_summary", thread_id, value)
    }

    fn set_optional_field(&self, field: &str, thread_id: &str, value: Option<&str>) -> Result<()> {
        debug_assert!(matches!(field, "manual_title" | "manual_summary"));
        let connection = self.connect()?;
        let sql = format!("UPDATE sessions SET {field} = ?2 WHERE thread_id = ?1");
        connection.execute(
            &sql,
            params![thread_id, value.filter(|item| !item.trim().is_empty())],
        )?;
        Ok(())
    }

    pub fn set_notes(&self, thread_id: &str, notes: &str) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE sessions SET notes = ?2 WHERE thread_id = ?1",
            params![thread_id, notes],
        )?;
        Ok(())
    }

    pub fn toggle_pin(&self, thread_id: &str) -> Result<bool> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE sessions SET pinned = CASE pinned WHEN 0 THEN 1 ELSE 0 END WHERE thread_id = ?1",
            [thread_id],
        )?;
        let pinned = connection.query_row(
            "SELECT pinned FROM sessions WHERE thread_id = ?1",
            [thread_id],
            |row| row.get::<_, bool>(0),
        )?;
        Ok(pinned)
    }

    pub fn set_tags(&self, thread_id: &str, tags: &[String]) -> Result<()> {
        let mut normalized = Vec::new();
        for tag in tags {
            let tag = tag.split_whitespace().collect::<Vec<_>>().join(" ");
            if !tag.is_empty()
                && !normalized
                    .iter()
                    .any(|existing: &String| existing.eq_ignore_ascii_case(&tag))
            {
                normalized.push(tag);
            }
        }

        let mut connection = self.connect()?;
        let transaction = connection.transaction()?;
        transaction.execute("DELETE FROM session_tags WHERE thread_id = ?1", [thread_id])?;
        for tag in normalized {
            transaction.execute(
                "INSERT INTO tags(name) VALUES (?1) ON CONFLICT(name) DO NOTHING",
                [tag.as_str()],
            )?;
            let tag_id: i64 = transaction.query_row(
                "SELECT id FROM tags WHERE name = ?1 COLLATE NOCASE",
                [tag.as_str()],
                |row| row.get(0),
            )?;
            transaction.execute(
                "INSERT OR IGNORE INTO session_tags(thread_id, tag_id) VALUES (?1, ?2)",
                params![thread_id, tag_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn list_sessions(&self) -> Result<Vec<Session>> {
        let connection = self.connect()?;
        let mut statement = connection.prepare(
            r#"
            SELECT thread_id, resume_command, cwd, source, model, model_provider,
                   created_at, updated_at, recency_at, ended_at, generated_title,
                   generated_summary, manual_title, manual_summary, notes, pinned,
                   summary_provider, summary_cap, summary_status, summary_error
            FROM sessions
            ORDER BY pinned DESC,
                     COALESCE(recency_at, updated_at, ended_at, created_at, 0) DESC,
                     thread_id ASC
            "#,
        )?;
        let mut sessions = statement
            .query_map([], session_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for session in &mut sessions {
            session.tags = load_tags(&connection, &session.thread_id)?;
        }
        Ok(sessions)
    }

    pub fn get_session(&self, thread_id: &str) -> Result<Option<Session>> {
        let connection = self.connect()?;
        let mut session = connection
            .query_row(
                r#"
                SELECT thread_id, resume_command, cwd, source, model, model_provider,
                       created_at, updated_at, recency_at, ended_at, generated_title,
                       generated_summary, manual_title, manual_summary, notes, pinned,
                       summary_provider, summary_cap, summary_status, summary_error
                FROM sessions WHERE thread_id = ?1
                "#,
                [thread_id],
                session_from_row,
            )
            .optional()?;
        if let Some(value) = &mut session {
            value.tags = load_tags(&connection, thread_id)?;
        }
        Ok(session)
    }

    pub fn count_sessions(&self) -> Result<usize> {
        let connection = self.connect()?;
        let count: i64 =
            connection.query_row("SELECT COUNT(*) FROM sessions", [], |row| row.get(0))?;
        Ok(count as usize)
    }

    pub fn queue_job(&self, thread_id: &str, provider: &str, cap: SummaryInput) -> Result<()> {
        let connection = self.connect()?;
        let now = unix_now();
        connection.execute(
            r#"
            INSERT INTO jobs(thread_id, provider, cap, status, attempts, created_at, updated_at)
            VALUES (?1, ?2, ?3, 'queued', 0, ?4, ?4)
            ON CONFLICT(thread_id) DO UPDATE SET
                provider = excluded.provider,
                cap = excluded.cap,
                status = 'queued',
                last_error = NULL,
                updated_at = excluded.updated_at
            "#,
            params![thread_id, provider, cap.to_string(), now],
        )?;
        Ok(())
    }

    pub fn get_job(&self, thread_id: &str) -> Result<Option<Job>> {
        let connection = self.connect()?;
        connection
            .query_row(
                "SELECT thread_id, provider, cap, status, attempts, last_error FROM jobs WHERE thread_id = ?1",
                [thread_id],
                |row| {
                    Ok(Job {
                        thread_id: row.get(0)?,
                        provider: row.get(1)?,
                        cap: row.get(2)?,
                        status: row.get(3)?,
                        attempts: row.get(4)?,
                        last_error: row.get(5)?,
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    pub fn mark_job_running(&self, thread_id: &str) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE jobs SET status = 'running', attempts = attempts + 1, updated_at = ?2 WHERE thread_id = ?1",
            params![thread_id, unix_now()],
        )?;
        Ok(())
    }

    pub fn mark_job_done(&self, thread_id: &str) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE jobs SET status = 'done', last_error = NULL, updated_at = ?2 WHERE thread_id = ?1",
            params![thread_id, unix_now()],
        )?;
        Ok(())
    }

    pub fn mark_job_error(&self, thread_id: &str, error: &str) -> Result<()> {
        let connection = self.connect()?;
        connection.execute(
            "UPDATE jobs SET status = 'error', last_error = ?2, updated_at = ?3 WHERE thread_id = ?1",
            params![thread_id, error, unix_now()],
        )?;
        Ok(())
    }

    pub fn integrity_check(&self) -> Result<String> {
        let connection = self.connect()?;
        connection
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .map_err(Into::into)
    }
}

fn enable_wal(connection: &Connection) -> Result<()> {
    for attempt in 0..=100 {
        match connection.query_row("PRAGMA journal_mode = WAL", [], |row| {
            row.get::<_, String>(0)
        }) {
            Ok(mode) if mode.eq_ignore_ascii_case("wal") => return Ok(()),
            Ok(mode) => anyhow::bail!("SQLite refused WAL journal mode and returned {mode:?}"),
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked
                ) && attempt < 100 =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
    unreachable!("WAL retry loop always returns on its final iteration")
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        thread_id: row.get(0)?,
        resume_command: row.get(1)?,
        cwd: row.get(2)?,
        source: row.get(3)?,
        model: row.get(4)?,
        model_provider: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        recency_at: row.get(8)?,
        ended_at: row.get(9)?,
        generated_title: row.get(10)?,
        generated_summary: row.get(11)?,
        manual_title: row.get(12)?,
        manual_summary: row.get(13)?,
        notes: row.get(14)?,
        pinned: row.get(15)?,
        summary_provider: row.get(16)?,
        summary_cap: row.get(17)?,
        summary_status: row.get(18)?,
        summary_error: row.get(19)?,
        tags: Vec::new(),
    })
}

fn load_tags(connection: &Connection, thread_id: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        r#"
        SELECT tags.name FROM tags
        JOIN session_tags ON session_tags.tag_id = tags.id
        WHERE session_tags.thread_id = ?1
        ORDER BY tags.name COLLATE NOCASE
        "#,
    )?;
    let tags = statement
        .query_map([thread_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(tags)
}

pub fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn database_path(name: &str) -> PathBuf {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("TMP/tests")
            .join(format!("{name}-{}-{serial}", std::process::id()));
        std::fs::create_dir_all(&root).expect("create test artifact directory");
        root.join("tracker.sqlite3")
    }

    fn metadata(id: &str, recency: i64) -> SessionMetadata {
        SessionMetadata {
            thread_id: id.into(),
            cwd: "/workspace/project".into(),
            source: "cli".into(),
            model: Some("gpt-5".into()),
            model_provider: Some("openai".into()),
            created_at: Some(recency - 10),
            updated_at: Some(recency),
            recency_at: Some(recency),
            generated_title: Some(format!("Thread {id}")),
        }
    }

    #[test]
    fn migration_and_metadata_round_trip() {
        let database = Database::open(database_path("migration")).expect("open");
        assert!(database.upsert_metadata(&metadata("thr_1", 20)).unwrap());
        assert!(!database.upsert_metadata(&metadata("thr_1", 30)).unwrap());
        database
            .set_generated_summary(
                "thr_1",
                "Generated title",
                "Generated summary",
                SummaryProvider::Local,
                SummaryInput::SixtyFourK,
            )
            .unwrap();
        database
            .set_tags("thr_1", &["rust".into(), " Codex  tools ".into()])
            .unwrap();
        let session = database.get_session("thr_1").unwrap().unwrap();
        assert_eq!(session.resume_command, "codex resume thr_1");
        assert_eq!(session.summary(), "Generated summary");
        assert_eq!(session.tags, vec!["Codex tools", "rust"]);
        assert_eq!(database.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn manual_overrides_do_not_destroy_generated_text() {
        let database = Database::open(database_path("overrides")).unwrap();
        database.upsert_metadata(&metadata("thr_2", 20)).unwrap();
        database
            .set_generated_summary(
                "thr_2",
                "Generated",
                "Generated summary",
                SummaryProvider::Local,
                SummaryInput::SixteenK,
            )
            .unwrap();
        database
            .set_manual_title("thr_2", Some("My title"))
            .unwrap();
        assert_eq!(
            database.get_session("thr_2").unwrap().unwrap().title(),
            "My title"
        );
        database.set_manual_title("thr_2", None).unwrap();
        assert_eq!(
            database.get_session("thr_2").unwrap().unwrap().title(),
            "Generated"
        );
    }

    #[test]
    fn pinned_sessions_sort_before_newer_sessions() {
        let database = Database::open(database_path("sorting")).unwrap();
        database.upsert_metadata(&metadata("old", 10)).unwrap();
        database.upsert_metadata(&metadata("new", 100)).unwrap();
        database.toggle_pin("old").unwrap();
        let sessions = database.list_sessions().unwrap();
        assert_eq!(sessions[0].thread_id, "old");
        assert_eq!(sessions[1].thread_id, "new");
    }

    #[test]
    fn concurrent_migrations_and_upserts_are_serialized() {
        let path = database_path("concurrency");
        let barrier = Arc::new(Barrier::new(6));
        let mut threads = Vec::new();
        for worker in 0..6 {
            let path = path.clone();
            let barrier = Arc::clone(&barrier);
            threads.push(std::thread::spawn(move || {
                barrier.wait();
                let database = Database::open(path).expect("concurrent migration");
                for item in 0..12 {
                    let id = format!("thread-{worker}-{item}");
                    database
                        .upsert_metadata(&metadata(&id, worker * 100 + item))
                        .expect("concurrent upsert");
                }
            }));
        }
        for thread in threads {
            thread.join().expect("worker join");
        }
        let database = Database::open(path).unwrap();
        assert_eq!(database.count_sessions().unwrap(), 72);
        assert_eq!(database.integrity_check().unwrap(), "ok");
    }

    #[test]
    fn hook_ingestion_is_idempotent_and_queues_jobs() {
        let database = Database::open(database_path("hook")).unwrap();
        database.ingest_hook("thr_hook", "/one", 10).unwrap();
        database.ingest_hook("thr_hook", "/two", 20).unwrap();
        database
            .queue_job("thr_hook", "codex", SummaryInput::SixtyFourK)
            .unwrap();
        database
            .queue_job("thr_hook", "local", SummaryInput::SixteenK)
            .unwrap();
        assert_eq!(database.count_sessions().unwrap(), 1);
        let session = database.get_session("thr_hook").unwrap().unwrap();
        assert_eq!(session.cwd, "/two");
        let job = database.get_job("thr_hook").unwrap().unwrap();
        assert_eq!(job.provider, "local");
        assert_eq!(job.status, "queued");
    }
}
