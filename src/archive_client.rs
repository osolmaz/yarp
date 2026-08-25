#[cfg(unix)]
mod unix {
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::archive;
    use crate::archive_protocol::{
        ACK_DEADLINE_MS, ArchiveAck, ArchiveOperation, BROKER_SCHEMA, BrokerEnvelope, BrokerHello,
        BrokerHelloAck, INGEST_SCHEMA_VERSION, MAX_FRAME_BYTES, ReplayPolicy,
    };
    use crate::archive_runtime::RuntimePaths;

    const STARTUP_TIMEOUT: Duration = Duration::from_secs(5);
    const CONNECT_DELAY: Duration = Duration::from_millis(20);
    const UNKNOWN_PRUNE_OUTCOME: &str = "archive prune acknowledgement was lost; the request may have committed and its exact outcome is unknown; inspect `yarp archive stats` or `yarp archive verify` before pruning again";

    pub(crate) fn run_ingest_bridge(
        mut input: impl Read,
        mut output: impl Write,
    ) -> Result<(), String> {
        let mut client = BrokerClient::connect_or_start()?;
        loop {
            let Some(frame) = read_raw_frame(&mut input, "ingest")? else {
                return Ok(());
            };
            let operation: ArchiveOperation = serde_json::from_slice(&frame)
                .map_err(|error| format!("invalid ingest frame: {error}"))?;
            if operation.schema_version() != INGEST_SCHEMA_VERSION {
                return Err(format!(
                    "unsupported ingest schema version {}",
                    operation.schema_version()
                ));
            }
            let ack = client.send(operation)?;
            serde_json::to_writer(&mut output, &ack)
                .map_err(|error| format!("could not write ingest acknowledgement: {error}"))?;
            output
                .write_all(b"\n")
                .and_then(|()| output.flush())
                .map_err(|error| format!("could not flush ingest acknowledgement: {error}"))?;
        }
    }

    pub(crate) fn execute(operation: ArchiveOperation) -> Result<Option<String>, String> {
        let mut client = BrokerClient::connect_or_start()?;
        let ack = client.send(operation)?;
        if ack.ok {
            Ok(ack.archive_ref)
        } else {
            Err(ack
                .error
                .unwrap_or_else(|| "archive broker rejected the request".to_owned()))
        }
    }

    struct BrokerClient {
        stream: UnixStream,
        paths: RuntimePaths,
    }

    impl BrokerClient {
        fn connect_or_start() -> Result<Self, String> {
            let archive_path = archive::archive_path()?;
            let paths = RuntimePaths::resolve(&archive_path)?;
            if let Ok(stream) = connect_and_handshake(&paths, STARTUP_TIMEOUT) {
                return Ok(Self { stream, paths });
            }

            let startup_lock = paths.lock_exclusive()?;
            match connect_and_handshake(&paths, STARTUP_TIMEOUT) {
                Ok(stream) => {
                    drop(startup_lock);
                    return Ok(Self { stream, paths });
                }
                Err(error) if UnixStream::connect(&paths.socket).is_ok() => {
                    drop(startup_lock);
                    return Err(format!(
                        "an existing archive broker rejected the handshake: {error}"
                    ));
                }
                Err(_) => {}
            }
            if !paths.no_live_broker()? {
                drop(startup_lock);
                return Err(
                    "an archive broker is running but did not accept a compatible connection"
                        .to_owned(),
                );
            }
            paths.remove_stale_socket()?;
            spawn_broker()?;
            let deadline = Instant::now() + STARTUP_TIMEOUT;
            loop {
                let remaining =
                    deadline
                        .checked_duration_since(Instant::now())
                        .ok_or_else(|| {
                            format!(
                                "archive broker did not become ready within {} ms",
                                STARTUP_TIMEOUT.as_millis()
                            )
                        })?;
                match connect_and_handshake(&paths, remaining.min(STARTUP_TIMEOUT)) {
                    Ok(stream) => {
                        drop(startup_lock);
                        return Ok(Self { stream, paths });
                    }
                    Err(error) => {
                        let remaining = deadline
                            .checked_duration_since(Instant::now())
                            .ok_or_else(|| {
                                format!(
                                    "archive broker did not become ready within {} ms: {error}",
                                    STARTUP_TIMEOUT.as_millis()
                                )
                            })?;
                        thread::sleep(CONNECT_DELAY.min(remaining));
                    }
                }
            }
        }

        fn reconnect(&mut self, deadline: Instant) -> Result<(), String> {
            loop {
                let remaining =
                    deadline
                        .checked_duration_since(Instant::now())
                        .ok_or_else(|| {
                            "archive broker acknowledgement deadline expired during reconnect"
                                .to_owned()
                        })?;
                match connect_and_handshake(&self.paths, remaining.min(STARTUP_TIMEOUT)) {
                    Ok(stream) => {
                        self.stream = stream;
                        return Ok(());
                    }
                    Err(error) => {
                        let remaining = deadline
                            .checked_duration_since(Instant::now())
                            .ok_or_else(|| {
                                format!("could not reconnect to archive broker: {error}")
                            })?;
                        thread::sleep(CONNECT_DELAY.min(remaining));
                    }
                }
            }
        }

        fn send(&mut self, operation: ArchiveOperation) -> Result<ArchiveAck, String> {
            let request_id = operation.request_id();
            let replay_policy = operation.replay_policy();
            let deadline_ms = operation.acknowledgement_deadline_ms()?;
            let deadline = Instant::now() + Duration::from_millis(deadline_ms);
            let mut envelope = BrokerEnvelope::new(operation, deadline_ms);
            match send_once(&mut self.stream, &mut envelope, request_id, deadline) {
                Ok(ack) => Ok(ack),
                Err(first_error) => match replay_policy {
                    ReplayPolicy::SafeReplay => {
                        self.reconnect(deadline)?;
                        send_once(&mut self.stream, &mut envelope, request_id, deadline).map_err(
                            |error| {
                                format!(
                                    "archive broker request failed after reconnect: {first_error}; {error}"
                                )
                            },
                        )
                    }
                    ReplayPolicy::UnknownOnDisconnect => Err(UNKNOWN_PRUNE_OUTCOME.to_owned()),
                },
            }
        }
    }

    fn connect_and_handshake(
        paths: &RuntimePaths,
        handshake_timeout: Duration,
    ) -> Result<UnixStream, String> {
        let mut stream = UnixStream::connect(&paths.socket).map_err(|error| {
            format!(
                "could not connect to archive broker {}: {error}",
                paths.socket.display()
            )
        })?;
        stream
            .set_read_timeout(Some(handshake_timeout))
            .and_then(|()| stream.set_write_timeout(Some(handshake_timeout)))
            .map_err(|error| format!("could not set archive broker handshake timeout: {error}"))?;
        let hello = BrokerHello {
            schema: BROKER_SCHEMA.to_owned(),
            binary_version: env!("CARGO_PKG_VERSION").to_owned(),
            archive_id: paths.archive_id.clone(),
        };
        write_frame(&mut stream, &hello, "broker handshake")?;
        let ack: BrokerHelloAck = read_frame(&mut stream, "broker handshake acknowledgement")?
            .ok_or_else(|| "archive broker closed during handshake".to_owned())?;
        if !ack.ok {
            return Err(ack
                .error
                .unwrap_or_else(|| "archive broker rejected handshake".to_owned()));
        }
        if ack.schema != BROKER_SCHEMA
            || ack.binary_version != env!("CARGO_PKG_VERSION")
            || ack.archive_id != paths.archive_id
        {
            return Err("archive broker handshake does not match this client".to_owned());
        }
        stream
            .set_read_timeout(None)
            .and_then(|()| stream.set_write_timeout(None))
            .map_err(|error| {
                format!("could not clear archive broker handshake timeout: {error}")
            })?;
        Ok(stream)
    }

    fn spawn_broker() -> Result<(), String> {
        let executable = std::env::current_exe()
            .map_err(|error| format!("could not locate YARP binary: {error}"))?;
        let mut child = Command::new(executable)
            .args(["archive", "broker"])
            .env("YARP_BROKER_ACTIVATED", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|error| format!("could not start archive broker: {error}"))?;
        thread::spawn(move || {
            let _ = child.wait();
        });
        Ok(())
    }

    fn send_once(
        stream: &mut UnixStream,
        envelope: &mut BrokerEnvelope,
        request_id: u64,
        deadline: Instant,
    ) -> Result<ArchiveAck, String> {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .ok_or_else(|| "archive broker acknowledgement deadline expired".to_owned())?;
        envelope.deadline_ms = u64::try_from(remaining.as_millis())
            .unwrap_or(u64::MAX)
            .clamp(1, ACK_DEADLINE_MS);
        stream
            .set_read_timeout(Some(remaining))
            .and_then(|()| stream.set_write_timeout(Some(remaining)))
            .map_err(|error| format!("could not set archive broker request timeout: {error}"))?;
        write_frame(stream, envelope, "broker request")?;
        let ack: ArchiveAck = read_frame(stream, "broker acknowledgement")?
            .ok_or_else(|| "archive broker closed before acknowledgement".to_owned())?;
        if ack.request_id != request_id {
            return Err(format!(
                "archive broker acknowledgement id {} does not match request {request_id}",
                ack.request_id
            ));
        }
        Ok(ack)
    }

    pub(crate) fn write_frame(
        output: &mut impl Write,
        value: &impl Serialize,
        label: &str,
    ) -> Result<(), String> {
        let body = serde_json::to_vec(value)
            .map_err(|error| format!("could not encode {label}: {error}"))?;
        let length =
            u64::try_from(body.len()).map_err(|_| format!("{label} is too large to frame"))?;
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(format!("invalid {label} length {length}"));
        }
        output
            .write_all(&length.to_be_bytes())
            .and_then(|()| output.write_all(&body))
            .and_then(|()| output.flush())
            .map_err(|error| format!("could not write {label}: {error}"))
    }

    pub(crate) fn read_frame<T: DeserializeOwned>(
        input: &mut impl Read,
        label: &str,
    ) -> Result<Option<T>, String> {
        let Some(body) = read_raw_frame(input, label)? else {
            return Ok(None);
        };
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| format!("invalid {label}: {error}"))
    }

    fn read_raw_frame(input: &mut impl Read, label: &str) -> Result<Option<Vec<u8>>, String> {
        let mut length_bytes = [0_u8; 8];
        match input.read(&mut length_bytes[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => {}
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(error) => return Err(format!("could not read {label} frame length: {error}")),
        }
        input
            .read_exact(&mut length_bytes[1..])
            .map_err(|error| format!("truncated {label} frame length: {error}"))?;
        let length = u64::from_be_bytes(length_bytes);
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(format!("invalid {label} frame length {length}"));
        }
        let frame_length = usize::try_from(length)
            .map_err(|_| format!("{label} frame length {length} does not fit in memory"))?;
        let mut frame = vec![0_u8; frame_length];
        input
            .read_exact(&mut frame)
            .map_err(|error| format!("could not read {label} frame: {error}"))?;
        Ok(Some(frame))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::fs;
        use std::os::unix::fs::PermissionsExt as _;
        use std::os::unix::net::UnixListener;
        use std::time::{SystemTime, UNIX_EPOCH};

        use crate::archive::{CallIdentity, SessionIdentity};
        use tempfile::tempdir;

        fn begin_operation(request_id: u64) -> ArchiveOperation {
            ArchiveOperation::BeginCall {
                request_id,
                schema_version: INGEST_SCHEMA_VERSION,
                session: SessionIdentity {
                    agent: "agent".to_owned(),
                    account: "account".to_owned(),
                    source_session_id: "session".to_owned(),
                    started_at_ms: Some(1),
                },
                call: CallIdentity {
                    source_call_id: "call".to_owned(),
                    tool_name: "read".to_owned(),
                    provider: None,
                    model: None,
                    working_directory: None,
                    started_at_ms: 2,
                    requires_streams: false,
                },
                input_before: serde_json::json!({}),
                input_after: serde_json::json!({}),
                captured_at_ms: 3,
                deadline_at_ms: i64::try_from(
                    SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .expect("current Unix time")
                        .as_millis(),
                )
                .expect("timestamp fits in i64")
                    + i64::try_from(ACK_DEADLINE_MS).expect("deadline fits in i64"),
            }
        }

        fn runtime_paths() -> (tempfile::TempDir, RuntimePaths) {
            let directory = tempdir().expect("tempdir");
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
                .expect("private tempdir");
            let archive = directory.path().join("archive.sqlite3");
            let paths =
                RuntimePaths::resolve_in(directory.path(), &archive).expect("runtime paths");
            (directory, paths)
        }

        #[test]
        fn safe_capture_reconnects_with_same_identity_and_remaining_deadline() {
            let (_directory, paths) = runtime_paths();
            let listener = UnixListener::bind(&paths.socket).expect("listener");
            let (client_stream, mut first_server) = UnixStream::pair().expect("stream pair");
            let archive_id = paths.archive_id.clone();
            let server = thread::spawn(move || {
                let first: BrokerEnvelope = read_frame(&mut first_server, "first request")
                    .expect("read first request")
                    .expect("first request");
                thread::sleep(Duration::from_millis(20));
                drop(first_server);

                let (mut retry_server, _) = listener.accept().expect("accept retry");
                let hello: BrokerHello = read_frame(&mut retry_server, "retry hello")
                    .expect("read retry hello")
                    .expect("retry hello");
                write_frame(
                    &mut retry_server,
                    &BrokerHelloAck {
                        schema: hello.schema,
                        binary_version: hello.binary_version,
                        archive_id,
                        ok: true,
                        error: None,
                    },
                    "retry hello acknowledgement",
                )
                .expect("write retry hello acknowledgement");
                let second: BrokerEnvelope = read_frame(&mut retry_server, "second request")
                    .expect("read second request")
                    .expect("second request");
                write_frame(
                    &mut retry_server,
                    &ArchiveAck::success(
                        second.operation.request_id(),
                        Some("yr_retry".to_owned()),
                    ),
                    "retry acknowledgement",
                )
                .expect("write retry acknowledgement");
                (first, second)
            });

            let mut client = BrokerClient {
                stream: client_stream,
                paths,
            };
            let ack = client.send(begin_operation(41)).expect("capture replay");
            assert!(ack.ok);
            assert_eq!(ack.archive_ref.as_deref(), Some("yr_retry"));

            let (first, second) = server.join().expect("server");
            assert_eq!(first.source_key, second.source_key);
            assert_eq!(first.sequence, second.sequence);
            assert_eq!(
                serde_json::to_value(&first.operation).expect("first operation"),
                serde_json::to_value(&second.operation).expect("second operation")
            );
            assert!(second.deadline_ms < first.deadline_ms);
            assert!(second.deadline_ms > 0);
        }

        #[test]
        fn lost_prune_acknowledgement_sends_once_and_returns_unknown_outcome() {
            let (directory, paths) = runtime_paths();
            let (client_stream, mut server_stream) = UnixStream::pair().expect("stream pair");
            let server = thread::spawn(move || {
                read_frame::<BrokerEnvelope>(&mut server_stream, "prune request")
                    .expect("read prune request")
                    .expect("prune request")
            });
            let mut client = BrokerClient {
                stream: client_stream,
                paths,
            };
            let error = client
                .send(ArchiveOperation::PruneBefore {
                    request_id: 42,
                    schema_version: INGEST_SCHEMA_VERSION,
                    timestamp_ms: 10,
                })
                .expect_err("lost prune acknowledgement");
            let envelope = server.join().expect("server");

            assert!(matches!(
                envelope.operation,
                ArchiveOperation::PruneBefore { request_id: 42, .. }
            ));
            assert_eq!(error, UNKNOWN_PRUNE_OUTCOME);
            assert!(!error.contains(directory.path().to_string_lossy().as_ref()));
        }

        #[test]
        fn broker_rejection_is_not_transport_retried() {
            let (_directory, paths) = runtime_paths();
            let (client_stream, mut server_stream) = UnixStream::pair().expect("stream pair");
            let server = thread::spawn(move || {
                let envelope: BrokerEnvelope = read_frame(&mut server_stream, "request")
                    .expect("read request")
                    .expect("request");
                write_frame(
                    &mut server_stream,
                    &ArchiveAck::failure(envelope.operation.request_id(), "conflict"),
                    "rejection",
                )
                .expect("write rejection");
            });
            let mut client = BrokerClient {
                stream: client_stream,
                paths,
            };
            let ack = client.send(begin_operation(43)).expect("broker rejection");
            server.join().expect("server");

            assert!(!ack.ok);
            assert_eq!(ack.error.as_deref(), Some("conflict"));
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::{execute, run_ingest_bridge, write_frame};

#[cfg(not(unix))]
mod unsupported {
    use std::io::{Read, Write};

    use crate::archive_protocol::ArchiveOperation;

    pub(crate) fn run_ingest_bridge(_input: impl Read, _output: impl Write) -> Result<(), String> {
        Err("the local archive broker requires Unix-domain sockets".to_owned())
    }

    pub(crate) fn execute(_operation: ArchiveOperation) -> Result<Option<String>, String> {
        Err("the local archive broker requires Unix-domain sockets".to_owned())
    }
}

#[cfg(not(unix))]
pub(crate) use unsupported::{execute, run_ingest_bridge};
