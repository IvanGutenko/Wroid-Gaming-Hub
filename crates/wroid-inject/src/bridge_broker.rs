use std::io::{self, BufRead, BufReader, Write};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{BridgeHelperCommand, PrivilegedBridgeHelper};

pub const BRIDGE_PROTOCOL_VERSION: u32 = 1;
pub const BRIDGE_WORKER_PROTOCOL_GENERATION: u32 = 1;
pub const BRIDGE_WORKER_FD: RawFd = 198;
const MAX_BRIDGE_FRAME_BYTES: usize = 4096;
// The worker legitimately spends time before its first Open: it stops a
// running desktop Waydroid session (Android shutdown can take tens of
// seconds), discovers input devices, and validates the profile. A short
// deadline here killed the broker mid-preparation and surfaced as a bare
// EPIPE in the worker. 120 s matches the verify window the broker already
// tolerates afterwards.
const INITIAL_OPEN_TIMEOUT: Duration = Duration::from_secs(120);
const VERIFY_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const CLIENT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(125);
const FRAME_WRITE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_ERROR_DETAIL_CHARS: usize = 512;

pub trait BridgeHelperSession: Send {
    fn verify_android_input(&mut self) -> io::Result<()>;
    fn finish(self: Box<Self>, waydroid_stopped: bool) -> io::Result<()>;
}

pub trait BridgeHelperFactory: Send + Sync + 'static {
    fn start(&self, event_node: &Path) -> io::Result<Box<dyn BridgeHelperSession>>;
}

#[derive(Debug, Clone)]
pub struct ProductionBridgeHelperFactory {
    command: BridgeHelperCommand,
}

impl ProductionBridgeHelperFactory {
    pub const fn new(command: BridgeHelperCommand) -> Self {
        Self { command }
    }
}

impl BridgeHelperSession for PrivilegedBridgeHelper {
    fn verify_android_input(&mut self) -> io::Result<()> {
        Self::verify_android_input(self)
    }

    fn finish(self: Box<Self>, waydroid_stopped: bool) -> io::Result<()> {
        (*self).finish(waydroid_stopped)
    }
}

impl BridgeHelperFactory for ProductionBridgeHelperFactory {
    fn start(&self, event_node: &Path) -> io::Result<Box<dyn BridgeHelperSession>> {
        PrivilegedBridgeHelper::start(&self.command, event_node)
            .map(|helper| Box::new(helper) as Box<dyn BridgeHelperSession>)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    Initial,
    Opened,
    Verified,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    Initial,
    Opened,
    Verified,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RequestFrame {
    protocol_version: u32,
    #[serde(flatten)]
    request: BridgeRequest,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "method", content = "params", rename_all = "snake_case")]
enum BridgeRequest {
    Open { event_node: PathBuf },
    VerifyAndroidInput,
    Finish { waydroid_stopped: bool },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResponseFrame {
    protocol_version: u32,
    #[serde(flatten)]
    response: BridgeResponse,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "result", content = "data", rename_all = "snake_case")]
enum BridgeResponse {
    Opened,
    AndroidInputReady,
    Finished,
    Error { code: String, detail: String },
}

#[derive(Debug)]
pub struct BridgeBrokerClient {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
    state: ClientState,
}

impl BridgeBrokerClient {
    pub fn from_owned_fd(fd: OwnedFd) -> io::Result<Self> {
        validate_owned_socket(&fd)?;
        Self::from_stream(UnixStream::from(fd))
    }

    pub fn from_owned_fd_for_peer(
        fd: OwnedFd,
        expected_pid: libc::pid_t,
        expected_uid: u32,
    ) -> io::Result<Self> {
        validate_owned_socket(&fd)?;
        let raw_fd = fd.as_raw_fd();
        let mut credentials = libc::ucred {
            pid: 0,
            uid: 0,
            gid: 0,
        };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: credentials and length are valid SO_PEERCRED output buffers
        // and raw_fd is the owned Unix socket validated above.
        if unsafe {
            libc::getsockopt(
                raw_fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                std::ptr::addr_of_mut!(credentials).cast(),
                &mut length,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if length as usize != std::mem::size_of::<libc::ucred>()
            || credentials.pid != expected_pid
            || credentials.uid != expected_uid
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "inherited bridge socket peer is not the expected Wroid daemon",
            ));
        }
        Self::from_stream(UnixStream::from(fd))
    }

    fn from_stream(stream: UnixStream) -> io::Result<Self> {
        stream.set_write_timeout(Some(FRAME_WRITE_TIMEOUT))?;
        let reader = BufReader::new(stream.try_clone()?);
        Ok(Self {
            reader,
            writer: stream,
            state: ClientState::Initial,
        })
    }

    pub fn open(&mut self, event_node: &Path) -> io::Result<()> {
        if self.state != ClientState::Initial {
            return Err(invalid_client_state("open"));
        }
        self.exchange(
            BridgeRequest::Open {
                event_node: event_node.to_path_buf(),
            },
            ExpectedResponse::Opened,
        )?;
        self.state = ClientState::Opened;
        Ok(())
    }

    pub fn verify_android_input(&mut self) -> io::Result<()> {
        if self.state != ClientState::Opened {
            return Err(invalid_client_state("verify_android_input"));
        }
        self.exchange(
            BridgeRequest::VerifyAndroidInput,
            ExpectedResponse::AndroidInputReady,
        )?;
        self.state = ClientState::Verified;
        Ok(())
    }

    pub fn finish(mut self, waydroid_stopped: bool) -> io::Result<()> {
        if self.state == ClientState::Initial {
            // The bridge was never opened; there is nothing to finish. An
            // error here would bury the real session failure under a bogus
            // cleanup message.
            return Ok(());
        }
        // Opened (verification never ran or failed) still owns a live helper
        // with the bridge installed, so Finish must stay valid from it.
        self.exchange(
            BridgeRequest::Finish { waydroid_stopped },
            ExpectedResponse::Finished,
        )
    }

    fn exchange(&mut self, request: BridgeRequest, expected: ExpectedResponse) -> io::Result<()> {
        write_frame(
            &mut self.writer,
            &RequestFrame {
                protocol_version: BRIDGE_PROTOCOL_VERSION,
                request,
            },
        )
        .map_err(|error| broker_transport_error("send a bridge request", error))?;
        let response: ResponseFrame = read_required_frame_until(
            &mut self.reader,
            Some(Instant::now() + CLIENT_RESPONSE_TIMEOUT),
        )
        .map_err(|error| broker_transport_error("read the bridge broker reply", error))?;
        if response.protocol_version != BRIDGE_PROTOCOL_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge broker response protocol version mismatch",
            ));
        }
        match (response.response, expected) {
            (BridgeResponse::Opened, ExpectedResponse::Opened)
            | (BridgeResponse::AndroidInputReady, ExpectedResponse::AndroidInputReady)
            | (BridgeResponse::Finished, ExpectedResponse::Finished) => Ok(()),
            (BridgeResponse::Error { code, detail }, _) => Err(error_from_response(&code, detail)),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge broker returned an unexpected response",
            )),
        }
    }

    #[cfg(test)]
    fn stream_fd(&self) -> RawFd {
        self.writer.as_raw_fd()
    }
}

fn validate_owned_socket(fd: &OwnedFd) -> io::Result<()> {
    let raw_fd = fd.as_raw_fd();
    if raw_fd <= libc::STDERR_FILENO {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "bridge channel cannot use a standard I/O descriptor",
        ));
    }
    // SAFETY: stat is valid writable storage and raw_fd remains owned for the
    // duration of both fstat and fcntl calls.
    let mut stat = unsafe { std::mem::zeroed::<libc::stat>() };
    if unsafe { libc::fstat(raw_fd, &mut stat) } != 0 {
        return Err(io::Error::last_os_error());
    }
    if stat.st_mode & libc::S_IFMT != libc::S_IFSOCK {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "inherited bridge descriptor is not a Unix socket",
        ));
    }
    // SAFETY: F_GETFD/F_SETFD only inspect and update this owned fd.
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(raw_fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum ExpectedResponse {
    Opened,
    AndroidInputReady,
    Finished,
}

pub fn serve_bridge_broker(
    stream: UnixStream,
    factory: Arc<dyn BridgeHelperFactory>,
) -> io::Result<()> {
    stream.set_write_timeout(Some(FRAME_WRITE_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    let mut state = ServerState::Initial;
    let mut helper: Option<Box<dyn BridgeHelperSession>> = None;
    let mut state_deadline = Some(Instant::now() + INITIAL_OPEN_TIMEOUT);

    loop {
        let frame = match read_frame_until::<RequestFrame>(&mut reader, state_deadline) {
            Ok(Some(frame)) => frame,
            Ok(None) => return Ok(()),
            Err(error) => {
                let _ = write_error(&mut writer, "invalid_frame", &error.to_string());
                return Err(error);
            }
        };
        if frame.protocol_version != BRIDGE_PROTOCOL_VERSION {
            let error = io::Error::new(
                io::ErrorKind::InvalidData,
                "bridge broker request protocol version mismatch",
            );
            let _ = write_error(&mut writer, "version_mismatch", &error.to_string());
            return Err(error);
        }

        match (state, frame.request) {
            (ServerState::Initial, BridgeRequest::Open { event_node }) => {
                if !valid_event_node_path(&event_node) {
                    let error = io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "bridge event node must be an absolute /dev/input/eventN path",
                    );
                    let _ = write_error(&mut writer, "invalid_event_node", &error.to_string());
                    return Err(error);
                }
                match factory.start(&event_node) {
                    Ok(session) => helper = Some(session),
                    Err(error) => {
                        let _ = write_error(&mut writer, "helper_start_failed", &error.to_string());
                        return Err(error);
                    }
                }
                write_response(&mut writer, BridgeResponse::Opened)?;
                state = ServerState::Opened;
                state_deadline = Some(Instant::now() + VERIFY_REQUEST_TIMEOUT);
            }
            (ServerState::Opened, BridgeRequest::VerifyAndroidInput) => {
                let result = helper
                    .as_mut()
                    .expect("opened broker owns a helper")
                    .verify_android_input();
                if let Err(error) = result {
                    let _ = write_error(&mut writer, "android_verify_failed", &error.to_string());
                    return Err(error);
                }
                write_response(&mut writer, BridgeResponse::AndroidInputReady)?;
                state = ServerState::Verified;
                state_deadline = None;
            }
            (
                ServerState::Opened | ServerState::Verified,
                BridgeRequest::Finish { waydroid_stopped },
            ) => {
                let result = helper
                    .take()
                    .expect("opened broker owns a helper")
                    .finish(waydroid_stopped);
                if let Err(error) = result {
                    let _ = write_error(&mut writer, "helper_finish_failed", &error.to_string());
                    return Err(error);
                }
                write_response(&mut writer, BridgeResponse::Finished)?;
                return Ok(());
            }
            _ => {
                let error = io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "bridge broker request is invalid in the current state",
                );
                let _ = write_error(&mut writer, "invalid_state", &error.to_string());
                return Err(error);
            }
        }
    }
}

fn valid_event_node_path(path: &Path) -> bool {
    path.parent() == Some(Path::new("/dev/input"))
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .and_then(|name| name.strip_prefix("event"))
            .is_some_and(|suffix| !suffix.is_empty() && suffix.parse::<u32>().is_ok())
}

fn invalid_client_state(operation: &str) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        format!("bridge broker cannot {operation} in the current state"),
    )
}

fn error_from_response(code: &str, detail: String) -> io::Error {
    let kind = match code {
        "invalid_event_node" => io::ErrorKind::PermissionDenied,
        "invalid_state" => io::ErrorKind::InvalidInput,
        "invalid_frame" | "version_mismatch" => io::ErrorKind::InvalidData,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(kind, detail)
}

/// Replace bare transport failures (EPIPE, EOF, reset) with an error that
/// says what actually happened and where to look. Without this the game
/// session reported a meaningless "Broken pipe (os error 32)" whenever the
/// daemon-side broker had already exited.
fn broker_transport_error(stage: &str, error: io::Error) -> io::Error {
    match error.kind() {
        io::ErrorKind::BrokenPipe
        | io::ErrorKind::UnexpectedEof
        | io::ErrorKind::ConnectionReset => io::Error::new(
            error.kind(),
            format!(
                "could not {stage}: the wroidd bridge broker closed the channel before \
                     replying. The privileged helper may have failed to stop a running Waydroid \
                     container; check the wroidd log for the helper's error (original: {error})"
            ),
        ),
        _ => error,
    }
}

fn write_error(writer: &mut UnixStream, code: &str, detail: &str) -> io::Result<()> {
    write_response(
        writer,
        BridgeResponse::Error {
            code: code.to_owned(),
            detail: detail.chars().take(MAX_ERROR_DETAIL_CHARS).collect(),
        },
    )
}

fn write_response(writer: &mut UnixStream, response: BridgeResponse) -> io::Result<()> {
    write_frame(
        writer,
        &ResponseFrame {
            protocol_version: BRIDGE_PROTOCOL_VERSION,
            response,
        },
    )
}

fn write_frame<T: Serialize>(writer: &mut UnixStream, frame: &T) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(frame).map_err(io::Error::other)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_BRIDGE_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "bridge protocol frame exceeds 4096 bytes",
        ));
    }
    writer.write_all(&bytes)?;
    writer.flush()
}

fn read_required_frame_until<T: DeserializeOwned>(
    reader: &mut BufReader<UnixStream>,
    deadline: Option<Instant>,
) -> io::Result<T> {
    read_frame_until(reader, deadline)?.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "bridge protocol peer closed before replying",
        )
    })
}

fn read_frame_until<T: DeserializeOwned>(
    reader: &mut BufReader<UnixStream>,
    deadline: Option<Instant>,
) -> io::Result<Option<T>> {
    let mut bytes = Vec::new();
    loop {
        let read_timeout = match deadline {
            Some(deadline) => Some(deadline.checked_duration_since(Instant::now()).ok_or_else(
                || io::Error::new(io::ErrorKind::TimedOut, "bridge protocol deadline expired"),
            )?),
            None => None,
        };
        reader.get_ref().set_read_timeout(read_timeout)?;
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |position| position + 1);
        bytes.extend_from_slice(&available[..count]);
        reader.consume(count);
        if bytes.len() > MAX_BRIDGE_FRAME_BYTES || newline.is_some() {
            break;
        }
    }
    if bytes.is_empty() {
        return Ok(None);
    }
    if bytes.len() > MAX_BRIDGE_FRAME_BYTES || !bytes.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid or oversized bridge protocol frame",
        ));
    }
    bytes.pop();
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    #[derive(Clone)]
    struct FakeFactory {
        calls: Arc<Mutex<Vec<String>>>,
    }

    struct FakeSession {
        calls: Arc<Mutex<Vec<String>>>,
        finished: bool,
    }

    struct FailingFactory;

    struct FailingSession;

    impl BridgeHelperFactory for FakeFactory {
        fn start(&self, event_node: &Path) -> std::io::Result<Box<dyn BridgeHelperSession>> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("open:{}", event_node.display()));
            Ok(Box::new(FakeSession {
                calls: self.calls.clone(),
                finished: false,
            }))
        }
    }

    impl BridgeHelperSession for FakeSession {
        fn verify_android_input(&mut self) -> std::io::Result<()> {
            self.calls.lock().unwrap().push("verify".to_owned());
            Ok(())
        }

        fn finish(mut self: Box<Self>, waydroid_stopped: bool) -> std::io::Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("finish:{waydroid_stopped}"));
            self.finished = true;
            Ok(())
        }
    }

    impl Drop for FakeSession {
        fn drop(&mut self) {
            if !self.finished {
                self.calls.lock().unwrap().push("drop".to_owned());
            }
        }
    }

    impl BridgeHelperFactory for FailingFactory {
        fn start(&self, _event_node: &Path) -> std::io::Result<Box<dyn BridgeHelperSession>> {
            Ok(Box::new(FailingSession))
        }
    }

    impl BridgeHelperSession for FailingSession {
        fn verify_android_input(&mut self) -> std::io::Result<()> {
            Err(std::io::Error::other(format!(
                "verify failed:{}",
                "\0".repeat(MAX_ERROR_DETAIL_CHARS)
            )))
        }

        fn finish(self: Box<Self>, _waydroid_stopped: bool) -> std::io::Result<()> {
            Ok(())
        }
    }

    type BrokerFixture = (
        BridgeBrokerClient,
        thread::JoinHandle<std::io::Result<()>>,
        Arc<Mutex<Vec<String>>>,
    );

    fn fixture() -> BrokerFixture {
        let (client, server) = UnixStream::pair().unwrap();
        let calls = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(FakeFactory {
            calls: calls.clone(),
        });
        let broker = thread::spawn(move || serve_bridge_broker(server, factory));
        (
            BridgeBrokerClient::from_stream(client).unwrap(),
            broker,
            calls,
        )
    }

    #[test]
    fn bridge_broker_accepts_only_open_verify_finish() {
        let (mut client, broker, calls) = fixture();
        client.open(Path::new("/dev/input/event42")).unwrap();
        client.verify_android_input().unwrap();
        client.finish(true).unwrap();
        broker.join().unwrap().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["open:/dev/input/event42", "verify", "finish:true"]
        );
    }

    #[test]
    fn finish_without_open_is_a_no_op() {
        let (client, broker, calls) = fixture();
        client.finish(true).unwrap();
        broker.join().unwrap().unwrap();
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn finish_after_open_without_verify_still_cleans_up() {
        let (mut client, broker, calls) = fixture();
        client.open(Path::new("/dev/input/event42")).unwrap();
        client.finish(false).unwrap();
        broker.join().unwrap().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["open:/dev/input/event42", "finish:false"]
        );
    }

    #[test]
    fn bridge_broker_rejects_reordered_calls_before_helper_activation() {
        let (mut client, broker, calls) = fixture();
        assert_eq!(
            client.verify_android_input().unwrap_err().kind(),
            std::io::ErrorKind::InvalidInput
        );
        drop(client);
        broker.join().unwrap().unwrap();
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn bridge_broker_rejects_non_event_paths() {
        let (mut client, broker, calls) = fixture();
        assert_eq!(
            client.open(Path::new("/tmp/event42")).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        drop(client);
        assert!(broker.join().unwrap().is_err());
        assert!(calls.lock().unwrap().is_empty());
    }

    #[test]
    fn bridge_broker_eof_after_open_drops_active_helper() {
        let (mut client, broker, calls) = fixture();
        client.open(Path::new("/dev/input/event8")).unwrap();
        drop(client);
        broker.join().unwrap().unwrap();
        assert_eq!(*calls.lock().unwrap(), ["open:/dev/input/event8", "drop"]);
    }

    #[test]
    fn bridge_broker_preserves_forced_finish_flag() {
        let (mut client, broker, calls) = fixture();
        client.open(Path::new("/dev/input/event8")).unwrap();
        client.verify_android_input().unwrap();
        client.finish(false).unwrap();
        broker.join().unwrap().unwrap();
        assert_eq!(
            *calls.lock().unwrap(),
            ["open:/dev/input/event8", "verify", "finish:false"]
        );
    }

    #[test]
    fn inherited_bridge_fd_must_be_a_socket_and_becomes_close_on_exec() {
        let directory = tempfile::tempdir().unwrap();
        let file = fs::File::create(directory.path().join("ordinary-file")).unwrap();
        let file_fd: OwnedFd = file.into();
        assert!(BridgeBrokerClient::from_owned_fd(file_fd).is_err());

        let (stream, peer) = UnixStream::pair().unwrap();
        let owned: OwnedFd = stream.into();
        let client = BridgeBrokerClient::from_owned_fd(owned).unwrap();
        let flags = unsafe { libc::fcntl(client.stream_fd(), libc::F_GETFD) };
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        drop(client);
        drop(peer);

        let (stream, peer) = UnixStream::pair().unwrap();
        let owned: OwnedFd = stream.into();
        let client = BridgeBrokerClient::from_owned_fd_for_peer(
            owned,
            std::process::id() as libc::pid_t,
            // SAFETY: geteuid has no preconditions.
            unsafe { libc::geteuid() },
        )
        .unwrap();
        drop(client);
        drop(peer);

        let (stream, peer) = UnixStream::pair().unwrap();
        let owned: OwnedFd = stream.into();
        assert!(BridgeBrokerClient::from_owned_fd_for_peer(
            owned,
            std::process::id() as libc::pid_t + 1,
            // SAFETY: geteuid has no preconditions.
            unsafe { libc::geteuid() },
        )
        .is_err());
        drop(peer);
    }

    #[test]
    fn oversized_and_wrong_version_frames_fail_closed() {
        for frame in [
            vec![b'x'; MAX_BRIDGE_FRAME_BYTES + 1],
            br#"{"protocolVersion":99,"method":"open","params":{"eventNode":"/dev/input/event7"}}
"#
            .to_vec(),
        ] {
            let (mut client, server) = UnixStream::pair().unwrap();
            let calls = Arc::new(Mutex::new(Vec::new()));
            let factory = Arc::new(FakeFactory {
                calls: calls.clone(),
            });
            let broker = thread::spawn(move || serve_bridge_broker(server, factory));
            use std::io::Write;
            client.write_all(&frame).unwrap();
            drop(client);
            assert!(broker.join().unwrap().is_err());
            assert!(calls.lock().unwrap().is_empty());
        }
    }

    #[test]
    fn partial_frame_bytes_cannot_extend_the_total_state_deadline() {
        let (mut sender, receiver) = UnixStream::pair().unwrap();
        let writer = thread::spawn(move || {
            for _ in 0..20 {
                if sender.write_all(b"{").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(5));
            }
        });
        let mut reader = BufReader::new(receiver);
        let started = Instant::now();

        assert!(read_frame_until::<RequestFrame>(
            &mut reader,
            Some(started + Duration::from_millis(25))
        )
        .is_err());
        assert!(started.elapsed() < Duration::from_millis(80));
        writer.join().unwrap();
    }

    #[test]
    fn oversized_helper_error_is_returned_as_a_bounded_error_frame() {
        let (client, server) = UnixStream::pair().unwrap();
        let broker = thread::spawn(move || serve_bridge_broker(server, Arc::new(FailingFactory)));
        let mut client = BridgeBrokerClient::from_stream(client).unwrap();
        client.open(Path::new("/dev/input/event7")).unwrap();

        let error = client.verify_android_input().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::Other);
        assert!(error.to_string().starts_with("verify failed:"));
        assert!(broker.join().unwrap().is_err());
    }

    #[test]
    fn transport_failures_explain_the_closed_broker_channel() {
        let error = broker_transport_error(
            "send a bridge request",
            io::Error::from_raw_os_error(libc::EPIPE),
        );
        let message = error.to_string();
        assert!(message.contains("wroidd bridge broker closed the channel"));
        assert!(message.contains("wroidd log"));

        let passthrough = broker_transport_error(
            "send a bridge request",
            io::Error::new(io::ErrorKind::InvalidData, "frame too large"),
        );
        assert_eq!(passthrough.to_string(), "frame too large");
    }

    #[test]
    fn open_against_a_closed_broker_channel_reports_an_actionable_error() {
        let (client, server) = UnixStream::pair().unwrap();
        drop(server);
        let mut client = BridgeBrokerClient::from_stream(client).unwrap();

        let error = client.open(Path::new("/dev/input/event7")).unwrap_err();

        let message = error.to_string();
        assert!(
            message.contains("wroidd bridge broker closed the channel"),
            "{message}"
        );
    }

    #[test]
    fn event_path_shape_is_exact() {
        for rejected in [
            PathBuf::from("/dev/input/event"),
            PathBuf::from("/dev/input/event7/child"),
            PathBuf::from("/dev/input/by-id/keyboard"),
            PathBuf::from("/dev/input/event-1"),
        ] {
            assert!(!valid_event_node_path(&rejected));
        }
        assert!(valid_event_node_path(Path::new("/dev/input/event0")));
        assert!(valid_event_node_path(Path::new(
            "/dev/input/event4294967295"
        )));
    }
}
