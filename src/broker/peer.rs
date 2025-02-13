use async_std::{channel, future};
use async_std::io::BufReader;
use async_std::net::TcpStream;
use futures::select;
use futures::FutureExt;
use futures::io::BufWriter;
use log::{debug, error, info};
use rand::distributions::{Alphanumeric, DistString};
use url::Url;
use crate::{client, RpcMessage, RpcMessageMetaTags, RpcValue};
use crate::client::LoginParams;
use crate::rpcframe::RpcFrame;
use crate::shvnode::{DOT_LOCAL_DIR, DOT_LOCAL_GRANT, DOT_LOCAL_HACK, METH_PING};
use crate::util::{join_path, login_from_url, sha1_hash};
use crate::broker::{BrokerCommand, BrokerToPeerMessage, PeerKind};
use crate::broker::config::ParentBrokerConfig;
use crate::broker::Sender;
use crate::framerw::{FrameReader, FrameWriter};
use crate::streamrw::{StreamFrameReader, StreamFrameWriter};

pub(crate) async fn peer_loop(client_id: i32, broker_writer: Sender<BrokerCommand>, stream: TcpStream) -> crate::Result<()> {
    debug!("Entreing peer loop client ID: {client_id}.");
    let (socket_reader, socket_writer) = (&stream, &stream);
    let (peer_writer, peer_reader) = channel::unbounded::<BrokerToPeerMessage>();

    broker_writer.send(BrokerCommand::NewPeer { client_id, sender: peer_writer, peer_kind: PeerKind::Client }).await?;

    let brd = BufReader::new(socket_reader);
    let bwr = BufWriter::new(socket_writer);

    let mut frame_reader = StreamFrameReader::new(brd);
    let mut frame_writer = StreamFrameWriter::new(bwr);

    let mut device_options = RpcValue::null();
    loop {
        let nonce = {
            let frame = frame_reader.receive_frame().await?;
            let rpcmsg = frame.to_rpcmesage()?;
            let resp_meta = RpcFrame::prepare_response_meta(&frame.meta)?;
            if rpcmsg.method().unwrap_or("") != "hello" {
                frame_writer.send_error(resp_meta, "Invalid login message.").await?;
                continue;
            }
            debug!("Client ID: {client_id}, hello received.");
            let nonce = Alphanumeric.sample_string(&mut rand::thread_rng(), 16);
            let mut result = crate::Map::new();
            result.insert("nonce".into(), RpcValue::from(&nonce));
            frame_writer.send_result(resp_meta, result.into()).await?;
            nonce
        };
        {
            let frame = frame_reader.receive_frame().await?;
            let rpcmsg = frame.to_rpcmesage()?;
            let resp_meta = RpcFrame::prepare_response_meta(&frame.meta)?;
            if rpcmsg.method().unwrap_or("") != "login" {
                frame_writer.send_error(resp_meta, "Invalid login message.").await?;
                continue;
            }
            debug!("Client ID: {client_id}, login received.");
            let params = rpcmsg.param().ok_or("No login params")?.as_map();
            let login = params.get("login").ok_or("Invalid login params")?.as_map();
            let user = login.get("user").ok_or("User login param is missing")?.as_str();
            let password = login.get("password").ok_or("Password login param is missing")?.as_str();
            let login_type = login.get("type").map(|v| v.as_str()).unwrap_or("");

            broker_writer.send(BrokerCommand::GetPassword { client_id, user: user.to_string() }).await.unwrap();
            match peer_reader.recv().await? {
                BrokerToPeerMessage::PasswordSha1(broker_shapass) => {
                    let chkpwd = || {
                        match broker_shapass {
                            None => {false}
                            Some(broker_shapass) => {
                                if login_type == "PLAIN" {
                                    let client_shapass = sha1_hash(password.as_bytes());
                                    client_shapass == broker_shapass
                                } else {
                                    //info!("nonce: {}", nonce);
                                    //info!("broker password: {}", std::str::from_utf8(&broker_shapass).unwrap());
                                    //info!("client password: {}", password);
                                    let mut data = nonce.as_bytes().to_vec();
                                    data.extend_from_slice(&broker_shapass[..]);
                                    let broker_shapass = sha1_hash(&data);
                                    password.as_bytes() == broker_shapass
                                }
                            }
                        }
                    };
                    if chkpwd() {
                        debug!("Client ID: {client_id}, password OK.");
                        let mut result = crate::Map::new();
                        result.insert("clientId".into(), RpcValue::from(client_id));
                        frame_writer.send_result(resp_meta, result.into()).await?;
                        if let Some(options) = params.get("options") {
                            if let Some(device) = options.as_map().get("device") {
                                device_options = device.clone();
                            }
                        }
                        break;
                    } else {
                        debug!("Client ID: {client_id}, login credentials.");
                        frame_writer.send_error(resp_meta, "Invalid login credentials.").await?;
                        continue;
                    }
                }
                _ => {
                    panic!("Internal error, PeerEvent::PasswordSha1 expected");
                }
            }
        }
    };
    debug!("Client ID: {client_id} login sucess.");
    let register_device = BrokerCommand::RegisterDevice {
        client_id,
        device_id: device_options.as_map().get("deviceId").map(|v| v.as_str().to_string()),
        mount_point: device_options.as_map().get("mountPoint").map(|v| v.as_str().to_string()),
        subscribe_path: None,
    };
    broker_writer.send(register_device).await.unwrap();

    let mut fut_receive_frame = frame_reader.receive_frame().fuse();
    let mut fut_receive_broker_event = peer_reader.recv().fuse();
    loop {
        select! {
            frame = fut_receive_frame => match frame {
                Ok(frame) => {
                    // log!(target: "RpcMsg", Level::Debug, "----> Recv frame, client id: {}", client_id);
                    broker_writer.send(BrokerCommand::FrameReceived { client_id, frame }).await?;
                    drop(fut_receive_frame);
                    fut_receive_frame = frame_reader.receive_frame().fuse();
                }
                Err(e) => {
                    error!("Read socket error: {}", &e);
                    break;
                }
            },
            event = fut_receive_broker_event => match event {
                Err(e) => {
                    debug!("Peer channel closed: {}", &e);
                    break;
                }
                Ok(event) => {
                    match event {
                        BrokerToPeerMessage::PasswordSha1(_) => {
                            panic!("PasswordSha1 cannot be received here")
                        }
                        BrokerToPeerMessage::DisconnectByBroker => {
                            info!("Disconnected by broker, client ID: {client_id}");
                            break;
                        }
                        BrokerToPeerMessage::SendFrame(frame) => {
                            // log!(target: "RpcMsg", Level::Debug, "<---- Send frame, client id: {}", client_id);
                            frame_writer.send_frame(frame).await?;
                        }
                        BrokerToPeerMessage::SendMessage(rpcmsg) => {
                            // log!(target: "RpcMsg", Level::Debug, "<---- Send message, client id: {}", client_id);
                            frame_writer.send_message(rpcmsg).await?;
                        },
                    }
                    fut_receive_broker_event = peer_reader.recv().fuse();
                }
            }
        }
    }
    broker_writer.send(BrokerCommand::PeerGone { peer_id: client_id }).await?;
    info!("Client loop exit, client id: {}", client_id);
    Ok(())
}
pub(crate) async fn parent_broker_peer_loop_with_reconnect(client_id: i32, config: ParentBrokerConfig, broker_writer: Sender<BrokerCommand>) -> crate::Result<()> {
    let url = Url::parse(&config.client.url)?;
    if url.scheme() != "tcp" {
        return Err(format!("Scheme {} is not supported yet.", url.scheme()).into());
    }
    let reconnect_interval: std::time::Duration = 'interval: {
        if let Some(time_str) = &config.client.reconnect_interval {
            if let Ok(interval) = duration_str::parse(time_str) {
                break 'interval interval;
            }
        }
        const DEFAULT_RECONNECT_INTERVAL_SEC: u64 = 10;
        info!("Parent broker connection reconnect interval is not set explicitly, default value {DEFAULT_RECONNECT_INTERVAL_SEC} will be used.");
        std::time::Duration::from_secs(DEFAULT_RECONNECT_INTERVAL_SEC)
    };
    info!("Reconnect interval set to: {:?}", reconnect_interval);
    loop {
        match parent_broker_peer_loop(client_id, config.clone(), broker_writer.clone()).await {
            Ok(_) => {
                info!("Parent broker peer loop finished without error");
            }
            Err(err) => {
                error!("Parent broker peer loop finished with error: {err}");
            }
        }
        info!("Reconnecting to parent broker after: {:?}", reconnect_interval);
        async_std::task::sleep(reconnect_interval).await;
    }
}

fn cut_prefix(shv_path: &str, prefix: &str) -> Option<String> {
    if shv_path.starts_with(prefix) && (shv_path.len() == prefix.len() || shv_path[prefix.len() ..].starts_with('/')) {
        let shv_path = &shv_path[prefix.len() ..];
        if let Some(stripped_path) = shv_path.strip_prefix('/') {
            Some(stripped_path.to_string())
        } else {
            Some(shv_path.to_string())
        }
    } else {
        None
    }
}
async fn parent_broker_peer_loop(client_id: i32, config: ParentBrokerConfig, broker_writer: Sender<BrokerCommand>) -> crate::Result<()> {
    let url = Url::parse(&config.client.url)?;
    let (scheme, host, port) = (url.scheme(), url.host_str().unwrap_or_default(), url.port().unwrap_or(3755));
    if scheme != "tcp" {
        return Err(format!("Scheme {scheme} is not supported yet.").into());
    }
    let address = format!("{host}:{port}");
    // Establish a connection
    info!("Connecting to parent broker: tcp://{address}");
    let stream = TcpStream::connect(&address).await?;
    let (reader, writer) = (&stream, &stream);

    let brd = BufReader::new(reader);
    let bwr = BufWriter::new(writer);
    let mut frame_reader = StreamFrameReader::new(brd);
    let mut frame_writer = StreamFrameWriter::new(bwr);

    // login
    let (user, password) = login_from_url(&url);
    let heartbeat_interval = config.client.heartbeat_interval_duration()?;
    let login_params = LoginParams{
        user,
        password,
        mount_point: config.client.mount.clone().unwrap_or_default().to_owned(),
        device_id: config.client.device_id.clone().unwrap_or_default().to_owned(),
        heartbeat_interval,
        ..Default::default()
    };

    info!("Parent broker connected OK");
    info!("Heartbeat interval set to: {:?}", &heartbeat_interval);
    client::login(&mut frame_reader, &mut frame_writer, &login_params).await?;

    let (broker_to_peer_sender, broker_to_peer_receiver) = channel::unbounded::<BrokerToPeerMessage>();
    broker_writer.send(BrokerCommand::NewPeer { client_id, sender: broker_to_peer_sender, peer_kind: PeerKind::ParentBroker }).await?;

    let mut fut_receive_frame = frame_reader.receive_frame().fuse();
    let mut fut_receive_broker_event = broker_to_peer_receiver.recv().fuse();
    let make_timeout = || {
        Box::pin(future::timeout(heartbeat_interval, future::pending::<()>())).fuse()
    };
    let mut fut_timeout = make_timeout();
    loop {
        select! {
            res_timeout = fut_timeout => {
                assert!(res_timeout.is_err());
                // send heartbeat
                let msg = RpcMessage::new_request(".app", METH_PING, None);
                debug!("sending ping");
                frame_writer.send_message(msg).await?;
                fut_timeout = make_timeout();
            },
            res_frame = fut_receive_frame => match res_frame {
                Ok(mut frame) => {
                    if frame.is_request() {
                        fn is_dot_local_granted(frame: &RpcFrame) -> bool {
                                let access = frame.access().unwrap_or_default();
                                access.split(',').any(|s| s == DOT_LOCAL_GRANT)
                        }
                        fn is_dot_local_request(frame: &RpcFrame) -> bool {
                            let shv_path = frame.shv_path().unwrap_or_default();
                            if shv_path.starts_with(DOT_LOCAL_DIR)&& (shv_path.len() == DOT_LOCAL_DIR.len() || shv_path[DOT_LOCAL_DIR.len() ..].starts_with('/')) {
                                return is_dot_local_granted(frame);
                            }
                            false
                        }
                        let shv_path = frame.shv_path().unwrap_or_default().to_owned();
                        let shv_path = if let Some(stripped_path) = shv_path.strip_prefix('/') {
                            // parent broker can send requests with absolute path
                            // to call subscribe(), rejectNotSubscribe() etc.
                            stripped_path.to_string()
                        } else if is_dot_local_request(&frame) {
                            let shv_path = &shv_path[DOT_LOCAL_DIR.len() ..];
                            let shv_path = if let Some(stripped_path) = shv_path.strip_prefix('/') {
                                stripped_path } else { shv_path };
                            shv_path.to_string()
                        } else {
                            if shv_path.is_empty() && is_dot_local_granted(&frame) {
                                frame.meta.insert(DOT_LOCAL_HACK, true.into());
                            }
                            join_path(&config.exported_root, &shv_path)
                        };
                        frame.set_shvpath(&shv_path);
                        broker_writer.send(BrokerCommand::FrameReceived { client_id, frame }).await.unwrap();
                    }
                    drop(fut_receive_frame);
                    fut_receive_frame = frame_reader.receive_frame().fuse();
                }
                Err(e) => {
                    return Err(format!("Read frame error: {e}").into());
                }
            },
            event = fut_receive_broker_event => match event {
                Err(e) => {
                    debug!("broker loop has closed peer channel, client ID {client_id}");
                    return Err(e.into());
                }
                Ok(event) => {
                    match event {
                        BrokerToPeerMessage::PasswordSha1(_) => {
                            panic!("PasswordSha1 cannot be received here")
                        }
                        BrokerToPeerMessage::DisconnectByBroker => {
                            info!("Disconnected by parent broker, client ID: {client_id}");
                            break;
                        }
                        BrokerToPeerMessage::SendFrame(frame) => {
                            // log!(target: "RpcMsg", Level::Debug, "<---- Send frame, client id: {}", client_id);
                            let mut frame = frame;
                            if frame.is_signal() {
                                if let Some(new_path) = cut_prefix(frame.shv_path().unwrap_or_default(), &config.exported_root) {
                                    frame.set_shvpath(&new_path);
                                }
                            }
                            debug!("Sending rpc frame");
                            frame_writer.send_frame(frame).await?;
                            fut_timeout = make_timeout();
                        }
                        BrokerToPeerMessage::SendMessage(rpcmsg) => {
                            // log!(target: "RpcMsg", Level::Debug, "<---- Send message, client id: {}", client_id);
                            let mut rpcmsg = rpcmsg;
                            if rpcmsg.is_signal() {
                                if let Some(new_path) = cut_prefix(rpcmsg.shv_path().unwrap_or_default(), &config.exported_root) {
                                    rpcmsg.set_shvpath(&new_path);
                                }
                            }
                            debug!("Sending rpc message");
                            frame_writer.send_message(rpcmsg).await?;
                            fut_timeout = make_timeout();
                        },
                    }
                    fut_receive_broker_event = broker_to_peer_receiver.recv().fuse();
                }
            }
        }
    };

    Ok(())
}
