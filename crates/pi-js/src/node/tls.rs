//! TLS for `node:tls` on rustls: client and server configs built from Node's options (`ca`,
//! `cert`, `key`, `rejectUnauthorized`, ALPN, SNI, versions) and the handshake info JS reads.

use std::sync::{Arc, Mutex};

use rquickjs::{Ctx, Value};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error as TlsError, RootCertStore, SignatureScheme};
use serde::Deserialize;
use serde_json::json;

use super::net::{BoxIo, parse_options};
use crate::error::JsError;
use crate::runtime::convert::throw;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Options {
    servername: Option<String>,
    ca: Option<Vec<String>>,
    cert: Option<String>,
    key: Option<String>,
    reject_unauthorized: Option<bool>,
    #[serde(default)]
    alpn: Vec<String>,
    min_version: Option<String>,
    max_version: Option<String>,
    #[serde(default)]
    request_cert: bool,
}

/// A client or server TLS setup for one connection or listener.
#[derive(Clone)]
pub(crate) enum Tls {
    Client { connector: tokio_rustls::TlsConnector, name: ServerName<'static>, verifier: Arc<Verifier> },
    Server(tokio_rustls::TlsAcceptor),
}

/// Checks the chain with webpki and records why it failed; host names are checked in JS.
#[derive(Debug)]
pub(crate) struct Verifier {
    inner: Arc<rustls::client::WebPkiServerVerifier>,
    reject: bool,
    failure: Mutex<Option<(String, String)>>,
}

fn failure_reason(error: &TlsError, end_entity: &[u8], intermediates: usize) -> (String, String) {
    let (code, message) = match error {
        TlsError::InvalidCertificate(CertificateError::Expired | CertificateError::ExpiredContext { .. }) => {
            ("CERT_HAS_EXPIRED", "certificate has expired")
        }
        TlsError::InvalidCertificate(CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. }) => {
            ("CERT_NOT_YET_VALID", "certificate is not yet valid")
        }
        TlsError::InvalidCertificate(CertificateError::Revoked) => ("CERT_REVOKED", "certificate revoked"),
        TlsError::InvalidCertificate(CertificateError::UnknownIssuer) if intermediates == 0 => {
            if super::x509::self_signed(end_entity) {
                ("DEPTH_ZERO_SELF_SIGNED_CERT", "self-signed certificate")
            } else {
                ("UNABLE_TO_VERIFY_LEAF_SIGNATURE", "unable to verify the first certificate")
            }
        }
        TlsError::InvalidCertificate(CertificateError::UnknownIssuer) => {
            ("UNABLE_TO_GET_ISSUER_CERT_LOCALLY", "unable to get local issuer certificate")
        }
        _ => ("UNABLE_TO_VERIFY_LEAF_SIGNATURE", "unable to verify the first certificate"),
    };
    (code.to_string(), message.to_string())
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        match self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp_response, now) {
            Ok(verified) => Ok(verified),
            Err(TlsError::InvalidCertificate(
                CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. },
            )) => Ok(ServerCertVerified::assertion()),
            Err(error) => {
                *self.failure.lock().unwrap_or_else(|poison| poison.into_inner()) =
                    Some(failure_reason(&error, end_entity, intermediates.len()));
                if self.reject { Err(error) } else { Ok(ServerCertVerified::assertion()) }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        self.inner.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.inner.supported_verify_schemes()
    }
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn versions(options: &Options) -> Vec<&'static rustls::SupportedProtocolVersion> {
    let min13 = options.min_version.as_deref() == Some("TLSv1.3");
    let max12 = matches!(options.max_version.as_deref(), Some("TLSv1.2" | "TLSv1.1" | "TLSv1"));
    let mut list = Vec::new();
    if !max12 {
        list.push(&rustls::version::TLS13);
    }
    if !min13 {
        list.push(&rustls::version::TLS12);
    }
    list
}

fn roots(ca: &Option<Vec<String>>) -> Result<RootCertStore, JsError> {
    let Some(list) = ca else {
        return Ok(RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() });
    };
    let mut store = RootCertStore::empty();
    for pem in list {
        for cert in CertificateDer::pem_slice_iter(pem.as_bytes()) {
            let cert = cert.map_err(|error| JsError::new(format!("Invalid ca certificate: {error}")))?;
            store.add(cert).map_err(|error| JsError::new(format!("Invalid ca certificate: {error}")))?;
        }
    }
    Ok(store)
}

fn identity(options: &Options) -> Result<Option<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)>, JsError> {
    let (Some(cert), Some(key)) = (&options.cert, &options.key) else { return Ok(None) };
    let certs: Result<Vec<_>, _> = CertificateDer::pem_slice_iter(cert.as_bytes()).collect();
    let certs = certs.map_err(|error| JsError::new(format!("Invalid certificate: {error}")))?;
    let key = PrivateKeyDer::from_pem_slice(key.as_bytes())
        .map_err(|_| JsError::new("error:1E08010C:DECODER routines::unsupported").with_code("ERR_OSSL_UNSUPPORTED"))?;
    Ok(Some((certs, key)))
}

fn config_error(error: TlsError) -> JsError {
    JsError::new(error.to_string()).with_code("ERR_SSL_CONFIG")
}

fn client(options: Options) -> Result<Tls, JsError> {
    let provider = provider();
    let inner =
        rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots(&options.ca)?), provider.clone())
            .build()
            .map_err(|error| JsError::new(error.to_string()))?;
    let reject = options.reject_unauthorized.unwrap_or(true);
    let verifier = Arc::new(Verifier { inner, reject, failure: Mutex::new(None) });
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&versions(&options))
        .map_err(config_error)?
        .dangerous()
        .with_custom_certificate_verifier(verifier.clone());
    let mut config = match identity(&options)? {
        Some((certs, key)) => builder.with_client_auth_cert(certs, key).map_err(config_error)?,
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = options.alpn.iter().map(|name| name.as_bytes().to_vec()).collect();
    let host = options.servername.unwrap_or_else(|| "localhost".into());
    let name = ServerName::try_from(host.trim_start_matches('[').trim_end_matches(']').to_string())
        .map_err(|_| JsError::type_error(format!("Invalid servername: {host}")))?;
    Ok(Tls::Client { connector: Arc::new(config).into(), name, verifier })
}

fn server(options: Options) -> Result<Tls, JsError> {
    let builder = rustls::ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&versions(&options))
        .map_err(config_error)?;
    let builder = if options.request_cert {
        let verifier =
            rustls::server::WebPkiClientVerifier::builder_with_provider(Arc::new(roots(&options.ca)?), provider());
        let verifier =
            if options.reject_unauthorized == Some(false) { verifier.allow_unauthenticated() } else { verifier };
        builder.with_client_cert_verifier(verifier.build().map_err(|error| JsError::new(error.to_string()))?)
    } else {
        builder.with_no_client_auth()
    };
    let Some((certs, key)) = identity(&options)? else {
        return Err(JsError::new("Missing PEM private key or certificate").with_code("ERR_TLS_NO_CERTIFICATE"));
    };
    let mut config = builder.with_single_cert(certs, key).map_err(config_error)?;
    config.alpn_protocols = options.alpn.iter().map(|name| name.as_bytes().to_vec()).collect();
    Ok(Tls::Server(Arc::new(config).into()))
}

fn handshake_error(error: &std::io::Error, failure: Option<(String, String)>) -> JsError {
    if let Some((code, message)) = failure {
        return JsError::new(message).with_code(code);
    }
    if let Some(TlsError::AlertReceived(alert)) = error.get_ref().and_then(|inner| inner.downcast_ref::<TlsError>()) {
        let name = format!("{alert:?}");
        let mut snake = String::new();
        for (index, character) in name.chars().enumerate() {
            if character.is_uppercase() && index > 0 {
                snake.push('_');
            }
            snake.push(character.to_ascii_uppercase());
        }
        let text = snake.to_lowercase().replace('_', " ");
        return JsError::new(format!("SSL routines::tlsv1 alert {text}"))
            .with_code(format!("ERR_SSL_TLSV1_ALERT_{snake}"));
    }
    if error.kind() == std::io::ErrorKind::UnexpectedEof {
        return JsError::new("Client network socket disconnected before secure TLS connection was established")
            .with_code("ECONNRESET");
    }
    JsError::new(error.to_string()).with_code("EPROTO")
}

fn common_info(connection: &rustls::CommonState) -> serde_json::Value {
    let protocol = connection.protocol_version().map(|version| match version {
        rustls::ProtocolVersion::TLSv1_3 => "TLSv1.3".to_string(),
        rustls::ProtocolVersion::TLSv1_2 => "TLSv1.2".to_string(),
        other => format!("{other:?}"),
    });
    let cipher = connection.negotiated_cipher_suite().map(|suite| {
        let name = format!("{:?}", suite.suite()).replace("TLS13_", "TLS_");
        json!({ "standardName": name, "version": protocol })
    });
    let alpn = connection.alpn_protocol().map(|name| String::from_utf8_lossy(name).into_owned());
    let chain: Vec<_> =
        connection.peer_certificates().unwrap_or_default().iter().map(|cert| super::x509::describe(cert)).collect();
    json!({ "protocol": protocol, "cipher": cipher, "alpnProtocol": alpn, "peerChain": chain })
}

impl Tls {
    /// Build from the options object JS passes, or `None` for `undefined`.
    pub(crate) fn from_js<'js>(ctx: &Ctx<'js>, value: Value<'js>, is_server: bool) -> rquickjs::Result<Option<Self>> {
        if value.is_undefined() || value.is_null() {
            return Ok(None);
        }
        let options: Options = parse_options(ctx, value)?;
        let built = if is_server { server(options) } else { client(options) };
        built.map(Some).map_err(|error| throw(ctx, &error))
    }

    pub(crate) async fn handshake(self, io: BoxIo) -> Result<(BoxIo, serde_json::Value), JsError> {
        match self {
            Tls::Client { connector, name, verifier } => {
                let servername = name.to_str().into_owned();
                let take = || verifier.failure.lock().unwrap_or_else(|poison| poison.into_inner()).take();
                let stream = connector.connect(name, io).await.map_err(|error| handshake_error(&error, take()))?;
                let failure = take();
                let mut info = common_info(stream.get_ref().1);
                info["servername"] = json!(servername);
                info["authorized"] = json!(failure.is_none());
                info["authorizationError"] = failure.map_or(json!(null), |(code, message)| json!([code, message]));
                Ok((Box::new(stream), info))
            }
            Tls::Server(acceptor) => {
                let stream = acceptor.accept(io).await.map_err(|error| handshake_error(&error, None))?;
                let connection = stream.get_ref().1;
                let mut info = common_info(connection);
                info["servername"] = json!(connection.server_name());
                info["authorized"] = json!(connection.peer_certificates().is_some_and(|chain| !chain.is_empty()));
                Ok((Box::new(stream), info))
            }
        }
    }
}

/// `tls.rootCertificates`: Mozilla's roots, the same set as webpki-roots, as PEM.
pub(crate) fn root_certificates() -> Vec<String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let wrap = |der: &[u8]| {
        let body = engine.encode(der);
        let lines: Vec<&str> =
            body.as_bytes().chunks(64).map(|chunk| std::str::from_utf8(chunk).unwrap_or("")).collect();
        format!("-----BEGIN CERTIFICATE-----\n{}\n-----END CERTIFICATE-----", lines.join("\n"))
    };
    webpki_root_certs::TLS_SERVER_ROOT_CERTS.iter().map(|cert| wrap(cert)).collect()
}

pub(crate) fn natives<'js>(ctx: &Ctx<'js>) -> rquickjs::Result<rquickjs::Object<'js>> {
    let object = rquickjs::Object::new(ctx.clone())?;
    super::set_fn(&object, "rootCertificates", root_certificates)?;
    Ok(object)
}
