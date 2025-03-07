use crate::{
    domain::{
        error::DomainError,
        handler::{BackendHandler, LoginHandler},
        opaque_handler::OpaqueHandler,
    },
    infra::{
        auth_service,
        configuration::{Configuration, MailOptions},
        tcp_backend_handler::*,
    },
};
use actix_files::{Files, NamedFile};
use actix_http::HttpServiceBuilder;
use actix_server::ServerBuilder;
use actix_service::map_config;
use actix_web::{dev::AppConfig, web, App, HttpResponse};
use anyhow::{Context, Result};
use hmac::{Hmac, NewMac};
use sha2::Sha512;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::RwLock;

async fn index() -> actix_web::Result<NamedFile> {
    let mut path = PathBuf::new();
    path.push("app");
    path.push("index.html");
    Ok(NamedFile::open(path)?)
}

pub(crate) fn error_to_http_response(error: DomainError) -> HttpResponse {
    match error {
        DomainError::AuthenticationError(_) | DomainError::AuthenticationProtocolError(_) => {
            HttpResponse::Unauthorized()
        }
        DomainError::DatabaseError(_)
        | DomainError::InternalError(_)
        | DomainError::UnknownCryptoError(_) => HttpResponse::InternalServerError(),
        DomainError::Base64DecodeError(_) | DomainError::BinarySerializationError(_) => {
            HttpResponse::BadRequest()
        }
    }
    .body(error.to_string())
}

fn http_config<Backend>(
    cfg: &mut web::ServiceConfig,
    backend_handler: Backend,
    jwt_secret: secstr::SecUtf8,
    jwt_blacklist: HashSet<u64>,
    server_url: String,
    mail_options: MailOptions,
) where
    Backend: TcpBackendHandler + BackendHandler + LoginHandler + OpaqueHandler + Sync + 'static,
{
    cfg.app_data(web::Data::new(AppState::<Backend> {
        backend_handler,
        jwt_key: Hmac::new_varkey(jwt_secret.unsecure().as_bytes()).unwrap(),
        jwt_blacklist: RwLock::new(jwt_blacklist),
        server_url,
        mail_options,
    }))
    .service(web::scope("/auth").configure(auth_service::configure_server::<Backend>))
    // API endpoint.
    .service(
        web::scope("/api")
            .wrap(auth_service::CookieToHeaderTranslatorFactory)
            .configure(super::graphql::api::configure_endpoint::<Backend>),
    )
    // Serve the /pkg path with the compiled WASM app.
    .service(Files::new("/pkg", "./app/pkg"))
    // Serve static files
    .service(Files::new("/static", "./app/static"))
    // Serve static fonts
    .service(Files::new("/static/fonts", "./app/static/fonts"))
    // Default to serve index.html for unknown routes, to support routing.
    .service(
        web::scope("/")
            .route("", web::get().to(index)) // this is necessary because the below doesn't match a request for "/"
            .route(".*", web::get().to(index)),
    );
}

pub(crate) struct AppState<Backend> {
    pub backend_handler: Backend,
    pub jwt_key: Hmac<Sha512>,
    pub jwt_blacklist: RwLock<HashSet<u64>>,
    pub server_url: String,
    pub mail_options: MailOptions,
}

pub async fn build_tcp_server<Backend>(
    config: &Configuration,
    backend_handler: Backend,
    server_builder: ServerBuilder,
) -> Result<ServerBuilder>
where
    Backend: TcpBackendHandler + BackendHandler + LoginHandler + OpaqueHandler + Sync + 'static,
{
    let jwt_secret = config.jwt_secret.clone();
    let jwt_blacklist = backend_handler
        .get_jwt_blacklist()
        .await
        .context("while getting the jwt blacklist")?;
    let server_url = config.http_url.clone();
    let mail_options = config.smtp_options.clone();
    server_builder
        .bind("http", ("0.0.0.0", config.http_port), move || {
            let backend_handler = backend_handler.clone();
            let jwt_secret = jwt_secret.clone();
            let jwt_blacklist = jwt_blacklist.clone();
            let server_url = server_url.clone();
            let mail_options = mail_options.clone();
            HttpServiceBuilder::new()
                .finish(map_config(
                    App::new().configure(move |cfg| {
                        http_config(
                            cfg,
                            backend_handler,
                            jwt_secret,
                            jwt_blacklist,
                            server_url,
                            mail_options,
                        )
                    }),
                    |_| AppConfig::default(),
                ))
                .tcp()
        })
        .with_context(|| {
            format!(
                "While bringing up the TCP server with port {}",
                config.http_port
            )
        })
}
