use std::fs::OpenOptions;
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::app_server::{CodexAppServer, ThreadRepository};
use crate::config::{Config, OnClose, SummaryInput, SummaryProvider};
use crate::db::{unix_now, Database};
use crate::paths::AppPaths;
use crate::sync::{enrich_metadata, summarize_messages};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
pub struct SessionEndInput {
    pub session_id: String,
    pub cwd: String,
    pub hook_event_name: String,
    #[serde(default)]
    pub reason: String,
    #[serde(default, rename = "transcript_path")]
    _transcript_path: Option<String>,
}

pub trait WorkerSpawner {
    fn spawn(&self, thread_id: &str) -> Result<()>;
}

pub struct DetachedWorkerSpawner<'a> {
    pub executable: &'a Path,
    pub log_path: &'a Path,
}

impl WorkerSpawner for DetachedWorkerSpawner<'_> {
    fn spawn(&self, thread_id: &str) -> Result<()> {
        if let Some(parent) = self.log_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create worker log directory {}", parent.display()))?;
        }
        let stdout = OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path)
            .with_context(|| format!("open worker log {}", self.log_path.display()))?;
        let stderr = stdout.try_clone()?;
        Command::new("setsid")
            .arg(self.executable)
            .args(worker_command_args(thread_id))
            .stdin(Stdio::null())
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .context("start detached enrichment worker with setsid")?;
        Ok(())
    }
}

pub fn worker_command_args(thread_id: &str) -> Vec<String> {
    vec!["worker".into(), "--thread-id".into(), thread_id.into()]
}

pub fn ingest<R: Read, S: WorkerSpawner>(
    mut input: R,
    paths: &AppPaths,
    spawner: &S,
) -> Result<String> {
    let mut body = String::new();
    input.read_to_string(&mut body)?;
    let event: SessionEndInput =
        serde_json::from_str(&body).context("parse SessionEnd hook input")?;
    if event.hook_event_name != "SessionEnd" {
        bail!(
            "expected SessionEnd hook input, received {:?}",
            event.hook_event_name
        );
    }
    if event.session_id.trim().is_empty() {
        bail!("SessionEnd session_id cannot be empty");
    }
    if event.cwd.trim().is_empty() {
        bail!("SessionEnd cwd cannot be empty");
    }

    paths.ensure_dirs()?;
    let config = Config::load(paths).context("load tracker config for SessionEnd hook")?;
    let database = Database::open(paths.database())?;
    database.ingest_hook(&event.session_id, &event.cwd, unix_now())?;
    let provider = match config.on_close {
        OnClose::Automatic => config.summary_provider.to_string(),
        OnClose::LocalFirst => SummaryProvider::Local.to_string(),
        OnClose::Manual => "metadata".to_owned(),
    };
    database.queue_job(&event.session_id, &provider, config.summary_input)?;
    spawner.spawn(&event.session_id)?;
    Ok(event.session_id)
}

pub fn run_hook(input: impl Read, paths: &AppPaths, executable: &Path) -> Result<String> {
    let spawner = DetachedWorkerSpawner {
        executable,
        log_path: &paths.worker_log(),
    };
    ingest(input, paths, &spawner)
}

pub fn run_worker(paths: &AppPaths, thread_id: &str) -> Result<()> {
    let config = Config::load(paths)?;
    let database = Database::open(paths.database())?;
    let mut repository = CodexAppServer::connect()?;
    process_job(&database, &mut repository, &config, paths, thread_id)
}

pub fn process_job<R: ThreadRepository>(
    database: &Database,
    repository: &mut R,
    config: &Config,
    paths: &AppPaths,
    thread_id: &str,
) -> Result<()> {
    let job = database
        .get_job(thread_id)?
        .with_context(|| format!("no queued enrichment job for {thread_id}"))?;
    database.mark_job_running(thread_id)?;
    let outcome = (|| {
        let thread = enrich_metadata(database, repository, config, thread_id)?;
        if job.provider == "metadata" {
            database.mark_not_requested(thread_id)?;
            return Ok(());
        }
        let provider: SummaryProvider = job.provider.parse()?;
        let cap: SummaryInput = job.cap.parse()?;
        database.mark_summary_pending(thread_id)?;
        let messages = repository.read_messages(thread_id)?;
        summarize_messages(database, config, paths, &thread, &messages, provider, cap)
    })();
    match outcome {
        Ok(()) => database.mark_job_done(thread_id),
        Err(error) => {
            let message = format!("{error:#}");
            database.mark_job_error(thread_id, &message)?;
            database.mark_summary_error(thread_id, &message)?;
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::io::Cursor;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;

    use anyhow::Context;

    use crate::app_server::{MessageRole, RemoteThread, VisibleMessage};
    use crate::db::SessionMetadata;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct CapturingSpawner(Mutex<Vec<String>>);

    impl WorkerSpawner for CapturingSpawner {
        fn spawn(&self, thread_id: &str) -> Result<()> {
            self.0.lock().unwrap().push(thread_id.to_owned());
            Ok(())
        }
    }

    struct FakeRepository {
        thread: RemoteThread,
        messages: HashMap<String, Vec<VisibleMessage>>,
    }

    impl ThreadRepository for FakeRepository {
        fn list_threads(
            &mut self,
            _sources: &[crate::config::SourceKind],
        ) -> Result<Vec<RemoteThread>> {
            Ok(vec![self.thread.clone()])
        }

        fn read_messages(&mut self, thread_id: &str) -> Result<Vec<VisibleMessage>> {
            self.messages
                .get(thread_id)
                .cloned()
                .context("fixture messages missing")
        }
    }

    fn fixture(name: &str) -> (AppPaths, Database) {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("TMP/tests")
            .join(format!("hook-{name}-{}-{serial}", std::process::id()));
        let paths =
            AppPaths::from_roots(root.join("data"), root.join("config"), root.join("state"));
        Config::default().save(&paths).unwrap();
        let database = Database::open(paths.database()).unwrap();
        (paths, database)
    }

    fn repository() -> FakeRepository {
        FakeRepository {
            thread: RemoteThread {
                metadata: SessionMetadata {
                    thread_id: "thr_hook".into(),
                    cwd: "/workspace".into(),
                    source: "cli".into(),
                    model: None,
                    model_provider: Some("openai".into()),
                    created_at: Some(1),
                    updated_at: Some(2),
                    recency_at: Some(2),
                    generated_title: Some("Preview".into()),
                },
            },
            messages: HashMap::from([(
                "thr_hook".into(),
                vec![
                    VisibleMessage {
                        role: MessageRole::User,
                        text: "Hook request".into(),
                    },
                    VisibleMessage {
                        role: MessageRole::Assistant,
                        text: "Hook result".into(),
                    },
                ],
            )]),
        }
    }

    #[test]
    fn hook_upserts_fast_record_and_queues_saved_provider() {
        let (paths, database) = fixture("ingest");
        let spawner = CapturingSpawner(Mutex::new(Vec::new()));
        let input = Cursor::new(
            r#"{"session_id":"thr_hook","transcript_path":"/private/transcript.jsonl","cwd":"/workspace","hook_event_name":"SessionEnd","reason":"other"}"#,
        );
        let id = ingest(input, &paths, &spawner).unwrap();
        assert_eq!(id, "thr_hook");
        assert_eq!(spawner.0.lock().unwrap().as_slice(), ["thr_hook"]);
        let session = database.get_session("thr_hook").unwrap().unwrap();
        assert_eq!(session.resume_command, "codex resume thr_hook");
        assert_eq!(session.cwd, "/workspace");
        assert!(!session.searchable_text().contains("transcript.jsonl"));
        let job = database.get_job("thr_hook").unwrap().unwrap();
        assert_eq!(job.provider, "codex");
        assert_eq!(job.status, "queued");
    }

    #[test]
    fn local_first_worker_enriches_and_generates_local_summary() {
        let (paths, database) = fixture("worker");
        let config = Config {
            on_close: OnClose::LocalFirst,
            ..Config::default()
        };
        config.save(&paths).unwrap();
        database.ingest_hook("thr_hook", "/workspace", 1).unwrap();
        database
            .queue_job("thr_hook", "local", SummaryInput::SixtyFourK)
            .unwrap();
        let mut repository = repository();
        process_job(&database, &mut repository, &config, &paths, "thr_hook").unwrap();
        let session = database.get_session("thr_hook").unwrap().unwrap();
        assert_eq!(session.title(), "Hook request");
        assert_eq!(session.summary(), "Hook result");
        assert_eq!(
            database.get_job("thr_hook").unwrap().unwrap().status,
            "done"
        );
    }

    #[test]
    fn manual_worker_only_enriches_metadata() {
        let (paths, database) = fixture("manual");
        let config = Config {
            on_close: OnClose::Manual,
            ..Config::default()
        };
        database.ingest_hook("thr_hook", "/workspace", 1).unwrap();
        database
            .queue_job("thr_hook", "metadata", SummaryInput::SixtyFourK)
            .unwrap();
        process_job(&database, &mut repository(), &config, &paths, "thr_hook").unwrap();
        let session = database.get_session("thr_hook").unwrap().unwrap();
        assert_eq!(session.title(), "Preview");
        assert_eq!(session.summary_status, "not_requested");
        assert!(session.generated_summary.is_none());
    }
}
