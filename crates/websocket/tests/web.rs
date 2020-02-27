//! Test suite for the Web and headless browsers.

use futures_rs::channel::mpsc;
use futures_rs::prelude::*;
use wasm_bindgen_test::*;

use gloo_websocket::{callback, Message};

#[cfg(feature = "futures")]
use gloo_websocket::futures;

wasm_bindgen_test_configure!(run_in_browser);

const ECHO_SERVER: &'static str = "wss://echo.websocket.org";

#[wasm_bindgen_test]
async fn echo() {
    let (sender, mut receiver) = mpsc::unbounded();

    let open_sender = sender.clone();
    let error_sender = sender.clone();
    let close_sender = sender.clone();
    let ws = callback::WebSocket::connect(
        ECHO_SERVER,
        move || {
            open_sender.unbounded_send(String::from("OPEN")).unwrap();
        },
        move || {
            error_sender.unbounded_send(String::from("ERROR")).unwrap();
        },
        move |_| {
            close_sender.unbounded_send(String::from("CLOSE")).unwrap();
        },
        move |msg| {
            sender
                .unbounded_send(match msg {
                    Message::Binary(msg) => {
                        format!("MESSAGE BINARY {}", String::from_utf8_lossy(&msg))
                    }
                    Message::Text(msg) => format!("MESSAGE TEXT {}", msg),
                })
                .unwrap();
        },
    )
    .unwrap();
    assert_eq!(&receiver.next().await.unwrap(), "OPEN");

    ws.send("Hello").unwrap();
    assert_eq!(&receiver.next().await.unwrap(), "MESSAGE TEXT Hello");

    ws.send(&b"Hello"[..]).unwrap();
    assert_eq!(&receiver.next().await.unwrap(), "MESSAGE BINARY Hello");

    ws.close();
    assert_eq!(&receiver.next().await.unwrap(), "CLOSE");
}

#[cfg(feature = "futures")]
#[wasm_bindgen_test]
async fn echo_async() {
    let mut ws = futures::WebSocket::connect(ECHO_SERVER).unwrap();

    // Send a text message.
    // Here the `Sink` takes care of waiting for the socket to be set up before sending.
    ws.send("Hello").await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().as_text().unwrap(),
        "Hello"
    );

    // Send a binary message.
    ws.send(&b"Hello"[..]).await.unwrap();
    assert_eq!(
        ws.next().await.unwrap().unwrap().as_binary().unwrap(),
        &b"Hello"[..]
    );
    <futures::WebSocket as sink::SinkExt<&str>>::close(&mut ws)
        .await
        .unwrap();
}
