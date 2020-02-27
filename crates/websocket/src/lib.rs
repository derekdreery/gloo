//!  - The [HTML standard](https://html.spec.whatwg.org/multipage/web-sockets.html)
//!  - The [protocol specification](https://tools.ietf.org/html/rfc6455)

use ::{
    gloo_events::EventListener,
    std::{fmt, rc::Rc},
    wasm_bindgen::{prelude::*, throw_str, JsCast},
};

mod sealed {
    pub trait Sealed {}
}
use sealed::Sealed;

pub trait OutgoingMessage: Sealed {
    fn send(&self, socket: &web_sys::WebSocket) -> Result<(), JsValue>;
}

impl Sealed for [u8] {}
impl OutgoingMessage for [u8] {
    fn send(&self, socket: &web_sys::WebSocket) -> Result<(), JsValue> {
        // TODO we copy the data here because wasm-bindgen assumes the data must be mutable. It
        // would be nice to remove the copy.
        let mut data = self.to_owned();
        socket.send_with_u8_array(&mut data)
    }
}

impl Sealed for str {}
impl OutgoingMessage for str {
    fn send(&self, socket: &web_sys::WebSocket) -> Result<(), JsValue> {
        socket.send_with_str(self)
    }
}

pub mod callback {
    use super::*;

    pub struct WebSocket {
        inner: Rc<web_sys::WebSocket>,
        open_listener: Option<EventListener>,
        error_listener: Option<EventListener>,
        close_listener: Option<EventListener>,
        message_listener: Option<EventListener>,
    }

    impl WebSocket {
        /// Connect to a websocket with remote at `url`.
        ///
        ///  - The open listener will run once the connection is ready to receive send requests.
        ///  - The message listener will run when there is a message received from the socket.
        ///  - The close listener will fire when the closing handshake is started.
        ///  - The error listener will fire when there is an error with the websocket. This can be
        ///    caused by (amongst other reasons)
        ///    - Trying to send data when the websocket buffer is full.
        ///    - The websocket could not be established.
        ///    - Trying to close a websocket before it is established.
        ///    - The page is closed before the websocket is established.
        ///
        ///    Note that the error event receives no information about the error. This is
        ///    intentional to prevent network sniffing. See
        ///    [the spec](https://html.spec.whatwg.org/multipage/web-sockets.html#closeWebSocket)
        ///    for more details.
        pub fn connect(
            url: &str,
            // Note this function cannot take `Option`s as it would not be able to resolve types in the
            // `None` case. Would have to be `Option<Box<dyn FnOnce + 'static>>`.
            on_open: impl FnOnce() + 'static,
            mut on_error: impl FnMut() + 'static,
            on_close: impl FnOnce(CloseEvent) + 'static,
            mut on_message: impl FnMut(Message) + 'static,
        ) -> Result<Self, ConnectError> {
            let inner = Rc::new(web_sys::WebSocket::new(url).map_err(ConnectError::from_jsvalue)?);
            // using js_sys::ArrayBuffer means we don't need a second callback to decode a `Blob`.
            // downside is that user agent can't temp write to disk (I think).
            inner.set_binary_type(web_sys::BinaryType::Arraybuffer);

            let open_listener = Some(EventListener::once(&inner, "open", move |_event| on_open()));
            let error_listener = Some(EventListener::new(&inner, "error", move |_event| {
                on_error()
            }));
            let close_listener = Some(EventListener::once(&inner, "close", move |event| {
                debug_assert!(event.has_type::<web_sys::CloseEvent>());
                let event: &web_sys::CloseEvent = event.unchecked_ref();
                on_close(CloseEvent {
                    was_clean: event.was_clean(),
                    code: event.code(),
                    reason: event.reason(),
                })
            }));
            let message_listener = Some(EventListener::new(&inner, "message", move |event| {
                debug_assert!(event.has_type::<web_sys::MessageEvent>());
                let event: &web_sys::MessageEvent = event.unchecked_ref();
                let data = event.data();
                match data.as_string() {
                    Some(msg) => on_message(Message::Text(msg)),
                    None => match data.dyn_into::<js_sys::ArrayBuffer>() {
                        Ok(msg) => {
                            let buf = js_sys::Uint8Array::new(&msg.into());
                            on_message(Message::Binary(buf.to_vec()));
                        }
                        Err(_) => panic!("websocket message data was not of expected type"),
                    },
                }
            }));
            Ok(WebSocket {
                inner,
                open_listener,
                error_listener,
                close_listener,
                message_listener,
            })
        }

        /// Send a message over the socket.
        ///
        /// The message can be either a `&[u8]` for binary messages, or a `&str` for text messages.
        pub fn send(&self, data: &(impl OutgoingMessage + ?Sized)) -> Result<(), SendError> {
            if self.state().is_connecting() {
                return Err(SendError);
            }
            // This should not throw because we already checked the error condition.
            Ok(data.send(&self.inner).unwrap_throw())
        }

        /// Whether it is possible to send messages.
        pub fn can_send(&self) -> bool {
            self.state().is_open()
        }

        /// Get the current state of the websocket.
        pub fn state(&self) -> State {
            match self.inner.ready_state() {
                web_sys::WebSocket::CONNECTING => State::Connecting,
                web_sys::WebSocket::OPEN => State::Open,
                web_sys::WebSocket::CLOSING => State::Closing,
                web_sys::WebSocket::CLOSED => State::Closed,
                // This should be unreachable in practice.
                _ => throw_str("unexpected websocket ready state"),
            }
        }

        /// The amount of data that has been sent using the `WebSocket::send` function, but that is
        /// still waiting to be sent over the TCP connection.
        pub fn buffered_amount(&self) -> u32 {
            self.inner.buffered_amount()
        }

        pub fn extensions(&self) -> String {
            self.inner.extensions()
        }

        pub fn protocol(&self) -> String {
            self.inner.protocol()
        }

        /// Lose access to the websocket but keep the callbacks in case any events are recieved.
        ///
        /// It's best not to use this function in production, as the callbacks and possibly the
        /// websocket itself will leak. TODO should this method exist at all?
        pub fn forget(mut self) {
            self.open_listener.take().map(|listener| listener.forget());
            self.error_listener.take().map(|listener| listener.forget());
            self.close_listener.take().map(|listener| listener.forget());
            self.message_listener
                .take()
                .map(|listener| listener.forget());
        }

        /// Start the closing handshake.
        pub fn close(&self) {
            let _ = self.inner.close();
        }

        /// Start the closing handshake, with a reason code and optional reason string.
        ///
        /// The code must be *1000* or between *3000* and *4999* inclusive, and the reason string, if
        /// present, must be less than or equal to 123 bytes in length (when utf8 encoded). If either
        /// of these conditions are violated, the function will error without closing the connection.
        pub fn close_with_reason(&self, code: u16, reason: Option<&str>) -> Result<(), CloseError> {
            let code = match code {
                1000 | 3000..=4999 => code,
                _ => return Err(CloseError::InvalidCode(code)),
            };
            match reason {
                // We've handled the case where `close_with_code_and_reason` may fail.
                Some(r) if r.len() <= 123 => Ok(self
                    .inner
                    .close_with_code_and_reason(code, r)
                    .unwrap_throw()),
                Some(r) => Err(CloseError::InvalidReason(r.to_owned())),
                // We've handled the case where `close_with_code` may throw above.
                None => Ok(self.inner.close_with_code(code).unwrap_throw()),
            }
        }
    }

    impl std::ops::Drop for WebSocket {
        fn drop(&mut self) {
            // Ignore errors on drop, because we can't handle them.
            // According to the spec, the connection will get closed anyway when the ws is garbage
            // collected, but it's more rust-y to do this eagerly when we're done with the ws.
            let _ = self.inner.close();
        }
    }
}

/// An incoming websocket message.
pub enum Message {
    /// Message was in the binary variation.
    Binary(Vec<u8>),
    /// Message was in the text variation.
    Text(String),
}

impl Message {
    pub fn as_binary(&self) -> Option<&[u8]> {
        match self {
            Message::Binary(msg) => Some(msg),
            Message::Text(_) => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Message::Text(msg) => Some(msg),
            Message::Binary(_) => None,
        }
    }
}

pub struct CloseEvent {
    /// Whether the websocket was shut down cleanly.
    pub was_clean: bool,
    /// The code representing the reason for the closure (see [the spec] for details).
    ///
    /// [the spec]: https://tools.ietf.org/html/rfc6455#page-45
    pub code: u16,
    /// A text description of the reason for the closure.
    pub reason: String,
}

/// The state of the websocket.
///
/// The websocket will usually transition between the states in order, but this is not always the
/// case.
pub enum State {
    /// The connection has not yet been established.
    Connecting,
    /// The WebSocket connection is established and communication is possible.
    Open,
    /// The connection is going through the closing handshake, or the close() method has been
    /// invoked.
    Closing,
    /// The connection has been closed or could not be opened.
    Closed,
}

impl State {
    /// Is this `State` `Connecting`.
    pub fn is_connecting(&self) -> bool {
        match self {
            State::Connecting => true,
            _ => false,
        }
    }
    /// Is this `State` `Open`.
    pub fn is_open(&self) -> bool {
        match self {
            State::Open => true,
            _ => false,
        }
    }
    /// Is this `State` `Closing`.
    pub fn is_closing(&self) -> bool {
        match self {
            State::Closing => true,
            _ => false,
        }
    }
    /// Is this `State` `Closed`.
    pub fn is_closed(&self) -> bool {
        match self {
            State::Closed => true,
            _ => false,
        }
    }
}

/// This error occurs only if the URL passed to `connect`, or the URL or protocols passed to
/// `connect_with_protocols` is malformed.
#[derive(Debug)]
pub struct ConnectError {
    msg: String,
}

impl ConnectError {
    /// Get from a JsValue, assuming it is a DOMException.
    fn from_jsvalue(value: JsValue) -> Self {
        ConnectError {
            msg: js_value_to_exception(value, "SyntaxError"),
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for ConnectError {}

/// There was an error closing the connection.
#[derive(Debug, Clone)]
pub enum CloseError {
    /// An invalid reason code was passed.
    InvalidCode(u16),
    /// An invalid reason string was passed.
    InvalidReason(String),
}

impl fmt::Display for CloseError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CloseError::InvalidCode(code) => write!(
                f,
                "expected a number in the set `1000 | 3000..=4999`, found {}",
                code
            ),
            CloseError::InvalidReason(reason) => write!(
                f,
                "expected a string with length <= 123, found one with length {} (\"{}\")",
                reason.len(),
                reason
            ),
        }
    }
}

impl std::error::Error for CloseError {}

/// Attempted to send a message before the connection was established.
#[derive(Debug, Clone)]
pub struct SendError;

impl fmt::Display for SendError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "tried to send a message before the websocket connection was established"
        )
    }
}

impl std::error::Error for SendError {}

#[cfg(feature = "futures")]
pub mod futures {
    use super::{callback, CloseEvent, ConnectError, Message, OutgoingMessage, State as WsState};
    use ::{
        futures_core::stream::Stream,
        futures_sink::Sink,
        std::{
            cell::RefCell,
            collections::VecDeque,
            pin::Pin,
            rc::Rc,
            task::{Context, Poll, Waker},
        },
        wasm_bindgen::UnwrapThrowExt,
    };

    struct State {
        ws_state: WsState,
        has_errd: bool,
        messages: VecDeque<Message>,
        waker: Option<Waker>,
    }

    impl State {
        fn new() -> Self {
            State {
                ws_state: WsState::Connecting,
                has_errd: false,
                messages: VecDeque::new(),
                waker: None,
            }
        }

        /// Store a waker, and panic if a waker is already stored.
        fn set_waker<T>(&mut self, cx: &mut Context) -> Poll<T> {
            if let Some(_waker) = self.waker.replace(cx.waker().to_owned()) {
                // I'm pretty sure that you can't poll the same websocket multiple times (because
                // you must have a unique reference (&mut), but I need to check this.
                panic!("polling websocket multiple times")
            }
            Poll::Pending
        }

        /// Update the state and then wake the registered task (if any).
        fn modify(this: Rc<RefCell<Self>>, cb: impl FnOnce(&mut State)) {
            let mut guard = this.borrow_mut();
            if guard.has_errd {
                panic!("no events should fire after error");
            }
            cb(&mut *guard);
            let waker = guard.waker.take();
            drop(guard);
            if let Some(waker) = waker {
                waker.wake();
            }
        }
    }

    pub struct WebSocket {
        inner: callback::WebSocket,
        state: Rc<RefCell<State>>,
    }

    impl Stream for WebSocket {
        type Item = Result<Message, ConnectionError>;

        fn poll_next(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Option<Self::Item>> {
            let mut state = self.state.borrow_mut();
            if state.has_errd {
                return Poll::Ready(Some(Err(ConnectionError)));
            }
            if let Some(msg) = state.messages.pop_front() {
                Poll::Ready(Some(Ok(msg)))
            } else if state.ws_state.is_closing() || state.ws_state.is_closed() {
                Poll::Ready(None)
            } else {
                state.set_waker(cx)
            }
        }
    }

    impl Sink<&str> for WebSocket {
        type Error = ConnectionError;

        fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.poll_ready_impl(cx)
        }

        fn start_send(self: Pin<&mut Self>, item: &str) -> Result<(), Self::Error> {
            self.start_send_impl(item)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.poll_flush_impl(cx)
        }

        fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.poll_close_impl(cx)
        }
    }

    impl Sink<&[u8]> for WebSocket {
        type Error = ConnectionError;

        fn poll_ready(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.poll_ready_impl(cx)
        }

        fn start_send(self: Pin<&mut Self>, item: &[u8]) -> Result<(), Self::Error> {
            self.start_send_impl(item)
        }

        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.poll_flush_impl(cx)
        }

        fn poll_close(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Self::Error>> {
            self.poll_close_impl(cx)
        }
    }

    impl WebSocket {
        pub fn connect(url: &str) -> Result<Self, ConnectError> {
            // todo check if we need unbounded.
            let state = Rc::new(RefCell::new(State::new()));
            let open_state = state.clone();
            let error_state = state.clone();
            let close_state = state.clone();
            let message_state = state.clone();

            let inner = callback::WebSocket::connect(
                url,
                // on open
                move || State::modify(open_state, |state| state.ws_state = WsState::Open),
                // on error
                move || State::modify(error_state.clone(), |state| state.has_errd = true),
                move |close| {
                    State::modify(close_state, |state| {
                        if close.was_clean {
                            state.ws_state = WsState::Closed;
                        } else {
                            state.has_errd = true;
                        }
                    })
                },
                move |message| {
                    State::modify(message_state.clone(), |state| {
                        state.messages.push_back(message)
                    })
                },
            )?;

            Ok(WebSocket { inner, state })
        }

        // These functions are identical for both text and binary messages, and so are refactored
        // out of the Sink impls.
        fn poll_ready_impl(
            self: Pin<&mut Self>,
            cx: &mut Context,
        ) -> Poll<Result<(), ConnectionError>> {
            let mut state = self.state.borrow_mut();
            if state.has_errd {
                Poll::Ready(Err(ConnectionError))
            } else {
                match state.ws_state {
                    WsState::Connecting => state.set_waker(cx),
                    WsState::Open => Poll::Ready(Ok(())),
                    _ => Poll::Ready(Err(ConnectionError)),
                }
            }
        }

        fn start_send_impl(
            self: Pin<&mut Self>,
            item: &(impl OutgoingMessage + ?Sized),
        ) -> Result<(), ConnectionError> {
            let state = self.state.borrow();
            if state.has_errd {
                Err(ConnectionError)
            } else {
                match state.ws_state {
                    WsState::Connecting => panic!("Called start_send without poll_ready"),
                    WsState::Open => Ok(self.inner.send(item).unwrap_throw()),
                    WsState::Closing | WsState::Closed => Err(ConnectionError),
                }
            }
        }

        fn poll_flush_impl(
            self: Pin<&mut Self>,
            _cx: &mut Context,
        ) -> Poll<Result<(), ConnectionError>> {
            // the underlying websocket implementation does not provide an event for when no data
            // is waiting to be sent, so we have no way to wake the task.
            if self.inner.buffered_amount() > 0 {
                eprintln!(
                    "warn: this function will say that all data has been flushed, but in \
                           reality some data is still buffered"
                );
            }
            Poll::Ready(Ok(()))
        }

        fn poll_close_impl(
            self: Pin<&mut Self>,
            cx: &mut Context,
        ) -> Poll<Result<(), ConnectionError>> {
            let mut state = self.state.borrow_mut();
            if state.has_errd {
                Poll::Ready(Err(ConnectionError))
            } else {
                match state.ws_state {
                    WsState::Closed => Poll::Ready(Ok(())),
                    WsState::Closing => state.set_waker(cx),
                    _ => {
                        self.inner.close();
                        state.set_waker(cx)
                    }
                }
            }
        }
    }

    /// There was an error with the connection (the "error" event was fired).
    #[derive(Debug)]
    pub struct ConnectionError;
}

/// Convert a JsValue to an DomExcpetion, and check the exception name.
fn js_value_to_exception(value: JsValue, name: &str) -> String {
    debug_assert!(value.has_type::<web_sys::DomException>());
    let value: web_sys::DomException = value.unchecked_into();
    debug_assert_eq!(&value.name(), name);
    value.message()
}
