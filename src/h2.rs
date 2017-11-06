use std::{io, cmp, mem};
use std::rc::Rc;
use std::io::{Read, Write};
use std::cell::UnsafeCell;
use std::time::Duration;
use std::collections::VecDeque;

use actix::Arbiter;
use http::request::Parts;
use http::header::CONTENT_ENCODING;
use http2::{Reason, RecvStream};
use http2::server::{Server, Handshake, Respond};
use bytes::{Buf, Bytes};
use futures::{Async, Poll, Future, Stream};
use tokio_io::{AsyncRead, AsyncWrite};
use tokio_core::reactor::Timeout;

use task::Task;
use h2writer::H2Writer;
use channel::HttpHandler;
use httpcodes::HTTPNotFound;
use httprequest::HttpRequest;
use httpresponse::ContentEncoding;
use payload::{Payload, PayloadError, PayloadSender, PayloadWriter, EncodedPayload};

const KEEPALIVE_PERIOD: u64 = 15; // seconds

pub(crate) struct Http2<T, A, H>
    where T: AsyncRead + AsyncWrite + 'static, A: 'static, H: 'static
{
    router: Rc<Vec<H>>,
    #[allow(dead_code)]
    addr: A,
    state: State<IoWrapper<T>>,
    disconnected: bool,
    tasks: VecDeque<Entry>,
    keepalive_timer: Option<Timeout>,
}

enum State<T: AsyncRead + AsyncWrite> {
    Handshake(Handshake<T, Bytes>),
    Server(Server<T, Bytes>),
    Empty,
}

impl<T, A, H> Http2<T, A, H>
    where T: AsyncRead + AsyncWrite + 'static,
          A: 'static,
          H: HttpHandler + 'static
{
    pub fn new(stream: T, addr: A, router: Rc<Vec<H>>, buf: Bytes) -> Self {
        Http2{ router: router,
               addr: addr,
               disconnected: false,
               tasks: VecDeque::new(),
               state: State::Handshake(
                   Server::handshake(IoWrapper{unread: Some(buf), inner: stream})),
               keepalive_timer: None,
        }
    }

    pub fn poll(&mut self) -> Poll<(), ()> {
        // server
        if let State::Server(ref mut server) = self.state {

            // keep-alive timer
            if let Some(ref mut timeout) = self.keepalive_timer {
                match timeout.poll() {
                    Ok(Async::Ready(_)) =>
                        return Ok(Async::Ready(())),
                    Ok(Async::NotReady) => (),
                    Err(_) => unreachable!(),
                }
            }

            loop {
                let mut not_ready = true;

                // check in-flight connections
                for item in &mut self.tasks {
                    // read payload
                    item.poll_payload();

                    if !item.eof {
                        let req = unsafe {item.req.get().as_mut().unwrap()};
                        match item.task.poll_io(&mut item.stream, req) {
                            Ok(Async::Ready(ready)) => {
                                item.eof = true;
                                if ready {
                                    item.finished = true;
                                }
                                not_ready = false;
                            },
                            Ok(Async::NotReady) => (),
                            Err(_) => {
                                item.eof = true;
                                item.error = true;
                                item.stream.reset(Reason::INTERNAL_ERROR);
                            }
                        }
                    } else if !item.finished {
                        match item.task.poll() {
                            Ok(Async::NotReady) => (),
                            Ok(Async::Ready(_)) => {
                                not_ready = false;
                                item.finished = true;
                            },
                            Err(_) => {
                                item.error = true;
                                item.finished = true;
                            }
                        }
                    }
                }

                // cleanup finished tasks
                while !self.tasks.is_empty() {
                    if self.tasks[0].eof && self.tasks[0].finished || self.tasks[0].error {
                        self.tasks.pop_front();
                    } else {
                        break
                    }
                }

                // get request
                if !self.disconnected {
                    match server.poll() {
                        Ok(Async::Ready(None)) => {
                            not_ready = false;
                            self.disconnected = true;
                            for entry in &mut self.tasks {
                                entry.task.disconnected()
                            }
                        },
                        Ok(Async::Ready(Some((req, resp)))) => {
                            not_ready = false;
                            let (parts, body) = req.into_parts();
                            self.tasks.push_back(
                                Entry::new(parts, body, resp, &self.router));
                            self.keepalive_timer.take();
                        }
                        Ok(Async::NotReady) => {
                            // start keep-alive timer
                            if self.tasks.is_empty() {
                                if self.keepalive_timer.is_none() {
                                    trace!("Start keep-alive timer");
                                    let mut timeout = Timeout::new(
                                        Duration::new(KEEPALIVE_PERIOD, 0),
                                        Arbiter::handle()).unwrap();
                                    // register timeout
                                    let _ = timeout.poll();
                                    self.keepalive_timer = Some(timeout);
                                }
                            }
                        }
                        Err(err) => {
                            trace!("Connection error: {}", err);
                            self.disconnected = true;
                            for entry in &mut self.tasks {
                                entry.task.disconnected()
                            }
                            self.keepalive_timer.take();
                        },
                    }
                }

                if not_ready {
                    if self.tasks.is_empty() && self.disconnected {
                        return Ok(Async::Ready(()))
                    } else {
                        return Ok(Async::NotReady)
                    }
                }
            }
        }

        // handshake
        self.state = if let State::Handshake(ref mut handshake) = self.state {
            match handshake.poll() {
                Ok(Async::Ready(srv)) => {
                    State::Server(srv)
                },
                Ok(Async::NotReady) =>
                    return Ok(Async::NotReady),
                Err(err) => {
                    trace!("Error handling connection: {}", err);
                    return Err(())
                }
            }
        } else {
            mem::replace(&mut self.state, State::Empty)
        };

        self.poll()
    }
}

struct PayloadInfo(PayloadInfoItem);
enum PayloadInfoItem {
    Sender(PayloadSender),
    Encoding(EncodedPayload),
}

impl PayloadInfo {

    fn as_mut(&mut self) -> &mut PayloadWriter {
        match self.0 {
            PayloadInfoItem::Sender(ref mut sender) => sender,
            PayloadInfoItem::Encoding(ref mut enc) => enc,
        }
    }
}

struct Entry {
    task: Task,
    req: UnsafeCell<HttpRequest>,
    payload: PayloadInfo,
    recv: RecvStream,
    stream: H2Writer,
    eof: bool,
    error: bool,
    finished: bool,
    reof: bool,
    capacity: usize,
}

impl Entry {
    fn new<H>(parts: Parts,
              recv: RecvStream,
              resp: Respond<Bytes>,
              router: &Rc<Vec<H>>) -> Entry
        where H: HttpHandler + 'static
    {
        let path = parts.uri.path().to_owned();
        let query = parts.uri.query().unwrap_or("").to_owned();

        let mut req = HttpRequest::new(
            parts.method, path, parts.version, parts.headers, query);

        // Payload and Content-Encoding
        let (psender, payload) = Payload::new(false);
        let enc = if let Some(enc) = req.headers().get(CONTENT_ENCODING) {
            if let Ok(enc) = enc.to_str() {
                ContentEncoding::from(enc)
            } else {
                ContentEncoding::Auto
            }
        } else {
            ContentEncoding::Auto
        };
        let psender = match enc {
            ContentEncoding::Auto | ContentEncoding::Identity =>
                PayloadInfoItem::Sender(psender),
            _ => PayloadInfoItem::Encoding(EncodedPayload::new(psender, enc)),
        };

        // start request processing
        let mut task = None;
        for h in router.iter() {
            if req.path().starts_with(h.prefix()) {
                task = Some(h.handle(&mut req, payload));
                break
            }
        }

        Entry {task: task.unwrap_or_else(|| Task::reply(HTTPNotFound)),
               req: UnsafeCell::new(req),
               payload: PayloadInfo(psender),
               recv: recv,
               stream: H2Writer::new(resp),
               eof: false,
               error: false,
               finished: false,
               reof: false,
               capacity: 0,
        }
    }

    fn poll_payload(&mut self) {
        if !self.reof {
            match self.recv.poll() {
                Ok(Async::Ready(Some(chunk))) => {
                    self.payload.as_mut().feed_data(chunk);
                },
                Ok(Async::Ready(None)) => {
                    self.reof = true;
                },
                Ok(Async::NotReady) => (),
                Err(err) => {
                    self.payload.as_mut().set_error(PayloadError::Http2(err))
                }
            }

            let capacity = self.payload.as_mut().capacity();
            if self.capacity != capacity {
                self.capacity = capacity;
                if let Err(err) = self.recv.release_capacity().release_capacity(capacity) {
                    self.payload.as_mut().set_error(PayloadError::Http2(err))
                }
            }
        }
    }
}

struct IoWrapper<T> {
    unread: Option<Bytes>,
    inner: T,
}

impl<T: Read> Read for IoWrapper<T> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if let Some(mut bytes) = self.unread.take() {
            let size = cmp::min(buf.len(), bytes.len());
            buf[..size].copy_from_slice(&bytes[..size]);
            if bytes.len() > size {
                bytes.split_to(size);
                self.unread = Some(bytes);
            }
            Ok(size)
        } else {
            self.inner.read(buf)
        }
    }
}

impl<T: Write> Write for IoWrapper<T> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.inner.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

impl<T: AsyncRead + 'static> AsyncRead for IoWrapper<T> {
    unsafe fn prepare_uninitialized_buffer(&self, buf: &mut [u8]) -> bool {
        self.inner.prepare_uninitialized_buffer(buf)
    }
}

impl<T: AsyncWrite + 'static> AsyncWrite for IoWrapper<T> {
    fn shutdown(&mut self) -> Poll<(), io::Error> {
        self.inner.shutdown()
    }
    fn write_buf<B: Buf>(&mut self, buf: &mut B) -> Poll<usize, io::Error> {
        self.inner.write_buf(buf)
    }
}