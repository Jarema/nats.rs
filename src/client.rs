// Copyright 2020-2021 The NATS Authors
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    io::{self, prelude::*, BufReader, BufWriter, Error, ErrorKind},
    mem,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::{Duration, Instant},
};

use crossbeam_channel as channel;
use crossbeam_channel::RecvTimeoutError;
use parking_lot::Mutex;

use crate::connector::{Connector, NatsStream};
use crate::proto::{self, ClientOp, ServerOp};
use crate::{inject_delay, inject_io_failure, Headers, Options, ServerInfo};

const BUF_CAPACITY: usize = 32 * 1024;

/// Client state.
///
/// NB: locking protocol - writes must ALWAYS be locked
///     first and released after when both are used.
///     Failure to follow this strict rule WILL create
///     a deadlock!
struct State {
    write: Mutex<WriteState>,
    read: Mutex<ReadState>,
}

struct WriteState {
    /// Buffered writer with an active connection.
    ///
    /// When `None`, the client is either reconnecting or closed.
    writer: Option<BufWriter<NatsStream>>,

    /// Signals to the client thread that the writer needs a flush.
    flush_kicker: channel::Sender<()>,

    /// The reconnect buffer.
    ///
    /// When the client is reconnecting, PUB messages get buffered here. When
    /// the connection is re-established, contents of the buffer are
    /// flushed to the server.
    buffer: Buffer,

    /// Next subscription ID.
    next_sid: u64,
}

struct ReadState {
    /// Current subscriptions.
    subscriptions: HashMap<u64, Subscription>,

    /// Expected pongs and their notification channels.
    pongs: VecDeque<channel::Sender<()>>,

    /// Tracks the last activity from the server.
    last_active: Instant,

    /// Used for client side monitoring of connection health.
    pings_out: u8,
}

/// A registered subscription.
struct Subscription {
    subject: String,
    queue_group: Option<String>,
    messages: channel::Sender<Message>,
}

/// A NATS client.
#[derive(Clone)]
pub struct Client {
    /// Shared client state.
    state: Arc<State>,

    /// Server info provided by the last INFO message.
    pub(crate) server_info: Arc<Mutex<ServerInfo>>,

    /// Set to `true` if shutdown has been requested.
    shutdown: Arc<Mutex<bool>>,

    /// The options that this `Client` was created using.
    pub(crate) options: Arc<Options>,
}

impl Client {
    /// Creates a new client that will begin connecting in the background.
    pub(crate) fn connect(url: &str, options: Options) -> io::Result<Client> {
        // A channel for coordinating flushes.
        let (flush_kicker, flush_wanted) = channel::bounded(1);

        // Channels for coordinating initial connect.
        let (run_sender, run_receiver) = channel::bounded(1);
        let (pong_sender, pong_receiver) = channel::bounded::<()>(1);

        // The client state.
        let client = Client {
            state: Arc::new(State {
                write: Mutex::new(WriteState {
                    writer: None,
                    flush_kicker,
                    buffer: Buffer::new(options.reconnect_buffer_size),
                    next_sid: 1,
                }),
                read: Mutex::new(ReadState {
                    subscriptions: HashMap::new(),
                    pongs: VecDeque::from(vec![pong_sender]),
                    last_active: Instant::now(),
                    pings_out: 0,
                }),
            }),
            server_info: Arc::new(Mutex::new(ServerInfo::default())),
            shutdown: Arc::new(Mutex::new(false)),
            options: Arc::new(options),
        };

        let options = client.options.clone();

        // Connector for creating the initial connection and reconnecting when
        // it is broken.
        let connector = Connector::new(url, options.clone())?;

        // Spawn the client thread responsible for:
        // - Maintaining a connection to the server and reconnecting when it is
        //   broken.
        // - Reading messages from the server and processing them.
        // - Forwarding MSG operations to subscribers.
        thread::spawn({
            let client = client.clone();
            move || {
                let res = client.run(connector);
                run_sender.send(res).ok();

                // One final flush before shutting down.
                // This way we make sure buffered published messages reach the
                // server.
                {
                    let mut write = client.state.write.lock();
                    if let Some(writer) = write.writer.as_mut() {
                        writer.flush().ok();
                    }
                }

                options.close_callback.call();
            }
        });

        channel::select! {
            recv(run_receiver) -> res => {
                res.expect("client thread has panicked")?;
                unreachable!()
            }
            recv(pong_receiver) -> _ => {}
        }

        // Spawn a thread that periodically flushes buffered messages.
        thread::spawn({
            let client = client.clone();
            move || {
                // Track last flush/write time.
                const MIN_FLUSH_BETWEEN: Duration = Duration::from_millis(5);

                // Handle recv timeouts and check if we should send a PING.
                // TODO(dlc) - Make configurable.
                const PING_INTERVAL: Duration = Duration::from_secs(2 * 60);
                const MAX_PINGS_OUT: u8 = 2;

                let mut last = Instant::now() - MIN_FLUSH_BETWEEN;

                // Wait until at least one message is buffered.
                loop {
                    match flush_wanted.recv_timeout(PING_INTERVAL) {
                        Ok(_) => {
                            let since = last.elapsed();
                            if since < MIN_FLUSH_BETWEEN {
                                thread::sleep(MIN_FLUSH_BETWEEN - since);
                            }

                            // Flush the writer.
                            let mut write = client.state.write.lock();
                            if let Some(writer) = write.writer.as_mut() {
                                let res = writer.flush();
                                last = Instant::now();
                                // If flushing fails, disconnect.
                                if res.is_err() {
                                    // NB see locking protocol for state.write and state.read
                                    writer.get_ref().shutdown();
                                    write.writer = None;
                                    let mut read = client.state.read.lock();
                                    read.pongs.clear();
                                }
                            }
                            drop(write);
                        }
                        Err(RecvTimeoutError::Timeout) => {
                            let mut write = client.state.write.lock();
                            let mut read = client.state.read.lock();

                            if read.pings_out >= MAX_PINGS_OUT {
                                if let Some(writer) = write.writer.as_mut() {
                                    writer.get_ref().shutdown();
                                }
                                write.writer = None;
                                read.pongs.clear();
                            } else if read.last_active.elapsed() > PING_INTERVAL {
                                read.pings_out += 1;
                                read.pongs.push_back(write.flush_kicker.clone());
                                // Send out a PING here.
                                if let Some(mut writer) = write.writer.as_mut() {
                                    // Ok to ignore errors here.
                                    proto::encode(&mut writer, ClientOp::Ping).ok();
                                    let res = writer.flush();
                                    if res.is_err() {
                                        // NB see locking protocol for state.write and state.read
                                        writer.get_ref().shutdown();
                                        write.writer = None;
                                        read.pongs.clear();
                                    }
                                }
                            }

                            drop(read);
                            drop(write);
                        }
                        _ => {
                            // Any other err break and exit.
                            break;
                        }
                    }
                }
            }
        });

        Ok(client)
    }

    /// Retrieves server info as received by the most recent connection.
    pub fn server_info(&self) -> ServerInfo {
        self.server_info.lock().clone()
    }

    /// Makes a round trip to the server to ensure buffered messages reach it.
    pub(crate) fn flush(&self, timeout: Duration) -> io::Result<()> {
        let pong = {
            // Inject random delays when testing.
            inject_delay();

            let mut write = self.state.write.lock();

            // Check if the client is closed.
            self.check_shutdown()?;

            let (sender, receiver) = channel::bounded(1);

            // If connected, send a PING.
            match write.writer.as_mut() {
                None => {}
                Some(mut writer) => {
                    // TODO(stjepang): We probably want to set the deadline
                    // rather than the timeout because right now the timeout
                    // applies to each write syscall individually.
                    writer.get_ref().set_write_timeout(Some(timeout))?;
                    proto::encode(&mut writer, ClientOp::Ping)?;
                    writer.flush()?;
                    writer.get_ref().set_write_timeout(None)?;
                }
            }

            // Enqueue an expected PONG.
            let mut read = self.state.read.lock();
            read.pongs.push_back(sender);

            // NB see locking protocol for state.write and state.read
            drop(read);
            drop(write);

            receiver
        };

        // Wait until the PONG operation is received.
        match pong.recv() {
            Ok(()) => Ok(()),
            Err(_) => Err(Error::new(ErrorKind::ConnectionReset, "flush failed")),
        }
    }

    /// Closes the client.
    pub(crate) fn close(&self) {
        // Inject random delays when testing.
        inject_delay();

        let mut write = self.state.write.lock();
        let mut read = self.state.read.lock();

        // Initiate shutdown process.
        if self.shutdown() {
            // Clear all subscriptions.
            let old_subscriptions = mem::take(&mut read.subscriptions);
            for (sid, _) in old_subscriptions {
                // Send an UNSUB message and ignore errors.
                if let Some(writer) = write.writer.as_mut() {
                    let max_msgs = None;
                    proto::encode(writer, ClientOp::Unsub { sid, max_msgs }).ok();
                    write.flush_kicker.try_send(()).ok();
                }
            }
            read.subscriptions.clear();

            // Flush the writer in case there are buffered messages.
            if let Some(writer) = write.writer.as_mut() {
                writer.flush().ok();
            }

            // Wake up all pending flushes.
            read.pongs.clear();

            // NB see locking protocol for state.write and state.read
            drop(read);
            drop(write);
        }
    }

    /// Kicks off the shutdown process, but doesn't wait for its completion.
    /// Returns true if this is the first attempt to shut down the system.
    pub(crate) fn shutdown(&self) -> bool {
        let mut shutdown = self.shutdown.lock();
        let old = *shutdown;
        *shutdown = true;
        !old
    }

    fn check_shutdown(&self) -> io::Result<()> {
        if *self.shutdown.lock() {
            Err(Error::new(ErrorKind::NotConnected, "the client is closed"))
        } else {
            Ok(())
        }
    }

    /// Subscribes to a subject.
    pub(crate) fn subscribe(
        &self,
        subject: &str,
        queue_group: Option<&str>,
    ) -> io::Result<(u64, channel::Receiver<Message>)> {
        // Inject random delays when testing.
        inject_delay();

        let mut write = self.state.write.lock();
        let mut read = self.state.read.lock();

        // Check if the client is closed.
        self.check_shutdown()?;

        // Generate a subject ID.
        let sid = write.next_sid;
        write.next_sid += 1;

        // If connected, send a SUB operation.
        if let Some(writer) = write.writer.as_mut() {
            let op = ClientOp::Sub {
                subject,
                queue_group,
                sid,
            };
            proto::encode(writer, op).ok();
            write.flush_kicker.try_send(()).ok();
        }

        // Register the subscription in the hash map.
        let (sender, receiver) = channel::unbounded();
        read.subscriptions.insert(
            sid,
            Subscription {
                subject: subject.to_string(),
                queue_group: queue_group.map(ToString::to_string),
                messages: sender,
            },
        );

        // NB see locking protocol for state.write and state.read
        drop(read);
        drop(write);

        Ok((sid, receiver))
    }

    /// Unsubscribes from a subject.
    pub(crate) fn unsubscribe(&self, sid: u64) -> io::Result<()> {
        // Inject random delays when testing.
        inject_delay();

        let mut write = self.state.write.lock();
        let mut read = self.state.read.lock();

        // Remove the subscription from the map.
        if read.subscriptions.remove(&sid).is_none() {
            // already unsubscribed

            // NB see locking protocol for state.write and state.read
            drop(read);
            drop(write);

            return Ok(());
        }

        // Send an UNSUB message.
        if let Some(writer) = write.writer.as_mut() {
            let max_msgs = None;
            proto::encode(writer, ClientOp::Unsub { sid, max_msgs })?;
            write.flush_kicker.try_send(()).ok();
        }

        // NB see locking protocol for state.write and state.read
        drop(read);
        drop(write);

        Ok(())
    }

    /// Publishes a message with optional reply subject and headers.
    pub fn publish(
        &self,
        subject: &str,
        reply_to: Option<&str>,
        headers: Option<&Headers>,
        msg: &[u8],
    ) -> io::Result<()> {
        // Inject random delays when testing.
        inject_delay();

        let server_info = self.server_info.lock();
        if headers.is_some() && !server_info.headers {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "the server does not support headers",
            ));
        }
        drop(server_info);

        // Check if the client is closed.
        self.check_shutdown()?;

        let op = if let Some(headers) = headers {
            ClientOp::Hpub {
                subject,
                reply_to,
                payload: msg,
                headers,
            }
        } else {
            ClientOp::Pub {
                subject,
                reply_to,
                payload: msg,
            }
        };

        let mut write = self.state.write.lock();

        let written = write.buffer.written;

        match write.writer.as_mut() {
            None => {
                // If reconnecting, write into the buffer.
                proto::encode(&mut write.buffer, op)?;
                write.buffer.flush()?;
                Ok(())
            }
            Some(mut writer) => {
                assert_eq!(written, 0);

                // If connected, write into the writer.
                let res = proto::encode(&mut writer, op);

                // If writing fails, disconnect.
                if res.is_err() {
                    write.writer = None;

                    // NB see locking protocol for state.write and state.read
                    let mut read = self.state.read.lock();
                    read.pongs.clear();
                }

                write.flush_kicker.try_send(()).ok();

                res
            }
        }
    }

    /// Attempts to publish a message without blocking.
    ///
    /// This only works when the write buffer has enough space to encode the
    /// whole message.
    pub fn try_publish(
        &self,
        subject: &str,
        reply_to: Option<&str>,
        headers: Option<&Headers>,
        msg: &[u8],
    ) -> Option<io::Result<()>> {
        // Check if the client is closed.
        if let Err(e) = self.check_shutdown() {
            return Some(Err(e));
        }

        // Estimate how many bytes the message will consume when written into
        // the stream. We must make a conservative guess: it's okay to
        // overestimate but not to underestimate.
        let mut estimate = 1024 + subject.len() + reply_to.map_or(0, str::len) + msg.len();
        if let Some(headers) = headers {
            estimate += headers
                .iter()
                .map(|(k, v)| k.len() + v.len() + 3)
                .sum::<usize>();
        }

        let op = if let Some(headers) = headers {
            ClientOp::Hpub {
                subject,
                reply_to,
                payload: msg,
                headers,
            }
        } else {
            ClientOp::Pub {
                subject,
                reply_to,
                payload: msg,
            }
        };

        let mut write = self.state.write.try_lock()?;

        match write.writer.as_mut() {
            None => {
                // If reconnecting, write into the buffer.
                let res = proto::encode(&mut write.buffer, op).and_then(|_| write.buffer.flush());
                Some(res)
            }
            Some(mut writer) => {
                // Check if there's enough space in the buffer to encode the
                // whole message.
                if BUF_CAPACITY - writer.buffer().len() < estimate {
                    return None;
                }

                // If connected, write into the writer. This is not going to
                // block because there's enough space in the buffer.
                let res = proto::encode(&mut writer, op);
                write.flush_kicker.try_send(()).ok();

                // If writing fails, disconnect.
                if res.is_err() {
                    write.writer = None;

                    // NB see locking protocol for state.write and state.read
                    let mut read = self.state.read.lock();
                    read.pongs.clear();
                }
                Some(res)
            }
        }
    }

    /// Runs the loop that connects and reconnects the client.
    fn run(&self, mut connector: Connector) -> io::Result<()> {
        let mut first_connect = true;

        loop {
            // Don't use backoff on first connect.
            let use_backoff = !first_connect;
            // Make a connection to the server.
            let (server_info, stream) = connector.connect(use_backoff)?;

            let reader = BufReader::with_capacity(BUF_CAPACITY, stream.clone());
            let writer = BufWriter::with_capacity(BUF_CAPACITY, stream);

            // Set up the new connection for this client.
            if self.reconnect(server_info, writer).is_ok() {
                // Connected! Now dispatch MSG operations.
                if !first_connect {
                    connector.get_options().reconnect_callback.call();
                }
                if self.dispatch(reader, &mut connector).is_ok() {
                    // If the client stopped gracefully, return.
                    return Ok(());
                } else {
                    connector.get_options().disconnect_callback.call();
                    self.state.write.lock().writer = None;
                }
            }

            // Clear our pings_out.
            let mut read = self.state.read.lock();
            read.pings_out = 0;
            drop(read);

            // Inject random delays when testing.
            inject_delay();

            // Check if the client is closed.
            if self.check_shutdown().is_err() {
                return Ok(());
            }
            first_connect = false;
        }
    }

    /// Puts the client back into connected state with the given writer.
    fn reconnect(
        &self,
        server_info: ServerInfo,
        mut writer: BufWriter<NatsStream>,
    ) -> io::Result<()> {
        // Inject random delays when testing.
        inject_delay();

        // Check if the client is closed.
        self.check_shutdown()?;

        let mut write = self.state.write.lock();
        let mut read = self.state.read.lock();

        // Drop the current writer, if there is one.
        write.writer = None;

        // Inject random I/O failures when testing.
        inject_io_failure()?;

        // Restart subscriptions that existed before the last reconnect.
        for (sid, subscription) in &read.subscriptions {
            // Send a SUB operation to the server.
            proto::encode(
                &mut writer,
                ClientOp::Sub {
                    subject: subscription.subject.as_str(),
                    queue_group: subscription.queue_group.as_deref(),
                    sid: *sid,
                },
            )?;
        }

        // Take out expected PONGs.
        let pongs = mem::take(&mut read.pongs);

        // Take out buffered operations.
        let buffered = write.buffer.clear();

        // Write buffered PUB operations into the new writer.
        writer.write_all(buffered)?;
        writer.flush()?;

        // All good, continue with this connection.
        *self.server_info.lock() = server_info;
        write.writer = Some(writer);

        // Complete PONGs because the connection is healthy.
        for p in pongs {
            p.try_send(()).ok();
        }

        // NB see locking protocol for state.write and state.read
        drop(read);
        drop(write);

        Ok(())
    }

    /// Updates our last activity from the server.
    fn update_activity(&self) {
        let mut read = self.state.read.lock();
        read.last_active = Instant::now();
    }

    /// Reads messages from the server and dispatches them to subscribers.
    fn dispatch(&self, mut reader: impl BufRead, connector: &mut Connector) -> io::Result<()> {
        // Handle operations received from the server.
        while let Some(op) = proto::decode(&mut reader)? {
            // Inject random delays when testing.
            inject_delay();

            if self.check_shutdown().is_err() {
                break;
            }

            // Track activity.
            self.update_activity();

            match op {
                ServerOp::Info(server_info) => {
                    for url in &server_info.connect_urls {
                        connector.add_url(url).ok();
                    }
                    *self.server_info.lock() = server_info;
                }

                ServerOp::Ping => {
                    // Respond with a PONG if connected.
                    let mut write = self.state.write.lock();
                    let read = self.state.read.lock();

                    if let Some(w) = write.writer.as_mut() {
                        proto::encode(w, ClientOp::Pong)?;
                        write.flush_kicker.try_send(()).ok();
                    }

                    // NB see locking protocol for state.write and state.read
                    drop(read);
                    drop(write);
                }

                ServerOp::Pong => {
                    // If a PONG is received while disconnected, it came from a
                    // connection that isn't alive anymore and therefore doesn't
                    // correspond to the next expected PONG.
                    let write = self.state.write.lock();
                    let mut read = self.state.read.lock();

                    // Clear any outstanding pings.
                    read.pings_out = 0;

                    if write.writer.is_some() {
                        // Take the next expected PONG and complete it by
                        // sending a message.
                        if let Some(pong) = read.pongs.pop_front() {
                            pong.try_send(()).ok();
                        }
                    }

                    // NB see locking protocol for state.write and state.read
                    drop(read);
                    drop(write);
                }

                ServerOp::Msg {
                    subject,
                    sid,
                    reply_to,
                    payload,
                } => {
                    let read = self.state.read.lock();

                    // Send the message to matching subscription.
                    if let Some(subscription) = read.subscriptions.get(&sid) {
                        let msg = Message {
                            subject,
                            reply: reply_to,
                            data: payload,
                            headers: None,
                            client: self.clone(),
                            double_acked: Default::default(),
                        };

                        // Send a message or drop it if the channel is
                        // disconnected or full.
                        subscription.messages.try_send(msg).ok();
                    }
                }

                ServerOp::Hmsg {
                    subject,
                    headers,
                    sid,
                    reply_to,
                    payload,
                } => {
                    let read = self.state.read.lock();
                    // Send the message to matching subscription.
                    if let Some(subscription) = read.subscriptions.get(&sid) {
                        let msg = Message {
                            subject,
                            reply: reply_to,
                            data: payload,
                            headers: Some(headers),
                            client: self.clone(),
                            double_acked: Default::default(),
                        };

                        // Send a message or drop it if the channel is
                        // disconnected or full.
                        subscription.messages.try_send(msg).ok();
                    }
                }

                ServerOp::Err(msg) => {
                    connector
                        .get_options()
                        .error_callback
                        .call(self, Error::new(ErrorKind::Other, msg));
                }

                ServerOp::Unknown(line) => {
                    log::warn!("unknown op: {}", line);
                }
            }
        }
        // The stream of operation is broken, meaning the connection was lost.
        Err(ErrorKind::ConnectionReset.into())
    }
}

impl fmt::Debug for Client {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("Client").finish()
    }
}

/// Reconnect buffer.
///
/// If the connection was broken and the client is currently reconnecting, PUB
/// messages get stored in this buffer of limited size. As soon as the
/// connection is then re-established, buffered messages will be sent to the
/// server.
struct Buffer {
    /// Bytes in the buffer.
    ///
    /// There are three interesting ranges in this slice:
    ///
    /// - `..flushed` contains buffered PUB messages.
    /// - `flushed..written` contains a partial PUB message at the end.
    /// - `written..` is empty space in the buffer.
    bytes: Box<[u8]>,

    /// Number of written bytes.
    written: usize,

    /// Number of bytes marked as "flushed".
    flushed: usize,
}

impl Buffer {
    /// Creates a new buffer with the given size.
    fn new(size: usize) -> Buffer {
        Buffer {
            bytes: vec![0_u8; size].into_boxed_slice(),
            written: 0,
            flushed: 0,
        }
    }

    /// Clears the buffer and returns buffered bytes.
    fn clear(&mut self) -> &[u8] {
        let buffered = &self.bytes[..self.flushed];
        self.written = 0;
        self.flushed = 0;
        buffered
    }
}

impl Write for Buffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();

        // Check if `buf` will fit into this `Buffer`.
        if self.bytes.len() - self.written < n {
            // Fill the buffer to prevent subsequent smaller writes.
            self.written = self.bytes.len();

            Err(Error::new(
                ErrorKind::Other,
                "the disconnect buffer is full",
            ))
        } else {
            // Append `buf` into the buffer.
            let range = self.written..self.written + n;
            self.bytes[range].copy_from_slice(&buf[..n]);
            self.written += n;
            Ok(n)
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.flushed = self.written;
        Ok(())
    }
}

/// A message wrapped in a struct with access to Client and all relevant methods
#[allow(clippy::module_name_repetitions)]
#[derive(Clone)]
pub struct Message {
    /// The subject this message came from.
    pub subject: String,

    /// Optional reply subject that may be used for sending a response to this
    /// message.
    pub reply: Option<String>,

    /// The message contents.
    pub data: Vec<u8>,

    /// Optional headers associated with this `Message`.
    pub headers: Option<Headers>,

    /// Client for publishing on the reply subject.
    #[doc(hidden)]
    pub(crate) client: Client,

    /// Whether this message has already been successfully double-acked
    /// using `JetStream`.
    #[doc(hidden)]
    pub double_acked: Arc<AtomicBool>,
}

/// Only Into implementation, as Client would be lost while doing the transformation other way around
#[allow(clippy::from_over_into)]
impl Into<crate::Message> for Message {
    fn into(self) -> crate::Message {
        crate::Message {
            subject: self.subject,
            reply: self.reply,
            data: self.data,
            headers: self.headers,
        }
    }
}

impl From<crate::asynk::Message> for Message {
    fn from(asynk: crate::asynk::Message) -> Message {
        Message {
            subject: asynk.subject,
            reply: asynk.reply,
            data: asynk.data,
            headers: asynk.headers,
            client: asynk.client,
            double_acked: asynk.double_acked,
        }
    }
}

impl Message {
    /// transforms raw Message into `client::Message` with Client injected
    #[allow(dead_code)] // temporary, as it will be used internally by any mothod allowing user to pass Raw Message.
    pub(crate) fn from_message(client: Client, message: Message) -> Message {
        Message {
            subject: message.subject,
            reply: message.reply,
            data: message.data,
            headers: message.headers,
            client,
            double_acked: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Respond to a request message.
    pub fn respond(&self, msg: impl AsRef<[u8]>) -> io::Result<()> {
        match self.reply.as_ref() {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "no reply subject available",
            )),
            Some(reply) => self.client.publish(reply, None, None, msg.as_ref()),
        }
    }

    /// Determine if the message is a no responders response from the server.
    pub fn is_no_responders(&self) -> bool {
        use crate::headers::STATUS_HEADER;
        if !self.data.is_empty() {
            return false;
        }
        if let Some(hdrs) = &self.headers {
            if let Some(set) = hdrs.get(STATUS_HEADER) {
                if set.get("503").is_some() {
                    return true;
                }
            }
        }
        false
    }

    /// Acknowledge a `JetStream` message with a default acknowledgement.
    /// See `AckKind` documentation for details of what other types of
    /// acks are available. If you need to send a non-default ack, use
    /// the `ack_kind` method below. If you need to block until the
    /// server acks your ack, use the `double_ack` method instead.
    ///
    /// Returns immediately if this message has already been
    /// double-acked.
    pub fn ack(&self) -> io::Result<()> {
        if self.double_acked.load(Ordering::Acquire) {
            return Ok(());
        }
        self.respond(b"")
    }

    /// Acknowledge a `JetStream` message. See `AckKind` documentation for
    /// details of what each variant means. If you need to block until the
    /// server acks your ack, use the `double_ack` method instead.
    ///
    /// Does not check whether this message has already been double-acked.
    pub fn ack_kind(&self, ack_kind: crate::jetstream::AckKind) -> io::Result<()> {
        self.respond(ack_kind)
    }

    /// Acknowledge a `JetStream` message and wait for acknowledgement from the server
    /// that it has received our ack. Retry acknowledgement until we receive a response.
    /// See `AckKind` documentation for details of what each variant means.
    ///
    /// Returns immediately if this message has already been double-acked.
    pub fn double_ack(&self, ack_kind: crate::jetstream::AckKind) -> io::Result<()> {
        if self.double_acked.load(Ordering::Acquire) {
            return Ok(());
        }
        let original_reply = match self.reply.as_ref() {
            None => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "No reply subject available (not a JetStream message)",
                ))
            }
            Some(original_reply) => original_reply,
        };
        let mut retries = 0;
        loop {
            retries += 1;
            if retries == 2 {
                log::warn!("double_ack is retrying until the server connection is reestablished");
            }
            let ack_reply = format!("_INBOX.{}", nuid::next());
            let sub_ret = self.client.subscribe(&ack_reply, None);
            if sub_ret.is_err() {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            let (sid, receiver) = sub_ret?;
            let sub =
                crate::Subscription::new(sid, ack_reply.to_string(), receiver, self.client.clone());

            let pub_ret =
                self.client
                    .publish(original_reply, Some(&ack_reply), None, ack_kind.as_ref());
            if pub_ret.is_err() {
                std::thread::sleep(std::time::Duration::from_millis(100));
                continue;
            }
            if sub
                .next_timeout(std::time::Duration::from_millis(100))
                .is_ok()
            {
                self.double_acked.store(true, Ordering::Release);
                return Ok(());
            }
        }
    }

    /// Returns the `JetStream` message ID
    /// if this is a `JetStream` message.
    /// Returns `None` if this is not
    /// a `JetStream` message with headers
    /// set.
    #[allow(clippy::eval_order_dependence)]
    pub fn jetstream_message_info(&self) -> Option<crate::jetstream::JetStreamMessageInfo<'_>> {
        const PREFIX: &str = "$JS.ACK.";
        const SKIP: usize = PREFIX.len();

        let mut reply: &str = self.reply.as_ref()?;

        if !reply.starts_with(PREFIX) {
            return None;
        }

        reply = &reply[SKIP..];

        let mut split = reply.split('.');

        // we should avoid allocating to prevent
        // large performance degradations in
        // parsing this.
        let mut tokens: [Option<&str>; 10] = [None; 10];
        let mut n_tokens = 0;
        for each_token in &mut tokens {
            if let Some(token) = split.next() {
                *each_token = Some(token);
                n_tokens += 1;
            }
        }

        let mut token_index = 0;

        macro_rules! try_parse {
            () => {
                match str::parse(try_parse!(str)) {
                    Ok(parsed) => parsed,
                    Err(e) => {
                        log::error!(
                            "failed to parse jetstream reply \
                            subject: {}, error: {:?}. Is your \
                            nats-server up to date?",
                            reply,
                            e
                        );
                        return None;
                    }
                }
            };
            (str) => {
                if let Some(next) = tokens[token_index].take() {
                    #[allow(unused)]
                    {
                        // this isn't actually unused, but it's
                        // difficult for the compiler to infer this.
                        token_index += 1;
                    }
                    next
                } else {
                    log::error!(
                        "unexpectedly few tokens while parsing \
                        jetstream reply subject: {}. Is your \
                        nats-server up to date?",
                        reply
                    );
                    return None;
                }
            };
        }

        // now we can try to parse the tokens to
        // individual types. We use an if-else
        // chain instead of a match because it
        // produces more optimal code usually,
        // and we want to try the 9 (11 - the first 2)
        // case first because we expect it to
        // be the most common. We use >= to be
        // future-proof.
        if n_tokens >= 9 {
            Some(crate::jetstream::JetStreamMessageInfo {
                domain: {
                    let domain: &str = try_parse!(str);
                    if domain == "_" {
                        None
                    } else {
                        Some(domain)
                    }
                },
                acc_hash: Some(try_parse!(str)),
                stream: try_parse!(str),
                consumer: try_parse!(str),
                delivered: try_parse!(),
                stream_seq: try_parse!(),
                consumer_seq: try_parse!(),
                published: {
                    let nanos: u64 = try_parse!();
                    let offset = std::time::Duration::from_nanos(nanos);
                    std::time::UNIX_EPOCH + offset
                },
                pending: try_parse!(),
                token: if n_tokens >= 9 {
                    Some(try_parse!(str))
                } else {
                    None
                },
            })
        } else if n_tokens == 7 {
            // we expect this to be increasingly rare, as older
            // servers are phased out.
            Some(crate::jetstream::JetStreamMessageInfo {
                domain: None,
                acc_hash: None,
                stream: try_parse!(str),
                consumer: try_parse!(str),
                delivered: try_parse!(),
                stream_seq: try_parse!(),
                consumer_seq: try_parse!(),
                published: {
                    let nanos: u64 = try_parse!();
                    let offset = std::time::Duration::from_nanos(nanos);
                    std::time::UNIX_EPOCH + offset
                },
                pending: try_parse!(),
                token: None,
            })
        } else {
            None
        }
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut body = format!("[{} bytes]", self.data.len());
        if let Ok(str) = std::str::from_utf8(&self.data) {
            body = str.to_string();
        }
        if let Some(reply) = &self.reply {
            write!(
                f,
                "Message {{\n  subject: \"{}\",\n  reply: \"{}\",\n  data: \
                 \"{}\"\n  double_ack: \"{}\"\n}}",
                self.subject,
                reply,
                body,
                self.double_acked.load(Ordering::Acquire)
            )
        } else {
            write!(
                f,
                "Message {{\n  subject: \"{}\",\n  data: \"{}\"\n  double_ack: \"{}\"\n}}",
                self.subject,
                body,
                self.double_acked.load(Ordering::Acquire)
            )
        }
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_struct("Message")
            .field("subject", &self.subject)
            .field("headers", &self.headers)
            .field("reply", &self.reply)
            .field("length", &self.data.len())
            .field("double_ack", &self.double_acked)
            .finish()
    }
}
