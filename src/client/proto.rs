use std::mem;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, AtomicBool, Ordering};
use std::collections::VecDeque;

use tk_bufstream::{IoBuf, WriteBuf, ReadBuf};
use tokio_core::io::Io;
use futures::{Future, AsyncSink, Async, Sink, StartSend, Poll};

use OptFuture;
use client::parser::Parser;
use client::encoder::{self, get_inner};
use client::{Codec, Error, EncoderDone};


enum OutState<S: Io> {
    Idle(WriteBuf<S>),
    Write(Box<Future<Item=EncoderDone<S>, Error=Error>>),
    Void,
}

enum InState<S: Io, C: Codec<S>> {
    Idle(ReadBuf<S>),
    Read(Parser<S, C>),
    Void,
}

pub struct Proto<S: Io, C: Codec<S>> {
    writing: OutState<S>,
    waiting: VecDeque<(C, Arc<AtomicUsize>)>,
    reading: InState<S, C>,
    close: Arc<AtomicBool>,
}


impl<S: Io, C: Codec<S>> Proto<S, C> {
    pub fn new(conn: S) -> Proto<S, C> {
        let (cout, cin) = IoBuf::new(conn).split();
        return Proto {
            writing: OutState::Idle(cout),
            waiting: VecDeque::new(),
            reading: InState::Idle(cin),
            close: Arc::new(AtomicBool::new(false)),
        }
    }
}

impl<S: Io, C: Codec<S>> Sink for Proto<S, C> {
    type SinkItem = C;
    type SinkError = Error;
    fn start_send(&mut self, mut item: Self::SinkItem)
        -> StartSend<Self::SinkItem, Self::SinkError>
    {
        let (r, st) = match mem::replace(&mut self.writing, OutState::Void) {
            OutState::Idle(mut io) => {
                if self.close.load(Ordering::SeqCst) {
                    // TODO(tailhook) maybe shutdown?
                    io.flush()?;
                    (AsyncSink::NotReady(item), OutState::Idle(io))
                } else {
                    // TODO(tailhook) check if there are too many waiting
                    let state = Arc::new(AtomicUsize::new(0));
                    let (r, st) =
                        match item.start_write(encoder::new(io,
                            state.clone(), self.close.clone()))
                        {
                            OptFuture::Value(Ok(done)) => {
                                (AsyncSink::Ready,
                                 OutState::Idle(get_inner(done)))
                            }
                            // Note we break connection if serializer errored,
                            // because we don't actually know if connection
                            // can be reused safefully in this case
                            OptFuture::Value(Err(e)) => return Err(e),
                            OptFuture::Future(fut) => {
                                (AsyncSink::Ready, OutState::Write(fut))
                            }
                            OptFuture::Done => unreachable!(),
                        };
                    self.waiting.push_back((item, state));
                    (r, st)
                }
            }
            OutState::Write(fut) => {
                // TODO(tailhook) should we check "close"?
                // Points:
                // * Performance
                // * Dropping future
                (AsyncSink::NotReady(item), OutState::Write(fut))
            }
            OutState::Void => unreachable!(),
        };
        self.writing = st;
        return Ok(r);
    }
    fn poll_complete(&mut self) -> Poll<(), Self::SinkError> {
        self.writing = match mem::replace(&mut self.writing, OutState::Void) {
            OutState::Idle(mut io) => {
                io.flush()?;
                OutState::Idle(io)
            }
            // Note we break connection if serializer errored, because
            // we don't actually know if connection can be reused
            // safefully in this case
            OutState::Write(mut fut) => match fut.poll()? {
                Async::Ready(done) => {
                    let mut io = get_inner(done);
                    io.flush()?;
                    OutState::Idle(io)
                }
                Async::NotReady => OutState::Write(fut),
            },
            OutState::Void => unreachable!(),
        };
        loop {
            let (state, cont) =
                match mem::replace(&mut self.reading, InState::Void) {
                    InState::Idle(io) => {
                        if let Some((nr, state)) = self.waiting.pop_front() {
                            let parser = Parser::new(io, nr,
                                state, self.close.clone());
                            (InState::Read(parser), true)
                        } else {
                            (InState::Idle(io), false)
                        }
                    }
                    InState::Read(mut parser) => {
                        match parser.poll()? {
                            Async::NotReady => (InState::Read(parser), false),
                            Async::Ready(Some(io)) => (InState::Idle(io), true),
                            Async::Ready(None) => {
                                return Err(Error::Closed);
                            }
                        }
                    }
                    InState::Void => unreachable!(),
                };
            self.reading = state;
            if !cont {
                break;
            }
        }
        // Temporarily return Ready so that `Sink::send` works as expected
        //if self.waiting.len() == 0 &&
        //        matches!(self.writing, OutState::Idle(_)) &&
        //        matches!(self.reading, InState::Idle(_))
        //{
        //    return Ok(Async::Ready(()));
        //} else {
        //    return Ok(Async::NotReady);
        //}

        // We never return ready as we don't care for flush() combinator
        // Also we wan't timeouts on idle keep-alive connections
        // TODO(tailhook) implement timeouts
        return Ok(Async::NotReady);
    }
}