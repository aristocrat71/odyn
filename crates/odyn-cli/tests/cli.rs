//! End-to-end tests: the real binary against a mock provider on loopback.
//! Nothing here touches the network.

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use odyn_core::chat::Role;
use odyn_core::storage::{MemoryTier, Storage};
use serde_json::Value;

/// What the canned stream below adds up to.
const ANSWER: &str = "2 + 2 = 4";

/// A unique directory under the system temp dir, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new(label: &str) -> Self {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "odyn-cli-test-{}-{label}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        Self(dir)
    }

    fn config(&self) -> PathBuf {
        self.0.join("odyn.toml")
    }

    fn db(&self) -> PathBuf {
        self.0.join("odyn.db")
    }

    fn write_config(&self, text: &str) -> &Self {
        std::fs::write(self.config(), text).expect("write config");
        self
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Env goes through the child, never `set_var`, so tests stay parallel-safe.
fn odyn(dir: &TempDir, args: &[&str], stdin: Option<&str>) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_odyn"))
        .args(args)
        .env("ODYN_CONFIG", dir.config())
        .env("ODYN_DB", dir.db())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn odyn");
    {
        let mut pipe = child.stdin.take().expect("child stdin");
        if let Some(text) = stdin {
            pipe.write_all(text.as_bytes()).expect("write stdin");
        }
    }
    child.wait_with_output().expect("wait for odyn")
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn openai_config(addr: SocketAddr) -> String {
    format!(
        "default_provider = \"mock\"\n\
         [providers.mock]\n\
         kind = \"openai_compat\"\n\
         base_url = \"http://{addr}\"\n\
         default_model = \"mock-model\"\n"
    )
}

fn ollama_config(addr: SocketAddr) -> String {
    format!(
        "default_provider = \"ollama\"\n\
         [providers.ollama]\n\
         kind = \"ollama\"\n\
         base_url = \"http://{addr}\"\n"
    )
}

/// An OpenAI-compatible stream, split the way a server flushes it.
fn sse_frames() -> Vec<String> {
    vec![
        ": keep-alive\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"\"}}]}\n\n"
            .to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"2 + 2\"}}]}\n\n".to_string(),
        "data: {\"choices\":[{\"delta\":{\"content\":\" = 4\"}}]}\n\n".to_string(),
        concat!(
            r#"data: {"choices":[],"usage":{"prompt_tokens":11,"completion_tokens":5}}"#,
            "\n\ndata: [DONE]\n\n"
        )
        .to_string(),
    ]
}

/// One HTTP chunk per frame, so the client really sees those boundaries.
fn chunked_sse_response() -> Vec<Vec<u8>> {
    let mut pieces = vec![
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n"
            .to_vec(),
    ];
    for frame in sse_frames() {
        pieces.push(format!("{:x}\r\n{frame}\r\n", frame.len()).into_bytes());
    }
    pieces.push(b"0\r\n\r\n".to_vec());
    pieces
}

/// Answers `requests` requests with the canned stream, one at a time.
fn spawn_provider(requests: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
    let addr = listener.local_addr().expect("mock provider address");
    std::thread::spawn(move || {
        for _ in 0..requests {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = read_request(&mut stream);
            for piece in chunked_sse_response() {
                if stream.write_all(&piece).is_err() {
                    return;
                }
                let _ = stream.flush();
            }
            let _ = stream.shutdown(Shutdown::Write);
            // Drain before dropping so the client sees a clean FIN, not a reset.
            let _ = stream.read_to_end(&mut Vec::new());
        }
    });
    addr
}

/// Bind then drop: an address we know nothing is listening on.
fn dead_addr() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("address");
    drop(listener);
    addr
}

fn read_request(stream: &mut TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    // Byte at a time so we stop exactly at the header terminator.
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => head.push(byte[0]),
            _ => return String::from_utf8_lossy(&head).into_owned(),
        }
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let mut len = 0usize;
    for line in head.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(value) = lower.strip_prefix("content-length:") {
            if let Ok(parsed) = value.trim().parse::<usize>() {
                len = parsed;
            }
        }
    }
    let mut body = vec![0u8; len];
    if len > 0 && stream.read_exact(&mut body).is_err() {
        return head;
    }
    format!("{head}{}", String::from_utf8_lossy(&body))
}

fn events(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|err| panic!("{line:?}: {err}")))
        .collect()
}

#[test]
fn ask_streams_the_answer_from_stdin_to_stdout() {
    let dir = TempDir::new("ask");
    dir.write_config(&openai_config(spawn_provider(1)));

    let output = odyn(&dir, &["ask"], Some("2+2\n"));

    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{ANSWER}\n"));
}

#[test]
fn ask_json_emits_deltas_then_done() {
    let dir = TempDir::new("json");
    dir.write_config(&openai_config(spawn_provider(1)));

    let output = odyn(&dir, &["ask", "--json"], Some("2+2\n"));

    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let events = events(&output);
    let (done, deltas) = events.split_last().expect("at least one event");
    assert_eq!(done["type"], "done");
    assert_eq!(done["usage"]["input_tokens"], 11);
    assert_eq!(done["usage"]["output_tokens"], 5);
    assert!(!deltas.is_empty(), "{events:?}");
    let text: String = deltas
        .iter()
        .map(|event| {
            assert_eq!(event["type"], "delta");
            event["text"].as_str().expect("delta text")
        })
        .collect();
    assert_eq!(text, ANSWER);
}

#[test]
fn a_provider_that_is_down_exits_one() {
    let dir = TempDir::new("down");
    dir.write_config(&ollama_config(dead_addr()));

    let output = odyn(&dir, &["ask", "--model", "llama3.2:3b"], Some("2+2\n"));

    assert_eq!(code(&output), Some(1), "{}", stderr(&output));
    let stderr = stderr(&output);
    assert!(stderr.contains("not reachable"), "{stderr}");
    // anstream drops the styling when stderr is a pipe.
    assert!(!stderr.contains('\u{1b}'), "{stderr:?}");
    assert!(stdout(&output).is_empty(), "{}", stdout(&output));
}

#[test]
fn a_provider_that_is_down_ends_the_json_stream_with_an_error() {
    let dir = TempDir::new("down-json");
    dir.write_config(&ollama_config(dead_addr()));

    let output = odyn(
        &dir,
        &["ask", "--json", "--model", "llama3.2:3b"],
        Some("2+2\n"),
    );

    assert_eq!(code(&output), Some(1));
    let events = events(&output);
    let last = events.last().expect("an error event");
    assert_eq!(last["type"], "error");
    let message = last["message"].as_str().expect("error message");
    assert!(message.contains("not reachable"), "{message}");
}

#[test]
fn an_invalid_config_exits_two() {
    let dir = TempDir::new("badconfig");
    dir.write_config("default_provider = \"mock\"\n[providers.mock\n");

    let output = odyn(&dir, &["ask", "2+2"], None);

    assert_eq!(code(&output), Some(2));
    let stderr = stderr(&output);
    assert!(stderr.contains("invalid config"), "{stderr}");
    assert!(stderr.contains("line 2"), "{stderr}");
}

#[test]
fn an_unknown_provider_exits_two() {
    let dir = TempDir::new("unknown");
    dir.write_config(&ollama_config(dead_addr()));

    let output = odyn(&dir, &["ask", "--provider", "zen", "2+2"], None);

    assert_eq!(code(&output), Some(2));
    let stderr = stderr(&output);
    assert!(stderr.contains("no provider named `zen`"), "{stderr}");
}

#[test]
fn a_provider_without_a_default_model_says_what_to_pass() {
    let dir = TempDir::new("nomodel");
    dir.write_config(&ollama_config(dead_addr()));

    let output = odyn(&dir, &["ask", "2+2"], None);

    assert_eq!(code(&output), Some(2));
    let stderr = stderr(&output);
    assert!(stderr.contains("--model"), "{stderr}");
    assert!(stderr.contains("default_model"), "{stderr}");
}

#[test]
fn save_persists_the_conversation_and_both_messages() {
    let dir = TempDir::new("save");
    dir.write_config(&openai_config(spawn_provider(1)));

    let output = odyn(&dir, &["ask", "--save", "2+2"], None);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));

    let storage = Storage::open(dir.db()).expect("open the database");
    let conversations = storage.list_conversations().expect("list conversations");
    assert_eq!(conversations.len(), 1, "{conversations:?}");
    assert_eq!(conversations[0].title, "2+2");
    assert_eq!(conversations[0].provider, "mock");
    assert_eq!(conversations[0].model, "mock-model");

    let messages = storage
        .messages(conversations[0].id)
        .expect("list messages");
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert_eq!(messages[0].role, Role::User);
    assert_eq!(messages[0].content, "2+2");
    assert_eq!(messages[1].role, Role::Assistant);
    assert_eq!(messages[1].content, ANSWER);
    assert_eq!(messages[1].input_tokens, Some(11));
    assert_eq!(messages[1].output_tokens, Some(5));
}

#[test]
fn ask_is_ephemeral_without_save() {
    let dir = TempDir::new("ephemeral");
    dir.write_config(&openai_config(spawn_provider(1)));

    let output = odyn(&dir, &["ask", "2+2"], None);

    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{ANSWER}\n"));
    assert!(!dir.db().exists(), "the database must not be touched");
}

#[test]
fn config_path_prints_the_file_odyn_would_read() {
    let dir = TempDir::new("configpath");
    dir.write_config(&ollama_config(dead_addr()));

    let output = odyn(&dir, &["config", "path"], None);

    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), format!("{}\n", dir.config().display()));
}

#[test]
fn config_set_and_get_round_trip_through_the_binary() {
    let dir = TempDir::new("configedit");
    dir.write_config(&ollama_config(dead_addr()));

    let set = odyn(&dir, &["config", "set", "memory.episodic_top_k", "3"], None);
    assert_eq!(code(&set), Some(0), "{}", stderr(&set));
    assert!(stdout(&set).is_empty(), "{}", stdout(&set));

    let got = odyn(&dir, &["config", "get", "memory.episodic_top_k"], None);
    assert_eq!(code(&got), Some(0), "{}", stderr(&got));
    assert_eq!(stdout(&got), "3\n");
}

#[test]
fn a_config_key_that_cannot_be_read_or_written_exits_two() {
    let dir = TempDir::new("configbad");
    let written = ollama_config(dead_addr());
    dir.write_config(&written);

    let missing = odyn(&dir, &["config", "get", "memory.nope"], None);
    assert_eq!(code(&missing), Some(2));
    assert!(
        stderr(&missing).contains("memory.nope"),
        "{}",
        stderr(&missing)
    );

    let rejected = odyn(&dir, &["config", "set", "default_provider", "zen"], None);
    assert_eq!(code(&rejected), Some(2));
    let stderr = stderr(&rejected);
    assert!(stderr.contains("default_provider"), "{stderr}");
    assert_eq!(
        std::fs::read_to_string(dir.config()).expect("read config"),
        written
    );
}

#[test]
fn chat_streams_every_turn_into_one_conversation() {
    let dir = TempDir::new("chat");
    dir.write_config(&openai_config(spawn_provider(2)));

    let output = odyn(
        &dir,
        &["chat"],
        Some("2+2\n/model mock/other-model\nand again\n/quit\n"),
    );

    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    // No prompt is drawn when stdin is a pipe, so this is the whole transcript.
    assert_eq!(
        stdout(&output),
        format!("mock / mock-model\n{ANSWER}\n\nmock / other-model\n{ANSWER}\n\n")
    );

    let storage = Storage::open(dir.db()).expect("open the database");
    let conversations = storage.list_conversations().expect("list conversations");
    assert_eq!(conversations.len(), 1, "{conversations:?}");
    assert_eq!(conversations[0].title, "2+2");
    assert_eq!(conversations[0].model, "other-model");

    let messages = storage
        .messages(conversations[0].id)
        .expect("list messages");
    let turns: Vec<(Role, &str)> = messages
        .iter()
        .map(|message| (message.role, message.content.as_str()))
        .collect();
    assert_eq!(
        turns,
        vec![
            (Role::User, "2+2"),
            (Role::Assistant, ANSWER),
            (Role::User, "and again"),
            (Role::Assistant, ANSWER),
        ]
    );
}

/// Like `spawn_provider(1)`, but hands back what the model was actually sent.
fn spawn_capturing_provider() -> (SocketAddr, std::sync::mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock provider");
    let addr = listener.local_addr().expect("mock provider address");
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = sender.send(read_request(&mut stream));
        for piece in chunked_sse_response() {
            if stream.write_all(&piece).is_err() {
                return;
            }
            let _ = stream.flush();
        }
        let _ = stream.shutdown(Shutdown::Write);
        let _ = stream.read_to_end(&mut Vec::new());
    });
    (addr, receiver)
}

/// Core memories only, so the test never loads (or downloads) the embedder.
#[test]
fn ask_injects_core_memories_and_saving_records_the_injections() {
    let dir = TempDir::new("inject");
    let (addr, request) = spawn_capturing_provider();
    dir.write_config(&openai_config(addr));
    Storage::open(dir.db())
        .expect("seed the database")
        .add_memory(MemoryTier::Core, "likes botany", None)
        .expect("add a core memory");

    let output = odyn(&dir, &["ask", "--save", "--show-context", "2+2"], None);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));

    let request = request.recv().expect("the provider saw the request");
    assert!(
        request.contains(r#""role":"system""#),
        "no system message in: {request}"
    );
    assert!(
        request.contains(r"## Core profile\n- [c-01] likes botany"),
        "wrong context in: {request}"
    );

    let shown = stderr(&output);
    assert!(shown.contains("----- context -----"), "{shown}");
    assert!(shown.contains("## Core profile"), "{shown}");
    assert!(shown.contains("c-01 3"), "{shown}");

    let storage = Storage::open(dir.db()).expect("reopen the database");
    let conversations = storage.list_conversations().expect("list conversations");
    let messages = storage
        .messages(conversations[0].id)
        .expect("list messages");
    let injections: Vec<(Option<i64>, i64)> = storage
        .injections(conversations[0].id)
        .expect("list injections")
        .into_iter()
        .map(|injection| (injection.message_id, injection.memory_id))
        .collect();
    assert_eq!(injections, vec![(Some(messages[0].id), 1)]);
}

#[test]
fn show_context_is_empty_and_creates_no_database_on_a_fresh_machine() {
    let dir = TempDir::new("nocontext");
    dir.write_config(&openai_config(spawn_provider(1)));

    let output = odyn(&dir, &["ask", "--show-context", "2+2"], None);

    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert!(stderr(&output).contains("----- context: empty -----"));
    assert!(!dir.db().exists(), "the database must not be created");
}

#[test]
fn show_context_json_is_an_event_on_the_stream() {
    let dir = TempDir::new("jsoncontext");
    let (addr, _request) = spawn_capturing_provider();
    dir.write_config(&openai_config(addr));
    Storage::open(dir.db())
        .expect("seed the database")
        .add_memory(MemoryTier::Core, "likes botany", None)
        .expect("add a core memory");

    let output = odyn(&dir, &["ask", "--json", "--show-context", "2+2"], None);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));

    let events = events(&output);
    assert_eq!(events[0]["type"], "context");
    assert_eq!(
        events[0]["system"],
        "## Core profile\n- [c-01] likes botany"
    );
    assert_eq!(events[0]["items"][0]["id"], "c-01");
    assert_eq!(events[0]["items"][0]["tokens"], 3);
}

/// Core memories only: the episodic path needs the real embedding model.
#[test]
fn mem_core_add_list_edit_rm_round_trip() {
    let dir = TempDir::new("memcli");
    dir.write_config("default_provider = \"x\"\n[providers.x]\nkind = \"ollama\"\nbase_url = \"http://127.0.0.1:1\"\n");

    let output = odyn(
        &dir,
        &["mem", "add", "--core", "likes  botany\nand moss"],
        None,
    );
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "c-01  6 tk  likes  botany and moss\n");

    let output = odyn(&dir, &["mem", "list", "--tier", "core"], None);
    assert_eq!(stdout(&output), "c-01  6 tk  likes  botany and moss\n");
    let output = odyn(&dir, &["mem", "list", "--tier", "episodic"], None);
    assert_eq!(stdout(&output), "");

    let output = odyn(&dir, &["mem", "edit", "c-01", "prefers ferns"], None);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "c-01  4 tk  prefers ferns\n");

    let output = odyn(&dir, &["mem", "rm", "c-01"], None);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    let output = odyn(&dir, &["mem", "list"], None);
    assert_eq!(stdout(&output), "");

    let output = odyn(&dir, &["mem", "rm", "c-01"], None);
    assert_eq!(code(&output), Some(1), "deleting again must fail");
    let output = odyn(&dir, &["mem", "rm", "botany"], None);
    assert_eq!(code(&output), Some(2), "a non-id must be a usage error");
}

/// An empty brain must answer without touching the network for the model.
#[test]
fn mem_search_on_an_empty_brain_is_quietly_empty() {
    let dir = TempDir::new("memsearch");
    dir.write_config("default_provider = \"x\"\n[providers.x]\nkind = \"ollama\"\nbase_url = \"http://127.0.0.1:1\"\n");

    let output = odyn(&dir, &["mem", "search", "anything"], None);
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));
    assert_eq!(stdout(&output), "");
}

#[test]
fn the_brevity_flag_injects_the_style_directive() {
    let dir = TempDir::new("brevity");
    let (addr, request) = spawn_capturing_provider();
    dir.write_config(&openai_config(addr));

    let output = odyn(
        &dir,
        &["ask", "--brevity", "ultra", "--show-context", "2+2"],
        None,
    );
    assert_eq!(code(&output), Some(0), "{}", stderr(&output));

    let shown = stderr(&output);
    assert!(shown.contains("## Style"), "{shown}");
    assert!(shown.contains("Minimum viable words."), "{shown}");

    let request = request.recv().expect("the provider saw the request");
    assert!(
        request.contains(r"## Style\nMinimum viable words."),
        "no ultra directive in: {request}"
    );

    let output = odyn(&dir, &["ask", "--brevity", "caveman", "2+2"], None);
    assert_eq!(code(&output), Some(2), "a bad level is a usage error");
}
