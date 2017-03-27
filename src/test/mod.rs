use std::{cell, io, net, time};
// TODO: Cross platform
use std::os::unix::net::UnixStream;
use hyper::{self, client, server};
use futures::{future, Future, Async};
use tokio_core::reactor;
use tokio_io::{AsyncRead, AsyncWrite};
use mio;

pub struct TestServer<S> {
    core: reactor::Core,
    http: server::Http,
    timeout: u64,
    service: S,
}

#[derive(Debug)]
pub enum TestRequestError {
    TimedOut,
    IoError(io::Error),
    HyperError(hyper::Error),
}

impl<S> TestServer<S>
    where S: server::Service<Request = server::Request,
                             Response = server::Response,
                             Error = hyper::Error> + Clone + 'static
{
    pub fn new(service: S) -> Result<TestServer<S>, io::Error> {
        reactor::Core::new().map(|core| {
            TestServer {
                core: core,
                http: server::Http::new(),
                timeout: 10,
                service: service,
            }
        })
    }

    pub fn timeout(self, t: u64) -> TestServer<S> {
        TestServer { timeout: t, ..self }
    }

    pub fn client(&self) -> io::Result<client::Client<TestConnect>> {
        let handle = self.core.handle();

        let (cs, ss) = AsyncUnixStream::pair()?;
        let cs = reactor::PollEvented::new(cs, &handle)?;
        let ss = reactor::PollEvented::new(ss, &handle)?;
        let remote_addr = "127.0.0.1:0".parse().unwrap();

        self.http.bind_connection(&handle, ss, remote_addr, self.service.clone());
        Ok(client::Client::configure()
               .connector(TestConnect { stream: cell::RefCell::new(Some(cs)) })
               .build(&self.core.handle()))
    }

    pub fn run_request<F>(&mut self, f: F) -> Result<F::Item, TestRequestError>
        where F: Future<Error = hyper::Error>
    {
        let timeout_duration = time::Duration::from_secs(self.timeout);
        let timeout = reactor::Timeout::new(timeout_duration, &self.core.handle()).unwrap();

        let run_result = self.core.run(f.select2(timeout));
        match run_result {
            Ok(future::Either::A((item, _))) => Ok(item),
            Ok(future::Either::B(_)) => Err(TestRequestError::TimedOut),
            Err(future::Either::A((e, _))) => Err(TestRequestError::HyperError(e)),
            Err(future::Either::B((e, _))) => Err(TestRequestError::IoError(e)),
        }
    }
}

pub struct TestConnect {
    stream: cell::RefCell<Option<reactor::PollEvented<AsyncUnixStream>>>,
}

impl client::Service for TestConnect {
    type Request = hyper::Uri;
    type Error = io::Error;
    type Response = reactor::PollEvented<AsyncUnixStream>;
    type Future = future::FutureResult<Self::Response, Self::Error>;

    fn call(&self, _req: Self::Request) -> Self::Future {
        match self.stream.try_borrow_mut() {
            Ok(ref mut borrowed) => {
                match borrowed.take() {
                    Some(stream) => future::ok(stream),
                    None => {
                        future::err(io::Error::new(io::ErrorKind::Other, "stream already taken"))
                    }
                }
            }
            Err(_) => {
                future::err(io::Error::new(io::ErrorKind::Other, "stream.try_borrow_mut() failed"))
            }
        }
    }
}

pub struct AsyncUnixStream {
    stream: UnixStream,
}

fn io_error_to_async_io_error<T>(r: Result<T, io::Error>) -> Result<T, io::Error> {
    r.map_err(|e| match e.raw_os_error() {
                  Some(35) => io::Error::new(io::ErrorKind::WouldBlock, "test socket would block"),
                  _ => e,
              })
}

impl AsyncUnixStream {
    fn new(stream: UnixStream) -> Result<AsyncUnixStream, io::Error> {
        stream.set_nonblocking(true)?;
        Ok(AsyncUnixStream { stream: stream })
    }

    fn pair() -> Result<(AsyncUnixStream, AsyncUnixStream), io::Error> {
        let (cs, ss) = UnixStream::pair()?;
        let cs = AsyncUnixStream::new(cs)?;
        let ss = AsyncUnixStream::new(ss)?;
        Ok((cs, ss))
    }
}

use std::os::unix::io::AsRawFd;

impl io::Read for AsyncUnixStream {
    fn read(&mut self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let result = self.stream.read(buf);
        io_error_to_async_io_error(result)
    }
}

impl AsyncRead for AsyncUnixStream {}

impl io::Write for AsyncUnixStream {
    fn write(&mut self, buf: &[u8]) -> Result<usize, io::Error> {
        let result = self.stream.write(buf);
        io_error_to_async_io_error(result)
    }

    fn flush(&mut self) -> Result<(), io::Error> {
        let result = self.stream.flush();
        io_error_to_async_io_error(result)
    }
}

impl AsyncWrite for AsyncUnixStream {
    fn shutdown(&mut self) -> Result<Async<()>, io::Error> {
        self.stream.shutdown(net::Shutdown::Both).map(|_| Async::Ready(()))
    }
}

impl mio::event::Evented for AsyncUnixStream {
    fn register(&self,
                poll: &mio::Poll,
                token: mio::Token,
                ready: mio::Ready,
                poll_opt: mio::PollOpt)
                -> Result<(), io::Error> {
        mio::unix::EventedFd(&self.stream.as_raw_fd()).register(poll, token, ready, poll_opt)
    }

    fn reregister(&self,
                  poll: &mio::Poll,
                  token: mio::Token,
                  ready: mio::Ready,
                  poll_opt: mio::PollOpt)
                  -> Result<(), io::Error> {
        mio::unix::EventedFd(&self.stream.as_raw_fd()).reregister(poll, token, ready, poll_opt)
    }

    fn deregister(&self, poll: &mio::Poll) -> Result<(), io::Error> {
        mio::unix::EventedFd(&self.stream.as_raw_fd()).deregister(poll)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};
    use hyper::StatusCode;

    #[derive(Clone)]
    struct TestService {
        response: String,
    }

    impl server::Service for TestService {
        type Request = server::Request;
        type Response = server::Response;
        type Error = hyper::Error;
        type Future = Box<Future<Item = Self::Response, Error = Self::Error>>;

        fn call(&self, req: Self::Request) -> Self::Future {
            match req.path() {
                "/" => {
                    let response = server::Response::new()
                        .with_status(StatusCode::Ok)
                        .with_body(self.response.clone());

                    future::ok(response).boxed()
                }
                "/timeout" => future::empty().boxed(),
                _ => {
                    let not_found = server::Response::new()
                        .with_status(StatusCode::NotFound)
                        .with_body(self.response.clone());

                    future::ok(not_found).boxed()
                }
            }
        }
    }

    #[test]
    fn serves_requests() {
        let ticks = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
        let service = TestService { response: format!("time: {}", ticks) };

        let mut test_server = TestServer::new(service).unwrap();
        let response = test_server.client().unwrap().get("http://localhost/".parse().unwrap());
        let response = test_server.run_request(response).unwrap();

        assert_eq!(*response.status(), StatusCode::Ok);
    }

    #[test]
    fn times_out() {
        let service = TestService { response: "".to_owned() };
        let mut test_server = TestServer::new(service).unwrap().timeout(1);
        let response =
            test_server.client().unwrap().get("http://localhost/timeout".parse().unwrap());

        match test_server.run_request(response) {
            Err(TestRequestError::TimedOut) => (),
            e @ Err(_) => {
                e.unwrap();
            }
            Ok(_) => panic!("expected timeout, but was Ok(_)"),
        }
    }
}
