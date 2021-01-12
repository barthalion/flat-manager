#![allow(proc_macro_derive_resolution_fallback)]

extern crate actix;
extern crate actix_net;
extern crate actix_service;
extern crate actix_web;
extern crate actix_web_actors;
extern crate actix_multipart;
extern crate actix_files;
extern crate askama;
extern crate base64;
extern crate byteorder;
extern crate bytes;
extern crate chrono;
#[macro_use] extern crate diesel;
#[macro_use] extern crate diesel_migrations;
extern crate env_logger;
#[macro_use] extern crate failure;
extern crate futures;
extern crate r2d2;
extern crate serde;
#[macro_use] extern crate serde_json;
#[macro_use] extern crate serde_derive;
extern crate tempfile;
extern crate jsonwebtoken as jwt;
#[macro_use] extern crate log;
extern crate libc;
extern crate walkdir;
extern crate hex;
extern crate filetime;
extern crate num_cpus;
extern crate time;
extern crate tokio;
extern crate rand;

mod api;
mod app;
mod db;
pub mod errors;
mod models;
mod schema;
mod tokens;
mod jobs;
pub mod ostree;
mod deltas;
mod delayed;
mod logger;

use actix::prelude::*;
use actix_web::dev::Server;
use diesel::prelude::*;
use diesel::r2d2::{ConnectionManager, ManageConnection};
use std::path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::signal::unix::Signal;
use app::Config;
use deltas::{DeltaGenerator,StopDeltaGenerator};
use jobs::{JobQueue, StopJobQueue};

pub use deltas::{RemoteClientMessage,RemoteServerMessage};
pub use errors::{DeltaGenerationError};

type Pool = diesel::r2d2::Pool<ConnectionManager<PgConnection>>;

pub fn load_config(path: &path::Path) -> Arc<Config> {
    let config_data = app::load_config(&path).expect(&format!("Failed to read config file {:?}", &path));
    Arc::new(config_data)
}

embed_migrations!();

fn connect_to_db(config: &Arc<Config>) -> r2d2::Pool<ConnectionManager<PgConnection>> {
    let manager = ConnectionManager::<PgConnection>::new(config.database_url.clone());

    {
        let conn = manager.connect().unwrap();
        embedded_migrations::run_with_output(&conn, &mut std::io::stdout()).unwrap();
    }

    r2d2::Pool::builder()
        .build(manager)
        .expect("Failed to create pool.")
}

fn start_delta_generator(config: &Arc<Config>) -> Addr<DeltaGenerator> {
    deltas::start_delta_generator(config.clone())
}

fn start_job_queue(config: &Arc<Config>,
                   pool: &Pool,
                   delta_generator: &Addr<DeltaGenerator>) -> Addr<JobQueue> {
    jobs::cleanup_started_jobs(&pool).expect("Failed to cleanup started jobs");
    jobs::start_job_executor(config.clone(), delta_generator.clone(), pool.clone())
}

fn handle_signal(sig: i32, server: &Server, job_queue: Addr<JobQueue>, delta_generator: Addr<DeltaGenerator>) -> impl Future<Item = (), Error = std::io::Error> {
    let graceful = match sig {
        tokio::signal::unix::SignalKind::interrupt() => {
            info!("SIGINT received, exiting");
            false
        }
        tokio::signal::unix::SignalKind::terminate() => {
            info!("SIGTERM received, exiting");
            true
        }
        tokio::signal::unix::SignalKind::quit() => {
            info!("SIGQUIT received, exiting");
            false
        }
        _ => false,
    };

    info!("Stopping http server");
    server
        .stop(graceful)
        .then(move |_result| {
            info!("Stopping delta generator");
            delta_generator
                .send(StopDeltaGenerator())
        })
        .then(move |_result| {
            info!("Stopping job processing");
            job_queue
                .send(StopJobQueue())
        })
        .then( |_| {
            info!("Exiting...");
            actix::clock::Delay::new(Instant::now() + Duration::from_millis(300))
        })
        .then( |_| {
            System::current().stop();
            Ok(())
        })
}

fn handle_signals(server: Server,
                  job_queue: Addr<JobQueue>,
                  delta_generator: Addr<DeltaGenerator>) {
    let sigint = Signal::new(tokio::signal::unix::SignalKind::interrupt()).flatten_stream();
    let sigterm = Signal::new(tokio::signal::unix::SignalKind::terminate()).flatten_stream();
    let sigquit = Signal::new(tokio::signal::unix::SignalKind::quit()).flatten_stream();
    let handle_signals = sigint.select(sigterm).select(sigquit)
        .for_each(move |sig| {
            handle_signal(sig, &server, job_queue.clone(), delta_generator.clone())
        })
        .map_err(|_| ());

    actix::spawn(handle_signals);
}

pub fn start(config: &Arc<Config>) -> Server {
    let pool = connect_to_db(config);

    let delta_generator = start_delta_generator(config);

    let job_queue = start_job_queue(config, &pool, &delta_generator);

    let app = app::create_app(pool, config, job_queue.clone(), delta_generator.clone());

    handle_signals(app.clone(), job_queue, delta_generator);

    app
}
