mod axum_app;

use futures::future::join_all;

use tokio;
use teloxide::{prelude::*, update_listeners::webhooks};
use serde_json;
use reqwest::Url;


static BOT_TOKENS: &str = "[\"YOUR_API_KEY_HERE\"]";
static PORT: u16 = 8085;
static TELEGRAM_API_URL: &str = "http://127.0.0.1:8081";


fn make_bot(token: &String) -> Bot{
    Bot::with_client(token, teloxide::net::client_from_env())
        .set_api_url(Url::parse(&TELEGRAM_API_URL).expect("TELEGRAM_API_URL error"))
}


#[tokio::main]
async fn main() {
    let bot_tokens: Vec<String> = serde_json::from_str(BOT_TOKENS).expect("BOT_TOKENS error");
    let addr = ([0, 0, 0, 0], PORT).into();
    let url = format!("http://127.0.0.1:{PORT}/webhook/").parse().unwrap();

    println!("Starting... {:?}", bot_tokens);

    let bots: Vec<Bot> = bot_tokens.iter().map(make_bot).collect();
    let usernames: Vec<String> = join_all(bots.iter().map(|bot| async {
        let username = bot.get_me().await.expect("Bot is not working").username.clone().unwrap();
        format!("@{username}")
    })).await;
    println!("Started {:?}", usernames);
    let mut options = webhooks::Options::new(addr, url);
    let (stop_token, senders) = axum_app::make_axum_app(&mut options);

    join_all(bot_tokens.iter().zip(bots).map(|(bot_token, bot)| async {
        let listener = axum_app::start_bot(bot.clone(), bot_token, &stop_token, &senders, &options)
        .await
        .expect("Error while starting bot");
        teloxide::repl_with_listener(
            bot,
            |bot: Bot, msg: Message| async move {
                bot.send_message(msg.chat.id, "pong").await?;
                Ok(())
            },
            listener,
        ).await
    })).await;
    println!("Exiting");
}
