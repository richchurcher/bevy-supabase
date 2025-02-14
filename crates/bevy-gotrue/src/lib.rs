/// Heavily inspired by (and partially fully stolen) from https://crates.io/crates/go_true
mod builder;
mod client;

pub use builder::Builder;
pub use client::Client;

use std::io::{ErrorKind, Read, Write};
use std::net::TcpListener;
use std::ops::Deref;

use bevy::prelude::*;
use bevy::tasks::futures_lite::future;
use bevy::tasks::{block_on, AsyncComputeTaskPool, Task};
use bevy::utils::HashMap;
use bevy_http_client::prelude::{
    HttpTypedRequestTrait, TypedRequest, TypedResponse, TypedResponseError,
};
use bevy_http_client::HttpClient;
use ehttp::Headers;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Resource, Deserialize, Clone)]
pub struct Session {
    pub access_token: String,
    pub token_type: String,
    pub expires_in: i32,
    pub refresh_token: String,
    pub user: User,
}

#[derive(Debug, Resource, Deserialize, Default, Clone)]
pub struct User {
    pub id: String,
    pub email: String,
    pub aud: String,
    pub role: String,
    pub email_confirmed_at: Option<String>,
    pub phone: String,
    pub last_sign_in_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Resource)]
pub struct UserAttributes {
    pub email: String,
    pub password: String,
    pub data: Value,
}

pub struct UserList {
    pub users: Vec<User>,
}

pub struct UserUpdate {
    pub id: String,
    pub email: String,
    pub new_email: String,
    pub email_change_sent_at: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Resource, Clone)]
pub struct AuthCreds {
    pub id: String,
    pub password: String,
}

pub struct AuthPlugin {
    pub endpoint: String,
}

impl AuthPlugin {
    pub fn new(endpoint: String) -> Self {
        Self { endpoint }
    }
}

#[derive(Resource)]
struct AuthEndpoint(String);

impl Plugin for AuthPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AuthEndpoint(self.endpoint.clone()))
            .add_systems(PreStartup, (setup, start_provider_server))
            .add_systems(
                Update,
                (
                    sign_in_recv,
                    sign_in_err, // TODO runconditions
                    poll_listener.run_if(resource_exists::<ProviderListener>),
                ),
            )
            .register_request_type::<Session>();
    }
}

// logged_in runcondition
pub fn just_logged_in(session: Option<Res<Session>>) -> bool {
    if let Some(session) = session {
        session.is_added()
    } else {
        false
    }
}

pub fn is_logged_in(session: Option<Res<Session>>) -> bool {
    session.is_some()
}

fn setup(world: &mut World) {
    // TODO look for HttpClientPlugin, if not found panic and die.
    let endpoint = world.get_resource::<AuthEndpoint>().unwrap().0.clone();
    world.remove_resource::<AuthEndpoint>();
    let headers = Headers::new(&vec![]);
    let sign_in = world.register_system(sign_in);

    world.insert_resource(Client {
        endpoint,
        headers,
        sign_in,
        access_token: None,
    });
}

#[derive(Resource)]
struct ProviderListener(Task<Result<Session, std::io::Error>>);

pub fn start_provider_server(mut commands: Commands) {
    let pool = AsyncComputeTaskPool::get();
    let task = pool.spawn(async {
        let listener = TcpListener::bind("127.0.0.1:6969").expect("Couldn't bind port 6969.");

        let mut params = HashMap::new();

        loop {
            let (mut stream, _) = listener.accept().expect("Listener IO error");

            // This javascript is mental, I have to make fetch happen because GoTrue puts the
            // access token in the URI hash? Like is that intentional, surely should be on search
            // params. This fix does require JS in browser but most oAuth sign in pages probably do too, so
            // should be a non-issue.
            let message = String::from(
                "<script>fetch(`http://localhost:6969/token?${window.location.hash.replace('#','')})`)</script><h1>GoTrue-Rs</h1><h2>Signin sent to program.</h2><h3>You may close this tab.</h3>",
            );

            // TODO optional redirect to user provided URI

            let res = format!(
                "HTTP/1.1 200 OK\r\ncontent-length: {}\r\n\r\n{}",
                message.len(),
                message
            );

            loop {
                match stream.write(res.as_bytes()) {
                    Ok(_n) => break,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => println!("Couldn't respond. {}", e),
                }
            }

            let mut buf = [0; 4096];

            loop {
                match stream.read(&mut buf) {
                    Ok(0) => break,
                    Ok(_n) => break,
                    Err(ref e) if e.kind() == ErrorKind::WouldBlock => continue,
                    Err(e) => {
                        return Err(e);
                    }
                }
            }

            let raw = String::from_utf8(buf.to_vec()).unwrap();

            let request_line = raw.lines().collect::<Vec<_>>()[0];

            if !request_line.starts_with("GET /token?") {
                // If this request isn't the one we sent with JS fetch, ignore it and wait for the
                // right one.
                continue;
            }

            let split_req = request_line
                .strip_prefix("GET /token?")
                .unwrap()
                .split('&')
                .collect::<Vec<&str>>();

            for param in split_req {
                let split_param = param.split('=').collect::<Vec<&str>>();
                params.insert(split_param[0].to_owned(), split_param[1].to_owned());
            }

            if params.get("access_token").is_some() {
                break;
            }
        }

        let access_token = params.get("access_token").unwrap().clone();
        let refresh_token = params.get("refresh_token").unwrap().clone();
        let token_type = "JWT".to_string();
        let expires_in:i32 = params.get("expires_in").unwrap_or(&"3600".to_string()).clone().parse().unwrap();

        let session = Session {
            access_token,
            refresh_token,
            token_type,
            expires_in,
            user: User::default(),
        };

        Ok(session)
    });

    commands.insert_resource(ProviderListener(task));
}

fn poll_listener(mut commands: Commands, mut task: ResMut<ProviderListener>) {
    if let Some(Ok(result)) = block_on(future::poll_once(&mut task.0)) {
        commands.insert_resource(result);
        commands.remove_resource::<ProviderListener>();
    }
}

// Oneshot
pub fn sign_in(
    In(creds): In<AuthCreds>,
    auth: Res<Client>,
    mut evw: EventWriter<TypedRequest<Session>>,
) {
    let req = auth
        .builder()
        .sign_in(builder::EmailOrPhone::Email(creds.id), creds.password);

    let req = HttpClient::new().request(req).with_type::<Session>();
    evw.send(req);
}

fn sign_in_recv(
    mut evr: EventReader<TypedResponse<Session>>,
    mut client: ResMut<Client>,
    mut commands: Commands,
) {
    for res in evr.read() {
        let session = res.deref();
        client.access_token = Some(session.access_token.clone());
        commands.insert_resource(session.clone());
    }
}

fn sign_in_err(mut evr: EventReader<TypedResponseError<Session>>) {
    for err in evr.read() {
        println!("Login error: {:?}", err);
    }
}
