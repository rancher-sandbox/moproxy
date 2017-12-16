mod connect;
use std::cmp;
use std::rc::Rc;
use std::io::{self, ErrorKind};
use std::time::Duration;
use std::net::{SocketAddr, SocketAddrV4};
use std::os::unix::io::{RawFd, AsRawFd};
use nix::{self, sys};
use tokio_core::net::TcpStream;
use tokio_core::reactor::Handle;
use tokio_timer::Timer;
use tokio_io::io::read;
use futures::{future, Future};
use proxy::{ProxyServer, Destination};
use proxy::copy::{pipe, SharedBuf};
use monitor::ServerList;
use tls::{self, TlsClientHello};
use client::connect::try_connect_all;


#[derive(Debug)]
pub struct NewClient {
    left: TcpStream,
    src: SocketAddr,
    pub dest: Destination,
    list: ServerList,
    handle: Handle,
}

#[derive(Debug)]
pub struct NewClientWithData {
    left: TcpStream,
    src: SocketAddr,
    dest: Destination,
    pending_data: Box<[u8]>,
    allow_parallel: bool,
    list: ServerList,
    handle: Handle,
}

#[derive(Debug)]
pub struct ConnectedClient {
    left: TcpStream,
    right: TcpStream,
    src: SocketAddr,
    dest: Destination,
    server: Rc<ProxyServer>,
    handle: Handle,
}

pub trait Connectable {
    fn connect_server(self, n_parallel: usize)
        -> Box<Future<Item=ConnectedClient, Error=()>>;
}

impl NewClient {
    pub fn from_socket(left: TcpStream, list: ServerList, handle: Handle)
            -> Box<Future<Item=Self, Error=()>> {
        let src_dest = future::result(left.peer_addr())
            .join(future::result(get_original_dest(left.as_raw_fd())))
            .map_err(|err| warn!("fail to get original destination: {}", err));
        Box::new(src_dest.map(move |(src, dest)| {
            NewClient {
                left, src, dest: dest.into(), list, handle,
            }
        }))
    }
}

impl NewClient {
    pub fn retrive_dest(self)
            -> Box<Future<Item=NewClientWithData, Error=()>> {
        let NewClient { left, src, mut dest, list, handle } = self; 
        let timer = Timer::default();
        let wait = Duration::from_millis(200);
        // try to read TLS ClientHello for
        //   1. --remote-dns: parse host name from SNI
        //   2. --n-parallel: need the whole request to be forwarded
        let data = read(left, vec![0u8; 2048])
            .map_err(|err| warn!("fail to read hello from client: {}", err));
        let result = timer.timeout(data, wait)
                          .map(move |(left, mut data, len)| {
            data.truncate(len);
            let allow_parallel = match tls::parse_client_hello(&data) {
                Err(err) => {
                    info!("fail to parse hello: {}", err);
                    false
                },
                Ok(TlsClientHello { server_name, early_data, .. }) => {
                    if let Some(name) = server_name {
                        dest = (name, dest.port).into();
                        debug!("SNI found: {}", name);
                    } else {
                        debug!("not SNI found in client hello");
                    }
                    if early_data {
                        debug!("TLS with early data");
                    }
                    true
                },
            };
            NewClientWithData {
                left, src, dest, list, handle, allow_parallel,
                pending_data: data.into_boxed_slice(),
            }
        }).map_err(|_| info!("no tls request received before timeout"));
        Box::new(result)
    }
}

impl Connectable for NewClient {
    fn connect_server(self, _n_parallel: usize)
            -> Box<Future<Item=ConnectedClient, Error=()>> {
        let NewClient { left, src, dest, list, handle } = self;
        let conn = try_connect_all(dest.clone(), list, 1, false, None,
                                   handle.clone());
        let client = conn.map(move |(server, right)| {
            info!("{} => {} via {}", src, dest, server.tag);
            ConnectedClient {
                left, right, src, dest, server, handle
            }
        }).map_err(|_| warn!("all proxy server down"));
        Box::new(client)
    }
}

impl Connectable for NewClientWithData {
    fn connect_server(self, n_parallel: usize)
            -> Box<Future<Item=ConnectedClient, Error=()>> {
        let NewClientWithData {
            left, src, dest, list, handle,
            pending_data, allow_parallel } = self;
        let pending_data = Some(RcBox::new(pending_data));
        let n_parallel = if allow_parallel {
            cmp::min(list.len(), n_parallel)
        } else {
            1
        };
        let conn = try_connect_all(dest.clone(), list, n_parallel, true,
                                   pending_data, handle.clone());
        let client = conn.map(move |(server, right)| {
            info!("{} => {} via {}", src, dest, server.tag);
            ConnectedClient {
                left, right, src, dest, server, handle
            }
        }).map_err(|_| warn!("all proxy server down"));
        Box::new(client)
    }
}

impl ConnectedClient {
    pub fn serve(self, shared_buf: SharedBuf)
            -> Box<Future<Item=(), Error=()>> {
        let ConnectedClient { left, right, dest, server, .. } = self;
        // TODO: make keepalive configurable
        let timeout = Some(Duration::from_secs(300));
        if let Err(e) = left.set_keepalive(timeout)
                .and(right.set_keepalive(timeout)) {
            warn!("fail to set keepalive: {}", e);
        }

        server.update_stats_conn_open();
        let serve = pipe(left, right, server.clone(), shared_buf)
            .then(move |result| match result {
                Ok((tx, rx)) => {
                    server.update_stats_conn_close();
                    debug!("tx {}, rx {} bytes ({} => {})",
                        tx, rx, server.tag, dest);
                    Ok(())
                },
                Err(e) => {
                    server.update_stats_conn_close();
                    warn!("{} (=> {}) piping error: {}",
                        server.tag, dest, e);
                    Err(())
                }
            });
        Box::new(serve)
    }
}

#[derive(Debug)]
pub struct RcBox<T: ?Sized> {
    item: Rc<Box<T>>,
}
impl<T: ?Sized> RcBox<T> {
    fn new(item: Box<T>) -> Self {
        RcBox { item: Rc::new(item) }
    }
}
impl<T: ?Sized> AsRef<T> for RcBox<T> {
    fn as_ref(&self) -> &T {
        &self.item
    }
}
impl<T: ?Sized> Clone for RcBox<T> {
    fn clone(&self) -> Self {
        RcBox { item: self.item.clone() }
    }
}

fn get_original_dest(fd: RawFd) -> io::Result<SocketAddr> {
    let addr = sys::socket::getsockopt(fd, sys::socket::sockopt::OriginalDst)
        .map_err(|e| match e {
            nix::Error::Sys(err) => io::Error::from(err),
            _ => io::Error::new(ErrorKind::Other, e),
        })?;
    let addr = SocketAddrV4::new(addr.sin_addr.s_addr.to_be().into(),
                                 addr.sin_port.to_be());
    // TODO: support IPv6
    Ok(SocketAddr::V4(addr))
}
