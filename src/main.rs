
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    response::{Html, IntoResponse},
    routing::get,
    Router,
};
use futures::{sink::SinkExt, stream::StreamExt};
use std::{
    collections::HashSet,
    collections::HashMap,
    sync::Arc,
};
use tokio::sync::{broadcast, Mutex};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use serde::Deserialize;


#[derive(Deserialize, Clone, Default)]
struct LoginMessage {
    username: String,
    group: String,
}

#[derive(Deserialize)]
struct TextMessage {
    text: String,
}


// Our shared state
struct AppState {
    // We require unique usernames. This tracks which usernames have been taken.
    user_set: Mutex<HashMap<String, HashSet<String>>>,
    group_channels: Mutex<HashMap<String, broadcast::Sender<String>>>,
    // Channel used to send messages to all connected clients.
}

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "example_chat=trace".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Set up application state for use with with_state().
    let user_set = Mutex::new(HashMap::new());
    let group_channels = Mutex::new(HashMap::new());

    let app_state = Arc::new(AppState { user_set, group_channels });

    let app = Router::new()
        .route("/", get(index))
        .route("/websocket", get(websocket_handler))
        .with_state(app_state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000")
        .await
        .unwrap();
    tracing::debug!("listening on {}", listener.local_addr().unwrap());
    axum::serve(listener, app).await.unwrap();
}

async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(state): State<Arc<AppState>>,
) -> impl IntoResponse {
    ws.on_upgrade(|socket| websocket(socket, state))
}

// This function deals with a single websocket connection, i.e., a single
// connected client / user, for which we will spawn two independent tasks (for
// receiving / sending chat messages).
async fn websocket(stream: WebSocket, state: Arc<AppState>) {
    // By splitting, we can send and receive at the same time.
    let (mut sender, mut receiver) = stream.split();

    // Username gets set in the receive loop, if it's valid.
    let mut login_message: LoginMessage = Default::default();
    // Loop until a text message is found.
    while let Some(Ok(message)) = receiver.next().await {
        if let Message::Text(text) = message {
            let text_str = text.as_str();
            login_message = serde_json::from_str::<LoginMessage>(&text_str).unwrap();
            if check_username(&state, &login_message).await.is_ok() {
                break;
            } else {
                let _ = sender
                    .send(Message::Text(String::from("Username already taken.")))
                    .await;
                return;
            }
        }
    }


    let mut group_channels = state.group_channels.lock().await;
    let tx = group_channels.entry(login_message.group.clone()).or_insert_with(|| {
        let (tx, _rx) = broadcast::channel(100);
        tx
    }).clone();
    std::mem::drop(group_channels);

    let mut rx = tx.subscribe();

    let msg = format!("{} joined group {}.", login_message.username, login_message.group);
    tracing::debug!("{msg}");
    let _ = tx.send(msg);

    let tx_clone = tx.clone(); // Clone tx for use in recv_task

    // Spawn the first task that will receive broadcast messages and send text
    // messages over the websocket to our client.
    let mut send_task = tokio::spawn(async move {
        while let Ok(msg) = rx.recv().await {
            // In any websocket error, break loop.
            if sender.send(Message::Text(msg)).await.is_err() {
                break;
            }
        }
    });

    // Clone things we want to pass (move) to the receiving task.
    let username = login_message.username.clone();
    let group_clone = login_message.group.clone();

    // Spawn a task that takes messages from the websocket, prepends the user
    // name, and sends them to all broadcast subscribers.
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(message)) = receiver.next().await {
            if let Message::Text(text) = message {
                if let Ok(text_message) = serde_json::from_str::<TextMessage>(&text) {
                    let _ = tx_clone.send(format!("{} (group {}): {}", &login_message.username, &login_message.group, text_message.text));
                }
            }
        }
    });

    // If any one of the tasks run to completion, we abort the other.
    tokio::select! {
        _ = &mut send_task => recv_task.abort(),
        _ = &mut recv_task => send_task.abort(),
    };

    // Send "user left" message (similar to "joined" above).
    let msg = format!("{} left group {}.", &username, &group_clone);
    tracing::debug!("{msg}");
    let _ = tx.send(msg);

    // Remove username from map so new clients can take it again.
    state.user_set.lock().await.get_mut(&group_clone).unwrap().remove(&username);
}

async fn check_username(state: &AppState, login_message: &LoginMessage) -> Result<(), ()> {
    let mut user_set = state.user_set.lock().await;

    // Check if the group exists in the user_set
    if let Some(users) = user_set.get_mut(&login_message.group) {
        // Check if the username is already taken in the group
        if !users.contains(&login_message.username) {
            users.insert(login_message.username.to_owned());
            Ok(())
        } else {
            Err(())
        }
    } else {
        // If the group does not exist, create a new entry for the group
        let mut users = HashSet::new();
        users.insert(login_message.username.to_owned());
        user_set.insert(login_message.group.to_owned(), users);
        Ok(())
    }
}

// Include utf-8 file at **compile** time.
async fn index() -> Html<&'static str> {
    Html(std::include_str!("../chat.html"))
}
