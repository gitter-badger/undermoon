extern crate actix_web;
extern crate undermoon;
#[macro_use]
extern crate log;
extern crate config;
extern crate env_logger;
use actix_web::{middleware, App, HttpServer};
use std::env;
use std::sync::Arc;
use undermoon::broker::service::{configure_app, MemBrokerConfig, MemBrokerService};

fn gen_conf() -> MemBrokerConfig {
    let conf_file_path = env::args()
        .nth(1)
        .unwrap_or_else(|| "mem-broker.toml".to_string());

    let mut s = config::Config::new();
    s.merge(config::File::with_name(&conf_file_path))
        .map(|_| ())
        .unwrap_or_else(|e| warn!("failed to read config file: {:?}", e));
    // e.g. UNDERMOON_ADDRESS='127.0.0.1:5299'
    s.merge(config::Environment::with_prefix("undermoon"))
        .map(|_| ())
        .unwrap_or_else(|e| warn!("failed to read address from env vars {:?}", e));

    MemBrokerConfig {
        address: s
            .get::<String>("address")
            .unwrap_or_else(|_| "127.0.0.1:7799".to_string()),
        failure_ttl: s.get::<u64>("failure_ttl").unwrap_or_else(|_| 60),
        failure_quorum: s.get::<u64>("failure_quorum").unwrap_or_else(|_| 1),
    }
}

#[actix_rt::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();

    let config = gen_conf();
    let address = config.address.clone();

    let service = Arc::new(MemBrokerService::new(config));
    HttpServer::new(move || {
        App::new()
            .app_data(service.clone())
            .configure(configure_app)
            .wrap(middleware::Logger::default())
    })
    .bind(&address)?
    .keep_alive(300)
    .run()
    .await
}
