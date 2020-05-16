use std::{
    collections::{HashMap, VecDeque},
    sync::atomic::{AtomicU32, Ordering},
    task::{Context, Poll},
    time::{Duration, Instant},
};

use bytes::Bytes;
use futures::{
    future::{self, select, Either},
    pin_mut, Stream, TryStream,
};
use futures_util::{sink::SinkExt, FutureExt, StreamExt, TryStreamExt};
use lazy_static::lazy_static;
use tokio::{
    net::{TcpStream, ToSocketAddrs},
    sync::{
        mpsc::{self, UnboundedReceiver},
        oneshot::{self, Sender},
    },
    task::JoinHandle,
};
use tokio_io_timeout::TimeoutStream;
use tokio_util::codec::Framed;
use tracing::{debug, info};

use mqtt3::{
    proto::{
        ClientId, Connect, DecodeError, Packet, PacketCodec, PacketIdentifier,
        PacketIdentifierDupQoS, PubAck, Publication, Publish, QoS, Subscribe, SubscribeTo,
    },
    Client, Event, PublishError, PublishHandle, ReceivedPublication, ShutdownHandle,
    UpdateSubscriptionHandle, PROTOCOL_LEVEL, PROTOCOL_NAME,
};

pub struct Bridge {
    remote: Connection,
    local: Connection,
}

impl Bridge {
    pub async fn new(storage: &mut Storage) -> Self {
        let mut remote = Connection {
            prefix: "local".into(),
            queue: storage.new_queue("to_remote"),
            client: PacketStream::connect(
                ClientId::IdWithCleanSession("remote".into()),
                "localhost:2883",
                None,
                None,
            )
            .await,
        };

        let subscribe = Subscribe {
            packet_identifier: PacketIdentifier::new(1).unwrap(),
            subscribe_to: vec![mqtt3::proto::SubscribeTo {
                topic_filter: "topic".into(),
                qos: QoS::AtLeastOnce,
            }],
        };
        remote.client.send_subscribe(subscribe).await;

        let mut local = Connection {
            prefix: "remote".into(),
            queue: storage.new_queue("to_local"),
            client: PacketStream::connect(
                ClientId::IdWithCleanSession("local".into()),
                "localhost:1883",
                None,
                None,
            )
            .await,
        };

        let subscribe = Subscribe {
            packet_identifier: PacketIdentifier::new(1).unwrap(),
            subscribe_to: vec![mqtt3::proto::SubscribeTo {
                topic_filter: "topic".into(),
                qos: QoS::AtLeastOnce,
            }],
        };
        local.client.send_subscribe(subscribe).await;

        Self { remote, local }
    }

    pub async fn run(mut self) {
        loop {
            match future::select(self.local.client.next(), self.remote.client.next()).await {
                Either::Left((packet, _outgoing)) => {
                    debug!("a next packet received from local client: {:?}", packet);
                    match packet {
                        Some(packet) => {
                            process_packet(packet, &mut self.local, &mut self.remote).await
                        }
                        None => {
                            info!("local client disconnected from broker");
                            break;
                        }
                    }
                }
                Either::Right((packet, _incoming)) => {
                    debug!("a next packet received from remote client: {:?}", packet);
                    match packet {
                        Some(packet) => {
                            process_packet(packet, &mut self.remote, &mut self.local).await
                        }
                        None => {
                            info!("remote client disconnected from broker");
                            break;
                        }
                    }
                }
            }
        }
    }
}

async fn process_packet(packet: Packet, from: &mut Connection, to: &mut Connection) {
    match packet {
        Packet::Publish(publish) => {
            // translate topic for a publication
            let (publication, puback) = from.handle_publish(publish);

            // store converted publication in the queue to remote
            if let Some(publication) = publication {
                to.queue.push(publication)
            }

            // send a PUBACK that we received a publication and stored it
            if let Some(puback) = puback {
                from.client.send_puback(puback).await;
            }

            // send all messages to remote broker until we reach max inflight messages
            while let Ok(publish) = to.queue.take() {
                to.client.send_publish(publish).await;
            }
        }
        Packet::PubAck(puback) => {
            // discard in-flight publish from local queue
            from.queue.remove(puback.packet_identifier);

            // send all messages to local broker until we reach max inflight messages
            while let Ok(publish) = to.queue.take() {
                from.client.send_publish(publish).await;
            }
        }
        _ => {}
    }
}

struct Connection {
    prefix: String,
    queue: Queue,
    client: PacketStream,
}

impl Connection {
    fn handle_publish(&mut self, publish: Publish) -> (Option<Publication>, Option<PubAck>) {
        if let Some(topic) = self.filter(&publish.topic_name) {
            match publish.packet_identifier_dup_qos {
                PacketIdentifierDupQoS::AtMostOnce => {
                    let publication = Publication {
                        topic_name: topic,
                        qos: QoS::AtMostOnce,
                        retain: publish.retain,
                        payload: publish.payload,
                    };
                    (Some(publication), None)
                }
                PacketIdentifierDupQoS::AtLeastOnce(packet_identifier, _dup) => {
                    let publication = Publication {
                        topic_name: topic,
                        qos: QoS::AtLeastOnce,
                        retain: publish.retain,
                        payload: publish.payload,
                    };
                    let puback = PubAck { packet_identifier };
                    (Some(publication), Some(puback))
                }
                PacketIdentifierDupQoS::ExactlyOnce(_, _) => todo!(),
            }
        } else {
            (None, None)
        }
    }

    fn filter(&self, topic: &str) -> Option<String> {
        Some(format!("{}/{}", self.prefix, topic))
    }
}

pub struct Storage {
    // queues: HashMap<String, Queue>,
}

impl Storage {
    fn new_queue(&mut self, name: &str) -> Queue {
        Queue::new(5)
    }
}

struct Queue {
    max_in_flight: usize,
    items: VecDeque<Publication>,
    packet_identifiers: PacketIdentifiers,
    to_be_sent: HashMap<PacketIdentifier, Publish>,
}

impl Queue {
    fn new(max_in_flight: usize) -> Self {
        Self {
            max_in_flight,
            items: VecDeque::new(),
            packet_identifiers: PacketIdentifiers::default(),
            to_be_sent: HashMap::default(),
        }
    }
    fn push(&mut self, publication: mqtt3::proto::Publication) {
        println!("{}", self.items.len());
        self.items.push_front(publication);
    }

    fn remove(&mut self, id: PacketIdentifier) -> bool {
        match self.to_be_sent.remove(&id) {
            Some(_) => {
                self.packet_identifiers.discard(id);
                true
            }
            None => false,
        }
    }

    fn take(&mut self) -> Result<mqtt3::proto::Publish, TakeError> {
        if self.to_be_sent.len() >= self.max_in_flight {
            return Err(TakeError::MaxInFlightReached);
        }

        if let Some(publication) = self.items.pop_back() {
            let publish = match publication.qos {
                QoS::AtMostOnce => Publish {
                    packet_identifier_dup_qos: PacketIdentifierDupQoS::AtMostOnce,
                    retain: publication.retain,
                    topic_name: publication.topic_name,
                    payload: publication.payload,
                },
                QoS::AtLeastOnce => {
                    let id = self.packet_identifiers.reserve().unwrap();
                    let publish = Publish {
                        packet_identifier_dup_qos: PacketIdentifierDupQoS::AtLeastOnce(id, false),
                        retain: publication.retain,
                        topic_name: publication.topic_name,
                        payload: publication.payload,
                    };

                    self.to_be_sent.insert(id, publish.clone());

                    publish
                }
                QoS::ExactlyOnce => {
                    // let id = self.packet_identifiers.reserve().unwrap();
                    // Publish {
                    //     packet_identifier_dup_qos: PacketIdentifierDupQoS::ExactlyOnce(id, false),
                    //     retain: publication.retain,
                    //     topic_name: publication.topic_name,
                    //     payload: publication.payload,
                    // }
                    todo!()
                }
            };

            Ok(publish)
        } else {
            Err(TakeError::Empty)
        }
    }
}

#[derive(Debug)]
enum TakeError {
    Empty,
    MaxInFlightReached,
}

lazy_static! {
    static ref DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);
}

/// A simple wrapper around TcpStream + PacketCodec to send specific packets
/// to a broker for more granular integration testing.
#[derive(Debug)]
pub struct PacketStream {
    codec: Framed<TimeoutStream<TcpStream>, PacketCodec>,
}

impl PacketStream {
    /// Creates a client and opens TCP connection to the server.
    /// No MQTT packets are sent at this moment.
    pub async fn open(server_addr: impl ToSocketAddrs) -> Self {
        // broker may not be available immediately in the test,
        // so we'll try to connect for some time.
        let mut result = TcpStream::connect(&server_addr).await;
        let start_time = Instant::now();
        while let Err(_) = result {
            tokio::time::delay_for(Duration::from_millis(100)).await;
            if start_time.elapsed() > *DEFAULT_TIMEOUT {
                break;
            }
            result = TcpStream::connect(&server_addr).await;
        }

        let tcp_stream = result.expect("unable to establish tcp connection");
        let mut timeout = TimeoutStream::new(tcp_stream);
        timeout.set_read_timeout(Some(*DEFAULT_TIMEOUT));
        timeout.set_write_timeout(Some(*DEFAULT_TIMEOUT));

        let codec = Framed::new(timeout, PacketCodec::default());

        Self { codec }
    }

    /// Creates a client, opens TCP connection to the server,
    /// and sends CONNECT packet.
    pub async fn connect(
        client_id: ClientId,
        server_addr: impl ToSocketAddrs,
        username: Option<String>,
        password: Option<String>,
    ) -> Self {
        let mut client = Self::open(server_addr).await;
        client
            .send_connect(Connect {
                username,
                password,
                client_id,
                will: None,
                keep_alive: Duration::from_secs(30),
                protocol_name: PROTOCOL_NAME.into(),
                protocol_level: PROTOCOL_LEVEL,
            })
            .await;
        client
    }

    pub async fn send_connect(&mut self, connect: Connect) {
        self.send_packet(Packet::Connect(connect)).await;
    }

    pub async fn send_publish(&mut self, publish: Publish) {
        self.send_packet(Packet::Publish(publish)).await;
    }

    pub async fn send_puback(&mut self, puback: PubAck) {
        self.send_packet(Packet::PubAck(puback)).await;
    }

    pub async fn send_subscribe(&mut self, subscribe: Subscribe) {
        self.send_packet(Packet::Subscribe(subscribe)).await;
    }

    pub async fn send_packet(&mut self, packet: Packet) {
        self.codec
            .send(packet)
            .await
            .expect("Unable to send a packet");
    }
}

impl Stream for PacketStream {
    type Item = Packet;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        match self.codec.poll_next_unpin(cx) {
            Poll::Ready(Some(result)) => {
                Poll::Ready(Some(result.expect("Error decoding incoming packet")))
            }
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Pending => Poll::Pending,
        }
    }
}

struct PacketIdentifiers {
    in_use: Box<[usize; PacketIdentifiers::SIZE]>,
    previous: mqtt3::proto::PacketIdentifier,
}

impl PacketIdentifiers {
    #[allow(clippy::doc_markdown)]
    /// Size of a bitset for every packet identifier
    ///
    /// Packet identifiers are u16's, so the number of usize's required
    /// = number of u16's / number of bits in a usize
    /// = pow(2, number of bits in a u16) / number of bits in a usize
    /// = pow(2, 16) / (size_of::<usize>() * 8)
    ///
    /// We use a bitshift instead of usize::pow because the latter is not a const fn
    const SIZE: usize = (1 << 16) / (std::mem::size_of::<usize>() * 8);

    fn reserve(&mut self) -> Result<mqtt3::proto::PacketIdentifier, mqtt3::Error> {
        let start = self.previous;
        let mut current = start;

        current += 1;

        let (block, mask) = self.entry(current);
        if (*block & mask) != 0 {
            return Err(mqtt3::Error::PacketIdentifiersExhausted);
        }

        *block |= mask;
        self.previous = current;
        Ok(current)
    }

    fn discard(&mut self, packet_identifier: mqtt3::proto::PacketIdentifier) {
        let (block, mask) = self.entry(packet_identifier);
        *block &= !mask;
    }

    fn entry(&mut self, packet_identifier: mqtt3::proto::PacketIdentifier) -> (&mut usize, usize) {
        let packet_identifier = usize::from(packet_identifier.get());
        let (block, offset) = (
            packet_identifier / (std::mem::size_of::<usize>() * 8),
            packet_identifier % (std::mem::size_of::<usize>() * 8),
        );
        (&mut self.in_use[block], 1 << offset)
    }
}

impl std::fmt::Debug for PacketIdentifiers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PacketIdentifiers")
            .field("previous", &self.previous)
            .finish()
    }
}

impl Default for PacketIdentifiers {
    fn default() -> Self {
        PacketIdentifiers {
            in_use: Box::new([0; PacketIdentifiers::SIZE]),
            previous: mqtt3::proto::PacketIdentifier::max_value(),
        }
    }
}
