//! A real smart-HTTP receive-pack exchange, confined to an ephemeral loopback
//! listener. No Git executable, credentials, hooks, or external remote is used.

use std::{
    fs,
    io::{BufRead, BufReader, Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use git2::{Oid, Repository, Signature, Time};
use gitviews_git::{
    Git2Backend, GitBackend, RemoteAction, RemoteControl, RemoteErrorKind, RemoteRequest,
};

struct TestDirectory {
    root: PathBuf,
    parent: PathBuf,
}

impl TestDirectory {
    fn new() -> Self {
        let parent = fs::canonicalize(std::env::temp_dir()).unwrap();
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = parent.join(format!(
            "gitviews-remote-rejection-{}-{unique}",
            std::process::id()
        ));
        fs::create_dir(&root).unwrap();
        Self { root, parent }
    }
}

impl Drop for TestDirectory {
    fn drop(&mut self) {
        // Only remove the exact, freshly created child of the canonical temp
        // directory; never clean up a caller-provided or computed ancestor.
        if self.root.parent() == Some(self.parent.as_path())
            && self.root.file_name().is_some_and(|name| {
                name.to_string_lossy()
                    .starts_with("gitviews-remote-rejection-")
            })
        {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

struct RejectingReceivePack {
    url: String,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<Result<Vec<u8>, String>>>,
}

impl RejectingReceivePack {
    fn start(base: Oid) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://{}/repo", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let task = thread::spawn(move || serve_rejection(listener, base, thread_stop));
        Self {
            url,
            stop,
            task: Some(task),
        }
    }

    fn finish(mut self) -> Vec<u8> {
        self.stop.store(true, Ordering::Release);
        self.task
            .take()
            .unwrap()
            .join()
            .expect("fixture server panicked")
            .expect("fixture did not receive a push")
    }
}

impl Drop for RejectingReceivePack {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            // Accept polls stop every 10ms; an accepted socket has a two-second
            // read/write timeout, so a failed client cannot hang the test suite.
            let _ = task.join();
        }
    }
}

fn packet(payload: &[u8]) -> Vec<u8> {
    let mut bytes = format!("{:04x}", payload.len() + 4).into_bytes();
    bytes.extend_from_slice(payload);
    bytes
}

fn write_response(stream: &mut TcpStream, content_type: &str, body: &[u8]) -> Result<(), String> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ).map_err(|error| error.to_string())?;
    stream.write_all(body).map_err(|error| error.to_string())?;
    stream.flush().map_err(|error| error.to_string())
}

fn request(stream: &mut TcpStream) -> Result<(String, Vec<u8>), String> {
    const MAX_BODY: usize = 1024 * 1024;
    let mut reader = BufReader::new(stream);
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .map_err(|error| error.to_string())?;
    let mut content_length = None;
    let mut chunked = false;
    let mut header_bytes = first_line.len();
    loop {
        let mut line = String::new();
        if reader
            .read_line(&mut line)
            .map_err(|error| error.to_string())?
            == 0
        {
            return Err("incomplete HTTP headers".into());
        }
        header_bytes += line.len();
        if header_bytes > 32 * 1024 {
            return Err("fixture HTTP header limit exceeded".into());
        }
        if line == "\r\n" {
            break;
        }
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        if name.eq_ignore_ascii_case("content-length") {
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| error.to_string())?,
            );
        } else if name.eq_ignore_ascii_case("transfer-encoding") {
            chunked = value.trim().eq_ignore_ascii_case("chunked");
        } else if name.eq_ignore_ascii_case("expect")
            && value.trim().eq_ignore_ascii_case("100-continue")
        {
            reader
                .get_mut()
                .write_all(b"HTTP/1.1 100 Continue\r\n\r\n")
                .map_err(|error| error.to_string())?;
        }
    }
    let mut body = Vec::new();
    if chunked {
        loop {
            let mut line = String::new();
            reader
                .read_line(&mut line)
                .map_err(|error| error.to_string())?;
            let size = usize::from_str_radix(line.trim().split(';').next().unwrap_or(""), 16)
                .map_err(|error| error.to_string())?;
            if size == 0 {
                // A zero chunk is followed by optional trailers and a final
                // empty line. Drain them before closing: unread request bytes
                // can make Windows reset the socket and truncate report-status.
                let mut trailer_bytes = 0usize;
                loop {
                    let mut trailer = String::new();
                    let remaining = 32 * 1024 + 1 - trailer_bytes;
                    let read = Read::take(&mut reader, remaining as u64)
                        .read_line(&mut trailer)
                        .map_err(|error| error.to_string())?;
                    if read == 0 {
                        return Err("incomplete HTTP chunk trailers".into());
                    }
                    trailer_bytes += read;
                    if trailer_bytes > 32 * 1024 {
                        return Err("fixture HTTP trailer limit exceeded".into());
                    }
                    if trailer == "\r\n" {
                        break;
                    }
                    if !trailer.ends_with("\r\n") {
                        return Err("invalid HTTP chunk trailer delimiter".into());
                    }
                }
                break;
            }
            if size > MAX_BODY - body.len() {
                return Err("fixture HTTP body limit exceeded".into());
            }
            let offset = body.len();
            body.resize(offset + size, 0);
            reader
                .read_exact(&mut body[offset..])
                .map_err(|error| error.to_string())?;
            let mut newline = [0; 2];
            reader
                .read_exact(&mut newline)
                .map_err(|error| error.to_string())?;
            if newline != *b"\r\n" {
                return Err("invalid HTTP chunk delimiter".into());
            }
        }
    } else if let Some(size) = content_length {
        if size > MAX_BODY {
            return Err("fixture HTTP body limit exceeded".into());
        }
        body.resize(size, 0);
        reader
            .read_exact(&mut body)
            .map_err(|error| error.to_string())?;
    }
    Ok((first_line, body))
}

fn serve_rejection(
    listener: TcpListener,
    base: Oid,
    stop: Arc<AtomicBool>,
) -> Result<Vec<u8>, String> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut requests = 0;
    while Instant::now() < deadline && !stop.load(Ordering::Acquire) {
        let mut stream = match listener.accept() {
            Ok((stream, _)) => stream,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => return Err(error.to_string()),
        };
        // Windows can inherit the listener's nonblocking mode on accepted
        // sockets. Header/body reads must wait for the client's next packet;
        // otherwise a normal packet boundary intermittently returns 10035.
        stream
            .set_nonblocking(false)
            .map_err(|error| error.to_string())?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| error.to_string())?;
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .map_err(|error| error.to_string())?;
        requests += 1;
        if requests > 8 {
            return Err("unexpected repeated HTTP requests".into());
        }
        let (line, body) = request(&mut stream)?;
        if line.starts_with("GET /repo/info/refs?service=git-receive-pack ") {
            let mut advertisement = packet(b"# service=git-receive-pack\n");
            advertisement.extend_from_slice(b"0000");
            advertisement.extend(packet(
                format!("{base} refs/heads/main\0report-status ofs-delta\n").as_bytes(),
            ));
            advertisement.extend_from_slice(b"0000");
            write_response(
                &mut stream,
                "application/x-git-receive-pack-advertisement",
                &advertisement,
            )?;
        } else if line.starts_with("POST /repo/git-receive-pack ") {
            let mut rejection = packet(b"unpack ok\n");
            rejection.extend(packet(b"ng refs/heads/main protected branch\n"));
            rejection.extend_from_slice(b"0000");
            write_response(
                &mut stream,
                "application/x-git-receive-pack-result",
                &rejection,
            )?;
            return Ok(body);
        } else {
            return Err(format!("unexpected fixture request: {}", line.trim()));
        }
    }
    Err("receive-pack fixture stopped or timed out before a push".into())
}

fn base_commit(repository: &Repository) -> Oid {
    repository.set_head("refs/heads/main").unwrap();
    let signature = Signature::new(
        "Remote Test",
        "remote@example.invalid",
        &Time::new(1_700_000_000, 0),
    )
    .unwrap();
    let tree_id = repository.treebuilder(None).unwrap().write().unwrap();
    let tree = repository.find_tree(tree_id).unwrap();
    repository
        .commit(Some("HEAD"), &signature, &signature, "base", &tree, &[])
        .unwrap()
}

#[test]
fn push_reports_protected_branch_rejection_and_keeps_remote_reference() {
    let directory = TestDirectory::new();
    let local_path = directory.root.join("client");
    let remote = Repository::init_bare(directory.root.join("remote.git")).unwrap();
    let repository = Repository::init(&local_path).unwrap();
    let base = base_commit(&remote);
    assert_eq!(base_commit(&repository), base);
    let parent = repository.find_commit(base).unwrap();
    let tree = parent.tree().unwrap();
    let signature = Signature::new(
        "Remote Test",
        "remote@example.invalid",
        &Time::new(1_700_000_001, 0),
    )
    .unwrap();
    let tip = repository
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "local tip",
            &tree,
            &[&parent],
        )
        .unwrap();
    let server = RejectingReceivePack::start(base);
    let mut config = repository.config().unwrap();
    config.set_str("http.proxy", "").unwrap();
    config
        .set_str(&format!("url.{}.insteadOf", server.url), &server.url)
        .unwrap();
    config
        .set_str(&format!("url.{}.pushInsteadOf", server.url), &server.url)
        .unwrap();
    repository.remote("origin", &server.url).unwrap();
    config
        .set_str("remote.origin.pushurl", &server.url)
        .unwrap();
    config.set_str("branch.main.remote", "origin").unwrap();
    config
        .set_str("branch.main.merge", "refs/heads/main")
        .unwrap();
    // Ignore any developer-machine proxy configuration: this fixture may only
    // connect to its explicitly bound loopback listener.
    let configured_remote = repository.find_remote("origin").unwrap();
    assert!(
        configured_remote.url().is_ok_and(|url| url == server.url),
        "fixture fetch URL was unexpectedly rewritten"
    );
    assert!(
        configured_remote
            .pushurl()
            .is_ok_and(|url| url == Some(server.url.as_str())),
        "fixture push URL was unexpectedly rewritten"
    );
    let error = Git2Backend
        .remote(
            &local_path,
            &RemoteRequest {
                action: RemoteAction::Push,
                expected_branch: Some("main".into()),
                expected_head: Some(tip.to_string()),
            },
            &RemoteControl::default(),
        )
        .unwrap_err();
    let body = server.finish();
    assert_eq!(
        error.kind,
        RemoteErrorKind::Rejected,
        "{error}; fixture received {} bytes",
        body.len()
    );
    let length = usize::from_str_radix(std::str::from_utf8(&body[..4]).unwrap(), 16).unwrap();
    let command = std::str::from_utf8(&body[4..length])
        .unwrap()
        .split('\0')
        .next()
        .unwrap();
    assert_eq!(command.trim(), format!("{base} {tip} refs/heads/main"));
    assert!(
        body.windows(4).any(|bytes| bytes == b"PACK"),
        "the client must send an actual pack before rejection"
    );
    assert_eq!(remote.refname_to_id("refs/heads/main").unwrap(), base);
    assert_eq!(repository.refname_to_id("refs/heads/main").unwrap(), tip);
    assert!(error.authentication.is_none());
    assert!(!error.message.contains("http://127.0.0.1"));
    assert!(!error.message.contains("protected branch"));
}
