use std::io;
use std::fmt::Display;

use futures::{Async, Future, Poll};
use tokio_core::io::Io;
use tk_bufstream::{Flushed, WriteBuf, WriteRaw, FutureWriteRaw};

use base_serializer::{MessageState, HeaderError};
use enums::{Version, Status};
use super::headers::Head;
use super::Error;


/// This a response writer that you receive in `Codec`
///
/// Methods of this structure ensure that everything you write into a buffer
/// is consistent and valid protocol
pub struct Encoder<S: Io> {
    state: MessageState,
    io: WriteBuf<S>,
}

/// This structure returned from `Encoder::done` and works as a continuation
/// that should be returned from the future that writes request.
pub struct EncoderDone<S: Io> {
    buf: WriteBuf<S>,
}

/// This structure contains all needed info to start response of the request
/// in a correct manner
///
/// This is ought to be used in serializer only
#[derive(Debug, Clone, Copy)]
pub struct ResponseConfig {
    /// Whether request is a HEAD request
    pub is_head: bool,
    /// Is `Connection: close` in request or HTTP version == 1.0
    pub do_close: bool,
    /// Version of HTTP request
    pub version: Version,
}

pub struct FutureRawBody<S>(FutureWriteRaw<S>);

pub struct RawBody<S> {
    io: WriteRaw<S>,
}


// TODO: Support responses to CONNECT and `Upgrade: websocket` requests.
impl<S: Io> Encoder<S> {
    /// Write a 100 (Continue) response.
    ///
    /// A server should respond with the 100 status code if it receives a
    /// 100-continue expectation.
    ///
    /// # Panics
    ///
    /// When the response is already started. It's expected that your response
    /// handler state machine will never call the method twice.
    pub fn response_continue(&mut self) {
        self.state.response_continue(&mut self.io.out_buf)
    }

    /// Write status line using `Status` enum
    ///
    /// This puts status line into a buffer immediately. If you don't
    /// continue with request it will be sent to the network shortly.
    ///
    /// # Panics
    ///
    /// When status line is already written. It's expected that your request
    /// handler state machine will never call the method twice.
    ///
    /// When the status code is 100 (Continue). 100 is not allowed
    /// as a final status code.
    pub fn status(&mut self, status: Status) {
        self.state.response_status(&mut self.io.out_buf,
            status.code(), status.reason())
    }

    /// Write custom status line
    ///
    /// # Panics
    ///
    /// When status line is already written. It's expected that your request
    /// handler state machine will never call the method twice.
    ///
    /// When the status code is 100 (Continue). 100 is not allowed
    /// as a final status code.
    pub fn custom_status(&mut self, code: u16, reason: &str) {
        self.state.response_status(&mut self.io.out_buf, code, reason)
    }

    /// Add a header to the message.
    ///
    /// Header is written into the output buffer immediately. And is sent
    /// as soon as the next loop iteration
    ///
    /// `Content-Length` header must be send using the `add_length` method
    /// and `Transfer-Encoding: chunked` must be set with the `add_chunked`
    /// method. These two headers are important for the security of HTTP.
    ///
    /// Note that there is currently no way to use a transfer encoding other
    /// than chunked.
    ///
    /// We return Result here to make implementing proxies easier. In the
    /// application handler it's okay to unwrap the result and to get
    /// a meaningful panic (that is basically an assertion).
    ///
    /// # Panics
    ///
    /// Panics when `add_header` is called in the wrong state.
    pub fn add_header<V: AsRef<[u8]>>(&mut self, name: &str, value: V)
        -> Result<(), HeaderError>
    {
        self.state.add_header(&mut self.io.out_buf, name, value.as_ref())
    }

    /// Same as `add_header` but allows value to be formatted directly into
    /// the buffer
    ///
    /// Useful for dates and numeric headers, as well as some strongly typed
    /// wrappers
    pub fn format_header<D: Display>(&mut self, name: &str, value: D)
        -> Result<(), HeaderError>
    {
        self.state.format_header(&mut self.io.out_buf, name, value)
    }

    /// Add a content length to the message.
    ///
    /// The `Content-Length` header is written to the output buffer immediately.
    /// It is checked that there are no other body length headers present in the
    /// message. When the body is send the length is validated.
    ///
    /// # Panics
    ///
    /// Panics when `add_length` is called in the wrong state.
    pub fn add_length(&mut self, n: u64)
        -> Result<(), HeaderError>
    {
        self.state.add_length(&mut self.io.out_buf, n)
    }
    /// Sets the transfer encoding to chunked.
    ///
    /// Writes `Transfer-Encoding: chunked` to the output buffer immediately.
    /// It is assured that there is only one body length header is present
    /// and the body is written in chunked encoding.
    ///
    /// # Panics
    ///
    /// Panics when `add_chunked` is called in the wrong state.
    pub fn add_chunked(&mut self)
        -> Result<(), HeaderError>
    {
        self.state.add_chunked(&mut self.io.out_buf)
    }
    /// Returns true if at least `status()` method has been called
    ///
    /// This is mostly useful to find out whether we can build an error page
    /// or it's already too late.
    pub fn is_started(&self) -> bool {
        self.state.is_started()
    }
    /// Closes the HTTP header and returns `true` if entity body is expected.
    ///
    /// Specifically `false` is returned when status is 1xx, 204, 304 or in
    /// the response to a `HEAD` request but not if the body has zero-length.
    ///
    /// Similarly to `add_header()` it's fine to `unwrap()` here, unless you're
    /// doing some proxying.
    ///
    /// # Panics
    ///
    /// Panics when the response is in a wrong state.
    pub fn done_headers(&mut self) -> Result<bool, HeaderError> {
        self.state.done_headers(&mut self.io.out_buf)
    }
    /// Write a chunk of the message body.
    ///
    /// Works both for fixed-size body and chunked body.
    ///
    /// For the chunked body each chunk is put into the buffer immediately
    /// prefixed by chunk size. Empty chunks are ignored.
    ///
    /// For both modes chunk is put into the buffer, but is only sent when
    /// rotor-stream state machine is reached. So you may put multiple chunks
    /// into the buffer quite efficiently.
    ///
    /// You may write a body in responses to HEAD requests just like in real
    /// requests but the data is not sent to the network. Of course it is
    /// more efficient to not construct the message body at all.
    ///
    /// # Panics
    ///
    /// When response is in wrong state. Or there is no headers which
    /// determine response body length (either Content-Length or
    /// Transfer-Encoding).
    pub fn write_body(&mut self, data: &[u8]) {
        self.state.write_body(&mut self.io.out_buf, data)
    }
    /// Returns true if `done()` method is already called and everything
    /// was okay.
    pub fn is_complete(&self) -> bool {
        self.state.is_complete()
    }
    /// Writes needed finalization data into the buffer and asserts
    /// that response is in the appropriate state for that.
    ///
    /// The method may be called multiple times.
    ///
    /// # Panics
    ///
    /// When the response is in the wrong state.
    pub fn done(mut self) -> EncoderDone<S> {
        self.state.done(&mut self.io.out_buf);
        EncoderDone { buf: self.io }
    }
    /// Returns a future which yields a socket when the buffer is flushed to
    /// the socket
    ///
    /// It yield only socket because there is no reason for holding empty
    /// buffer. This is useful to implement `sendfile` or any other custom
    /// way of sending data to the socket.
    ///
    /// # Panics
    ///
    /// Currently method panics when done_headers is not called yet
    pub fn steal_socket(self) -> Flushed<S> {
        assert!(self.state.is_after_headers());
        unimplemented!()
        //self.io.flushed()
    }
    /// Returns a raw body for zero-copy writing techniques
    ///
    /// Note: we don't assert on the format of the body if you're using this
    /// interface.
    ///
    /// Note 2: RawBody (returned by this future) locks the underlying BiLock,
    /// which basically means reading from this socket is not possible while
    /// you're writing to the raw body.
    ///
    /// Good idea is to use interface like this:
    ///
    /// 1. Set appropriate content-length
    /// 2. Write exactly this number of bytes or exit with error
    ///
    /// This is specifically designed for using with `sendfile`
    ///
    /// # Panics
    ///
    /// This method panics if it's called when headers are not written yet.
    pub fn raw_body(self) -> FutureRawBody<S> {
        assert!(self.state.is_after_headers());
        FutureRawBody(self.io.borrow_raw())
    }
}

impl<S: Io> RawBody<S> {
    pub fn done(mut self) -> EncoderDone<S> {
        EncoderDone { buf: self.io.into_buf() }
    }
}

impl<S: Io> io::Write for Encoder<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // TODO(tailhook) we might want to propatage error correctly
        // rather than panic
        self.write_body(buf);
        Ok((buf.len()))
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<S: Io> io::Write for RawBody<S> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.io.write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.io.flush()
    }
}

pub fn get_inner<S: Io>(e: EncoderDone<S>) -> WriteBuf<S> {
    e.buf
}

pub fn new<S: Io>(io: WriteBuf<S>, cfg: ResponseConfig) -> Encoder<S> {
    use base_serializer::Body::*;

    // TODO(tailhook) implement Connection: Close,
    // (including explicit one in HTTP/1.0) and maybe others
    Encoder {
        state: MessageState::ResponseStart {
            body: if cfg.is_head { Head } else { Normal },
            version: cfg.version,
            close: cfg.do_close || cfg.version == Version::Http10,
        },
        io: io,
    }
}

impl ResponseConfig {
    pub fn from(req: &Head) -> ResponseConfig {
        ResponseConfig {
            version: req.version(),
            is_head: req.method() == "HEAD",
            do_close: req.connection_close(),
        }
    }
}

impl<S: Io> Future for FutureRawBody<S> {
    type Item = RawBody<S>;
    type Error = io::Error;
    fn poll(&mut self) -> Poll<RawBody<S>, io::Error> {
        self.0.poll().map(|x| x.map(|y| RawBody { io: y }))
    }
}

#[cfg(feature="sendfile")]
mod sendfile {
    use std::io;
    use futures::Async;
    use tk_sendfile::{Destination, FileOpener, Sendfile};
    use tokio_core::net::TcpStream;
    use super::RawBody;

    impl Destination for RawBody<TcpStream> {
        fn write_file<O: FileOpener>(&mut self, file: &mut Sendfile<O>)
            -> Result<usize, io::Error>
        {
            self.io.write_file(file)
        }
        fn poll_write(&mut self) -> Async<()> {
            self.io.poll_write()
        }
    }

}
