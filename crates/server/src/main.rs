use std::{
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use tokio::{
    io::{self, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    runtime::Builder,
    sync::{
        broadcast::{self, error::RecvError},
        mpsc,
    },
};

const BIND_ADDR: &str = "0.0.0.0:1337";
const MAX_PACKET_BYTES: usize = 16 * 1024 * 1024;
const BROADCAST_CAPACITY: usize = 512;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MessageType {
    Ping = 0,
    Video = 1,
    Disconnect = 2,
    Audio = 3,
}

impl TryFrom<u8> for MessageType {
    type Error = io::Error;

    fn try_from(value: u8) -> io::Result<Self> {
        match value {
            0 => Ok(Self::Ping),
            1 => Ok(Self::Video),
            2 => Ok(Self::Disconnect),
            3 => Ok(Self::Audio),
            unknown => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown message type: {unknown}"),
            )),
        }
    }
}

#[derive(Clone, Debug)]
struct RelayedMessage {
    sender_id: u64,
    message_type: MessageType,
    payload: Arc<[u8]>,
}

fn main() -> io::Result<()> {
    Builder::new_multi_thread()
        .enable_io()
        .build()?
        .block_on(run_server())
}

async fn run_server() -> io::Result<()> {
    let listener = TcpListener::bind(BIND_ADDR).await?;
    let next_client_id = AtomicU64::new(1);
    let (message_tx, _) = broadcast::channel::<RelayedMessage>(BROADCAST_CAPACITY);

    eprintln!("pomme-screenshare-server listening on {BIND_ADDR}");

    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let client_id = next_client_id.fetch_add(1, Ordering::Relaxed);

        eprintln!("client {client_id} connected from {peer_addr}");

        tokio::spawn(handle_client(
            client_id,
            peer_addr,
            stream,
            message_tx.clone(),
            message_tx.subscribe(),
        ));
    }
}

async fn handle_client(
    client_id: u64,
    peer_addr: SocketAddr,
    stream: TcpStream,
    message_tx: broadcast::Sender<RelayedMessage>,
    mut message_rx: broadcast::Receiver<RelayedMessage>,
) {
    let (mut reader, mut writer) = stream.into_split();
    let (done_tx, mut done_rx) = mpsc::channel(2);

    let read_done_tx = done_tx.clone();
    let read_task = tokio::spawn(async move {
        let result = read_from_client(client_id, &mut reader, message_tx).await;
        let _ = read_done_tx.send(("read", result)).await;
    });

    let write_task = tokio::spawn(async move {
        let result = write_to_client(client_id, &mut writer, &mut message_rx).await;
        let _ = done_tx.send(("write", result)).await;
    });

    if let Some((direction, result)) = done_rx.recv().await {
        match direction {
            "read" => write_task.abort(),
            "write" => read_task.abort(),
            _ => {}
        }

        report_client_result(client_id, peer_addr, direction, result);
    } else {
        read_task.abort();
        write_task.abort();
    }
}

async fn read_from_client<R>(
    client_id: u64,
    reader: &mut R,
    message_tx: broadcast::Sender<RelayedMessage>,
) -> io::Result<()>
where
    R: AsyncRead + Unpin,
{
    loop {
        let Some(payload) = read_packet(reader).await? else {
            return Ok(());
        };

        let Some((&message_type, payload)) = payload.split_first() else {
            continue;
        };
        let message_type = MessageType::try_from(message_type)?;

        match message_type {
            MessageType::Ping => continue,
            MessageType::Disconnect => return Ok(()),
            MessageType::Video | MessageType::Audio => {
                if message_tx
                    .send(RelayedMessage {
                        sender_id: client_id,
                        message_type,
                        payload: payload.into(),
                    })
                    .is_err()
                {
                    return Ok(());
                }
            }
        }
    }
}

async fn write_to_client<W>(
    client_id: u64,
    writer: &mut W,
    message_rx: &mut broadcast::Receiver<RelayedMessage>,
) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    loop {
        let message = match message_rx.recv().await {
            Ok(message) => message,
            Err(RecvError::Lagged(skipped)) => {
                eprintln!("client {client_id} skipped {skipped} packets");
                continue;
            }
            Err(RecvError::Closed) => return Ok(()),
        };

        if message.sender_id == client_id {
            continue;
        }

        let mut payload = Vec::with_capacity(message.payload.len() + 1);
        payload.push(message.message_type as u8);
        payload.extend_from_slice(&message.payload);
        write_packet(writer, &payload).await?;
    }
}

fn report_client_result(
    client_id: u64,
    peer_addr: SocketAddr,
    direction: &str,
    result: io::Result<()>,
) {
    match result {
        Ok(()) => eprintln!("client {client_id} {direction} closed from {peer_addr}"),
        Err(error) => eprintln!("client {client_id} {direction} error from {peer_addr}: {error}"),
    }
}

async fn read_packet<R>(reader: &mut R) -> io::Result<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin,
{
    let len = match reader.read_u32().await {
        Ok(len) => len as usize,
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    };

    if len > MAX_PACKET_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("packet too large: {len} bytes"),
        ));
    }

    let mut payload = vec![0; len];
    reader.read_exact(&mut payload).await?;
    Ok(Some(payload))
}

async fn write_packet<W>(writer: &mut W, payload: &[u8]) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    writer.write_u32(payload.len() as u32).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}
