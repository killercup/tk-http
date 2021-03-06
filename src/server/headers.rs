use std::str::from_utf8;
use std::slice::Iter as SliceIter;
use std::ascii::AsciiExt;
use std::borrow::Cow;

use httparse::{self, EMPTY_HEADER, Request, Header};
use tokio_core::io::Io;
use tk_bufstream::Buf;

use server::error::{Error, ErrorEnum};
use super::{RequestTarget, Dispatcher};
use super::codec::BodyKind;
use super::encoder::ResponseConfig;
use super::websocket::{self, WebsocketHandshake};
use super::request_target;
use headers;
use {Version};


/// Number of headers to allocate on a stack
const MIN_HEADERS: usize = 16;
/// A hard limit on the number of headers
const MAX_HEADERS: usize = 1024;


struct RequestConfig<'a> {
    body: BodyKind,
    expect_continue: bool,
    connection_close: bool,
    connection: Option<Cow<'a, str>>,
    host: Option<&'a str>,
    target: RequestTarget<'a>,
    /// If this is true, then Host header differs from host value in
    /// request-target (first line). Note, specification allows throwing
    /// the header value by proxy in this case. But you might consider
    /// returning 400 Bad Request.
    conflicting_host: bool,
}

/// A borrowed structure that represents request headers
///
/// It's passed to `Codec::headers_received` and you are free to store or
/// discard any needed fields and headers from it.
#[derive(Debug)]
pub struct Head<'a> {
    method: &'a str,
    raw_target: &'a str,
    target: RequestTarget<'a>,
    host: Option<&'a str>,
    conflicting_host: bool,
    version: Version,
    headers: &'a [Header<'a>],
    body_kind: BodyKind,
    connection_close: bool,
    connection_header: Option<Cow<'a, str>>,
}

/// Iterator over all meaningful headers for the request
///
/// This iterator is created by `Head::headers`. And iterates over all
/// headers except hop-by-hop ones and `Host`.
///
/// Note: duplicate headers are not glued together neither they are sorted
pub struct HeaderIter<'a> {
    head: &'a Head<'a>,
    iter: SliceIter<'a, Header<'a>>,
}

impl<'a> Head<'a> {
    /// Returns a HTTP method
    pub fn method(&self) -> &str {
        self.method
    }
    /// Request-target (the middle part of the first line of request)
    pub fn request_target(&self) -> &RequestTarget<'a> {
        &self.target
    }
    /// Returns a raw request target as string
    pub fn raw_request_target(&self) -> &str {
        self.raw_target
    }
    /// Returns path portion of request uri
    ///
    /// Note: this may return something not starting from a slash when
    /// full uri is used as request-target
    ///
    /// If the request target is in asterisk form this returns None
    pub fn path(&self) -> Option<&str> {
        use super::RequestTarget::*;
        match self.target {
            Origin(x) => Some(x),
            Absolute { path, .. } => Some(path),
            Authority(..) => None,
            Asterisk => None,
        }
    }
    /// Return host of a request
    ///
    /// Note: this might be extracted from request-target portion of
    /// request headers (first line).
    ///
    /// If both `Host` header exists and doesn't match host in request-target
    /// then this method returns host from request-target and
    /// `has_conflicting_host()` method returns true.
    pub fn host(&self) -> Option<&str> {
        self.host
    }
    /// Returns true if `Host` header conflicts with host in request-uri
    ///
    /// By spec this fact may be ignored in proxy, but better to reply
    /// BadRequest in this case
    pub fn has_conflicting_host(&self) -> bool {
        self.conflicting_host
    }
    /// Version of HTTP request
    pub fn version(&self) -> Version {
        self.version
    }
    /// Iterator over the headers of HTTP request
    ///
    /// This iterator strips the following kinds of headers:
    ///
    /// 1. Hop-by-hop headers (`Connection` itself, and ones it enumerates)
    /// 2. `Content-Length` and `Transfer-Encoding`
    /// 3. `Host` header
    /// 4. `Upgrade` header regardless of whether it's in `Connection`
    ///
    /// You may use `all_headers()` if you really need to access to all of
    /// them (mostly useful for debugging puproses). But you may want to
    /// consider:
    ///
    /// 1. Host of the target request can be received using `host()` method,
    ///    which is also parsed from `target` path of request if that is
    ///    in absolute form (so conforming to the spec)
    /// 2. Payload size can be fetched using `body_length()` method. Note:
    ///    this also includes cases where length is implicitly set to zero.
    /// 3. `Connection` header might be discovered with `connection_close()`
    ///    or `connection_header()`
    /// 4. `Upgrade` might be discovered with `get_websocket_upgrade()` or
    ///    only looked in `all_headers()` if `upgrade` presents in
    ///    `connection_header()`
    pub fn headers(&self) -> HeaderIter {
        HeaderIter {
            head: self,
            iter: self.headers.iter(),
        }
    }
    /// All headers of HTTP request
    ///
    /// Unlike `self.headers()` this does include hop-by-hop headers. This
    /// method is here just for completeness, you shouldn't need it.
    pub fn all_headers(&self) -> &'a [Header<'a>] {
        self.headers
    }
    /// Return `true` if `Connection: close` header exists
    pub fn connection_close(&self) -> bool {
        self.connection_close
    }
    /// Returns the value of the `Connection` header (all of them, if multiple)
    pub fn connection_header(&'a self) -> Option<&'a str> {
        self.connection_header.as_ref().map(|x| &x[..])
    }

    /// Returns true if there was transfer-encoding or content-length != 0
    ///
    /// I.e. `false` may mean either `Content-Length: 0` or there were no
    /// content length. This is mostly important to check for requests which
    /// must not have body (`HEAD`, `CONNECT`, `Upgrade: websocket` ...)
    pub fn has_body(&self) -> bool {
        self.body_kind != BodyKind::Fixed(0)
    }

    /// Returns size of the request body if either `Content-Length` is set
    /// or it is safe to assume that request body is zero-length
    ///
    /// If request length can't be determined in advance (such as when there
    /// is a `Transfer-Encoding`) `None` is returned
    pub fn body_length(&self) -> Option<u64> {
        match self.body_kind {
            BodyKind::Fixed(x) => Some(x),
            _ => None,
        }
    }
    /// Check if connection is a websocket and return hanshake info
    ///
    /// `Err(())` is returned when there was handshake but where was something
    /// wrong with it (so you should return `BadRequest` even if you support
    /// plain http on the resource).
    ///
    /// `Ok(None)` is returned when it's a plain HTTP request (no upgrade).
    ///
    /// Note: this method computes handshake again, so it's better not to
    /// call it multiple times.
    pub fn get_websocket_upgrade(&self)
        -> Result<Option<WebsocketHandshake>, ()>
    {
        websocket::get_handshake(self)
    }
}

fn scan_headers<'x>(raw_request: &'x Request)
    -> Result<RequestConfig<'x>, ErrorEnum>
{
    // Implements the body length algorithm for requests:
    // http://httpwg.github.io/specs/rfc7230.html#message.body.length
    //
    // The length of a request body is determined by one of the following
    // (in order of precedence):
    //
    // 1. If the request contains a valid `Transfer-Encoding` header
    //    with `chunked` as the last encoding the request is chunked
    //    (3rd option in RFC).
    // 2. If the request contains a valid `Content-Length` header
    //    the request has the given length in octets
    //    (5th option in RFC).
    // 3. If neither `Transfer-Encoding` nor `Content-Length` are
    //    present the request has an empty body
    //    (6th option in RFC).
    // 4. In all other cases the request is a bad request.
    use super::codec::BodyKind::*;
    use server::error::ErrorEnum::*;

    let mut has_content_length = false;
    let mut close = raw_request.version.unwrap() == 0;
    let mut expect_continue = false;
    let mut body = Fixed(0);
    let mut connection = None::<Cow<_>>;
    let mut host_header = false;
    let target = request_target::parse(raw_request.path.unwrap())
        .ok_or(BadRequestTarget)?;
    let mut conflicting_host = false;
    let mut host = match target {
        RequestTarget::Authority(x) => Some(x),
        RequestTarget::Absolute { authority, .. } => Some(authority),
        _ => None,
    };
    for header in raw_request.headers.iter() {
        if header.name.eq_ignore_ascii_case("Transfer-Encoding") {
            if let Some(enc) = header.value.split(|&x| x == b',').last() {
                if headers::is_chunked(enc) {
                    if has_content_length {
                        // override but don't allow keep-alive
                        close = true;
                    }
                    body = Chunked;
                }
            }
        } else if header.name.eq_ignore_ascii_case("Content-Length") {
            if has_content_length {
                // duplicate content_length
                return Err(DuplicateContentLength);
            }
            has_content_length = true;
            if body != Chunked {
                let s = from_utf8(header.value)
                    .map_err(|_| ContentLengthInvalid)?;
                let len = s.parse().map_err(|_| ContentLengthInvalid)?;
                body = Fixed(len);
            } else {
                // transfer-encoding has preference and don't allow keep-alive
                close = true;
            }
        } else if header.name.eq_ignore_ascii_case("Connection") {
            let strconn = from_utf8(header.value)
                .map_err(|_| ConnectionInvalid)?.trim();
            connection = match connection {
                Some(x) => Some(x + ", " + strconn),
                None => Some(strconn.into()),
            };
            if header.value.split(|&x| x == b',').any(headers::is_close) {
                close = true;
            }
        } else if header.name.eq_ignore_ascii_case("Host") {
            if host_header {
                return Err(DuplicateHost);
            }
            host_header = true;
            let strhost = from_utf8(header.value)
                .map_err(|_| HostInvalid)?.trim();
            if host.is_none() {  // if host is not in uri
                // TODO(tailhook) additional validations for host
                host = Some(strhost);
            } else if host != Some(strhost) {
                conflicting_host = true;
            }
        } else if header.name.eq_ignore_ascii_case("Expect") {
            if headers::is_continue(header.value) {
                expect_continue = true;
            }
        }
    }
    if raw_request.method.unwrap() == "CONNECT" {
        body = Unsupported;
    }
    Ok(RequestConfig {
        body: body,
        expect_continue: expect_continue,
        connection: connection,
        host: host,
        target: target,
        connection_close: close,
        conflicting_host: conflicting_host,
    })
}

pub fn parse_headers<S, D>(buffer: &mut Buf, disp: &mut D)
    -> Result<Option<(BodyKind, D::Codec, ResponseConfig)>, Error>
    where S: Io,
          D: Dispatcher<S>,
{
    let (body_kind, codec, cfg, bytes) = {
        let mut vec;
        let mut headers = [EMPTY_HEADER; MIN_HEADERS];

        let mut raw = Request::new(&mut headers);
        let mut result = raw.parse(&buffer[..]);
        if matches!(result, Err(httparse::Error::TooManyHeaders)) {
            vec = vec![EMPTY_HEADER; MAX_HEADERS];
            raw = Request::new(&mut vec);
            result = raw.parse(&buffer[..]);
        }
        match result.map_err(ErrorEnum::ParseError)? {
            httparse::Status::Complete(bytes) => {
                let cfg = scan_headers(&raw)?;
                let ver = raw.version.unwrap();
                let head = Head {
                    method: raw.method.unwrap(),
                    raw_target: raw.path.unwrap(),
                    target: cfg.target,
                    version: if ver == 1
                        { Version::Http11 } else { Version::Http10 },
                    host: cfg.host,
                    conflicting_host: cfg.conflicting_host,
                    headers: raw.headers,
                    body_kind: cfg.body,
                    // For HTTP/1.0 we could implement
                    // Connection: Keep-Alive but hopefully it's rare
                    // enough to ignore nowadays
                    connection_close: cfg.connection_close || ver == 0,
                    connection_header: cfg.connection,
                };
                let codec = disp.headers_received(&head)?;
                // TODO(tailhook) send 100-expect response headers
                let response_config = ResponseConfig::from(&head);
                (cfg.body, codec, response_config, bytes)
            }
            _ => return Ok(None),
        }
    };
    buffer.consume(bytes);
    Ok(Some((body_kind, codec, cfg)))
}

impl<'a> Iterator for HeaderIter<'a> {
    type Item = (&'a str, &'a [u8]);
    fn next(&mut self) -> Option<(&'a str, &'a [u8])> {
        while let Some(header) = self.iter.next() {
            if header.name.eq_ignore_ascii_case("Connection") ||
                header.name.eq_ignore_ascii_case("Transfer-Encoding") ||
                header.name.eq_ignore_ascii_case("Content-Length") ||
                header.name.eq_ignore_ascii_case("Upgrade") ||
                header.name.eq_ignore_ascii_case("Host")
            {
                continue;
            }

            if let Some(ref conn) = self.head.connection_header {
                let mut conn_headers = conn.split(',').map(|x| x.trim());
                if conn_headers.any(|x| x.eq_ignore_ascii_case(header.name)) {
                    continue;
                }
            }
            return Some((header.name, header.value));
        }
        return None;
    }
}
