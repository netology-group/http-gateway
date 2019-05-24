use failure::{format_err, Error};
use futures::{sync::mpsc, Future, Stream};
use futures_locks::Mutex;
use http::{header, Method, StatusCode};
use log::{error, info};
use serde_derive::Deserialize;
use serde_json::Value as JsonValue;
use std::net::SocketAddr;
use std::time::Duration;
use std::{sync::Arc, thread};
use svc_agent::mqtt::{
    compat, AgentBuilder, ConnectionMode, Notification, OutgoingRequest, OutgoingRequestProperties,
    QoS, SubscriptionTopic,
};
use svc_agent::{
    AccountId, AgentId, Authenticable, ResponseSubscription, SharedGroup, Source, Subscription,
};
use tokio::net::TcpListener;
use tokio::prelude::FutureExt;
use tower_web::{
    impl_web, middleware::cors::CorsBuilder, middleware::log::LogMiddleware, Extract,
    ServiceBuilder,
};
use uuid::Uuid;

use crate::util::http_stream::OutgoingStream;
use crate::util::mqtt_request::Adapter;

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Extract, Deserialize)]
struct RequestPayload {
    me: AgentId,
    destination: AccountId,
    payload: JsonValue,
    method: String,
}

//////////////////////////////////////////////////////////////////////////////////
//
struct Request {
    tx: Mutex<Adapter>,
    timeout: Duration,
}

impl Request {
    fn new(tx: Mutex<Adapter>, timeout: Duration) -> Self {
        Self { tx, timeout }
    }
}

impl_web! {
    impl Request {
        #[post("/api/v1/request")]
        fn request(&self, body: RequestPayload, sub: AccountId) -> impl Future<Item = Result<JsonValue, tower_web::Error>, Error = ()> {
            let error = || tower_web::Error::builder().kind("request_error", "Error sending a request");
            let timeout = self.timeout;

            self.tx.lock()
                .map_err(move |_| {
                    let detail = "error acquiring a mutex for outgoing MQTT request";
                    error().status(StatusCode::UNPROCESSABLE_ENTITY).detail(&detail).build()
                })
                .and_then(move |mut tx| {
                    let payload_account_id = body.me.as_account_id();
                    if &sub != payload_account_id {
                        let detail = format!("account id = '{}' from the access token doesn't much one in payload = '{}' payload", &sub, payload_account_id);
                        return Err(error().status(StatusCode::FORBIDDEN).detail(&detail).build());
                    }

                    let response_topic = {
                        let src = Source::Unicast(Some(&body.destination));
                        let sub = ResponseSubscription::new(src);
                        sub.subscription_topic(&body.me).map_err(|err| error().status(StatusCode::UNPROCESSABLE_ENTITY).detail(&err.to_string()).build())?
                    };

                    let correlation_data = Uuid::new_v4().to_string();
                    let mut props = OutgoingRequestProperties::new(
                        &body.method,
                        &response_topic,
                        &correlation_data,
                    );
                    props.set_authn(body.me.into());
                    let req = OutgoingRequest::multicast(body.payload, props, &body.destination);

                    // Send request
                    tx.request(req).map_err(|err| error().status(StatusCode::UNPROCESSABLE_ENTITY).detail(&err.to_string()).build())
                })
                .and_then(move |req| {
                    req
                        .timeout(timeout)
                        .map_err(move |_| {
                            let detail = "timeout on an outgoing HTTP response";
                            error().status(StatusCode::GATEWAY_TIMEOUT).detail(&detail).build()
                        })
                })
                .then(|result| match result {
                    Ok(resp) => Ok(Ok(resp.payload().clone())),
                    Err(err) => Ok(Err(err))
                })
        }
    }
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Deserialize)]
pub(crate) struct HttpConfig {
    listener_address: SocketAddr,
    cors: Cors,
}

////////////////////////////////////////////////////////////////////////////////

#[derive(Debug, Deserialize)]
pub(crate) struct Cors {
    #[serde(deserialize_with = "crate::serde::allowed_origins")]
    #[serde(default)]
    allow_origins: tower_web::middleware::cors::AllowedOrigins,
    #[serde(deserialize_with = "crate::serde::duration")]
    #[serde(default)]
    max_age: Duration,
}

////////////////////////////////////////////////////////////////////////////////

struct State {
    event: endpoint::event::State,
}

////////////////////////////////////////////////////////////////////////////////

pub(crate) fn run() {
    // Config
    let config = config::load().expect("Failed to load config");
    info!("Config: {:?}", config);

    let agent_id = AgentId::new(&config.agent_label, config.id.clone());
    let group = SharedGroup::new("loadbalancer", agent_id.as_account_id().clone());
    info!("Agent Id: {}", agent_id);
    let (mut tx, rx) = AgentBuilder::new(agent_id)
        .mode(ConnectionMode::Bridge)
        .start(&config.mqtt)
        .expect("Failed to create an agent");

    // Event loop for incoming messages of MQTT Agent
    let (mq_tx, mq_rx) = mpsc::unbounded::<Notification>();
    thread::spawn(move || {
        for message in rx {
            if let Err(_) = mq_tx.unbounded_send(message) {
                error!("Error sending message to the internal channel");
            }
        }
    });

    // Create Subscriptions
    tx.subscribe(
        &Subscription::unicast_responses(),
        QoS::AtLeastOnce,
        Some(&group),
    )
    .expect("Error subscribing to app's responses topic");
    for (tenant_audience, tenant_events_config) in &config.events {
        for from_account_id in tenant_events_config.sources() {
            tx.subscribe(
                &Subscription::broadcast_events(
                    from_account_id,
                    &format!("audiences/{}/events", tenant_audience),
                ),
                QoS::AtLeastOnce,
                Some(&group),
            )
            .expect("Error subscribing to app's events topic");
        }
    }

    // Create MQTT Request Adapter
    let req_tx = Mutex::new(Adapter::new(tx));
    let resp_tx = req_tx.clone();

    // Application resources
    let state = Arc::new(State {
        event: endpoint::event::State::new(config.events),
    });

    let (hq_tx, hq_rx) = OutgoingStream::new(&config.http_client);
    let mq_rx = mq_rx.for_each(move |message| {
        let mut hq_tx = hq_tx.clone();
        let state = state.clone();
        resp_tx
            .lock()
            .and_then(move |mut resp_tx| {
                if let Notification::Publish(message) = message {
                    let topic: &str = &message.topic_name;

                    // Log incoming messages
                    info!(
                        "Incoming message = '{}' sent to the topic = '{}', dup = '{}', pkid = '{:?}'",
                        String::from_utf8_lossy(message.payload.as_slice()), topic, message.dup, message.pkid,
                    );

                    let result = handle_message(&mut resp_tx, &mut hq_tx, topic, message.payload.clone(), state.clone());
                    if let Err(err) = result {
                        error!(
                            "Error processing a message = '{text}' sent to the topic = '{topic}', {detail}",
                            text = String::from_utf8_lossy(message.payload.as_slice()),
                            topic = topic,
                            detail = err,
                        )
                    }
                }
                Ok(())
            })
    });

    // Resources
    let request = Request::new(req_tx, Duration::from_secs(config.http_client.timeout()));

    // Middleware
    let cors = CorsBuilder::new()
        .allow_origins(config.http.cors.allow_origins.clone())
        .allow_methods(vec![Method::POST])
        .allow_headers(vec![
            header::AUTHORIZATION,
            header::CONTENT_LENGTH,
            header::CONTENT_TYPE,
        ])
        .allow_credentials(true)
        .max_age(config.http.cors.max_age)
        .build();

    let tcp_stream =
        TcpListener::bind(&config.http.listener_address).expect("Invalid HTTP listener address");

    let server = ServiceBuilder::new()
        .config(config.authn.clone())
        .middleware(LogMiddleware::new("http_gateway::http"))
        .middleware(cors)
        .resource(request)
        .serve(tcp_stream.incoming());

    tokio::run(server.join(mq_rx).join(hq_rx).map(|_| ()));
}

//////////////////////////////////////////////////////////////////////////////////

fn handle_message(
    resp_tx: &mut Adapter,
    hq_tx: &mut OutgoingStream,
    topic: &str,
    payload: Arc<Vec<u8>>,
    state: Arc<State>,
) -> Result<(), Error> {
    let envelope = serde_json::from_slice::<compat::IncomingEnvelope>(payload.as_slice())?;
    match envelope.properties() {
        compat::IncomingEnvelopeProperties::Response(_) => {
            let inresp = compat::into_response(envelope)?;
            resp_tx.commit_response(inresp)
        }
        compat::IncomingEnvelopeProperties::Event(_) => {
            let inev = compat::into_event::<JsonValue>(envelope)?;
            let outev = state.event.handle(topic, &inev)?;
            hq_tx.send(outev)
        }
        _ => Err(format_err!(
            "unsupported message type, envelope = '{:?}'",
            envelope
        )),
    }
}

//////////////////////////////////////////////////////////////////////////////////

mod config;
mod endpoint;

//////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test {

    use super::RequestPayload;
    use serde_json::{self, json};

    #[test]
    fn ser() {
        let val = json!({
            "me": "web.12345.netology.ru",
            "destination": "conference.netology-group.services",
            "payload": "test",
            "method": "room.create",
        });

        let d: RequestPayload = serde_json::from_value(val).unwrap();

        dbg!(d);
    }
}

//////////////////////////////////////////////////////////////////////////////////