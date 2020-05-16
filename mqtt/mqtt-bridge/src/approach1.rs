pub enum Rule {
    Inward(Forward),
    Outward(Forward),
}

pub struct Forward {
    remote_prefix: String,
    local_prefix: String,
    pattern: String,
}

pub struct Connection {}

impl Connection {
    pub fn new<I>(rules: I) -> Self
    where
        I: IntoIterator<Item = Rule>,
    {
        Self {}
    }

    pub async fn run(self) {
        let password = None;
        let remote_addr = "localhost:1883";

        let mut client = mqtt3::Client::new(
            Some("mqtt-client-1".into()),
            Some("mqtt-client-1".into()),
            None,
            move || {
                let password = password.clone();
                Box::pin(async move {
                    let io = tokio::net::TcpStream::connect(&remote_addr).await;
                    io.map(|io| (io, password))
                })
            },
            std::time::Duration::from_secs(60),
            std::time::Duration::from_secs(60),
        );

        let mut shutdown_handle = client
            .shutdown_handle()
            .expect("couldn't get shutdown handle");

        let mut update_subscription_handle = client
            .update_subscription_handle()
            .expect("couldn't get subscription update handle");
        tokio::spawn(async move {
            let result = update_subscription_handle
                .subscribe(mqtt3::proto::SubscribeTo {
                    topic_filter: "topic".into(),
                    qos: mqtt3::proto::QoS::AtMostOnce,
                })
                .await;
            if let Err(err) = result {
                panic!("couldn't update subscription: {}", err);
            }
        });

        use futures_util::StreamExt;

        while let Some(event) = client.next().await {
            let event = event.unwrap();

            if let mqtt3::Event::Publication(publication) = event {
                match std::str::from_utf8(&publication.payload) {
                    Ok(s) => println!(
                        "Received publication: {:?} {:?} {:?}",
                        publication.topic_name, s, publication.qos,
                    ),
                    Err(_) => println!(
                        "Received publication: {:?} {:?} {:?}",
                        publication.topic_name, publication.payload, publication.qos,
                    ),
                }
            }
        }
    }
}

struct Bridge {

}

