use std::{convert::Infallible, future::Future, pin::Pin, collections::HashMap};
use std::sync::{Arc, RwLock};

use axum;
use axum::{
    extract::{FromRequestParts, State, Path},
    http::{request::Parts, status::StatusCode},
    response::IntoResponse,
};
use tokio::sync::mpsc;

use teloxide::{
    Bot,
    requests::Requester,
    stop::StopFlag,
    types::{Update, UpdateKind},
    update_listeners::{webhooks::Options, UpdateListener},
};
use teloxide::{
    stop::{mk_stop_token, StopToken},
    update_listeners::StatefulListener,
};
use teloxide::requests::Request;
use teloxide_core::requests::HasPayload;
use teloxide_core::RequestError;
use axum::routing::post;
use tokio_stream::wrappers::UnboundedReceiverStream;
use futures::FutureExt;


pub struct BotsManager {
    senders: SendersReference,
    stop_token: StopToken,
    options: Options,
}

type SendersReference = Arc<RwLock<Option<HashMap<u64, (UpdateSender, Bot)>>>>;
type UpdateSender = mpsc::UnboundedSender<Result<Update, std::convert::Infallible>>;
type UpdateCSender = ClosableSender<Result<Update, std::convert::Infallible>>;

#[derive(Clone)]
struct WebhookState {
    tx: UpdateCSender,
    flag: StopFlag,
    secret: Option<String>,
}

/// A terrible workaround to drop axum extension
struct ClosableSender<T> {
    origin: std::sync::Arc<std::sync::RwLock<Option<HashMap<u64, (mpsc::UnboundedSender<T>, Bot)>>>>,
}

impl<T> Clone for ClosableSender<T> {
    fn clone(&self) -> Self {
        Self { origin: self.origin.clone() }
    }
}

impl<T> ClosableSender<T> {
    fn new(sender: HashMap<u64, (mpsc::UnboundedSender<T>, Bot)>) -> Self {
        Self { origin: std::sync::Arc::new(std::sync::RwLock::new(Some(sender))) }
    }

    fn get(&self) -> Option<HashMap<u64, (mpsc::UnboundedSender<T>, Bot)>> {
        self.origin.read().unwrap().clone()
    }

    fn close(&mut self) {
        self.origin.write().unwrap().take();
    }
}

struct XTelegramBotApiSecretToken(Option<Vec<u8>>);

impl<S> FromRequestParts<S> for XTelegramBotApiSecretToken {
    type Rejection = StatusCode;

    fn from_request_parts<'l0, 'l1, 'at>(
        req: &'l0 mut Parts,
        _state: &'l1 S,
    ) -> Pin<Box<dyn Future<Output = Result<Self, Self::Rejection>> + Send + 'at>>
    where
        'l0: 'at,
        'l1: 'at,
        Self: 'at,
    {
        let res = req
            .headers
            .remove("x-telegram-bot-api-secret-token")
            .map(|header| {
                check_secret(header.as_bytes())
                    .map(<_>::to_owned)
                    .map_err(|_| StatusCode::BAD_REQUEST)
            })
            .transpose()
            .map(Self);

        Box::pin(async { res }) as _
    }
}


pub fn get_bot_id_from_bot_token(bot_token: &String) -> Option<u64> {
    bot_token.split(':').next()?.parse().ok()
}


async fn setup_webhook<R>(bot: R, bot_id: u64, options: &Options) -> Result<(), R::Err>
where
    R: Requester,
{
    let secret = options.secret_token.clone().expect("Need to start axum app first");
    let &Options {
        ref url, max_connections, drop_pending_updates, ..
    } = options;
    let url = format!("{url}/{bot_id}").parse().unwrap();

    let mut req = bot.set_webhook(url);
    req.payload_mut().certificate = None;
    req.payload_mut().max_connections = max_connections;
    req.payload_mut().drop_pending_updates = Some(drop_pending_updates);
    req.payload_mut().secret_token = Some(secret);

    req.send().await?;

    Ok(())
}


fn check_secret(bytes: &[u8]) -> Result<&[u8], &'static str> {
    let len = bytes.len();

    if !(1..=256).contains(&len) {
        return Err("secret token length must be in range 1..=256");
    }

    let is_not_supported =
        |c: &_| !matches!(c, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-');
    if bytes.iter().any(is_not_supported) {
        return Err("secret token must only contain of `a-z`, `A-Z`, `0-9`, `_` and `-` characters");
    }

    Ok(bytes)
}


async fn telegram_request(
    Path(bot_id): Path<u64>,
    State(WebhookState { secret, flag, mut tx }): State<WebhookState>,
    secret_header: XTelegramBotApiSecretToken,
    input: String,
) -> impl IntoResponse 
{
    // FIXME: use constant time comparison here
    if secret_header.0.as_deref() != secret.as_deref().map(str::as_bytes) {
        return StatusCode::UNAUTHORIZED;
    }

    let tx = match tx.get() {
        None => return StatusCode::SERVICE_UNAVAILABLE,
        // Do not process updates after `.stop()` is called even if the server is still
        // running (useful for when you need to stop the bot but can't stop the server).
        _ if flag.is_stopped() => {
            tx.close();
            return StatusCode::SERVICE_UNAVAILABLE;
        }
        Some(tx) => tx,
    };
    let tx = match tx.get(&bot_id) {
        Some(tx) => &tx.0,
        _ => return StatusCode::SERVICE_UNAVAILABLE,
    };

    match serde_json::from_str::<Update>(&input) {
        Ok(mut update) => {
            // See HACK comment in
            // `teloxide_core::net::request::process_response::{closure#0}`
            if let UpdateKind::Error(value) = &mut update.kind {
                *value = serde_json::from_str(&input).unwrap_or_default();
            }

            tx.send(Ok(update)).expect("Cannot send an incoming update from the webhook")
        }
        Err(error) => {
            println!(
                "Cannot parse an update.\nError: {:?}\nValue: {}\n\
                 This is a bug in teloxide-core, please open an issue here: \
                 https://github.com/teloxide/teloxide/issues.",
                error,
                input
            );
        }
    };

    StatusCode::OK
}


pub fn make_axum_app(mut options: Options, ) -> (StopFlag, BotsManager) {
    let Options { address, ref url, .. } = options;
    let address = address.clone();
    let path = format!("{}/:bot_id", url.path());

    let (stop_token, stop_flag) = mk_stop_token();

    let secret_token = Some(options.get_or_gen_secret_token().to_owned());
    let tx = ClosableSender::new(HashMap::new());
    let origin = tx.origin.clone();
    let senders = tx.origin.clone();
    let state = WebhookState {
        tx: tx,
        flag: stop_flag.clone(),
        secret: secret_token,
    };

    let delete_webhook_stop_flag = stop_flag.clone().then(|()| async move {
        let tx = match origin.read().unwrap().clone() {
            None => return,
            Some(tx) => tx,
        };
        for (_, bot) in tx.into_values() {
            // This assignment is needed to not require `R: Sync` since without it `&bot`
            // temporary lives across `.await` points.
            let req = bot.delete_webhook().send();
            let res = req.await;
            if let Err(err) = res {
                println!("Couldn't delete webhook: {}", err);
            }
        }
    });

    let stop_token_for_server = stop_token.clone();
    let app = axum::Router::new()
        .route(&path.as_str(), post(telegram_request))
        .with_state(state);
    tokio::spawn(async move {
        axum::Server::bind(&address)
            .serve(app.into_make_service())
            .with_graceful_shutdown(delete_webhook_stop_flag)
            .await
            .map_err(|err| {
                stop_token_for_server.stop();
                err
            })
            .expect("Axum server error");
    });
    (stop_flag, BotsManager{senders, stop_token, options})
}


pub async fn start_bot(
    bot: Bot,
    bot_id: u64,
    bots_manager: &BotsManager,
) -> Result<impl UpdateListener<Err = Infallible>, RequestError> {
    setup_webhook(&bot, bot_id, &bots_manager.options).await?;
    let senders = bots_manager.senders.clone();
    let receiver;
    {
        let mut lock = senders.write().unwrap();
        let mut translators = lock.clone().unwrap();
        let (tx, rx) = mpsc::unbounded_channel();
        translators.insert(bot_id, (tx, bot));
        receiver = rx;
        *lock = Some(translators);
    }
    let stream = UnboundedReceiverStream::new(receiver);

    fn tuple_first_mut<A, B>(tuple: &mut (A, B)) -> &mut A {
        &mut tuple.0
    }
    // FIXME: this should support `hint_allowed_updates()`
    Ok(StatefulListener::new(
        (stream, bots_manager.stop_token.clone()),
        tuple_first_mut,
        |state: &mut (_, StopToken)| state.1.clone(),
    ))
}


pub async fn stop_bot(
    bot_id: u64,
    bots_manager: &BotsManager,
) {
    let senders = bots_manager.senders.clone();
    let bot;
    {
        let mut lock = senders.write().unwrap();
        let mut translators = lock.clone().unwrap();
        bot = match translators.remove(&bot_id) {
            Some((_, bot)) => bot,
            _ => return
        };
        *lock = Some(translators);
    }
    
    // This assignment is needed to not require `R: Sync` since without it `&bot`
    // temporary lives across `.await` points.
    let req = bot.delete_webhook().send();
    let res = req.await;
    if let Err(err) = res {
        println!("Couldn't delete webhook: {}", err);
        return
    }
}
