use actix_web::{
    error::{ErrorBadRequest, ErrorInternalServerError},
    get, post,
    web::{self, Data},
    App, HttpResponse, HttpServer, Responder,
};
use anyhow::{anyhow, Result};
use futures::{future, try_join, FutureExt};
use rand::Rng;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Instant,
};
use structopt::StructOpt;
use tokio::sync::oneshot;
use ya_relay_client::{Client, ClientBuilder, FailFast, GenericSender};
use ya_relay_core::{
    crypto::FallbackCryptoProvider,
    key::{load_or_generate, Protected},
    NodeId,
};

use crate::response::{Pong, Transfer};

#[path = "http_client/response.rs"]
mod response;
#[path = "http_client/wrap.rs"]
mod wrap;

#[derive(StructOpt)]
struct Cli {
    #[structopt(long, env = "API_PORT")]
    api_port: u16,
    #[structopt(long, env = "P2P_BIND_ADDR")]
    p2p_bind_addr: Option<url::Url>,
    #[structopt(long, env = "RELAY_ADDR")]
    relay_addr: url::Url,
    #[structopt(long, env = "KEY_FILE")]
    key_file: Option<String>,
    #[structopt(long, env = "PASSWORD", parse(from_str = Protected::from))]
    password: Option<Protected>,
}

type ClientWrap = self::wrap::SendWrap<Client>;

type RequestIdToMessageResponse = HashMap<u32, (Instant, oneshot::Sender<Result<String, String>>)>;

#[derive(Clone, Default)]
struct Messages {
    inner: Arc<Mutex<RequestIdToMessageResponse>>,
}

struct RequestGuard {
    inner: Arc<Mutex<RequestIdToMessageResponse>>,
    id: u32,
    rx: oneshot::Receiver<Result<String, String>>,
}

impl Messages {
    pub fn request(&self) -> RequestGuard {
        let id = rand::thread_rng().gen();
        let inner = self.inner.clone();
        let (tx, rx) = oneshot::channel();

        inner.lock().unwrap().insert(id, (Instant::now(), tx));

        RequestGuard { inner, id, rx }
    }

    pub fn respond(
        &self,
        request_id: u32,
    ) -> Result<(Instant, oneshot::Sender<Result<String, String>>)> {
        self.inner
            .lock()
            .unwrap()
            .remove(&request_id)
            .ok_or_else(|| anyhow!("response to invalid request {}", request_id))
    }
}

impl RequestGuard {
    pub fn id(&self) -> u32 {
        self.id
    }

    pub async fn result(mut self) -> Result<String, String> {
        let rx = &mut self.rx;
        rx.await.map_err(|e| e.to_string())?
    }
}

impl Drop for RequestGuard {
    fn drop(&mut self) {
        let _ = self.inner.lock().unwrap().remove(&self.id);
    }
}

#[get("/find-node/{node_id}")]
async fn find_node(
    node_id: web::Path<String>,
    client_sender: web::Data<ClientWrap>,
) -> actix_web::Result<HttpResponse> {
    let node_id = node_id.parse::<NodeId>().map_err(ErrorBadRequest)?;
    let (node, duration) = client_sender
        .run_async(move |client: Client| async move {
            let now = Instant::now();
            let node = client.find_node(node_id).await?;

            Ok::<_, anyhow::Error>((node, now.elapsed()))
        })
        .await
        .map_err(|e| {
            log::error!("Run async failed {e}");
            ErrorInternalServerError(e)
        })?
        .map_err(|e| {
            log::error!("Find node failed {e}");
            ErrorInternalServerError(e)
        })?;

    let node = response::Node(node);
    let msg = response::FindNode { node, duration };
    log::debug!("[find-node]: {}", msg);
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
}

#[get("/ping/{node_id}")]
async fn ping(
    node_id: web::Path<NodeId>,
    client_sender: web::Data<ClientWrap>,
    messages: web::Data<Messages>,
) -> actix_web::Result<HttpResponse> {
    let node_id = node_id.into_inner();
    let msg = client_sender
        .run_async(move |client: Client| async move {
            let mut sender = client.forward_reliable(node_id).await?;
            let r = messages.request();
            let msg = format!("Ping:{}", r.id());

            sender.send(msg.as_bytes().to_vec().into()).await?;

            r.result().await.map_err(|e| anyhow!("{e}"))
        })
        .await
        .map_err(|e| {
            log::error!("Run async failed {e}");
            ErrorInternalServerError(e)
        })?
        .map_err(|e| {
            log::error!("Ping failed {e}");
            ErrorInternalServerError(e)
        })?;
    log::debug!("[ping]: {}", msg);
    response::ok_json::<Pong>(&msg)
}

#[get("/sessions")]
async fn sessions(client_sender: web::Data<ClientWrap>) -> impl Responder {
    let msg = client_sender
        .run_async(move |client: Client| async move {
            client.sessions().map(response::Sessions::from).await
        })
        .await
        .map_err(ErrorInternalServerError)?;
    Ok::<_, actix_web::Error>(HttpResponse::Ok().json(msg))
}

#[post("/transfer-file/{node_id}")]
async fn transfer_file(
    node_id: web::Path<NodeId>,
    client_sender: web::Data<ClientWrap>,
    messages: web::Data<Messages>,
    body: web::Bytes,
) -> actix_web::Result<HttpResponse> {
    let node_id = node_id.into_inner();
    let msg = client_sender
        .run_async(move |client: Client| async move {
            let data: Vec<u8> = body.into();

            let r = messages.request();
            let end_message = format!("Transfer:{}:{}", r.id(), data.len());

            let mut sender = client.forward_reliable(node_id).await?;

            sender.send(data.into()).await?;
            sender.send(end_message.as_bytes().to_vec().into()).await?;

            r.result().await.map_err(|e| anyhow!("{e}"))
        })
        .await
        .map_err(|e| {
            log::error!("Run async failed {e}");
            ErrorInternalServerError(e)
        })?
        .map_err(|e| {
            log::error!("Transfer file failed {e}");
            ErrorInternalServerError(e)
        })?;
    log::debug!("[transfer-file]: {}", msg);
    response::ok_json::<response::Transfer>(&msg)
}

async fn receiver_task(client: Client, messages: Messages) -> anyhow::Result<()> {
    let mut receiver = client
        .forward_receiver()
        .await
        .ok_or(anyhow!("Couldn't get forward receiver"))?;

    while let Some(fwd) = receiver.recv().await {
        if let Err(e) = handle_forward_message(fwd, &client, &messages).await {
            log::warn!("Handle forward message failed: {e}")
        }
    }
    Ok(())
}

async fn handle_forward_message(
    fwd: ya_relay_client::channels::Forwarded,
    client: &Client,
    messages: &Messages,
) -> Result<()> {
    match fwd.transport {
        ya_relay_client::model::TransportType::Reliable => {
            log::info!(
                "Got forward message. Node {}. Transport {}",
                fwd.node_id,
                fwd.transport
            );
            let msg = String::from_utf8(fwd.payload.into_vec())?;

            let mut s = msg.split(':');
            let command = s
                .next()
                .ok_or_else(|| anyhow!("No message command found"))?;
            let request_id = s
                .next()
                .ok_or_else(|| anyhow!("No request ID found"))?
                .parse::<u32>()?;

            match command {
                "Ping" => {
                    let mut sender = client.forward_reliable(fwd.node_id).await?;
                    sender
                        .send(format!("Pong:{request_id}").as_bytes().to_vec().into())
                        .await?;

                    Ok(())
                }
                "Pong" => {
                    match messages.respond(request_id) {
                        Ok((ts, sender)) => sender
                            .send(Ok(serde_json::to_string(&Pong {
                                node_id: fwd.node_id.to_string(),
                                duration: ts.elapsed(),
                            })?))
                            .ok(),
                        Err(e) => {
                            log::warn!("ping: {:?}", e);
                            None
                        }
                    };
                    Ok(())
                }
                "Transfer" => {
                    let mut sender = client.forward_reliable(fwd.node_id).await?;
                    let bytes_transferred = s
                        .next()
                        .ok_or_else(|| anyhow!("No data found"))?
                        .parse::<usize>()?;

                    sender
                        .send(
                            format!("TransferResponse:{request_id}:{bytes_transferred}")
                                .as_bytes()
                                .to_vec()
                                .into(),
                        )
                        .await?;

                    Ok(())
                }
                "TransferResponse" => {
                    match messages.respond(request_id) {
                        Ok((ts, sender)) => {
                            let bytes_transferred = s
                                .next()
                                .ok_or_else(|| anyhow!("No bytes_transferred found"))?
                                .parse::<usize>()?;
                            let mb_transfered = bytes_transferred / (1024 * 1024);

                            sender
                                .send(Ok(serde_json::to_string(&Transfer {
                                    mb_transfered,
                                    node_id: fwd.node_id.to_string(),
                                    duration: ts.elapsed(),
                                    speed: mb_transfered as f32 / ts.elapsed().as_secs_f32(),
                                })?))
                                .ok()
                        }
                        Err(e) => {
                            log::warn!("ping: {:?}", e);
                            None
                        }
                    };
                    Ok(())
                }
                other_cmd => Err(anyhow!("Invalid command: {other_cmd}")),
            }
        }
        ya_relay_client::model::TransportType::Unreliable => Ok(()),
        ya_relay_client::model::TransportType::Transfer => Ok(()),
    }
}

async fn run() -> Result<()> {
    env_logger::init();

    let cli = Cli::from_args();
    let client = build_client(
        cli.relay_addr,
        cli.p2p_bind_addr,
        cli.key_file.as_deref(),
        cli.password,
    )
    .await?;
    let client_cloned = client.clone();

    let messages = Messages::default();
    let messages_cloned = messages.clone();

    let receiver = receiver_task(client_cloned, messages_cloned);

    let client = Data::new(wrap::wrap(client));
    let web_messages = Data::new(messages);

    let port = cli.api_port;

    let http_server = HttpServer::new(move || {
        App::new()
            .app_data(client.clone())
            .app_data(web_messages.clone())
            .app_data(web::PayloadConfig::new(1024 * 1024 * 1024 * 4))
            .service(find_node)
            .service(ping)
            .service(sessions)
            .service(transfer_file)
    })
    .workers(4)
    .bind(("0.0.0.0", port))?
    .run();

    let handle = http_server.handle();

    try_join!(
        http_server.then(|_| future::err::<(), anyhow::Error>(anyhow!("stop"))),
        async move {
            try_join!(receiver)?;
            log::error!("exit!");
            handle.stop(true).await;
            Ok(())
        },
    )?;

    Ok(())
}

async fn build_client(
    relay_addr: url::Url,
    p2p_bind_addr: Option<url::Url>,
    key_file: Option<&str>,
    password: Option<Protected>,
) -> Result<Client> {
    let secret = key_file.map(|key_file| load_or_generate(key_file, password));
    let provider = if let Some(secret_key) = secret {
        FallbackCryptoProvider::new(secret_key)
    } else {
        FallbackCryptoProvider::default()
    };

    let mut builder = ClientBuilder::from_url(relay_addr).crypto(provider);

    if let Some(bind) = p2p_bind_addr {
        builder = builder.listen(bind);
    }

    let client = builder.connect(FailFast::Yes).build().await?;

    log::info!("CLIENT NODE ID: {}", client.node_id());
    log::info!("CLIENT BIND ADDR: {:?}", client.bind_addr().await);
    log::info!("CLIENT PUBLIC ADDR: {:?}", client.public_addr().await);

    Ok(client)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local_set = tokio::task::LocalSet::new();
    local_set.run_until(run()).await
}