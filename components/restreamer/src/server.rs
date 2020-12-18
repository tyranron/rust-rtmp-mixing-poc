//! HTTP servers.

use std::net::IpAddr;

use ephyr_log::log;
use futures::future;
use tokio::fs;

use crate::{
    cli::{Failure, Opts},
    ffmpeg, srs, State,
};

/// Runs all application's HTTP servers.
///
/// # Errors
///
/// If some [`HttpServer`] cannot run due to already used port, etc.
/// The actual error is witten to logs.
#[actix_web::main]
pub async fn run(mut cfg: Opts) -> Result<(), Failure> {
    if cfg.public_host.is_none() {
        cfg.public_host = Some(
            detect_public_ip()
                .await
                .ok_or_else(|| {
                    log::error!("Cannot detect server's public IP address")
                })?
                .to_string(),
        );
    }

    let ffmpeg_path =
        fs::canonicalize(&cfg.ffmpeg_path).await.map_err(|e| {
            log::error!("Failed to resolve FFmpeg binary path: {}", e)
        })?;

    let state = State::try_new(&cfg.state_path)
        .await
        .map_err(|e| log::error!("Failed to initialize server state: {}", e))?;

    let _srs = srs::Server::try_new(
        &cfg.srs_path,
        &srs::Config {
            callback_port: cfg.callback_http_port,
            log_level: cfg.verbose.map(Into::into).unwrap_or_default(),
        },
    )
    .await
    .map_err(|e| log::error!("Failed to initialize SRS server: {}", e))?;

    let mut restreamers =
        ffmpeg::RestreamersPool::new(ffmpeg_path, state.clone());
    State::on_change("spawn_restreamers", &state.restreams, move |restreams| {
        future::ready(restreamers.apply(restreams))
    });

    future::try_join(
        self::client::run(&cfg, state.clone()),
        self::callback::run(&cfg, state),
    )
    .await
    .map(|_| ())
}

/// Client HTTP server responding to client requests.
pub mod client {
    use std::time::Duration;

    use actix_service::Service as _;
    use actix_web::{
        dev::ServiceRequest, get, middleware, route, web, App, Error,
        HttpRequest, HttpResponse, HttpServer,
    };
    use actix_web_httpauth::extractors::{
        basic::{self, BasicAuth},
        AuthExtractor as _, AuthExtractorConfig, AuthenticationError,
    };
    use actix_web_static_files::ResourceFiles;
    use ephyr_log::log;
    use futures::{future, FutureExt as _};
    use juniper::http::playground::playground_source;
    use juniper_actix::{
        graphql_handler, subscriptions::subscriptions_handler,
    };
    use juniper_graphql_ws::ConnectionConfig;

    use crate::{
        api,
        cli::{Failure, Opts},
        State,
    };

    pub mod public_dir {
        #![allow(unused_results)]
        #![doc(hidden)]

        use std::collections::HashMap;

        include!(concat!(env!("OUT_DIR"), "/generated.rs"));
    }

    /// Runs client HTTP server.
    ///
    /// Client HTTP server serves [`api::graphql::client`] on `/` endpoint.
    ///
    /// # Playground
    ///
    /// If [`cli::Opts::debug`] is specified then additionally serves
    /// [GraphQL Playground][2] on `/playground` endpoint with no authorization
    /// required.
    ///
    /// # Errors
    ///
    /// If [`HttpServer`] cannot run due to already used port, etc.
    /// The actual error is logged.
    ///
    /// [`cli::Opts::debug`]: crate::cli::Opts::debug
    /// [2]: https://github.com/graphql/graphql-playground
    pub async fn run(cfg: &Opts, state: State) -> Result<(), Failure> {
        let in_debug_mode = cfg.debug;

        let stored_cfg = cfg.clone();

        Ok(HttpServer::new(move || {
            let public_dir_files = public_dir::generate();
            let mut app = App::new()
                .app_data(stored_cfg.clone())
                .app_data(state.clone())
                .app_data(
                    basic::Config::default().realm("Any login is allowed"),
                )
                .data(api::graphql::client::schema())
                .wrap(middleware::Logger::default())
                .wrap_fn(|req, srv| match authorize(req) {
                    Ok(req) => srv.call(req).left_future(),
                    Err(e) => future::err(e).right_future(),
                })
                .service(graphql);
            if in_debug_mode {
                app = app.service(playground);
            }
            app.service(ResourceFiles::new("/", public_dir_files))
        })
        .bind((cfg.client_http_ip, cfg.client_http_port))
        .map_err(|e| log::error!("Failed to bind client HTTP server: {}", e))?
        .run()
        .await
        .map_err(|e| log::error!("Failed to run client HTTP server: {}", e))?)
    }

    /// Endpoint serving [`api::graphql::client`] directly.
    ///
    /// # Errors
    ///
    /// If GraphQL operation execution errors or fails.
    #[route("/api", method = "GET", method = "POST")]
    async fn graphql(
        req: HttpRequest,
        payload: web::Payload,
        schema: web::Data<api::graphql::client::Schema>,
    ) -> Result<HttpResponse, Error> {
        let ctx = api::graphql::Context::new(req.clone());
        if req.head().upgrade() {
            let cfg = ConnectionConfig::new(ctx)
                .with_keep_alive_interval(Duration::from_secs(5));
            subscriptions_handler(req, payload, schema.into_inner(), cfg).await
        } else {
            graphql_handler(&schema, &ctx, req, payload).await
        }
    }

    /// Endpoint serving [GraphQL Playground][1] for exploring
    /// [`api::graphql::client`].
    ///
    /// [1]: https://github.com/graphql/graphql-playground
    #[get("/api/playground")]
    async fn playground() -> HttpResponse {
        // Constructs API URL relatively to the current HTTP request's scheme
        // and authority.
        let html = playground_source("__API_URL__", None).replace(
            "'__API_URL__'",
            r"document.URL.replace(/\/playground$/, '')",
        );
        HttpResponse::Ok()
            .content_type("text/html; charset=utf-8")
            .body(html)
    }

    fn authorize(req: ServiceRequest) -> Result<ServiceRequest, Error> {
        let hash =
            match req.app_data::<State>().unwrap().password_hash.get_cloned() {
                Some(h) => h,
                None => return Ok(req),
            };

        let err = || {
            AuthenticationError::new(
                req.app_data::<basic::Config>()
                    .unwrap()
                    .clone()
                    .into_inner(),
            )
        };

        let auth = BasicAuth::from_service_request(&req).into_inner()?;
        let pass = auth.password().ok_or_else(err)?;
        if argon2::verify_encoded(hash.as_str(), pass.as_bytes()) != Ok(true) {
            return Err(err().into());
        }

        return Ok(req);
    }
}

/// Callback HTTP server responding to [SRS] HTTP callbacks.
///
/// [SRS]: https://github.com/ossrs/srs
pub mod callback {
    use actix_web::{error, middleware, post, web, App, Error, HttpServer};
    use ephyr_log::log;

    use crate::{
        api,
        cli::{Failure, Opts},
        state::{State, Status},
    };

    pub async fn run(cfg: &Opts, state: State) -> Result<(), Failure> {
        Ok(HttpServer::new(move || {
            App::new()
                .data(state.clone())
                .wrap(middleware::Logger::default())
                .service(callback)
        })
        .bind((cfg.callback_http_ip, cfg.callback_http_port))
        .map_err(|e| log::error!("Failed to bind callback HTTP server: {}", e))?
        .run()
        .await
        .map_err(|e| {
            log::error!("Failed to run callback HTTP server: {}", e)
        })?)
    }

    #[post("/")]
    async fn callback(
        req: web::Json<api::srs::callback::Request>,
        state: web::Data<State>,
    ) -> Result<&'static str, Error> {
        use api::srs::callback::Action;
        match req.action {
            Action::OnConnect => on_connect(&req, &*state)?,
            Action::OnPublish => on_publish(&req, &*state)?,
            Action::OnUnpublish => on_unpublish(&req, &*state)?,
        }
        Ok("0")
    }

    fn on_connect(
        req: &api::srs::callback::Request,
        state: &State,
    ) -> Result<(), Error> {
        let restreams = state.restreams.get_cloned();
        let _ = restreams
            .iter()
            .find(|r| r.enabled && r.input.uses_srs_app(&req.app))
            .ok_or_else(|| error::ErrorNotFound("Such `app` doesn't exist"))?;
        Ok(())
    }

    fn on_publish(
        req: &api::srs::callback::Request,
        state: &State,
    ) -> Result<(), Error> {
        if req.stream.as_ref().map(String::as_str) != Some("in") {
            return Err(error::ErrorNotFound("Such `stream` doesn't exist"));
        }

        let mut restreams = state.restreams.lock_mut();
        let restream = restreams
            .iter_mut()
            .find(|r| r.enabled && r.input.uses_srs_app(&req.app))
            .ok_or_else(|| error::ErrorNotFound("Such `app` doesn't exist"))?;

        if restream.input.is_pull() && !req.ip.is_loopback() {
            return Err(error::ErrorForbidden("`app` is allowed only locally"));
        }

        if restream.srs_publisher_id.as_ref().map(|id| **id)
            != Some(req.client_id)
        {
            restream.srs_publisher_id = Some(req.client_id.into());
        }
        restream.input.set_status(Status::Online);
        Ok(())
    }

    fn on_unpublish(
        req: &api::srs::callback::Request,
        state: &State,
    ) -> Result<(), Error> {
        let mut restreams = state.restreams.lock_mut();
        let restream = restreams
            .iter_mut()
            .find(|r| r.input.uses_srs_app(&req.app))
            .ok_or_else(|| error::ErrorNotFound("Such `app` doesn't exist"))?;

        restream.srs_publisher_id = None;
        restream.input.set_status(Status::Offline);
        Ok(())
    }
}

pub async fn detect_public_ip() -> Option<IpAddr> {
    use public_ip::{dns, http, BoxToResolver, ToResolver as _};

    public_ip::resolve_address(
        vec![
            BoxToResolver::new(dns::OPENDNS_RESOLVER),
            BoxToResolver::new(http::HTTP_IPIFY_ORG_RESOLVER),
        ]
        .to_resolver(),
    )
    .await
}
