#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use codex_resume_tracker::app_server::{
    CodexAppServer, MessageRole, ThreadRepository, VisibleMessage,
};
use codex_resume_tracker::config::{SourceKind, SummaryProvider};

static NEXT: AtomicU64 = AtomicU64::new(0);

fn artifact_dir(name: &str) -> PathBuf {
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("TMP/tests")
        .join(format!(
            "app-server-process-{name}-{}-{serial}",
            std::process::id()
        ));
    fs::create_dir_all(&root).expect("create integration artifact directory");
    root
}

#[test]
fn stdio_client_performs_handshake_pagination_and_thread_read() {
    let root = artifact_dir("protocol");
    let fake_codex = root.join("fake-codex");
    fs::write(
        &fake_codex,
        r###"#!/bin/sh
set -eu
test "$1" = "app-server"
IFS= read -r initialize
printf '%s\n' '{"id":1,"result":{"userAgent":"fake","codexHome":"/fake","platformFamily":"unix","platformOs":"linux"}}'
IFS= read -r initialized
IFS= read -r first_list
printf '%s\n' '{"id":2,"result":{"data":[{"id":"thr_page_1","cwd":"/repo/one","source":"cli","modelProvider":"openai","createdAt":10,"updatedAt":20,"recencyAt":20,"preview":"First page"}],"nextCursor":"page-two"}}'
IFS= read -r second_list
printf '%s\n' '{"id":3,"result":{"data":[{"id":"thr_page_2","cwd":"/repo/two","source":"vscode","modelProvider":"openai","createdAt":30,"updatedAt":40,"recencyAt":40,"preview":"Second page"}],"nextCursor":null}}'
IFS= read -r read_thread
printf '%s\n' '{"id":4,"result":{"thread":{"id":"thr_page_2","turns":[{"id":"turn_1","status":"completed","items":[{"type":"userMessage","id":"u","content":[{"type":"text","text":"visible request"}]},{"type":"reasoning","id":"r","summary":["must not leak"]},{"type":"commandExecution","id":"c","aggregatedOutput":"must not leak either"},{"type":"agentMessage","id":"a","text":"visible answer"}]}]}}}'
"###,
    )
    .expect("write fake app server");
    let mut permissions = fs::metadata(&fake_codex).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&fake_codex, permissions).unwrap();

    let mut client = CodexAppServer::connect_with_program(fake_codex.to_str().unwrap()).unwrap();
    let threads = client
        .list_threads(&[SourceKind::Cli, SourceKind::Vscode])
        .unwrap();
    assert_eq!(threads.len(), 2);
    assert_eq!(threads[0].metadata.thread_id, "thr_page_1");
    assert_eq!(
        threads[1].metadata.generated_title.as_deref(),
        Some("Second page")
    );
    let messages = client.read_messages("thr_page_2").unwrap();
    assert_eq!(
        messages,
        vec![
            VisibleMessage {
                role: MessageRole::User,
                text: "visible request".into(),
            },
            VisibleMessage {
                role: MessageRole::Assistant,
                text: "visible answer".into(),
            },
        ]
    );
    assert_eq!(SummaryProvider::Local.to_string(), "local");
}
