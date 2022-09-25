// Copyright(C) Facebook, Inc. and its affiliates.
use crate::error::NetworkError;
use async_trait::async_trait;
use bytes::Bytes;
use futures::stream::SplitSink;
use futures::stream::StreamExt as _;
use log::{info, warn, error, debug};
use std::error::Error;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_util::codec::{Framed, LengthDelimitedCodec};
use std::collections::HashMap;
use std::sync::{Arc};
use tokio::sync::{RwLock};
use crate::dvf_message::DvfMessage;
use futures::SinkExt;

#[cfg(test)]
#[path = "tests/receiver_tests.rs"]
pub mod receiver_tests;

/// Convenient alias for the writer end of the TCP channel.
pub type Writer = SplitSink<Framed<TcpStream, LengthDelimitedCodec>, Bytes>;
#[async_trait]
pub trait MessageHandler: Clone + Send + Sync + 'static {
    /// Defines how to handle an incoming message. A typical usage is to define a `MessageHandler` with a
    /// number of `Sender<T>` channels. Then implement `dispatch` to deserialize incoming messages and
    /// forward them through the appropriate delivery channel. Then `writer` can be used to send back
    /// responses or acknowledgements to the sender machine (see unit tests for examples).
    async fn dispatch(&self, writer: &mut Writer, message: Bytes) -> Result<(), Box<dyn Error>>;
}

/// For each incoming request, we spawn a new runner responsible to receive messages and forward them
/// through the provided deliver channel.
pub struct Receiver<Handler: MessageHandler> {
    /// Address to listen to.
    address: SocketAddr,
    /// Struct responsible to define how to handle received messages.
    handler_map: Arc<RwLock<HashMap<u64, Handler>>>,
    name: &'static str,
}

impl<Handler: MessageHandler> Receiver<Handler> {
    /// Spawn a new network receiver handling connections from any incoming peer.
    pub fn spawn(address: SocketAddr, handler_map: Arc<RwLock<HashMap<u64, Handler>>>, name: &'static str) {
        tokio::spawn(async move {
            Self { address, handler_map, name}.run().await;
        });
    }

    /// Main loop responsible to accept incoming connections and spawn a new runner to handle it.
    async fn run(&self) {
        let listener = TcpListener::bind(&self.address)
            .await
            .expect(format!("Failed to bind TCP address {}", self.address).as_str());

        info!("Listening on {}. [{:?}]", self.address, self.name);
        loop {
            let (socket, peer) = match listener.accept().await {
                Ok(value) => value,
                Err(e) => {
                    warn!("{}", NetworkError::FailedToListen(e));
                    continue;
                }
            };
            debug!("Incoming connection established with {}. Local: {}. [{:?}]", peer, self.address, self.name);
            self.spawn_runner(socket, peer).await;
        }
    }

    async fn spawn_runner(&self, socket: TcpStream, peer: SocketAddr) {
        let handler_map = self.handler_map.clone(); 
        let name = self.name.clone();

        tokio::spawn(async move {
            let transport = Framed::new(socket, LengthDelimitedCodec::new());
            let (mut writer, mut reader) = transport.split();
            while let Some(frame) = reader.next().await {
                match frame.map_err(|e| NetworkError::FailedToReceiveMessage(peer, e)) {
                    Ok(message) => {
                        // get validator
                        match bincode::deserialize::<DvfMessage>(&message[..]) {
                            Ok(dvf_message) => {
                                let validator_id = dvf_message.validator_id;
                                match handler_map.read().await.get(&validator_id) {
                                    Some(handler) => {
                                        // trunctate the prefix
                                        let msg = dvf_message.message;
                                        if let Err(e) = handler.dispatch(&mut writer, Bytes::from(msg)).await {
                                            error!("[VA {}] Handler dispatch error ({})", validator_id, e);
                                            return;
                                        }
                                    },
                                    None => {
                                        // [zico] Constantly review this. For now, we sent back a message, which is different from a normal 'Ack' message
                                        let _ = writer.send(Bytes::from("No handler found")).await;
                                        error!("[VA {}] Receive a message, but no handler found! [{:?}]", validator_id, name);                                    
                                    }
                                }
                            },
                            Err(e) => {
                                warn!("can't deserialize {}", e);
                                return;
                            }
                        }
                    }
                    Err(e) => {
                        warn!("{}", e);
                        return;
                    }
                }
            }
            warn!("Connection closed by peer {}", peer);
        });
    }
}
