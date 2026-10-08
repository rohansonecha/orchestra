use std::{
    collections::VecDeque,
    io,
    time::{Duration, Instant},
};

use mio::{unix::SourceFd, Events, Interest, Poll, Token};
use signal_hook_mio::v1_0::Signals;

#[cfg(feature = "event-stream")]
use crate::event::sys::Waker;
use crate::event::{
    source::EventSource, sys::unix::parse::parse_event, timeout::PollTimeout, Event, InternalEvent,
};
use crate::terminal::sys::file_descriptor::{tty_fd, FileDesc};

// Tokens to identify file descriptor
const TTY_TOKEN: Token = Token(0);
const SIGNAL_TOKEN: Token = Token(1);
#[cfg(feature = "event-stream")]
const WAKE_TOKEN: Token = Token(2);

// I (@zrzka) wasn't able to read more than 1_022 bytes when testing
// reading on macOS/Linux -> we don't need bigger buffer and 1k of bytes
// is enough.
const TTY_BUFFER_SIZE: usize = 1_024;

// orchestra patch: an escape sequence still incomplete this long after its
// last byte is given up on, so a lost end can't swallow all later input. A
// bracketed paste is delivered as is; anything else (a cut-off mouse
// report, say) is dropped.
const INCOMPLETE_TIMEOUT: Duration = Duration::from_millis(500);

/// orchestra patch: whether the tty has bytes to read right now. The tty fd
/// is blocking, so reading it when it is empty would block the caller
/// until the next keypress, ignoring the poll timeout.
fn tty_readable(fd: &FileDesc<'static>) -> bool {
    use rustix::event::{poll, PollFd, PollFlags};
    let borrowed = unsafe { rustix::fd::BorrowedFd::borrow_raw(fd.raw_fd()) };
    let mut fds = [PollFd::new(&borrowed, PollFlags::IN)];
    matches!(poll(&mut fds, 0), Ok(n) if n > 0)
}

pub(crate) struct UnixInternalEventSource {
    poll: Poll,
    events: Events,
    parser: Parser,
    tty_buffer: [u8; TTY_BUFFER_SIZE],
    tty_fd: FileDesc<'static>,
    signals: Signals,
    #[cfg(feature = "event-stream")]
    waker: Waker,
}

impl UnixInternalEventSource {
    pub fn new() -> io::Result<Self> {
        UnixInternalEventSource::from_file_descriptor(tty_fd()?)
    }

    pub(crate) fn from_file_descriptor(input_fd: FileDesc<'static>) -> io::Result<Self> {
        let poll = Poll::new()?;
        let registry = poll.registry();

        let tty_raw_fd = input_fd.raw_fd();
        let mut tty_ev = SourceFd(&tty_raw_fd);
        registry.register(&mut tty_ev, TTY_TOKEN, Interest::READABLE)?;

        let mut signals = Signals::new([signal_hook::consts::SIGWINCH])?;
        registry.register(&mut signals, SIGNAL_TOKEN, Interest::READABLE)?;

        #[cfg(feature = "event-stream")]
        let waker = Waker::new(registry, WAKE_TOKEN)?;

        Ok(UnixInternalEventSource {
            poll,
            events: Events::with_capacity(3),
            parser: Parser::default(),
            tty_buffer: [0u8; TTY_BUFFER_SIZE],
            tty_fd: input_fd,
            signals,
            #[cfg(feature = "event-stream")]
            waker,
        })
    }
}

impl EventSource for UnixInternalEventSource {
    fn try_read(&mut self, timeout: Option<Duration>) -> io::Result<Option<InternalEvent>> {
        self.parser.flush_stale();
        if let Some(event) = self.parser.next() {
            return Ok(Some(event));
        }

        let timeout = PollTimeout::new(timeout);

        loop {
            // While a sequence is incomplete, wake up in time to give up on it.
            let wait = match (timeout.leftover(), self.parser.incomplete_deadline()) {
                (Some(t), Some(d)) => Some(t.min(d)),
                (None, d) => d,
                (t, None) => t,
            };
            if let Err(e) = self.poll.poll(&mut self.events, wait) {
                // Mio will throw an interrupted error in case of cursor position retrieval. We need to retry until it succeeds.
                // Previous versions of Mio (< 0.7) would automatically retry the poll call if it was interrupted (if EINTR was returned).
                // https://docs.rs/mio/0.7.0/mio/struct.Poll.html#notes
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                } else {
                    return Err(e);
                }
            };

            if self.events.is_empty() {
                self.parser.flush_stale();
                if let Some(event) = self.parser.next() {
                    return Ok(Some(event));
                }
                if timeout.elapsed() {
                    // No readiness events = timeout
                    return Ok(None);
                }
                continue;
            }

            for token in self.events.iter().map(|x| x.token()) {
                match token {
                    TTY_TOKEN => {
                        loop {
                            match self.tty_fd.read(&mut self.tty_buffer) {
                                Ok(read_count) => {
                                    if read_count > 0 {
                                        self.parser.advance(
                                            &self.tty_buffer[..read_count],
                                            read_count == TTY_BUFFER_SIZE,
                                        );
                                    }
                                }
                                Err(e) => {
                                    // No more data to read at the moment. We will receive another event
                                    if e.kind() == io::ErrorKind::WouldBlock {
                                        break;
                                    }
                                    // once more data is available to read.
                                    else if e.kind() == io::ErrorKind::Interrupted {
                                        continue;
                                    }
                                }
                            };

                            if let Some(event) = self.parser.next() {
                                return Ok(Some(event));
                            }
                            // Nothing complete yet: read again only if
                            // more bytes are already waiting.
                            if !tty_readable(&self.tty_fd) {
                                break;
                            }
                        }
                    }
                    SIGNAL_TOKEN => {
                        if self.signals.pending().next() == Some(signal_hook::consts::SIGWINCH) {
                            // TODO Should we remove tput?
                            //
                            // This can take a really long time, because terminal::size can
                            // launch new process (tput) and then it parses its output. It's
                            // not a really long time from the absolute time point of view, but
                            // it's a really long time from the mio, async-std/tokio executor, ...
                            // point of view.
                            let new_size = crate::terminal::size()?;
                            return Ok(Some(InternalEvent::Event(Event::Resize(
                                new_size.0, new_size.1,
                            ))));
                        }
                    }
                    #[cfg(feature = "event-stream")]
                    WAKE_TOKEN => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::Interrupted,
                            "Poll operation was woken up by `Waker::wake`",
                        ));
                    }
                    _ => unreachable!("Synchronize Evented handle registration & token handling"),
                }
            }

            // Processing above can take some time, check if timeout expired
            if timeout.elapsed() {
                return Ok(None);
            }
        }
    }

    #[cfg(feature = "event-stream")]
    fn waker(&self) -> Waker {
        self.waker.clone()
    }
}

//
// Following `Parser` structure exists for two reasons:
//
//  * mimic anes Parser interface
//  * move the advancing, parsing, ... stuff out of the `try_read` method
//
#[derive(Debug)]
struct Parser {
    buffer: Vec<u8>,
    internal_events: VecDeque<InternalEvent>,
    // orchestra patch: when the last byte was added to the buffer.
    last_byte: Option<Instant>,
}

impl Default for Parser {
    fn default() -> Self {
        Parser {
            // This buffer is used for -> 1 <- ANSI escape sequence. Are we
            // aware of any ANSI escape sequence that is bigger? Can we make
            // it smaller?
            //
            // Probably not worth spending more time on this as "there's a plan"
            // to use the anes crate parser.
            buffer: Vec::with_capacity(256),
            // TTY_BUFFER_SIZE is 1_024 bytes. How many ANSI escape sequences can
            // fit? What is an average sequence length? Let's guess here
            // and say that the average ANSI escape sequence length is 8 bytes. Thus
            // the buffer size should be 1024/8=128 to avoid additional allocations
            // when processing large amounts of data.
            //
            // There's no need to make it bigger, because when you look at the `try_read`
            // method implementation, all events are consumed before the next TTY_BUFFER
            // is processed -> events pushed.
            internal_events: VecDeque::with_capacity(128),
            last_byte: None,
        }
    }
}

impl Parser {
    /// How long until an incomplete sequence is given up on, if one is
    /// pending.
    fn incomplete_deadline(&self) -> Option<Duration> {
        if self.buffer.is_empty() {
            return None;
        }
        let since = self.last_byte.map(|t| t.elapsed()).unwrap_or_default();
        Some(INCOMPLETE_TIMEOUT.saturating_sub(since))
    }

    /// Give up on an incomplete sequence whose end never came: deliver a
    /// paste as is, drop anything else.
    fn flush_stale(&mut self) {
        if self.incomplete_deadline() == Some(Duration::ZERO) {
            if self.buffer.starts_with(b"\x1B[200~") {
                let text = String::from_utf8_lossy(&self.buffer[6..]).replace('\r', "\n");
                self.internal_events.push_back(InternalEvent::Event(Event::Paste(text)));
            }
            self.buffer.clear();
        }
    }

    fn advance(&mut self, buffer: &[u8], more: bool) {
        if !buffer.is_empty() {
            self.last_byte = Some(Instant::now());
        }
        for (idx, byte) in buffer.iter().enumerate() {
            let more = idx + 1 < buffer.len() || more;

            self.buffer.push(*byte);

            match parse_event(&self.buffer, more) {
                Ok(Some(ie)) => {
                    self.internal_events.push_back(ie);
                    self.buffer.clear();
                }
                Ok(None) => {
                    // Event can't be parsed, because we don't have enough bytes for
                    // the current sequence. Keep the buffer and process next bytes.
                }
                Err(_) => {
                    // Event can't be parsed (not enough parameters, parameter is not a number, ...).
                    // Clear the buffer and continue with another sequence.
                    self.buffer.clear();
                }
            }
        }
    }
}

impl Iterator for Parser {
    type Item = InternalEvent;

    fn next(&mut self) -> Option<Self::Item> {
        self.internal_events.pop_front()
    }
}
