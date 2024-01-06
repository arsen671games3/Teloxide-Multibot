mod axum_app;

use std::sync::Arc;

use axum_app::BotsManager;
use futures::future::join_all;

use tokio;
use teloxide::{prelude::*, update_listeners::webhooks};
use serde_json;
use reqwest::Url;


static PORT: u16 = 8085;
static TELEGRAM_API_URL: &str = "http://127.0.0.1:8081";


fn make_bot(token: &String) -> Bot{
    Bot::with_client(token, teloxide::net::client_from_env())
        .set_api_url(Url::parse(&TELEGRAM_API_URL).expect("TELEGRAM_API_URL error"))
}

async fn handler(bot: Bot, bots_manager: Arc<BotsManager>, msg: Message) -> Result<(), teloxide::RequestError> {
    bot.send_message(msg.chat.id, "pong").await?;
    let me = bot.get_me().await?;
    axum_app::stop_bot(me.id.0, &bots_manager.clone()).await;
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let idle;
    pretty_env_logger::init();
    {
        let bot_tokens: String = std::env::var("BOT_TOKENS").ok().expect("The BOT_TOKENS env variable needs to be set");
        let bot_tokens: Vec<String> = serde_json::from_str(&bot_tokens).expect("BOT_TOKENS error");
        let addr = ([127, 0, 0, 1], PORT).into();
        let url = format!("http://127.0.0.1:{PORT}/webhook/").parse().unwrap();

        println!("Starting...");

        let bots: Vec<Bot> = bot_tokens.iter().map(make_bot).collect();
        let usernames: Vec<String> = join_all(bots.iter().map(|bot| async {
            let username = bot.get_me().await.expect("Bot is not working").username.clone().unwrap();
            format!("@{username}")
        })).await;
        println!("Started {:?}", usernames);
        let (stop_flag, bots_manager) = axum_app::make_axum_app(webhooks::Options::new(addr, url));
        idle = stop_flag;
        let listeners_and_bots = join_all(bot_tokens.iter().zip(bots).map(|(bot_token, bot)| async {
            (axum_app::start_bot(
                bot.clone(), 
                axum_app::get_bot_id_from_bot_token(bot_token).unwrap(), 
                &bots_manager
            )
            .await
            .expect("Error while starting bot"), bot)
        })).await;
        let bots_manager_reference = Arc::new(bots_manager);

        for (listener, bot) in listeners_and_bots {
            let bots_manager_reference_clone = bots_manager_reference.clone();
            tokio::spawn((|| async move {
                Dispatcher::builder(bot, Update::filter_message().endpoint(handler))
                .default_handler(|_upd| Box::pin(async {}))
                .dependencies(dptree::deps![bots_manager_reference_clone])
                .enable_ctrlc_handler()
                .build()
                .dispatch_with_listener(
                    listener,
                    LoggingErrorHandler::with_custom_text("An error from the update listener")
                ).await 
            })());
        };
    }
    println!("idling");
    idle.await;
}
