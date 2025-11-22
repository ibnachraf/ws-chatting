use bytes::Bytes;
use messaging_service::Messaging;
use serde::Serialize;
use server_ws::messaging_service;
use std::{
    net::TcpListener,
    sync::{Arc, Mutex, RwLock},
    thread::spawn,
};
use tungstenite::{
    Message, accept_hdr,
    handshake::{
        headers,
        server::{Request, Response},
    },
};

#[derive(Serialize)]
struct ResponseData {
    original_message: String,
    processed_message: String,
}

fn main() {
    println!("Hello, world!");

    let messaging_service = messaging_service::MessagingService {
        messages_wall: std::sync::Arc::new(std::sync::RwLock::new(Vec::new())),
    };

    let concurrent_messaging_service = Arc::new(messaging_service);

    let server = TcpListener::bind("127.0.0.1:3012").expect("Error while biding tcp address");
    for stream in server.incoming() {
        match stream {
            Ok(_stream) => {
                let svc = Arc::clone(&concurrent_messaging_service);
                spawn(move || {
                    println!("New connection established");
                    let user_id: Arc<RwLock<String>> = Arc::new(RwLock::new(String::new()));
                    let callback = |req: &Request, mut response: Response| {
                        println!("Received a new ws handshake");
                        let headers = req.headers();
                        let id = headers.get("id").unwrap().to_str().unwrap();
                        *user_id.write().unwrap() = id.to_string();
                        let body = response.body_mut();
                        Ok(response)
                    };
                    let mut websocket = accept_hdr(_stream, callback).unwrap();
                    loop {
                        let msg = websocket.read().unwrap();
                        let user_id_guard = user_id.read().unwrap();
                        svc.publish_message(&msg.to_string(), &user_id_guard)
                            .unwrap();
                        if msg.is_binary() || msg.is_text() {
                            let response_data = ResponseData {
                                original_message: msg.to_string(),
                                processed_message: msg.to_string().to_uppercase(),
                            };
                            let response = Bytes::from(serde_json::to_vec(&response_data).unwrap());

                            websocket.send(Message::Binary(response)).unwrap();
                        }
                    }
                });
            }
            Err(e) => {
                eprintln!("Error: {}", e);
            }
        }
    }
}
