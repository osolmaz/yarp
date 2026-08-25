#[cfg(unix)]
mod unix {
    use std::collections::{HashMap, VecDeque};
    use std::io::Read;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
    use std::sync::{Arc, Condvar, Mutex};
    use std::thread;
    use std::time::{Duration, Instant};

    use crate::archive::{Archive, PreparedArchiveOperation};
    use crate::archive_client::write_frame;
    use crate::archive_protocol::{
        ACK_DEADLINE_MS, ArchiveAck, BROKER_SCHEMA, BrokerEnvelope, BrokerHello, BrokerHelloAck,
        MAX_FRAME_BYTES,
    };
    use crate::archive_runtime::RuntimePaths;

    const GLOBAL_REQUEST_LIMIT: usize = 256;
    const SOURCE_SEQUENCE_LIMIT: usize = 4096;
    const HANDSHAKE_MAX_BYTES: u64 = 4096;
    const BATCH_REQUEST_LIMIT: usize = 32;
    const BATCH_BYTE_LIMIT: u64 = 8 * 1024 * 1024;
    const BATCH_WAIT: Duration = Duration::from_millis(2);
    const IDLE_GRACE: Duration = Duration::from_mins(1);
    const ACCEPT_DELAY: Duration = Duration::from_millis(10);
    const BUSY_RETRY_MIN: Duration = Duration::from_millis(10);
    const BUSY_RETRY_MAX: Duration = Duration::from_millis(250);

    pub(crate) fn run() -> Result<(), String> {
        #[cfg(debug_assertions)]
        let idle_grace = std::env::var("YARP_BROKER_IDLE_MS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .map_or(IDLE_GRACE, Duration::from_millis);
        #[cfg(not(debug_assertions))]
        let idle_grace = IDLE_GRACE;
        run_with_idle(idle_grace)
    }

    fn run_with_idle(idle_grace: Duration) -> Result<(), String> {
        #[cfg(debug_assertions)]
        let serial = std::env::var_os("YARP_BROKER_SERIAL").is_some();
        #[cfg(not(debug_assertions))]
        let serial = false;
        let batch_request_limit = if serial { 1 } else { BATCH_REQUEST_LIMIT };
        let batch_wait = if serial { Duration::ZERO } else { BATCH_WAIT };
        let archive_path = crate::archive::archive_path()?;
        let paths = RuntimePaths::resolve(&archive_path)?;
        let activated = std::env::var_os("YARP_BROKER_ACTIVATED").is_some();
        let startup_lock = if activated {
            None
        } else {
            Some(paths.lock_exclusive()?)
        };
        if !paths.no_live_broker()? {
            return Err("an archive broker is already running".to_owned());
        }
        let lifetime_lock = paths.lock_lifetime()?;
        if !activated {
            if UnixStream::connect(&paths.socket).is_ok() {
                return Err("an archive broker is already running".to_owned());
            }
            paths.remove_stale_socket()?;
        }
        let listener = UnixListener::bind(&paths.socket).map_err(|error| {
            format!(
                "could not bind archive broker socket {}: {error}",
                paths.socket.display()
            )
        })?;
        paths.secure_socket()?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("could not configure archive broker listener: {error}"))?;

        let mut archive = match Archive::open() {
            Ok(archive) => archive,
            Err(error) => {
                let _ = paths.cleanup();
                return Err(error);
            }
        };
        archive.configure_for_broker()?;
        drop(startup_lock);

        let (sender, receiver) = mpsc::sync_channel(GLOBAL_REQUEST_LIMIT);
        let active_clients = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let budget = Arc::new(ByteBudget::new(MAX_FRAME_BYTES));
        let accept_thread = spawn_accept_loop(
            listener,
            sender,
            Arc::clone(&active_clients),
            Arc::clone(&shutdown),
            &budget,
            paths.archive_id.clone(),
        );

        let result = writer_loop(
            &mut archive,
            &receiver,
            &active_clients,
            idle_grace,
            batch_request_limit,
            batch_wait,
        );
        let shutdown_lock = paths.lock_exclusive()?;
        shutdown.store(true, Ordering::Release);
        let _ = accept_thread.join();
        drop(receiver);
        let cleanup_result = paths.cleanup();
        let checkpoint_result = archive.checkpoint();
        drop(lifetime_lock);
        drop(shutdown_lock);
        result.and(checkpoint_result).and(cleanup_result)
    }

    struct Work {
        source_key: String,
        redactions: Vec<String>,
        sequence: u8,
        ends_source_sequence: bool,
        deadline: Instant,
        request_id: u64,
        operation: Option<crate::archive_protocol::ArchiveOperation>,
        prepared: Option<PreparedArchiveOperation>,
        response: SyncSender<ArchiveAck>,
        _permit: BudgetPermit,
    }

    fn spawn_accept_loop(
        listener: UnixListener,
        sender: SyncSender<Work>,
        active_clients: Arc<AtomicUsize>,
        shutdown: Arc<AtomicBool>,
        budget: &Arc<ByteBudget>,
        archive_id: String,
    ) -> thread::JoinHandle<()> {
        let budget = Arc::clone(budget);
        thread::spawn(move || {
            while !shutdown.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        if active_clients.load(Ordering::Acquire) >= GLOBAL_REQUEST_LIMIT {
                            drop(stream);
                            continue;
                        }
                        let sender = sender.clone();
                        let active_clients = Arc::clone(&active_clients);
                        let budget = Arc::clone(&budget);
                        let archive_id = archive_id.clone();
                        active_clients.fetch_add(1, Ordering::AcqRel);
                        thread::spawn(move || {
                            let _guard = ClientGuard(active_clients);
                            let _ = handle_client(stream, &sender, &budget, &archive_id);
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(ACCEPT_DELAY);
                    }
                    Err(_) => break,
                }
            }
        })
    }

    struct ClientGuard(Arc<AtomicUsize>);

    impl Drop for ClientGuard {
        fn drop(&mut self) {
            self.0.fetch_sub(1, Ordering::AcqRel);
        }
    }

    fn handle_client(
        mut stream: UnixStream,
        sender: &SyncSender<Work>,
        budget: &Arc<ByteBudget>,
        archive_id: &str,
    ) -> Result<(), String> {
        require_same_user(&stream)?;
        stream
            .set_read_timeout(Some(Duration::from_millis(ACK_DEADLINE_MS)))
            .and_then(|()| stream.set_write_timeout(Some(Duration::from_millis(ACK_DEADLINE_MS))))
            .map_err(|error| format!("could not set archive broker client timeout: {error}"))?;
        let hello: BrokerHello =
            read_limited_frame(&mut stream, "broker handshake", HANDSHAKE_MAX_BYTES)?
                .ok_or_else(|| "archive broker client closed before handshake".to_owned())?;
        let hello_error = validate_hello(&hello, archive_id).err();
        let hello_ack = BrokerHelloAck {
            schema: BROKER_SCHEMA.to_owned(),
            binary_version: env!("CARGO_PKG_VERSION").to_owned(),
            archive_id: archive_id.to_owned(),
            ok: hello_error.is_none(),
            error: hello_error,
        };
        write_frame(&mut stream, &hello_ack, "broker handshake acknowledgement")?;
        if !hello_ack.ok {
            return Ok(());
        }

        loop {
            let request_started = Instant::now();
            let Some((envelope, permit)) =
                read_bounded_envelope(&mut stream, budget, request_started)?
            else {
                return Ok(());
            };
            let request_id = envelope.operation.request_id();
            if let Err(error) = envelope.validate() {
                write_frame(
                    &mut stream,
                    &ArchiveAck::failure(request_id, error),
                    "broker acknowledgement",
                )?;
                continue;
            }
            let deadline = request_started + Duration::from_millis(envelope.deadline_ms);
            let redactions = envelope.operation.redactions();
            let ends_source_sequence = envelope.operation.ends_source_sequence();
            let (response_sender, response_receiver) = mpsc::sync_channel(1);
            let work = Work {
                source_key: envelope.source_key,
                redactions,
                sequence: envelope.sequence,
                ends_source_sequence,
                deadline,
                request_id,
                operation: Some(envelope.operation),
                prepared: None,
                response: response_sender,
                _permit: permit,
            };
            send_with_deadline(sender, work, deadline)?;
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| "archive broker request deadline expired".to_owned())?;
            let ack = response_receiver
                .recv_timeout(remaining)
                .map_err(|error| format!("archive broker response failed: {error}"))?;
            write_frame(&mut stream, &ack, "broker acknowledgement")?;
        }
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn require_same_user(stream: &UnixStream) -> Result<(), String> {
        let peer = rustix::net::sockopt::socket_peercred(stream)
            .map_err(|error| format!("could not authenticate archive broker peer: {error}"))?;
        if peer.uid != rustix::process::getuid() {
            return Err("archive broker rejected a peer owned by another user".to_owned());
        }
        Ok(())
    }

    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    fn require_same_user(_stream: &UnixStream) -> Result<(), String> {
        Ok(())
    }

    fn read_limited_frame<T: serde::de::DeserializeOwned>(
        input: &mut impl Read,
        label: &str,
        maximum: u64,
    ) -> Result<Option<T>, String> {
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
        if length == 0 || length > maximum {
            return Err(format!("invalid {label} frame length {length}"));
        }
        let length =
            usize::try_from(length).map_err(|_| format!("{label} frame does not fit in memory"))?;
        let mut body = vec![0_u8; length];
        input
            .read_exact(&mut body)
            .map_err(|error| format!("could not read {label} frame: {error}"))?;
        serde_json::from_slice(&body)
            .map(Some)
            .map_err(|error| format!("invalid {label}: {error}"))
    }

    fn validate_hello(hello: &BrokerHello, archive_id: &str) -> Result<(), String> {
        if hello.schema != BROKER_SCHEMA {
            return Err("unsupported broker schema".to_owned());
        }
        if hello.binary_version != env!("CARGO_PKG_VERSION") {
            return Err("archive broker and client versions do not match".to_owned());
        }
        if hello.archive_id != archive_id {
            return Err("archive broker identity does not match requested archive".to_owned());
        }
        Ok(())
    }

    fn read_bounded_envelope(
        stream: &mut UnixStream,
        budget: &Arc<ByteBudget>,
        started: Instant,
    ) -> Result<Option<(BrokerEnvelope, BudgetPermit)>, String> {
        let mut length_bytes = [0_u8; 8];
        match stream.read(&mut length_bytes[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => {}
            Ok(_) => unreachable!("one-byte read returned more than one byte"),
            Err(error) => {
                return Err(format!(
                    "could not read broker request frame length: {error}"
                ));
            }
        }
        stream
            .read_exact(&mut length_bytes[1..])
            .map_err(|error| format!("truncated broker request frame length: {error}"))?;
        let length = u64::from_be_bytes(length_bytes);
        if length == 0 || length > MAX_FRAME_BYTES {
            return Err(format!("invalid broker request frame length {length}"));
        }
        let deadline = started + Duration::from_millis(ACK_DEADLINE_MS);
        let permit = budget.acquire(length, deadline)?;
        let length = usize::try_from(length)
            .map_err(|_| "broker request frame does not fit in memory".to_owned())?;
        let mut body = vec![0_u8; length];
        stream
            .read_exact(&mut body)
            .map_err(|error| format!("could not read broker request frame: {error}"))?;
        let envelope = serde_json::from_slice(&body)
            .map_err(|error| format!("invalid broker request: {error}"))?;
        Ok(Some((envelope, permit)))
    }

    fn send_with_deadline(
        sender: &SyncSender<Work>,
        mut work: Work,
        deadline: Instant,
    ) -> Result<(), String> {
        loop {
            match sender.try_send(work) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Full(returned)) if Instant::now() < deadline => {
                    work = returned;
                    thread::sleep(ACCEPT_DELAY);
                }
                Err(TrySendError::Full(_)) => {
                    return Err("archive broker queue deadline expired".to_owned());
                }
                Err(TrySendError::Disconnected(_)) => {
                    return Err("archive broker stopped accepting work".to_owned());
                }
            }
        }
    }

    #[derive(Default)]
    struct SourceSequences {
        values: HashMap<String, u8>,
        order: VecDeque<String>,
    }

    impl SourceSequences {
        fn is_regression(&self, source_key: &str, sequence: u8) -> bool {
            self.values
                .get(source_key)
                .is_some_and(|previous| sequence < *previous)
        }

        fn record(&mut self, source_key: &str, sequence: u8) {
            if self.values.contains_key(source_key) {
                self.order.retain(|key| key != source_key);
            } else {
                while self.values.len() >= SOURCE_SEQUENCE_LIMIT {
                    let Some(oldest) = self.order.pop_front() else {
                        self.values.clear();
                        break;
                    };
                    self.values.remove(&oldest);
                }
            }
            self.values.insert(source_key.to_owned(), sequence);
            self.order.push_back(source_key.to_owned());
        }

        fn finish(&mut self, source_key: &str) {
            self.values.remove(source_key);
            self.order.retain(|key| key != source_key);
        }
    }

    fn writer_loop(
        archive: &mut Archive,
        receiver: &Receiver<Work>,
        active_clients: &AtomicUsize,
        idle_grace: Duration,
        batch_request_limit: usize,
        batch_wait: Duration,
    ) -> Result<(), String> {
        let mut deferred = VecDeque::new();
        let mut source_sequences = SourceSequences::default();
        let mut idle_since = Instant::now();
        loop {
            let first = if let Some(work) = deferred.pop_front() {
                Some(work)
            } else {
                match receiver.recv_timeout(Duration::from_millis(100)) {
                    Ok(work) => Some(work),
                    Err(mpsc::RecvTimeoutError::Timeout) => None,
                    Err(mpsc::RecvTimeoutError::Disconnected) => return Ok(()),
                }
            };
            let Some(first) = first else {
                if active_clients.load(Ordering::Acquire) == 0 {
                    if idle_since.elapsed() >= idle_grace {
                        return Ok(());
                    }
                } else {
                    idle_since = Instant::now();
                }
                continue;
            };
            idle_since = Instant::now();
            let mut candidates = VecDeque::from([first]);
            let collect_until = Instant::now() + batch_wait;
            while candidates.len() < batch_request_limit && Instant::now() < collect_until {
                match receiver.try_recv() {
                    Ok(work) => candidates.push_back(work),
                    Err(mpsc::TryRecvError::Empty) => thread::yield_now(),
                    Err(mpsc::TryRecvError::Disconnected) => break,
                }
            }

            let mut ready = Vec::new();
            let mut batch_bytes = 0_u64;
            while let Some(mut work) = candidates.pop_front() {
                if Instant::now() >= work.deadline {
                    let _ = work.response.send(ArchiveAck::failure(
                        work.request_id,
                        "archive broker request deadline expired",
                    ));
                    continue;
                }
                if source_sequences.is_regression(&work.source_key, work.sequence) {
                    let _ = work.response.send(ArchiveAck::failure(
                        work.request_id,
                        "archive operation arrived after a later operation for the same source",
                    ));
                    continue;
                }
                if work.prepared.is_none() {
                    let operation = work
                        .operation
                        .take()
                        .ok_or_else(|| "archive broker work is missing its operation".to_owned())?;
                    match archive.prepare_operation(operation) {
                        Ok(prepared) => work.prepared = Some(prepared),
                        Err(error) => {
                            let safe_error = safe_request_error(&error, &work.redactions);
                            let _ = work
                                .response
                                .send(ArchiveAck::failure(work.request_id, safe_error));
                            continue;
                        }
                    }
                }
                let bytes = work
                    .prepared
                    .as_ref()
                    .ok_or_else(|| "archive broker work was not prepared".to_owned())?
                    .estimated_bytes();
                if !ready.is_empty()
                    && (ready.len() >= batch_request_limit
                        || batch_bytes.saturating_add(bytes) > BATCH_BYTE_LIMIT)
                {
                    while let Some(candidate) = candidates.pop_back() {
                        deferred.push_front(candidate);
                    }
                    deferred.push_front(work);
                    break;
                }
                batch_bytes = batch_bytes.saturating_add(bytes);
                ready.push(work);
                if bytes > BATCH_BYTE_LIMIT {
                    while let Some(candidate) = candidates.pop_back() {
                        deferred.push_front(candidate);
                    }
                    break;
                }
            }
            if ready.is_empty() {
                continue;
            }
            apply_ready_batch(archive, &mut ready, &mut source_sequences);
        }
    }

    fn apply_ready_batch(
        archive: &mut Archive,
        ready: &mut [Work],
        source_sequences: &mut SourceSequences,
    ) {
        let Some(deadline) = ready.iter().map(|value| value.deadline).min() else {
            return;
        };
        let mut delay = BUSY_RETRY_MIN;
        loop {
            let result = archive.apply_prepared_batch(
                ready.iter_mut().filter_map(|value| value.prepared.as_mut()),
                deadline,
            );
            match result {
                Ok(results) => {
                    for (ready, result) in ready.iter().zip(results) {
                        if ready.ends_source_sequence {
                            source_sequences.finish(&ready.source_key);
                        }
                        let ack = match result {
                            Ok(archive_ref) => {
                                if !ready.ends_source_sequence {
                                    source_sequences.record(&ready.source_key, ready.sequence);
                                }
                                ArchiveAck::success(ready.request_id, archive_ref)
                            }
                            Err(error) => ArchiveAck::failure(
                                ready.request_id,
                                safe_request_error(&error, &ready.redactions),
                            ),
                        };
                        let _ = ready.response.send(ack);
                    }
                    return;
                }
                Err(error) if error.is_busy() && Instant::now() < deadline => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        fail_batch(ready, error.message());
                        return;
                    }
                    thread::sleep(delay.min(remaining));
                    delay = (delay * 2).min(BUSY_RETRY_MAX);
                }
                Err(error) => {
                    fail_batch(ready, error.message());
                    return;
                }
            }
        }
    }

    fn fail_batch(ready: &[Work], error: &str) {
        for ready in ready {
            let safe_error = safe_request_error(error, &ready.redactions);
            let _ = ready
                .response
                .send(ArchiveAck::failure(ready.request_id, safe_error));
        }
    }

    fn safe_request_error(error: &str, redactions: &[String]) -> String {
        const MAX_ERROR_CHARS: usize = 1024;
        let mut safe = error.to_owned();
        for value in redactions.iter().filter(|value| !value.is_empty()) {
            safe = safe.replace(value, "<source>");
        }
        safe.chars()
            .map(|character| {
                if character.is_control() {
                    ' '
                } else {
                    character
                }
            })
            .take(MAX_ERROR_CHARS)
            .collect()
    }

    struct ByteBudget {
        state: Mutex<u64>,
        changed: Condvar,
        limit: u64,
    }

    impl ByteBudget {
        const fn new(limit: u64) -> Self {
            Self {
                state: Mutex::new(0),
                changed: Condvar::new(),
                limit,
            }
        }

        fn acquire(
            self: &Arc<Self>,
            bytes: u64,
            deadline: Instant,
        ) -> Result<BudgetPermit, String> {
            let mut used = self
                .state
                .lock()
                .map_err(|_| "archive broker byte budget lock is poisoned".to_owned())?;
            while used.saturating_add(bytes) > self.limit {
                let remaining = deadline
                    .checked_duration_since(Instant::now())
                    .ok_or_else(|| "archive broker byte budget deadline expired".to_owned())?;
                let (next, timeout) = self
                    .changed
                    .wait_timeout(used, remaining)
                    .map_err(|_| "archive broker byte budget lock is poisoned".to_owned())?;
                used = next;
                if timeout.timed_out() && used.saturating_add(bytes) > self.limit {
                    return Err("archive broker byte budget deadline expired".to_owned());
                }
            }
            *used = used.saturating_add(bytes);
            Ok(BudgetPermit {
                budget: Arc::clone(self),
                bytes,
            })
        }
    }

    struct BudgetPermit {
        budget: Arc<ByteBudget>,
        bytes: u64,
    }

    impl Drop for BudgetPermit {
        fn drop(&mut self) {
            if let Ok(mut used) = self.budget.state.lock() {
                *used = used.saturating_sub(self.bytes);
                self.budget.changed.notify_all();
            }
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn source_sequence_state_is_bounded_and_terminal_operations_release_it() {
            let mut sequences = SourceSequences::default();
            for index in 0..SOURCE_SEQUENCE_LIMIT + 10 {
                sequences.record(&format!("source-{index}"), 1);
            }
            assert_eq!(sequences.values.len(), SOURCE_SEQUENCE_LIMIT);
            assert_eq!(sequences.order.len(), SOURCE_SEQUENCE_LIMIT);
            assert!(!sequences.values.contains_key("source-0"));

            sequences.record("active", 4);
            assert!(sequences.is_regression("active", 3));
            sequences.finish("active");
            assert!(!sequences.is_regression("active", 3));
        }

        #[test]
        fn handshake_errors_do_not_echo_untrusted_values() {
            let hello = BrokerHello {
                schema: "secret-schema".repeat(100),
                binary_version: "secret-version".repeat(100),
                archive_id: "archive".to_owned(),
            };
            assert_eq!(
                validate_hello(&hello, "archive").unwrap_err(),
                "unsupported broker schema"
            );
            assert!(
                !validate_hello(&hello, "archive")
                    .unwrap_err()
                    .contains("secret")
            );

            let version = BrokerHello {
                schema: BROKER_SCHEMA.to_owned(),
                binary_version: "secret-version".repeat(100),
                archive_id: "archive".to_owned(),
            };
            assert_eq!(
                validate_hello(&version, "archive").unwrap_err(),
                "archive broker and client versions do not match"
            );
        }

        #[test]
        fn request_errors_are_bounded_and_hide_source_identity() {
            let redactions = vec!["session-secret".to_owned(), "call-secret".to_owned()];
            let error = format!(
                "tool call call-secret in session-secret failed\n{}",
                "x".repeat(2048)
            );
            let safe = safe_request_error(&error, &redactions);
            assert!(!safe.contains("call-secret"));
            assert!(!safe.contains("session-secret"));
            assert!(!safe.contains('\n'));
            assert!(safe.chars().count() <= 1024);
        }

        #[test]
        fn byte_budget_is_released_when_a_request_finishes() {
            let budget = Arc::new(ByteBudget::new(10));
            let permit = budget
                .acquire(10, Instant::now() + Duration::from_secs(1))
                .expect("full budget");
            assert!(
                budget
                    .acquire(1, Instant::now() + Duration::from_millis(1))
                    .is_err()
            );
            drop(permit);
            assert!(
                budget
                    .acquire(10, Instant::now() + Duration::from_secs(1))
                    .is_ok()
            );
        }
    }
}

#[cfg(unix)]
pub(crate) use unix::run;

#[cfg(not(unix))]
pub(crate) fn run() -> Result<(), String> {
    Err("the local archive broker requires Unix-domain sockets".to_owned())
}
