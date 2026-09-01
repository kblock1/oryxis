//! End-to-end integration tests for the SFTP path against a real
//! OpenSSH server running in a throwaway container.
//!
//! Requires Docker on the host and is gated behind `#[ignore]` so a
//! plain `cargo test` (CI without Docker, dev quick loop) skips them.
//! Run explicitly with:
//!
//! ```sh
//! cargo test -p oryxis-ssh -- --ignored
//! ```
//!
//! Each test spins up its own container so they can run in parallel
//! without stepping on a shared sshd.

use std::sync::Arc;
use std::time::Duration;

use oryxis_core::models::connection::{AuthMethod, Connection};
use oryxis_ssh::{HostKeyStatus, SshEngine};
use testcontainers::{
    core::{ContainerPort, IntoContainerPort, WaitFor},
    runners::AsyncRunner,
    GenericImage, ImageExt,
};

/// Username + password we hand the linuxserver/openssh-server image.
/// Hardcoded only because the image generates these inside the
/// container at boot, they never touch any real machine.
const TEST_USER: &str = "tester";
const TEST_PASS: &str = "testpass123";

/// Stand up a fresh SFTP-capable container and return `(connection,
/// password)` ready to hand to `SshEngine::connect`. Caller holds the
/// container handle in scope to keep it alive for the duration of the
/// test.
async fn start_sshd() -> (
    Connection,
    String,
    testcontainers::ContainerAsync<GenericImage>,
) {
    let container = GenericImage::new("linuxserver/openssh-server", "latest")
        .with_exposed_port(ContainerPort::Tcp(2222))
        // The "sshd is listening on port 2222" line fires *before* the
        // socket is actually accepting connections, so we wait for the
        // very last init line which only prints after sshd is reachable.
        .with_wait_for(WaitFor::message_on_stdout("[ls.io-init] done."))
        .with_env_var("PUID", "1000")
        .with_env_var("PGID", "1000")
        .with_env_var("PASSWORD_ACCESS", "true")
        .with_env_var("USER_NAME", TEST_USER)
        .with_env_var("USER_PASSWORD", TEST_PASS)
        .with_env_var("SUDO_ACCESS", "false")
        .start()
        .await
        .expect("docker daemon must be running");
    let port = container
        .get_host_port_ipv4(2222.tcp())
        .await
        .expect("port mapping");
    let host = container
        .get_host()
        .await
        .expect("host")
        .to_string();
    let mut conn = Connection::new("test", host);
    conn.port = port;
    conn.username = Some(TEST_USER.to_string());
    conn.auth_method = AuthMethod::Password;
    (conn, TEST_PASS.to_string(), container)
}

fn engine() -> SshEngine {
    // Trust whatever host key the container hands us, these are
    // ephemeral fixtures, not real servers, and the test is asserting
    // protocol behaviour, not host-key policy.
    SshEngine::new()
        .with_host_key_check(Arc::new(|_, _, _, _| HostKeyStatus::Known))
        .with_connect_timeout(Duration::from_secs(20))
        .with_auth_timeout(Duration::from_secs(20))
        .with_session_timeout(Duration::from_secs(20))
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn sftp_list_root_after_password_auth() {
    let (conn, password, _container) = start_sshd().await;
    let engine = engine();
    let (session, _rx) = engine
        .connect(&conn, Some(&password), None, 80, 24)
        .await
        .expect("connect");
    let session = Arc::new(session);
    let client = session.open_sftp().await.expect("open sftp");
    // The image's home dir for `tester` is /config, so canonicalize
    // gives an absolute path we can list.
    let initial = client.canonicalize(".").await.expect("canonicalize");
    let entries = client.list_dir(&initial).await.expect("list_dir");
    // The home dir is non-empty (the image plants `.ssh/` etc), but
    // we only assert the call resolved, content varies by image
    // tag and isn't load-bearing.
    let _ = entries;
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn sftp_write_read_round_trip() {
    let (conn, password, _container) = start_sshd().await;
    let engine = engine();
    let (session, _rx) = engine
        .connect(&conn, Some(&password), None, 80, 24)
        .await
        .expect("connect");
    let session = Arc::new(session);
    let client = session.open_sftp().await.expect("open sftp");
    let home = client.canonicalize(".").await.expect("canonicalize");

    let path = format!("{}/round-trip.txt", home.trim_end_matches('/'));
    let payload = b"hello from oryxis test\n";
    client.write_file(&path, payload).await.expect("write_file");
    let read_back = client.read_file(&path).await.expect("read_file");
    assert_eq!(read_back, payload);

    // Rename then verify the new path is listable + the old one isn't.
    let renamed = format!("{}/renamed.txt", home.trim_end_matches('/'));
    client.rename(&path, &renamed).await.expect("rename");
    let after = client
        .list_dir(&home)
        .await
        .expect("list_dir after rename");
    let names: Vec<_> = after.iter().map(|e| e.name.as_str()).collect();
    assert!(names.contains(&"renamed.txt"));
    assert!(!names.contains(&"round-trip.txt"));

    client.remove_file(&renamed).await.expect("remove_file");
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn sftp_recursive_dir_delete_via_exec() {
    // `remove_dir_recursive` shells out to `rm -rf` over an exec
    // channel, this exercises the SshSession→exec path, which the
    // unit tests can't cover.
    let (conn, password, _container) = start_sshd().await;
    let engine = engine();
    let (session, _rx) = engine
        .connect(&conn, Some(&password), None, 80, 24)
        .await
        .expect("connect");
    let session = Arc::new(session);
    let client = session.open_sftp().await.expect("open sftp");
    let home = client.canonicalize(".").await.expect("canonicalize");

    // Build /home/<user>/scratch/{a,b/c.txt}, then nuke it recursively.
    let scratch = format!("{}/scratch", home.trim_end_matches('/'));
    client.create_dir(&scratch).await.expect("mkdir scratch");
    let nested = format!("{}/b", scratch);
    client.create_dir(&nested).await.expect("mkdir nested");
    client
        .write_file(&format!("{}/a", scratch), b"a")
        .await
        .expect("write a");
    client
        .write_file(&format!("{}/c.txt", nested), b"c")
        .await
        .expect("write c");

    client
        .remove_dir_recursive(&scratch)
        .await
        .expect("remove_dir_recursive");

    let after = client.list_dir(&home).await.expect("list after");
    let names: Vec<_> = after.iter().map(|e| e.name.as_str()).collect();
    assert!(!names.contains(&"scratch"));
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn sftp_open_sibling_for_parallel_pool() {
    // Validates the SFTP sibling-channel path used by the parallel
    // transfer worker pool: opening N independent subsystem channels
    // on the same SSH connection should succeed and each should be
    // independently usable.
    let (conn, password, _container) = start_sshd().await;
    let engine = engine();
    let (session, _rx) = engine
        .connect(&conn, Some(&password), None, 80, 24)
        .await
        .expect("connect");
    let session = Arc::new(session);
    let primary = session.open_sftp().await.expect("primary sftp");
    let siblings: Vec<_> = futures_or_join(primary.clone(), 3).await;
    let home = primary.canonicalize(".").await.expect("canonicalize");
    // All siblings should successfully list the same directory in
    // parallel without serialising on the primary's mutex.
    for client in &siblings {
        let _ = client.list_dir(&home).await.expect("sibling list_dir");
    }
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn sftp_stream_upload_download_round_trip() {
    // Exercises the streamed `upload_from` / `download_to` path with a
    // payload larger than one SFTP request (255 KiB) so the chunked pump
    // loop runs multiple iterations in each direction. Bytes must survive
    // local -> remote -> local untouched.
    let (conn, password, _container) = start_sshd().await;
    let engine = engine();
    let (session, _rx) = engine
        .connect(&conn, Some(&password), None, 80, 24)
        .await
        .expect("connect");
    let session = Arc::new(session);
    let client = session.open_sftp().await.expect("open sftp");
    let home = client.canonicalize(".").await.expect("canonicalize");

    // 600 KiB of a non-repeating-ish pattern, spans ~3 chunks.
    let payload: Vec<u8> = (0..600 * 1024).map(|i| (i % 251) as u8).collect();
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let local_src = tmp.join(format!("oryxis-stream-src-{pid}.bin"));
    let local_dst = tmp.join(format!("oryxis-stream-dst-{pid}.bin"));
    std::fs::write(&local_src, &payload).expect("write local src");

    let remote = format!("{}/stream.bin", home.trim_end_matches('/'));
    client
        .upload_from(&local_src, &remote)
        .await
        .expect("upload_from");

    // Remote size matches what we sent.
    let stat = client.stat(&remote).await.expect("stat");
    assert_eq!(stat.size, payload.len() as u64);

    client
        .download_to(&remote, &local_dst, None)
        .await
        .expect("download_to");
    let round_tripped = std::fs::read(&local_dst).expect("read local dst");
    assert_eq!(round_tripped, payload);

    client.remove_file(&remote).await.expect("remove_file");
    let _ = std::fs::remove_file(&local_src);
    let _ = std::fs::remove_file(&local_dst);
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn sftp_windowed_large_round_trip() {
    // Drives the concurrent windowed path: a payload above STREAM_THRESHOLD
    // (8 MiB) makes both `upload_from` and `download_to` carry a sliding
    // window of interleaved requests on one handle. This is the real-server
    // check for the multiplexing assumption the unit tests can only fake.
    // Bytes must survive local -> remote -> local intact.
    let (conn, password, _container) = start_sshd().await;
    let engine = engine();
    let (session, _rx) = engine
        .connect(&conn, Some(&password), None, 80, 24)
        .await
        .expect("connect");
    let session = Arc::new(session);
    let client = session.open_sftp().await.expect("open sftp");
    let home = client.canonicalize(".").await.expect("canonicalize");

    // 10 MiB, comfortably over the 8 MiB window threshold so both
    // directions carry a sliding window of concurrent requests. Non-trivial
    // byte pattern so a misplaced chunk shows up as a mismatch, not a
    // coincidental match.
    let payload: Vec<u8> = (0..10 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let tmp = std::env::temp_dir();
    let pid = std::process::id();
    let local_src = tmp.join(format!("oryxis-windowed-src-{pid}.bin"));
    let local_dst = tmp.join(format!("oryxis-windowed-dst-{pid}.bin"));
    std::fs::write(&local_src, &payload).expect("write local src");

    let remote = format!("{}/windowed.bin", home.trim_end_matches('/'));
    client
        .upload_from(&local_src, &remote)
        .await
        .expect("upload_from windowed");
    let stat = client.stat(&remote).await.expect("stat");
    assert_eq!(stat.size, payload.len() as u64);

    client
        .download_to(&remote, &local_dst, None)
        .await
        .expect("download_to windowed");
    let round_tripped = std::fs::read(&local_dst).expect("read local dst");
    assert_eq!(round_tripped.len(), payload.len(), "size mismatch");
    assert_eq!(round_tripped, payload, "windowed reassembly mismatch");

    client.remove_file(&remote).await.expect("remove_file");
    let _ = std::fs::remove_file(&local_src);
    let _ = std::fs::remove_file(&local_dst);
}

/// Sequentially open `n` siblings off `primary`, keeps the test
/// simple without pulling in a futures crate just for `join_all`.
async fn futures_or_join(
    primary: oryxis_ssh::SftpClient,
    n: usize,
) -> Vec<oryxis_ssh::SftpClient> {
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        out.push(primary.open_sibling().await.expect("open_sibling"));
    }
    out
}

// ---------------------------------------------------------------------
// The interactive SFTP console (issue #188).
//
// The console's pure halves (line editing, parsing, globbing, rendering)
// are unit-tested without a server; what needs one is the loop that
// drives them: that a command's output actually reaches the pane's byte
// stream, that an error is reported without ending the session, and
// above all that a running command can be INTERRUPTED. That last one is
// the property a console lives or dies by and the one no unit test can
// establish, because it is about two futures racing.
// ---------------------------------------------------------------------

use oryxis_ssh::sftp_shell::SftpShellSession;
use tokio::sync::mpsc::UnboundedReceiver;

/// Drain whatever the console has emitted so far, giving it a moment to
/// produce it. Returns the text with the escape sequences left in: the
/// assertions look for content, and stripping would hide a bug where the
/// content only appears inside a sequence.
async fn drain(rx: &mut UnboundedReceiver<Vec<u8>>, patience: Duration) -> String {
    let deadline = tokio::time::Instant::now() + patience;
    let mut buf = Vec::new();
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(120), rx.recv()).await {
            Ok(Some(chunk)) => buf.extend_from_slice(&chunk),
            Ok(None) => break,
            Err(_) => {
                // A quiet stretch after something arrived means the
                // console has said its piece.
                if !buf.is_empty() {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// Wait for `needle` to show up in the console's output.
async fn expect_output(
    rx: &mut UnboundedReceiver<Vec<u8>>,
    needle: &str,
    patience: Duration,
) -> String {
    let deadline = tokio::time::Instant::now() + patience;
    let mut seen = String::new();
    while tokio::time::Instant::now() < deadline {
        seen.push_str(&drain(rx, Duration::from_millis(400)).await);
        if seen.contains(needle) {
            return seen;
        }
    }
    panic!("never saw {needle:?} in console output; got:\n{seen}");
}

/// Open a console on a fresh container.
async fn start_console() -> (
    SftpShellSession,
    UnboundedReceiver<Vec<u8>>,
    Arc<oryxis_ssh::SshSession>,
    testcontainers::ContainerAsync<GenericImage>,
) {
    let (conn, password, container) = start_sshd().await;
    let engine = engine();
    let (session, _rx) = engine
        .connect(&conn, Some(&password), None, 80, 24)
        .await
        .expect("connect");
    let session = Arc::new(session);
    let client = session.open_sftp().await.expect("open sftp");
    let home = client.canonicalize(".").await.expect("home");
    let (console, out) = SftpShellSession::spawn(
        Arc::clone(&session),
        client,
        home,
        std::env::temp_dir(),
        80,
        "test".to_string(),
    );
    (console, out, session, container)
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_greets_and_prompts() {
    let (console, mut out, _ssh, _container) = start_console().await;
    let seen = expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;
    assert!(seen.contains("Connected to test"), "no banner in:\n{seen}");
    assert!(console.is_alive());
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_lists_and_navigates() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    console.write(b"pwd\r").expect("write");
    let seen = expect_output(&mut out, "Remote working directory", Duration::from_secs(10)).await;
    assert!(seen.contains("/config"), "unexpected home in:\n{seen}");

    console.write(b"cd /etc\r").expect("write");
    console.write(b"pwd\r").expect("write");
    let seen = expect_output(&mut out, "/etc", Duration::from_secs(10)).await;
    assert!(seen.contains("/etc"), "cd did not take in:\n{seen}");

    // `ls <file>` lists the file itself, the way `ls` and `sftp(1)` do.
    // The first version of this treated any operand as a directory and
    // answered "no such file" about a file that was plainly there.
    console.write(b"ls -l passwd\r").expect("write");
    let seen = expect_output(&mut out, "passwd", Duration::from_secs(10)).await;
    // The long format's mode column, which is what proves the listing
    // went through the renderer rather than through some raw dump.
    assert!(seen.contains("-rw-"), "no long listing in:\n{seen}");
    assert!(!seen.contains("No such file"), "listed a file as a dir:\n{seen}");

    // And a directory operand still lists its contents.
    console.write(b"ls -1 /etc/ssh\r").expect("write");
    let seen = expect_output(&mut out, "ssh", Duration::from_secs(10)).await;
    assert!(!seen.contains("No such file"), "directory listing broke:\n{seen}");
}

/// An error is REPORTED and the console carries on. This is the whole
/// difference between a console and a script, and it is the behaviour
/// that the "ask the session, not the error" design exists to protect:
/// `SftpClient` reports a missing file with the same error variant a
/// dead link produces.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_survives_a_failing_command() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    console.write(b"cd /no/such/place\r").expect("write");
    expect_output(&mut out, "sftp", Duration::from_secs(10)).await;
    assert!(console.is_alive(), "a missing directory closed the console");

    // Still usable afterwards.
    console.write(b"pwd\r").expect("write");
    let seen = expect_output(&mut out, "Remote working directory", Duration::from_secs(10)).await;
    assert!(seen.contains("/config"), "cd to nowhere moved us:\n{seen}");
}

#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_round_trips_a_file() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    let dir = std::env::temp_dir().join(format!("oryxis-console-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let source = dir.join("upload.txt");
    std::fs::write(&source, b"console round trip").expect("write source");

    console
        .write(format!("lcd {}\r", dir.display()).as_bytes())
        .expect("write");
    console.write(b"put upload.txt\r").expect("write");
    expect_output(&mut out, "upload.txt", Duration::from_secs(20)).await;

    std::fs::remove_file(&source).expect("remove source");
    console.write(b"get upload.txt\r").expect("write");
    expect_output(&mut out, "upload.txt", Duration::from_secs(20)).await;

    // Give the download a moment to land before looking for it.
    for _ in 0..40 {
        if source.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    assert_eq!(
        std::fs::read(&source).expect("downloaded file"),
        b"console round trip"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// `mget` over a glob, which is the operand shape issue #188 asked for
/// by name.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_expands_a_glob() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    let dir = std::env::temp_dir().join(format!("oryxis-glob-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    for name in ["a.log", "b.log", "c.txt"] {
        std::fs::write(dir.join(name), name.as_bytes()).expect("write");
    }
    console
        .write(format!("lcd {}\r", dir.display()).as_bytes())
        .expect("write");
    console.write(b"mput *.log\r").expect("write");
    expect_output(&mut out, "b.log", Duration::from_secs(20)).await;

    // The remote now holds exactly the two `.log` files the glob picked,
    // and not the `.txt` it did not.
    console.write(b"ls *.log\r").expect("write");
    let seen = expect_output(&mut out, "a.log", Duration::from_secs(10)).await;
    assert!(seen.contains("b.log"), "second match missing:\n{seen}");
    assert!(!seen.contains("c.txt"), "glob over-matched:\n{seen}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// An unmatched pattern is reported rather than silently succeeding,
/// because an empty success looks exactly like a directory with nothing
/// in it and sends the user looking for files that were never fetched.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_reports_a_glob_that_matched_nothing() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;
    console.write(b"mget *.nonesuch\r").expect("write");
    let seen = expect_output(&mut out, "no matches found", Duration::from_secs(10)).await;
    assert!(seen.contains("nonesuch"), "pattern not named in:\n{seen}");
    assert!(console.is_alive());
}

/// A Ctrl+C during a transfer reaches the console and leaves it usable.
///
/// The CANCELLATION itself is proved deterministically in the unit test
/// `a_running_command_can_be_interrupted`, against a command that never
/// finishes. It cannot be proved here: this link runs at a few hundred
/// megabytes a second, so any file small enough to write quickly
/// transfers faster than a keystroke can be aimed at it, and a test that
/// asserted "interrupted" would fail on a fast machine and pass on a
/// slow one. What this one adds is the part the unit test cannot reach:
/// that a real interrupt, landing at whatever point it lands, leaves a
/// console that still answers.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_survives_an_interrupt_during_a_transfer() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    let dir = std::env::temp_dir().join(format!("oryxis-cancel-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(dir.join("big.bin"), vec![7u8; 64 * 1024 * 1024]).expect("write big");

    console
        .write(format!("lcd {}\r", dir.display()).as_bytes())
        .expect("write");
    console.write(b"put big.bin\r").expect("write");
    tokio::time::sleep(Duration::from_millis(150)).await;
    console.write(&[0x03]).expect("write ctrl-c");
    drain(&mut out, Duration::from_secs(15)).await;

    assert!(console.is_alive(), "the interrupt killed the console");
    // Still takes commands, which is what makes it a cancellation rather
    // than a crash, and what proves the health probe did not mistake the
    // interrupt for a dead link.
    console.write(b"pwd\r").expect("write");
    expect_output(&mut out, "Remote working directory", Duration::from_secs(15)).await;
    let _ = std::fs::remove_dir_all(&dir);
}

/// The OSC 133 marks reach the stream, in the order a reader needs them.
///
/// A mark that is emitted in the wrong place is invisible: nothing draws
/// it and nothing errors, the consumer just never fires. So the order is
/// asserted against the bytes rather than trusted.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_emits_semantic_prompt_marks() {
    let (console, mut out, _ssh, _container) = start_console().await;
    let banner = expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;
    // The first prompt is already wrapped: A, the prompt, then B.
    let a = banner.find("\x1b]133;A").expect("no prompt-start in banner");
    let b = banner.find("\x1b]133;B").expect("no prompt-end in banner");
    assert!(a < b, "prompt-end came before prompt-start");

    console.write(b"pwd\r").expect("write");
    let seen = expect_output(&mut out, "\x1b]133;D", Duration::from_secs(10)).await;
    let c = seen.find("\x1b]133;C").expect("no output-start");
    let d = seen.find("\x1b]133;D").expect("no command-end");
    assert!(c < d, "command-end came before output-start");
    // `pwd` cannot fail, so the status is zero.
    assert!(seen.contains("\x1b]133;D;0"), "wrong status in:\n{seen:?}");

    // A command that fails reports a non-zero status, which is what lets
    // a tab show that something went wrong without reading the text.
    console.write(b"cd /no/such/place\r").expect("write");
    let seen = expect_output(&mut out, "\x1b]133;D;1", Duration::from_secs(10)).await;
    assert!(seen.contains("\x1b]133;D;1"), "failure not reported in:\n{seen:?}");
}

/// `bye` ends the session, and the ordering contract means it reads as
/// dead BEFORE its output stream ends.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_quits_on_bye_and_reads_dead_before_silent() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;
    console.write(b"bye\r").expect("write");

    // Drain to the end of the stream. When it closes, the session must
    // ALREADY report itself dead: the app reads the end of this stream
    // as the pane's death notice and consults `is_alive` before acting
    // on it, so a session still claiming to be alive here would have its
    // own notice discarded.
    let closed = tokio::time::timeout(Duration::from_secs(10), async {
        while out.recv().await.is_some() {}
    })
    .await;
    assert!(closed.is_ok(), "output stream never ended after bye");
    assert!(
        !console.is_alive(),
        "stream ended while the console still read as alive"
    );
}

/// Tab with several candidates and nothing left to share LISTS them.
///
/// The reported failure (issue #188): with `abcd.txt` and `abde.txt` in
/// the directory, `ab` shares nothing beyond what was already typed, so
/// the version that only ever inserted a common prefix repainted an
/// identical line. From the outside that is a key that does nothing, and
/// it was reported as one.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_tab_lists_candidates_that_share_no_more() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    let dir = std::env::temp_dir().join(format!("oryxis-tab-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(dir.join("abcd.txt"), b"one").expect("write abcd");
    std::fs::write(dir.join("abde.txt"), b"two").expect("write abde");

    console
        .write(format!("lcd {}\r", dir.display()).as_bytes())
        .expect("write");
    console.write(b"put abcd.txt\r").expect("write");
    expect_output(&mut out, "abcd.txt", Duration::from_secs(20)).await;
    console.write(b"put abde.txt\r").expect("write");
    expect_output(&mut out, "abde.txt", Duration::from_secs(20)).await;

    // Remote side: both names must appear, and the line must come back.
    console.write(b"get ab\t").expect("write");
    let seen = expect_output(&mut out, "abde.txt", Duration::from_secs(10)).await;
    assert!(seen.contains("abcd.txt"), "only one candidate listed:\n{seen}");
    assert!(seen.contains("sftp> get ab"), "the line was not repainted:\n{seen}");

    // One more character settles it, and then the word completes whole.
    console.write(b"c\t").expect("write");
    let seen = expect_output(&mut out, "abcd.txt", Duration::from_secs(10)).await;
    assert!(
        seen.contains("sftp> get abcd.txt "),
        "a single candidate did not complete:\n{seen}"
    );

    console.write(&[0x03]).expect("write ctrl-c");
    let _ = std::fs::remove_dir_all(&dir);
}

/// `put` completes against the LOCAL filesystem.
///
/// The other half of the report: every word went to `list_dir` on the
/// server, so a local filename found no candidates and painted nothing.
/// Tab read as unwired for the whole `put` / `mput` / `lcd` family.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_tab_completes_a_local_path_for_put() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    let dir = std::env::temp_dir().join(format!("oryxis-tab-local-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(dir.join("upload-me.txt"), b"local").expect("write source");

    console
        .write(format!("lcd {}\r", dir.display()).as_bytes())
        .expect("write");
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    // Nothing on the SERVER is named this, which is exactly what makes
    // the assertion meaningful: the candidate can only have come from
    // the local directory.
    console.write(b"put upl\t").expect("write");
    let seen = expect_output(&mut out, "upload-me.txt", Duration::from_secs(10)).await;
    assert!(
        seen.contains("sftp> put upload-me.txt "),
        "put did not complete locally:\n{seen}"
    );

    // And the completed line actually transfers, which is the property
    // the quoting exists for.
    console.write(b"\r").expect("write");
    let seen = expect_output(&mut out, "upload-me.txt", Duration::from_secs(20)).await;
    assert!(!seen.contains("No such file"), "the completed path was wrong:\n{seen}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The first word completes against the command vocabulary, which is how
/// someone who has never used the console finds out what it can do.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_tab_completes_a_verb() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    console.write(b"prog\t").expect("write");
    let seen = expect_output(&mut out, "progress", Duration::from_secs(10)).await;
    assert!(seen.contains("sftp> progress "), "verb not completed:\n{seen}");

    // It is a real line, not a decoration: Enter runs it.
    console.write(b"\r").expect("write");
    expect_output(&mut out, "Progress meter disabled", Duration::from_secs(10)).await;
}

/// A filename holding a glob character is an ordinary filename. Completing
/// it has to produce a line the executor reads as a NAME, which is what
/// the tokenizer's glob-escaping is for: unescaped, the transfer reported
/// "no matches found" about a file that was plainly there.
#[tokio::test]
#[ignore = "requires Docker, run with --ignored"]
async fn console_transfers_a_file_whose_name_looks_like_a_pattern() {
    let (console, mut out, _ssh, _container) = start_console().await;
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    let dir = std::env::temp_dir().join(format!("oryxis-tab-glob-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).expect("scratch dir");
    std::fs::write(dir.join("report[1].txt"), b"bracketed").expect("write source");

    console
        .write(format!("lcd {}\r", dir.display()).as_bytes())
        .expect("write");
    expect_output(&mut out, "sftp>", Duration::from_secs(10)).await;

    console.write(b"put rep\t").expect("write");
    let seen = expect_output(&mut out, "report", Duration::from_secs(10)).await;
    assert!(
        seen.contains(r"report\[1\].txt"),
        "the brackets were not escaped into the line:\n{seen}"
    );

    console.write(b"\r").expect("write");
    let seen = expect_output(&mut out, "report", Duration::from_secs(20)).await;
    assert!(
        !seen.contains("no matches found"),
        "the name was read as a pattern:\n{seen}"
    );

    // And it really landed, under its real name.
    console.write(b"ls -1\r").expect("write");
    let seen = expect_output(&mut out, "report[1].txt", Duration::from_secs(10)).await;
    assert!(seen.contains("report[1].txt"), "the upload did not arrive:\n{seen}");
    let _ = std::fs::remove_dir_all(&dir);
}
