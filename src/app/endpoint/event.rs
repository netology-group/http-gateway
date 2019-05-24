use failure::{format_err, Error};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use svc_agent::{AccountId, Authenticable};

use crate::util::http_stream::OutgoingMessage;

////////////////////////////////////////////////////////////////////////////////

pub(crate) type ConfigMap = HashMap<String, Config>;

#[derive(Debug, Deserialize)]
pub(crate) struct Config {
    callback: String,
    sources: Vec<AccountId>,
}

impl Config {
    pub(crate) fn callback(&self) -> &str {
        &self.callback
    }

    pub(crate) fn sources(&self) -> &Vec<AccountId> {
        &self.sources
    }
}

////////////////////////////////////////////////////////////////////////////////

type IncomingEvent = svc_agent::mqtt::IncomingEvent<JsonValue>;

////////////////////////////////////////////////////////////////////////////////

pub(crate) struct State {
    config: ConfigMap,
}

impl State {
    pub(crate) fn new(config: ConfigMap) -> Self {
        Self { config }
    }

    pub(crate) fn handle(
        &self,
        topic: &str,
        inev: &IncomingEvent,
    ) -> Result<OutgoingMessage, Error> {
        let from_account_id = inev.properties().as_account_id();
        let audience = extract_audience(topic)?;
        let config = self.config.get(audience).ok_or_else(|| {
            format_err!(
                "sending events for audience = '{}' is not allowed",
                audience
            )
        })?;
        if !config.sources().contains(from_account_id) {
            return Err(format_err!(
                "sending events for audience = '{}' from application = '{}' is not allowed",
                audience,
                from_account_id
            ));
        }

        let outev = OutgoingMessage::new(inev.payload().clone(), config.callback());
        Ok(outev)
    }
}

//////////////////////////////////////////////////////////////////////////////////

fn extract_audience(topic: &str) -> Result<&str, Error> {
    use std::ffi::OsStr;
    use std::path::{Component, Path};

    let topic_path = Path::new(topic);
    let mut topic = topic_path.components();

    let events_literal = Some(Component::Normal(OsStr::new("events")));
    if topic.next_back() == events_literal {

    } else {
        return Err(format_err!(
            "topic does not match the pattern 'audiences/AUDIENCE/events': {}",
            topic_path.display()
        ));
    }

    let maybe_audience = topic.next_back();

    let audiences_literal = Some(Component::Normal(OsStr::new("audiences")));
    if topic.next_back() == audiences_literal {
        match maybe_audience {
            Some(Component::Normal(audience)) => {
                let audience = audience.to_str().ok_or_else(|| {
                    format_err!(
                        "non utf-8 characters in audience name: {}",
                        topic_path.display()
                    )
                })?;
                Ok(audience)
            }
            _ => Err(format_err!(
                "topic does not match the pattern 'audiences/AUDIENCE/events': {}",
                topic_path.display()
            )),
        }
    } else {
        Err(format_err!(
            "topic does not match the pattern 'audiences/AUDIENCE/events': {}",
            topic_path.display()
        ))
    }
}

//////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test {
    #[test]
    fn extracts_audience() {
        let topic = "test/test/test/audiences/test-audience/events";
        let res = super::extract_audience(topic);

        assert!(res.is_ok());
        assert_eq!("test-audience", res.unwrap());

        let topic = "test/test/test/audiences";
        let res = super::extract_audience(topic);
        assert!(res.is_err());
        dbg!(res);

        let topic = "test/test/test/audiences/events";
        let res = super::extract_audience(topic);
        assert!(res.is_err());
        dbg!(res);
    }
}