mod axum_app;

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


#[tokio::main]
async fn main() {
    let idle;
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

        axum_app::stop_bot(
            axum_app::get_bot_id_from_bot_token(&bot_tokens[0]).unwrap(),
            &bots_manager
        ).await;

        for (listener, bot) in listeners_and_bots {
            tokio::spawn(teloxide::repl_with_listener(
                bot,
                |bot: Bot, msg: Message| async move {
                    bot.send_message(msg.chat.id, "pong").await?;
                    Ok(())
                },
                listener,
            ));
        };
    }
    idle.await;
}
