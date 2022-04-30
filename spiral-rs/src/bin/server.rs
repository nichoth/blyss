use futures::StreamExt;
use spiral_rs::aligned_memory::*;
use spiral_rs::client::*;
use spiral_rs::params::*;
use spiral_rs::server::*;
use spiral_rs::util::*;
use std::collections::HashMap;
use std::env;
use std::fs::File;
use std::sync::Mutex;

use actix_cors::Cors;
use actix_files as fs;
use actix_http::HttpServiceBuilder;
use actix_server::{Server, ServerBuilder};
use actix_service::map_config;
use actix_service::Service;
use actix_web::error::PayloadError;
use actix_web::{get, http, middleware, post, web, App, HttpServer};
use openssl::ssl::{SslAcceptor, SslFiletype, SslMethod};

const CERT_FNAME: &str = "/etc/letsencrypt/live/spiralwiki.com/fullchain.pem";
const KEY_FNAME: &str = "/etc/letsencrypt/live/spiralwiki.com/privkey.pem";

const NO_CACHE_PATHS: [&str; 2] = ["/setup", "/query"];

struct ServerState<'a> {
    params: &'a Params,
    db: AlignedMemory64,
    pub_params_map: Mutex<HashMap<String, PublicParameters<'a>>>,
}

async fn get_request_bytes(
    mut body: web::Payload,
    sz_bytes: usize,
) -> Result<Vec<u8>, http::Error> {
    let mut bytes = web::BytesMut::new();
    while let Some(item) = body.next().await {
        let item_ref = &item?;
        bytes.extend_from_slice(item_ref);
        if bytes.len() > sz_bytes {
            println!("too big! {}", sz_bytes);
            return Err(PayloadError::Overflow.into());
        }
    }
    Ok(bytes.to_vec())
}

fn get_other_io_err() -> PayloadError {
    PayloadError::Io(std::io::Error::from(std::io::ErrorKind::Other))
}

fn other_io_err<T>(_: T) -> PayloadError {
    get_other_io_err()
}

fn get_not_found_err() -> PayloadError {
    PayloadError::Io(std::io::Error::from(std::io::ErrorKind::NotFound))
}

#[get("/")]
async fn index<'a>(data: web::Data<ServerState<'a>>) -> String {
    format!("Hello {} {}!", data.params.poly_len, data.db.as_slice()[5])
}

#[post("/setup")]
async fn setup<'a>(
    body: web::Bytes,
    data: web::Data<ServerState<'a>>,
) -> Result<String, http::Error> {
    println!("/setup");

    // Parse the request
    let pub_params = PublicParameters::deserialize(data.params, &body);

    // Generate a UUID and store it
    let uuid = uuid::Uuid::new_v4();
    let mut pub_params_map = data.pub_params_map.lock().map_err(other_io_err)?;
    pub_params_map.insert(uuid.to_string(), pub_params);

    Ok(format!("{{\"id\":\"{}\"}}", uuid.to_string()))
}

const UUID_V4_STR_BYTES: usize = 36;

#[post("/query")]
async fn query<'a>(
    body: web::Payload,
    data: web::Data<ServerState<'a>>,
) -> Result<Vec<u8>, http::Error> {
    println!("/query");

    // Parse the UUID
    let request_bytes =
        get_request_bytes(body, UUID_V4_STR_BYTES + data.params.query_bytes()).await?;
    let uuid_bytes = &request_bytes.as_slice()[..UUID_V4_STR_BYTES];
    let data_bytes = &request_bytes.as_slice()[UUID_V4_STR_BYTES..];
    let uuid =
        uuid::Uuid::try_parse_ascii(uuid_bytes).map_err(|_| PayloadError::EncodingCorrupted)?;

    // Look up UUID and get public parameters
    let pub_params_map = data.pub_params_map.lock().map_err(other_io_err)?;
    let pub_params = pub_params_map
        .get(&uuid.to_string())
        .ok_or(get_not_found_err())?;

    // Parse the query
    let query = Query::deserialize(data.params, data_bytes);

    // Process the query
    let result = process_query(data.params, pub_params, &query, data.db.as_slice());

    Ok(result)
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let args: Vec<String> = env::args().collect();
    let db_preprocessed_path = &args[1];

    let cfg_expand = r#"
        {'n': 2,
        'nu_1': 10,
        'nu_2': 6,
        'p': 512,
        'q2_bits': 21,
        's_e': 85.83255142749422,
        't_gsw': 10,
        't_conv': 4,
        't_exp_left': 16,
        't_exp_right': 56,
        'instances': 11,
        'db_item_size': 100000 }
    "#;
    let box_params = Box::new(params_from_json(&cfg_expand.replace("'", "\"")));
    let params: &'static Params = Box::leak(box_params);

    let mut file = File::open(db_preprocessed_path).unwrap();
    let db = load_preprocessed_db_from_file(params, &mut file);

    let server_state = ServerState {
        params: params,
        db: db,
        pub_params_map: Mutex::new(HashMap::new()),
    };
    let state = web::Data::new(server_state);

    let app_builder = move || {
        let cors = Cors::default()
            .allow_any_origin()
            .allowed_headers([
                http::header::ORIGIN,
                http::header::CONTENT_TYPE,
                http::header::ACCEPT,
            ])
            .allow_any_method()
            .max_age(3600);

        App::new()
            .wrap(middleware::Compress::default())
            .wrap(cors)
            .app_data(state.clone())
            .app_data(web::PayloadConfig::new(1 << 25))
            .service(setup)
            .service(query)
            .service(fs::Files::new("/", "../client/static").index_file("index.html"))
            .wrap_fn(|req, srv | {
                let should_cache = !NO_CACHE_PATHS.contains(&req.path());
                let fut = srv.call(req);
                async move {
                    let mut res = fut.await?;
                    if should_cache {
                        res.headers_mut()
                            .insert(http::header::CACHE_CONTROL, http::header::HeaderValue::from_static("private, max-age=31536000"));
                    }
                    Ok(res)
                }
            })
    };

    Server::build()
        .bind("http/1", "0.0.0.0:443", move || {
            let mut builder = SslAcceptor::mozilla_intermediate(SslMethod::tls()).unwrap();
            builder
                .set_private_key_file(KEY_FNAME, SslFiletype::PEM)
                .unwrap();
            builder.set_certificate_chain_file(CERT_FNAME).unwrap();
            builder.set_alpn_protos(b"\x08http/1.1").unwrap();

            HttpServiceBuilder::default()
                .h1(map_config(app_builder(), |_| {
                    actix_web::dev::AppConfig::default()
                }))
                .openssl(builder.build())
        })?
        .run()
        .await
}
