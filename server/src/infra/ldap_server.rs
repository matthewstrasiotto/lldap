use crate::{
    domain::{
        handler::{BackendHandler, LoginHandler, UserId},
        opaque_handler::OpaqueHandler,
    },
    infra::{configuration::Configuration, ldap_handler::LdapHandler},
};
use actix_rt::net::TcpStream;
use actix_server::ServerBuilder;
use actix_service::{fn_service, ServiceFactoryExt};
use anyhow::{Context, Result};
use ldap3_server::{proto::LdapMsg, LdapCodec};
use log::*;
use native_tls::{Identity, TlsAcceptor};
use tokio_native_tls::TlsAcceptor as NativeTlsAcceptor;
use tokio_util::codec::{FramedRead, FramedWrite};

async fn handle_incoming_message<Backend, Writer>(
    msg: Result<LdapMsg, std::io::Error>,
    resp: &mut Writer,
    session: &mut LdapHandler<Backend>,
) -> Result<bool>
where
    Backend: BackendHandler + LoginHandler + OpaqueHandler,
    Writer: futures_util::Sink<LdapMsg> + Unpin,
    <Writer as futures_util::Sink<LdapMsg>>::Error: std::error::Error + Send + Sync + 'static,
{
    use futures_util::SinkExt;
    let msg = msg.context("while receiving LDAP op")?;
    debug!("Received LDAP message: {:?}", &msg);
    match session.handle_ldap_message(msg.op).await {
        None => return Ok(false),
        Some(result) => {
            if result.is_empty() {
                debug!("No response");
            }
            for result_op in result.into_iter() {
                debug!("Replying with LDAP op: {:?}", &result_op);
                resp.send(LdapMsg {
                    msgid: msg.msgid,
                    op: result_op,
                    ctrl: vec![],
                })
                .await
                .context("while sending a response: {:#}")?
            }

            resp.flush()
                .await
                .context("while flushing responses: {:#}")?
        }
    }
    Ok(true)
}

fn get_file_as_byte_vec(filename: &str) -> Result<Vec<u8>> {
    (|| -> Result<Vec<u8>> {
        use std::fs::{metadata, File};
        use std::io::Read;
        let mut f = File::open(&filename).context("file not found")?;
        let metadata = metadata(&filename).context("unable to read metadata")?;
        let mut buffer = vec![0; metadata.len() as usize];
        f.read(&mut buffer).context("buffer overflow")?;
        Ok(buffer)
    })()
    .context(format!("while reading file {}", filename))
}

async fn handle_ldap_stream<Stream, Backend>(
    stream: Stream,
    backend_handler: Backend,
    ldap_base_dn: String,
    ldap_user_dn: UserId,
) -> Result<Stream>
where
    Backend: BackendHandler + LoginHandler + OpaqueHandler + 'static,
    Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite,
{
    use tokio_stream::StreamExt;
    let (r, w) = tokio::io::split(stream);
    // Configure the codec etc.
    let mut requests = FramedRead::new(r, LdapCodec);
    let mut resp = FramedWrite::new(w, LdapCodec);

    let mut session = LdapHandler::new(backend_handler, ldap_base_dn, ldap_user_dn);

    while let Some(msg) = requests.next().await {
        if !handle_incoming_message(msg, &mut resp, &mut session)
            .await
            .context("while handling incoming messages")?
        {
            break;
        }
    }
    Ok(requests.into_inner().unsplit(resp.into_inner()))
}

fn get_tls_acceptor(config: &Configuration) -> Result<NativeTlsAcceptor> {
    // Load TLS key and cert files
    let cert_file = get_file_as_byte_vec(&config.ldaps_options.cert_file)?;
    let key_file = get_file_as_byte_vec(&config.ldaps_options.key_file)?;
    let identity = Identity::from_pkcs8(&cert_file, &key_file)?;
    Ok(TlsAcceptor::new(identity)?.into())
}

pub fn build_ldap_server<Backend>(
    config: &Configuration,
    backend_handler: Backend,
    server_builder: ServerBuilder,
) -> Result<ServerBuilder>
where
    Backend: BackendHandler + LoginHandler + OpaqueHandler + 'static,
{
    let context = (
        backend_handler,
        config.ldap_base_dn.clone(),
        config.ldap_user_dn.clone(),
    );

    let tls_context = (
        context.clone(),
        get_tls_acceptor(config).context("while setting up the SSL certificate")?,
    );

    let binder = move || {
        let context = context.clone();
        fn_service(move |stream: TcpStream| {
            let context = context.clone();
            async move {
                let (handler, base_dn, user_dn) = context;
                handle_ldap_stream(stream, handler, base_dn, user_dn).await
            }
        })
        .map_err(|err: anyhow::Error| error!("[LDAP] Service Error: {:#}", err))
    };

    let tls_binder = move || {
        let tls_context = tls_context.clone();
        fn_service(move |stream: TcpStream| {
            let tls_context = tls_context.clone();
            async move {
                let ((handler, base_dn, user_dn), tls_acceptor) = tls_context;
                let tls_stream = tls_acceptor.clone().accept(stream).await?;
                handle_ldap_stream(tls_stream, handler, base_dn, user_dn).await
            }
        })
        .map_err(|err: anyhow::Error| error!("[LDAPS] Service Error: {:#}", err))
    };

    let server_builder = server_builder
        .bind("ldap", ("0.0.0.0", config.ldap_port), binder)
        .with_context(|| format!("while binding to the port {}", config.ldap_port));
    if config.ldaps_options.enabled {
        server_builder.and_then(|s| {
            s.bind("ldaps", ("0.0.0.0", config.ldaps_options.port), tls_binder)
                .with_context(|| format!("while binding to the port {}", config.ldaps_options.port))
        })
    } else {
        server_builder
    }
}
