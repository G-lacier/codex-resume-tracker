use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::app_server::{RemoteThread, ThreadRepository, VisibleMessage};
use crate::config::{Config, SummaryInput, SummaryProvider};
use crate::db::Database;
use crate::paths::AppPaths;
use crate::summary;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SyncReport {
    pub imported: usize,
    pub updated: usize,
    pub summarized: usize,
    pub errors: Vec<String>,
}

impl SyncReport {
    pub fn total(&self) -> usize {
        self.imported + self.updated
    }
}

pub fn sync_all<R: ThreadRepository>(
    database: &Database,
    repository: &mut R,
    config: &Config,
    paths: &AppPaths,
) -> Result<SyncReport> {
    let threads = repository.list_threads(&config.sources)?;
    let mut report = SyncReport::default();
    for thread in threads {
        match database.upsert_metadata(&thread.metadata) {
            Ok(true) => report.imported += 1,
            Ok(false) => report.updated += 1,
            Err(error) => {
                report.errors.push(format!(
                    "{}: persist metadata: {error:#}",
                    thread.metadata.thread_id
                ));
                continue;
            }
        }
        match repository
            .read_messages(&thread.metadata.thread_id)
            .and_then(|messages| {
                let result = summary::generate(
                    SummaryProvider::Local,
                    config.summary_input,
                    &messages,
                    Path::new(&thread.metadata.cwd),
                    config,
                    paths,
                )?;
                database.set_generated_summary(
                    &thread.metadata.thread_id,
                    &result.title,
                    &result.summary,
                    SummaryProvider::Local,
                    config.summary_input,
                )
            }) {
            Ok(()) => report.summarized += 1,
            Err(error) => {
                let message = format!("{}: {error:#}", thread.metadata.thread_id);
                let _ = database.mark_summary_error(&thread.metadata.thread_id, &message);
                report.errors.push(message);
            }
        }
    }
    Ok(report)
}

pub fn find_remote_thread<R: ThreadRepository>(
    repository: &mut R,
    config: &Config,
    thread_id: &str,
) -> Result<RemoteThread> {
    repository
        .list_threads(&config.sources)?
        .into_iter()
        .find(|thread| thread.metadata.thread_id == thread_id)
        .with_context(|| {
            format!("thread {thread_id} was not returned by the configured source filters")
        })
}

pub fn enrich_metadata<R: ThreadRepository>(
    database: &Database,
    repository: &mut R,
    config: &Config,
    thread_id: &str,
) -> Result<RemoteThread> {
    let thread = find_remote_thread(repository, config, thread_id)?;
    database.upsert_metadata(&thread.metadata)?;
    Ok(thread)
}

pub fn summarize_one<R: ThreadRepository>(
    database: &Database,
    repository: &mut R,
    config: &Config,
    paths: &AppPaths,
    thread_id: &str,
    provider: SummaryProvider,
    cap: SummaryInput,
) -> Result<()> {
    database.mark_summary_pending(thread_id)?;
    let outcome = (|| {
        let thread = enrich_metadata(database, repository, config, thread_id)?;
        let messages = repository.read_messages(thread_id)?;
        summarize_messages(database, config, paths, &thread, &messages, provider, cap)
    })();
    if let Err(error) = &outcome {
        database.mark_summary_error(thread_id, &format!("{error:#}"))?;
    }
    outcome
}

pub fn summarize_messages(
    database: &Database,
    config: &Config,
    paths: &AppPaths,
    thread: &RemoteThread,
    messages: &[VisibleMessage],
    provider: SummaryProvider,
    cap: SummaryInput,
) -> Result<()> {
    let result = summary::generate(
        provider,
        cap,
        messages,
        Path::new(&thread.metadata.cwd),
        config,
        paths,
    )?;
    database.set_generated_summary(
        &thread.metadata.thread_id,
        &result.title,
        &result.summary,
        provider,
        cap,
    )
}

pub fn summarize_all<R: ThreadRepository>(
    database: &Database,
    repository: &mut R,
    config: &Config,
    paths: &AppPaths,
    provider: SummaryProvider,
    cap: SummaryInput,
) -> Result<SyncReport> {
    let threads = repository.list_threads(&config.sources)?;
    if threads.is_empty() {
        bail!("no matching non-archived threads were returned by Codex");
    }
    let mut report = SyncReport::default();
    for thread in threads {
        match database.upsert_metadata(&thread.metadata) {
            Ok(true) => report.imported += 1,
            Ok(false) => report.updated += 1,
            Err(error) => {
                report.errors.push(format!(
                    "{}: persist metadata: {error:#}",
                    thread.metadata.thread_id
                ));
                continue;
            }
        }
        let thread_id = thread.metadata.thread_id.clone();
        let outcome = repository.read_messages(&thread_id).and_then(|messages| {
            summarize_messages(database, config, paths, &thread, &messages, provider, cap)
        });
        match outcome {
            Ok(()) => report.summarized += 1,
            Err(error) => {
                let message = format!("{thread_id}: {error:#}");
                let _ = database.mark_summary_error(&thread_id, &message);
                report.errors.push(message);
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::app_server::{MessageRole, VisibleMessage};
    use crate::config::SourceKind;
    use crate::db::SessionMetadata;

    use super::*;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    struct FakeRepository {
        threads: Vec<RemoteThread>,
        messages: HashMap<String, Vec<VisibleMessage>>,
        seen_sources: Vec<SourceKind>,
    }

    impl ThreadRepository for FakeRepository {
        fn list_threads(&mut self, sources: &[SourceKind]) -> Result<Vec<RemoteThread>> {
            self.seen_sources = sources.to_vec();
            Ok(self.threads.clone())
        }

        fn read_messages(&mut self, thread_id: &str) -> Result<Vec<VisibleMessage>> {
            self.messages
                .get(thread_id)
                .cloned()
                .with_context(|| format!("missing fixture {thread_id}"))
        }
    }

    fn roots(name: &str) -> (Database, AppPaths) {
        let serial = NEXT.fetch_add(1, Ordering::Relaxed);
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("TMP/tests")
            .join(format!("sync-{name}-{}-{serial}", std::process::id()));
        let paths =
            AppPaths::from_roots(root.join("data"), root.join("config"), root.join("state"));
        let database = Database::open(paths.database()).unwrap();
        (database, paths)
    }

    fn repository() -> FakeRepository {
        let thread = RemoteThread {
            metadata: SessionMetadata {
                thread_id: "thr_sync".into(),
                cwd: "/repo".into(),
                source: "cli".into(),
                model: None,
                model_provider: Some("openai".into()),
                created_at: Some(1),
                updated_at: Some(2),
                recency_at: Some(2),
                generated_title: Some("Preview title".into()),
            },
        };
        FakeRepository {
            threads: vec![thread],
            messages: HashMap::from([(
                "thr_sync".into(),
                vec![
                    VisibleMessage {
                        role: MessageRole::User,
                        text: "First request".into(),
                    },
                    VisibleMessage {
                        role: MessageRole::Assistant,
                        text: "Final response".into(),
                    },
                ],
            )]),
            seen_sources: Vec::new(),
        }
    }

    #[test]
    fn initial_sync_uses_local_extraction_without_external_provider() {
        let (database, paths) = roots("initial");
        let mut repository = repository();
        let config = Config {
            summary_provider: SummaryProvider::Openai,
            ..Config::default()
        };
        let report = sync_all(&database, &mut repository, &config, &paths).unwrap();
        assert_eq!(report.imported, 1);
        assert_eq!(report.summarized, 1);
        assert!(report.errors.is_empty());
        let session = database.get_session("thr_sync").unwrap().unwrap();
        assert_eq!(session.title(), "First request");
        assert_eq!(session.summary(), "Final response");
        assert_eq!(session.summary_provider.as_deref(), Some("local"));
        assert_eq!(
            repository.seen_sources,
            vec![SourceKind::Cli, SourceKind::Vscode]
        );
    }

    #[test]
    fn sync_surfaces_per_thread_read_errors_without_losing_metadata() {
        let (database, paths) = roots("error");
        let mut repository = repository();
        repository.messages.clear();
        let report = sync_all(&database, &mut repository, &Config::default(), &paths).unwrap();
        assert_eq!(report.total(), 1);
        assert_eq!(report.errors.len(), 1);
        let session = database.get_session("thr_sync").unwrap().unwrap();
        assert_eq!(session.title(), "Preview title");
        assert_eq!(session.summary_status, "error");
    }
}
