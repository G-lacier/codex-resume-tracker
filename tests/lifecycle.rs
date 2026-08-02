#![cfg(unix)]

use std::collections::HashMap;
use std::fs;
use std::io::Cursor;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use codex_resume_tracker::app_server::{
    MessageRole, RemoteThread, ThreadRepository, VisibleMessage,
};
use codex_resume_tracker::config::{Config, OnClose, SourceKind, SummaryProvider};
use codex_resume_tracker::db::{Database, SessionMetadata};
use codex_resume_tracker::hook::{self, WorkerSpawner};
use codex_resume_tracker::paths::AppPaths;
use codex_resume_tracker::sync;
use codex_resume_tracker::terminal;
use codex_resume_tracker::tui::TrackerApp;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn artifact_dir(name: &str) -> PathBuf {
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("TMP/tests")
        .join(format!("lifecycle-{name}-{}-{serial}", std::process::id()));
    fs::create_dir_all(&root).expect("create lifecycle artifact directory");
    root
}

#[derive(Clone)]
struct FixtureRepository {
    threads: Vec<RemoteThread>,
    messages: HashMap<String, Vec<VisibleMessage>>,
}

impl ThreadRepository for FixtureRepository {
    fn list_threads(&mut self, sources: &[SourceKind]) -> Result<Vec<RemoteThread>> {
        assert_eq!(sources, &[SourceKind::Cli, SourceKind::Vscode]);
        Ok(self.threads.clone())
    }

    fn read_messages(&mut self, thread_id: &str) -> Result<Vec<VisibleMessage>> {
        self.messages
            .get(thread_id)
            .cloned()
            .with_context(|| format!("fixture thread {thread_id} not found"))
    }
}

struct CaptureSpawner(Mutex<Vec<String>>);

impl WorkerSpawner for CaptureSpawner {
    fn spawn(&self, thread_id: &str) -> Result<()> {
        self.0.lock().unwrap().push(thread_id.to_owned());
        Ok(())
    }
}

fn remote(id: &str, cwd: &str, title: &str, recency: i64) -> RemoteThread {
    RemoteThread {
        metadata: SessionMetadata {
            thread_id: id.into(),
            cwd: cwd.into(),
            source: "cli".into(),
            model: Some("gpt-5".into()),
            model_provider: Some("openai".into()),
            created_at: Some(recency - 10),
            updated_at: Some(recency),
            recency_at: Some(recency),
            generated_title: Some(title.into()),
        },
    }
}

fn visible(role: MessageRole, text: &str) -> VisibleMessage {
    VisibleMessage {
        role,
        text: text.into(),
    }
}

#[test]
fn complete_local_fixture_lifecycle_import_hook_enrich_search_edit_and_launch() {
    let root = artifact_dir("complete");
    let paths = AppPaths::from_roots(root.join("data"), root.join("config"), root.join("state"));
    let config = Config {
        summary_provider: SummaryProvider::Codex,
        on_close: OnClose::LocalFirst,
        ..Config::default()
    };
    config.save(&paths).unwrap();
    let database = Database::open(paths.database()).unwrap();

    let imported = remote("thr_imported", "/work/imported", "Import preview", 20);
    let hooked = remote("thr_hooked", "/work/hooked", "Hook preview", 40);
    let mut repository = FixtureRepository {
        threads: vec![hooked.clone(), imported.clone()],
        messages: HashMap::from([
            (
                "thr_imported".into(),
                vec![
                    visible(MessageRole::User, "Import request"),
                    visible(MessageRole::Assistant, "TRANSIENT_MIDDLE_SECRET"),
                    visible(MessageRole::Assistant, "Import complete"),
                ],
            ),
            (
                "thr_hooked".into(),
                vec![
                    visible(MessageRole::User, "Hook request"),
                    visible(MessageRole::Assistant, "Hook complete"),
                ],
            ),
        ]),
    };

    // Initial import is local even though the saved provider is Codex.
    repository.threads = vec![imported];
    let report = sync::sync_all(&database, &mut repository, &config, &paths).unwrap();
    assert_eq!(report.imported, 1);
    assert_eq!(report.summarized, 1);
    assert_eq!(
        database
            .get_session("thr_imported")
            .unwrap()
            .unwrap()
            .summary_provider
            .as_deref(),
        Some("local")
    );

    // SessionEnd performs an idempotent upsert and queues a detached worker request.
    let spawner = CaptureSpawner(Mutex::new(Vec::new()));
    let event = Cursor::new(
        r#"{"session_id":"thr_hooked","transcript_path":"/never/store/raw.jsonl","cwd":"/work/hooked","hook_event_name":"SessionEnd","reason":"other"}"#,
    );
    hook::ingest(event, &paths, &spawner).unwrap();
    assert_eq!(spawner.0.lock().unwrap().as_slice(), ["thr_hooked"]);

    // The worker enriches through the repository and honors local-first.
    repository.threads = vec![hooked];
    hook::process_job(&database, &mut repository, &config, &paths, "thr_hooked").unwrap();
    let hooked_session = database.get_session("thr_hooked").unwrap().unwrap();
    assert_eq!(hooked_session.title(), "Hook request");
    assert_eq!(hooked_session.summary(), "Hook complete");
    assert_eq!(hooked_session.resume_command, "codex resume thr_hooked");

    // Metadata editing and the same incremental search model used by the TUI.
    database
        .set_manual_title("thr_hooked", Some("Pinned Rust follow-up"))
        .unwrap();
    database
        .set_manual_summary("thr_hooked", Some("Resume the implementation work"))
        .unwrap();
    database
        .set_notes("thr_hooked", "Review before resuming")
        .unwrap();
    database
        .set_tags("thr_hooked", &["rust".into(), "release".into()])
        .unwrap();
    database.toggle_pin("thr_hooked").unwrap();
    let sessions = database.list_sessions().unwrap();
    assert_eq!(sessions[0].thread_id, "thr_hooked");
    let mut app = TrackerApp::new(sessions, &config);
    app.query = "release".into();
    app.refresh_filter();
    assert_eq!(app.filtered.len(), 1);
    assert_eq!(app.selected_session().unwrap().thread_id, "thr_hooked");

    // A fake terminal captures the exact argv; values are never shell-interpolated.
    let capture = root.join("terminal-argv.txt");
    let fake_terminal = root.join("fake-terminal");
    fs::write(
        &fake_terminal,
        format!(
            "#!/bin/sh\nset -eu\nprintf '%s\\n' \"$@\" > '{}'\n",
            capture.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&fake_terminal).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_terminal, permissions).unwrap();
    let template = vec![
        fake_terminal.to_string_lossy().into_owned(),
        "-e".into(),
        "codex".into(),
        "-C".into(),
        "{cwd}".into(),
        "resume".into(),
        "{thread_id}".into(),
    ];
    terminal::launch(&template, &hooked_session.cwd, &hooked_session.thread_id).unwrap();
    for _ in 0..50 {
        if capture.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        fs::read_to_string(&capture).unwrap(),
        "-e\ncodex\n-C\n/work/hooked\nresume\nthr_hooked\n"
    );

    // Only normalized metadata/results are durable; full transient message text and paths are not.
    for entry in fs::read_dir(&paths.data_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            let bytes = fs::read(path).unwrap();
            assert!(!bytes
                .windows("TRANSIENT_MIDDLE_SECRET".len())
                .any(|window| window == b"TRANSIENT_MIDDLE_SECRET"));
            assert!(!bytes
                .windows("/never/store/raw.jsonl".len())
                .any(|window| window == b"/never/store/raw.jsonl"));
        }
    }
}
