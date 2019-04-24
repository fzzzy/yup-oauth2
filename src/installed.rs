// Copyright (c) 2016 Google Inc (lewinb@google.com).
//
// Refer to the project root for licensing information.
//
extern crate serde_json;
extern crate url;

use std::convert::AsRef;
use std::error::Error;
use std::io;
use std::sync::{Arc, Mutex};

use futures;
use futures::stream::Stream;
use futures::sync::oneshot;
use futures::Future;
use hyper;
use hyper::{header, StatusCode, Uri};
use serde_json::error;
use url::form_urlencoded;
use url::percent_encoding::{percent_encode, QUERY_ENCODE_SET};

use crate::authenticator_delegate::AuthenticatorDelegate;
use crate::types::{ApplicationSecret, Token};

const OOB_REDIRECT_URI: &'static str = "urn:ietf:wg:oauth:2.0:oob";

/// Assembles a URL to request an authorization token (with user interaction).
/// Note that the redirect_uri here has to be either None or some variation of
/// http://localhost:{port}, or the authorization won't work (error "redirect_uri_mismatch")
fn build_authentication_request_url<'a, T, I>(
    auth_uri: &str,
    client_id: &str,
    scopes: I,
    redirect_uri: Option<String>,
) -> String
where
    T: AsRef<str> + 'a,
    I: IntoIterator<Item = &'a T>,
{
    let mut url = String::new();
    let mut scopes_string = scopes.into_iter().fold(String::new(), |mut acc, sc| {
        acc.push_str(sc.as_ref());
        acc.push_str(" ");
        acc
    });
    // Remove last space
    scopes_string.pop();

    url.push_str(auth_uri);
    vec![
        format!("?scope={}", scopes_string),
        format!(
            "&redirect_uri={}",
            redirect_uri.unwrap_or(OOB_REDIRECT_URI.to_string())
        ),
        format!("&response_type=code"),
        format!("&client_id={}", client_id),
    ]
    .into_iter()
    .fold(url, |mut u, param| {
        u.push_str(&percent_encode(param.as_ref(), QUERY_ENCODE_SET).to_string());
        u
    })
}

pub struct InstalledFlow<C> {
    client: hyper::Client<C, hyper::Body>,
    server: Option<InstalledFlowServer>,
}

/// cf. https://developers.google.com/identity/protocols/OAuth2InstalledApp#choosingredirecturi
pub enum InstalledFlowReturnMethod {
    /// Involves showing a URL to the user and asking to copy a code from their browser
    /// (default)
    Interactive,
    /// Involves spinning up a local HTTP server and Google redirecting the browser to
    /// the server with a URL containing the code (preferred, but not as reliable). The
    /// parameter is the port to listen on.
    HTTPRedirect(u16),
}

impl<C: 'static> InstalledFlow<C>
where
    C: hyper::client::connect::Connect,
{
    /// Starts a new Installed App auth flow.
    /// If HTTPRedirect is chosen as method and the server can't be started, the flow falls
    /// back to Interactive.
    pub fn new(
        client: hyper::Client<C, hyper::Body>,
        method: Option<InstalledFlowReturnMethod>,
    ) -> InstalledFlow<C> {
        let default = InstalledFlow {
            client: client,
            server: None,
        };
        match method {
            None => default,
            Some(InstalledFlowReturnMethod::Interactive) => default,
            // Start server on localhost to accept auth code.
            Some(InstalledFlowReturnMethod::HTTPRedirect(port)) => {
                match InstalledFlowServer::new(port) {
                    Result::Err(_) => default,
                    Result::Ok(server) => InstalledFlow {
                        client: default.client,
                        server: Some(server),
                    },
                }
            }
        }
    }

    /// Handles the token request flow; it consists of the following steps:
    /// . Obtain a auhorization code with user cooperation or internal redirect.
    /// . Obtain a token and refresh token using that code.
    /// . Return that token
    ///
    /// It's recommended not to use the DefaultAuthenticatorDelegate, but a specialized one.
    pub fn obtain_token<'a, AD: AuthenticatorDelegate, S, T>(
        &mut self,
        auth_delegate: &mut AD,
        appsecret: &ApplicationSecret,
        scopes: S,
    ) -> Result<Token, Box<Error>>
    where
        T: AsRef<str> + 'a,
        S: Iterator<Item = &'a T>,
    {
        let authcode = self.get_authorization_code(auth_delegate, &appsecret, scopes)?;
        let tokens = self.request_token(&appsecret, &authcode, auth_delegate.redirect_uri())?;

        // Successful response
        if tokens.access_token.is_some() {
            let mut token = Token {
                access_token: tokens.access_token.unwrap(),
                refresh_token: tokens.refresh_token.unwrap(),
                token_type: tokens.token_type.unwrap(),
                expires_in: tokens.expires_in,
                expires_in_timestamp: None,
            };

            token.set_expiry_absolute();
            Result::Ok(token)
        } else {
            let err = io::Error::new(
                io::ErrorKind::Other,
                format!(
                    "Token API error: {} {}",
                    tokens.error.unwrap_or("<unknown err>".to_string()),
                    tokens.error_description.unwrap_or("".to_string())
                )
                .as_str(),
            );
            Result::Err(Box::new(err))
        }
    }

    /// Obtains an authorization code either interactively or via HTTP redirect (see
    /// InstalledFlowReturnMethod).
    fn get_authorization_code<'a, AD: AuthenticatorDelegate, S, T>(
        &mut self,
        auth_delegate: &mut AD,
        appsecret: &ApplicationSecret,
        scopes: S,
    ) -> Result<String, Box<Error>>
    where
        T: AsRef<str> + 'a,
        S: Iterator<Item = &'a T>,
    {
        let server = self.server.take(); // Will shutdown the server if present when goes out of scope
        let result: Result<String, Box<Error>> = match server {
            None => {
                let url = build_authentication_request_url(
                    &appsecret.auth_uri,
                    &appsecret.client_id,
                    scopes,
                    auth_delegate.redirect_uri(),
                );
                match auth_delegate.present_user_url(&url, true /* need_code */) {
                    None => Result::Err(Box::new(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "couldn't read code",
                    ))),
                    Some(mut code) => {
                        // Partial backwards compatibilty in case an implementation adds a new line
                        // due to previous behaviour.
                        let ends_with_newline =
                            code.chars().last().map(|c| c == '\n').unwrap_or(false);
                        if ends_with_newline {
                            code.pop();
                        }
                        Result::Ok(code)
                    }
                }
            }
            Some(mut server) => {
                // The redirect URI must be this very localhost URL, otherwise Google refuses
                // authorization.
                let url = build_authentication_request_url(
                    &appsecret.auth_uri,
                    &appsecret.client_id,
                    scopes,
                    auth_delegate
                        .redirect_uri()
                        .or_else(|| Some(format!("http://localhost:{}", server.port))),
                );
                auth_delegate.present_user_url(&url, false /* need_code */);

                match server.block_till_auth() {
                    Result::Err(e) => Result::Err(Box::new(e)),
                    Result::Ok(s) => Result::Ok(s),
                }
            }
        };

        result
    }

    /// Sends the authorization code to the provider in order to obtain access and refresh tokens.
    fn request_token(
        &self,
        appsecret: &ApplicationSecret,
        authcode: &str,
        custom_redirect_uri: Option<String>,
    ) -> Result<JSONTokenResponse, Box<Error>> {
        let redirect_uri = custom_redirect_uri.unwrap_or_else(|| match &self.server {
            None => OOB_REDIRECT_URI.to_string(),
            Some(server) => format!("http://localhost:{}", server.port),
        });

        let body = form_urlencoded::Serializer::new(String::new())
            .extend_pairs(vec![
                ("code".to_string(), authcode.to_string()),
                ("client_id".to_string(), appsecret.client_id.clone()),
                ("client_secret".to_string(), appsecret.client_secret.clone()),
                ("redirect_uri".to_string(), redirect_uri),
                ("grant_type".to_string(), "authorization_code".to_string()),
            ])
            .finish();

        let request = hyper::Request::post(&appsecret.token_uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .body(hyper::Body::from(body))
            .unwrap(); // TODO: error check

        let result = self.client.request(request).wait();

        let resp = String::new();

        match result {
            Result::Err(e) => return Result::Err(Box::new(e)),
            Result::Ok(res) => {
                let result = res
                    .into_body()
                    .concat2()
                    .wait()
                    .map(|c| String::from_utf8(c.into_bytes().to_vec()).unwrap()); // TODO: error handling

                match result {
                    Result::Err(e) => return Result::Err(Box::new(e)),
                    Result::Ok(_) => (),
                }
            }
        }

        let token_resp: Result<JSONTokenResponse, error::Error> = serde_json::from_str(&resp);

        match token_resp {
            Result::Err(e) => return Result::Err(Box::new(e)),
            Result::Ok(tok) => Result::Ok(tok) as Result<JSONTokenResponse, Box<Error>>,
        }
    }
}

#[derive(Deserialize)]
struct JSONTokenResponse {
    access_token: Option<String>,
    refresh_token: Option<String>,
    token_type: Option<String>,
    expires_in: Option<i64>,

    error: Option<String>,
    error_description: Option<String>,
}

struct InstalledFlowServer {
    port: u16,
    shutdown_tx: Option<oneshot::Sender<()>>,
    auth_code_rx: Option<oneshot::Receiver<String>>,
    threadpool: Option<tokio_threadpool::ThreadPool>,
}

impl InstalledFlowServer {
    fn new(port: u16) -> Result<InstalledFlowServer, ()> {
        let bound_port = hyper::server::conn::AddrIncoming::bind(&([127, 0, 0, 1], port).into());
        match bound_port {
            Result::Err(_) => Result::Err(()),
            Result::Ok(bound_port) => {
                let port = bound_port.local_addr().port();

                let (auth_code_tx, auth_code_rx) = oneshot::channel::<String>();
                let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

                let threadpool = tokio_threadpool::Builder::new()
                    .pool_size(1)
                    .name_prefix("InstalledFlowServer-")
                    .build();
                let service_maker = InstalledFlowServiceMaker::new(auth_code_tx);
                let server = hyper::server::Server::builder(bound_port)
                    .http1_only(true)
                    .serve(service_maker)
                    .with_graceful_shutdown(shutdown_rx)
                    .map_err(|err| panic!("Failed badly: {}", err)); // TODO: Error handling

                threadpool.spawn(server);

                Result::Ok(InstalledFlowServer {
                    port,
                    shutdown_tx: Option::Some(shutdown_tx),
                    auth_code_rx: Option::Some(auth_code_rx),
                    threadpool: Option::Some(threadpool),
                })
            }
        }
    }

    fn block_till_auth(&mut self) -> Result<String, oneshot::Canceled> {
        match self.auth_code_rx.take() {
            Some(auth_code_rx) => auth_code_rx.wait(),
            None => Result::Err(oneshot::Canceled),
        }
    }
}

impl std::ops::Drop for InstalledFlowServer {
    fn drop(&mut self) {
        self.shutdown_tx.take().map(|tx| tx.send(()));
        self.auth_code_rx.take().map(|mut rx| rx.close());
        self.threadpool.take();
    }
}

pub struct InstalledFlowHandlerResponseFuture {
    inner: Box<
        futures::Future<Item = hyper::Response<hyper::Body>, Error = hyper::http::Error> + Send,
    >,
}

impl InstalledFlowHandlerResponseFuture {
    fn new(
        fut: Box<
            futures::Future<Item = hyper::Response<hyper::Body>, Error = hyper::http::Error> + Send,
        >,
    ) -> Self {
        Self { inner: fut }
    }
}

impl futures::Future for InstalledFlowHandlerResponseFuture {
    type Item = hyper::Response<hyper::Body>;
    type Error = hyper::http::Error;

    fn poll(&mut self) -> futures::Poll<Self::Item, Self::Error> {
        self.inner.poll()
    }
}

/// Creates InstalledFlowService on demand
struct InstalledFlowServiceMaker {
    auth_code_tx: Arc<Mutex<Option<oneshot::Sender<String>>>>,
}

impl InstalledFlowServiceMaker {
    fn new(auth_code_tx: oneshot::Sender<String>) -> InstalledFlowServiceMaker {
        let auth_code_tx = Arc::new(Mutex::new(Option::Some(auth_code_tx)));
        InstalledFlowServiceMaker { auth_code_tx }
    }
}

impl<Ctx> hyper::service::MakeService<Ctx> for InstalledFlowServiceMaker {
    type ReqBody = hyper::Body;
    type ResBody = hyper::Body;
    type Error = hyper::http::Error;
    type Service = InstalledFlowService;
    type Future = futures::future::FutureResult<Self::Service, Self::Error>;
    type MakeError = hyper::http::Error;

    fn make_service(&mut self, _ctx: Ctx) -> Self::Future {
        let service = InstalledFlowService {
            auth_code_tx: self.auth_code_tx.clone(),
        };
        futures::future::ok(service)
    }
}

/// HTTP service handling the redirect from the provider.
struct InstalledFlowService {
    auth_code_tx: Arc<Mutex<Option<oneshot::Sender<String>>>>,
}

impl hyper::service::Service for InstalledFlowService {
    type ReqBody = hyper::Body;
    type ResBody = hyper::Body;
    type Error = hyper::http::Error;
    type Future = InstalledFlowHandlerResponseFuture;

    fn call(&mut self, req: hyper::Request<Self::ReqBody>) -> Self::Future {
        match req.uri().path_and_query() {
            Some(path_and_query) => {
                // We use a fake URL because the redirect goes to a URL, meaning we
                // can't use the url form decode (because there's slashes and hashes and stuff in
                // it).
                let url = Uri::builder()
                    .scheme("http")
                    .authority("example.com")
                    .path_and_query(path_and_query.clone())
                    .build();

                if url.is_err() {
                    let response = hyper::Response::builder()
                        .status(StatusCode::BAD_REQUEST)
                        .body(hyper::Body::from("Unparseable URL"));

                    match response {
                        Ok(response) => InstalledFlowHandlerResponseFuture::new(Box::new(
                            futures::future::ok(response),
                        )),
                        Err(err) => InstalledFlowHandlerResponseFuture::new(Box::new(
                            futures::future::err(err),
                        )),
                    }
                } else {
                    self.handle_url(url.unwrap());
                    let response =
                        hyper::Response::builder()
                            .status(StatusCode::OK)
                            .body(hyper::Body::from(
                                "<html><head><title>Success</title></head><body>You may now \
                                 close this window.</body></html>",
                            ));

                    match response {
                        Ok(response) => InstalledFlowHandlerResponseFuture::new(Box::new(
                            futures::future::ok(response),
                        )),
                        Err(err) => InstalledFlowHandlerResponseFuture::new(Box::new(
                            futures::future::err(err),
                        )),
                    }
                }
            }
            None => {
                let response = hyper::Response::builder()
                    .status(StatusCode::BAD_REQUEST)
                    .body(hyper::Body::from("Invalid Request!"));

                match response {
                    Ok(response) => InstalledFlowHandlerResponseFuture::new(Box::new(
                        futures::future::ok(response),
                    )),
                    Err(err) => {
                        InstalledFlowHandlerResponseFuture::new(Box::new(futures::future::err(err)))
                    }
                }
            }
        }
    }
}

impl InstalledFlowService {
    fn handle_url(&mut self, url: hyper::Uri) {
        // Google redirects to the specified localhost URL, appending the authorization
        // code, like this: http://localhost:8080/xyz/?code=4/731fJ3BheyCouCniPufAd280GHNV5Ju35yYcGs
        // We take that code and send it to the get_authorization_code() function that
        // waits for it.
        for (param, val) in form_urlencoded::parse(url.query().unwrap_or("").as_bytes()) {
            if param == "code".to_string() {
                let mut auth_code_tx = self.auth_code_tx.lock().unwrap();
                match auth_code_tx.take() {
                    Some(auth_code_tx) => {
                        let _ = auth_code_tx.send(val.to_owned().to_string());
                    }
                    None => {
                        // call to the server after a previous call. Each server is only designed
                        // to receive a single request.
                    }
                };
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_url_builder() {
        assert_eq!(
            "https://accounts.google.\
             com/o/oauth2/auth?scope=email%20profile&redirect_uri=urn:ietf:wg:oauth:2.0:\
             oob&response_type=code&client_id=812741506391-h38jh0j4fv0ce1krdkiq0hfvt6n5amr\
             f.apps.googleusercontent.com",
            build_authentication_request_url(
                "https://accounts.google.com/o/oauth2/auth",
                "812741506391-h38jh0j4fv0ce1krdkiq0hfvt6n5am\
                 rf.apps.googleusercontent.com",
                vec![&"email".to_string(), &"profile".to_string()],
                None
            )
        );
    }

    #[test]
    fn test_server_random_local_port() {
        let addr1 = InstalledFlowServer::new(0).unwrap();
        let addr2 = InstalledFlowServer::new(0).unwrap();
        assert_ne!(addr1.port, addr2.port);
    }

    #[test]
    fn test_http_handle_url() {
        let (tx, rx) = oneshot::channel();
        let mut handler = InstalledFlowService {
            auth_code_tx: Arc::new(Mutex::new(Option::Some(tx))),
        };
        // URLs are usually a bit botched
        let url: Uri = "http://example.com:1234/?code=ab/c%2Fd#".parse().unwrap();
        handler.handle_url(url);
        assert_eq!(rx.wait().unwrap(), "ab/c/d".to_string());
    }

    #[test]
    fn test_server() {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let client: hyper::Client<hyper::client::HttpConnector, hyper::Body> =
            hyper::Client::builder()
                .executor(runtime.executor())
                .build_http();
        let mut server = InstalledFlowServer::new(0).unwrap();

        let response = client
            .get(
                format!("http://127.0.0.1:{}/", server.port)
                    .parse()
                    .unwrap(),
            )
            .wait();
        match response {
            Result::Ok(response) => {
                assert!(response.status().is_success());
            }
            Result::Err(err) => {
                assert!(false, "Failed to request from local server: {:?}", err);
            }
        }

        let response = client
            .get(
                format!("http://127.0.0.1:{}/?code=ab/c%2Fd#", server.port)
                    .parse()
                    .unwrap(),
            )
            .wait();
        match response {
            Result::Ok(response) => {
                assert!(response.status().is_success());
            }
            Result::Err(err) => {
                assert!(false, "Failed to request from local server: {:?}", err);
            }
        }

        match server.block_till_auth() {
            Result::Ok(response) => {
                assert_eq!(response, "ab/c/d".to_string());
            }
            Result::Err(err) => {
                assert!(false, "Server failed to pass on the message: {:?}", err);
            }
        }
    }
}
