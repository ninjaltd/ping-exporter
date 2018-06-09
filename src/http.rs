use std::net::IpAddr;

use futures::{Future, future};
use hyper::{Server, Request, Response, Body, Method, StatusCode};
use hyper::service::{Service, NewService};
use serde_urlencoded;
use tacho;

use metrics::{METRICS, REPORTER};
use pinger::Pinger;
use settings::Settings;
use utils::{Protocol, NameOrIpAddr, boxed};

lazy_static! {
    static ref HTTP_PING: tacho::Counter = METRICS.counter("http_ping", "Number of /ping requests");
}

pub fn init() {
    ::lazy_static::initialize(&HTTP_PING);
}

struct NewApp {
    settings: Settings,
    pinger: Pinger,
}

impl NewService for NewApp {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = Box<::std::error::Error + Send + Sync>;
    type Service = App;
    type Future = future::FutureResult<Self::Service, Self::InitError>;
    type InitError = Box<::std::error::Error + Send + Sync>;

    fn new_service(&self) -> <Self as NewService>::Future {
        future::ok(App {
            settings: self.settings.clone(),
            pinger: self.pinger.clone(),
        })
    }
}

enum RequestType {
    Ping,
    Metrics,
    Unknown,
}

#[derive(Debug, Deserialize)]
struct PingRequest {
    target: NameOrIpAddr,
    protocol: Option<Protocol>,
    count: Option<usize>,
    ping_timeout: Option<u64>,
    resolve_timeout: Option<u64>,
}

struct App {
    settings: Settings,
    pinger: Pinger,
}

impl Service for App {
    type ReqBody = Body;
    type ResBody = Body;
    type Error = Box<::std::error::Error + Send + Sync>;
    type Future = Box<Future<Item=Response<Self::ResBody>, Error=Self::Error> + Send>;

    fn call(&mut self, req: Request<<Self as Service>::ReqBody>) -> <Self as Service>::Future {
        let st = ::time::precise_time_ns();
        let request_type = {
            let method = req.method();
            let path = req.uri().path();

            if method == &Method::GET && (path == "/ping" || path == "/ping/") {
                HTTP_PING.incr(1);
                RequestType::Ping
            } else if method == &Method::GET && (path == "/metrics" || path == "/metrics/") {
                RequestType::Metrics
            } else {
                RequestType::Unknown
            }
        };

        let future = match request_type {
            RequestType::Unknown => {
                boxed(future::err((StatusCode::NOT_FOUND, Body::from("Not Found"))))
            }
            RequestType::Metrics => {
                boxed(get_metrics())
            }
            RequestType::Ping => {
                let query = req.uri().query().unwrap_or("");

                let mb_req = serde_urlencoded::from_str::<PingRequest>(query)
                    .map_err(|err| {
                        (StatusCode::BAD_REQUEST, Body::from(format!("Bad Request: {}", err)))
                    });

                let future = future::result(mb_req);

                let settings = self.settings.clone();
                let pinger = self.pinger.clone();
                let future = future.and_then(move |request| ping(request, settings, pinger));
                boxed(future)
            }
        };

        let future = future.then(|request| {
            match request {
                Err((status_code, body)) => {
                    let mut response = Response::new(body);
                    *response.status_mut() = status_code;
                    future::ok(response)
                }
                Ok(body) => {
                    let mut response = Response::new(body);
                    future::ok(response)
                }
            }
        });

        boxed(future.then(move |resp| {
            let delta = ::time::precise_time_ns() - st;
            let delta_ms = delta / 1000000;
            if let Some(path_and_query) = req.uri().path_and_query() {
                info!("{} {} {}ms", req.method(), path_and_query, delta_ms);
            } else {
                info!("{} {} {}ms", req.method(), req.uri().path(), delta_ms);
            }
            resp
        }))
    }
}

fn get_metrics() -> impl Future<Item=Body, Error=((StatusCode, Body))> {
    match tacho::prometheus::string(&REPORTER.peek()) {
        Err(_) => future::err((StatusCode::INTERNAL_SERVER_ERROR, Body::from("Internal Error"))),
        Ok(s) => future::ok(Body::from(s)),
    }
}

fn ping(request: PingRequest, settings: Settings, pinger: Pinger) -> impl Future<Item=Body, Error=((StatusCode, Body))> {
    let count = request.count.unwrap_or(settings.count);
    if count > settings.max_count {
        return boxed(future::err((StatusCode::BAD_REQUEST, Body::from("Too many pings"))))
    }

    if count < 1 {
        return boxed(future::err((StatusCode::BAD_REQUEST, Body::from("Too few pings"))))
    }

    let ping_timeout = request.ping_timeout.unwrap_or(settings.ping_timeout);

    if ping_timeout > settings.max_ping_timeout {
        return boxed(future::err((StatusCode::BAD_REQUEST, Body::from("Too large ping timeout"))))
    }

    let resolve_timeout = request.resolve_timeout.unwrap_or(settings.resolve_timeout);

    if resolve_timeout > settings.resolve_timeout {
        return boxed(future::err((StatusCode::BAD_REQUEST, Body::from("Too large resolve timeout"))))
    }

    let mut protocol = request.protocol.unwrap_or(settings.protocol);
    match &request.target {
        &NameOrIpAddr::IpAddr(IpAddr::V4(_)) => protocol = Protocol::V4,
        &NameOrIpAddr::IpAddr(IpAddr::V6(_)) => protocol = Protocol::V6,
        _ => ()
    }

    let name = request.target;

    let future = pinger.ping(name.clone(), protocol, count, resolve_timeout, ping_timeout);
    let future = future.map_err(|error| {
        (StatusCode::OK, Body::from(format!("{}", error)))
    });

    let future = future.and_then(move |(resolve_time_ns, ip, pings)| {
        let (metrics, reporter) = tacho::new();
        let metrics = metrics
            .labeled("target", name)
            .labeled("protocol", protocol)
            .labeled("count", count)
            .labeled("ping_timeout", ping_timeout)
            .labeled("resolve_timeout", resolve_timeout)
            .labeled("ip", ip);

        metrics.gauge("ping_resolve_time", "Resolve time").set((resolve_time_ns / 1000000) as usize);

        let times = metrics.stat("ping_times", "Response times");

        let mut failures = 0;
        let mut successful = 0;
        let total = pings.len();

        for reply_time in pings {
            match reply_time {
                Some(reply_time) => {
                    times.add((reply_time * 1000.0) as u64);
                    successful += 1;
                },
                None => {
                    failures += 1;
                }
            }
        }

        metrics.gauge("ping_packets_total", "Total packets").set(total);
        metrics.gauge("ping_packets_success", "Sucessful pings").set(successful);
        metrics.gauge("ping_packets_failed", "Failed ping").set(failures);

        if total > 0 {
            let loss = failures as f64 / total as f64 * 100.0;
            metrics.gauge("ping_packets_loss", "Packets loss percents").set(loss as usize);
        }

        match tacho::prometheus::string(&reporter.peek()) {
            Err(_) => Err((StatusCode::INTERNAL_SERVER_ERROR, Body::from("Internal Error"))),
            Ok(s) => Ok(Body::from(s)),
        }
    });

    boxed(future)
}

pub fn server(settings: Settings, pinger: Pinger) -> impl Future<Item=(), Error=()> {
    let builder = Server::try_bind(&settings.listen);
    let future = future::result(builder).and_then(move |builder| {
        builder.serve(NewApp { settings, pinger })
    });
    let future = future.map_err(|error| {
        error!("Server error: {}", error);
    });
    future
}

#[cfg(test)]
mod tests {
    use super::init;

    #[test]
    fn test_lazy_static() {
        init()
    }
}