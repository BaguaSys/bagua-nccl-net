use crate::utils;
use crate::utils::NCCLSocketDev;
use bytes::Bytes;
use nix::sys::socket::{InetAddr, SockAddr};
use opentelemetry::{
    metrics::BoundValueRecorder,
    trace::{Span, TraceContextExt, Tracer},
    KeyValue,
};
use socket2::{Domain, Socket, Type};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net;
use std::sync::{Arc, Mutex};
use thiserror::Error;

const NCCL_PTR_HOST: i32 = 1;
const NCCL_PTR_CUDA: i32 = 2;

lazy_static! {
    static ref HANDLER_ALL: [KeyValue; 1] = [KeyValue::new("handler", "all")];
}

#[derive(Debug)]
pub struct NCCLNetProperties {
    pub name: String,
    pub pci_path: String,
    pub guid: u64,
    pub ptr_support: i32, // NCCL_PTR_HOST or NCCL_PTR_HOST|NCCL_PTR_CUDA
    pub speed: i32,       // Port speed in Mbps.
    pub port: i32,
    pub max_comms: i32,
}

#[derive(Debug)]
pub struct SocketHandle {
    pub addr: nix::sys::socket::SockAddr,
}

pub struct SocketListenComm {
    pub tcp_listener: Arc<Mutex<net::TcpListener>>,
}

// TODO: make Rotating communicator
#[derive(Clone)]
pub struct SocketSendComm {
    pub tcp_sender: Arc<std::thread::JoinHandle<()>>,
    pub msg_sender: flume::Sender<(Bytes, Arc<Mutex<(bool, usize)>>)>,
}

#[derive(Clone)]
pub struct SocketRecvComm {
    pub tcp_sender: Arc<std::thread::JoinHandle<()>>,
    pub msg_sender: flume::Sender<(&'static mut [u8], Arc<Mutex<(bool, usize)>>)>,
}

#[derive(Error, Debug)]
pub enum BaguaNetError {
    #[error("io error")]
    IOError(String),
    #[error("tcp error")]
    TCPError(String),
    #[error("inner error")]
    InnerError(String),
}

pub struct SocketSendRequest {
    pub state: Arc<Mutex<(bool, usize)>>,
    pub trace_span: opentelemetry::global::BoxedSpan,
}

pub struct SocketRecvRequest {
    pub state: Arc<Mutex<(bool, usize)>>,
    pub trace_span: opentelemetry::global::BoxedSpan,
}

pub enum SocketRequest {
    SendRequest(SocketSendRequest),
    RecvRequest(SocketRecvRequest),
}

static TELEMETRY_INIT_ONCE: std::sync::Once = std::sync::Once::new();
// static TELEMETRY_GUARD: Option<TelemetryGuard> = None;

struct AppState {
    exporter: opentelemetry_prometheus::PrometheusExporter,
    isend_nbytes_gauge: BoundValueRecorder<'static, u64>,
    irecv_nbytes_gauge: BoundValueRecorder<'static, u64>,
    uploader: std::thread::JoinHandle<()>,
}

type SocketListenCommID = usize;
type SocketSendCommID = usize;
type SocketRecvCommID = usize;
type SocketRequestID = usize;

pub struct BaguaNet {
    pub socket_devs: Vec<NCCLSocketDev>,
    pub listen_comm_next_id: usize,
    pub listen_comm_map: HashMap<SocketListenCommID, SocketListenComm>,
    pub send_comm_next_id: usize,
    pub send_comm_map: HashMap<SocketSendCommID, SocketSendComm>,
    pub recv_comm_next_id: usize,
    pub recv_comm_map: HashMap<SocketRecvCommID, SocketRecvComm>,
    pub socket_request_next_id: usize,
    pub socket_request_map: HashMap<SocketRequestID, SocketRequest>,
    pub trace_span_context: opentelemetry::Context,
    pub trace_on_flag: bool,
    pub rank: i32,
    state: Arc<AppState>,
}

impl BaguaNet {
    const DEFAULT_SOCKET_MAX_COMMS: i32 = 65536;
    const DEFAULT_LISTEN_BACKLOG: i32 = 16384;
    const NSTREAMS: usize = 1;

    pub fn new() -> Result<BaguaNet, BaguaNetError> {
        let rank: i32 = std::env::var("RANK")
            .unwrap_or("-1".to_string())
            .parse()
            .unwrap();
        TELEMETRY_INIT_ONCE.call_once(|| {
            if rank == -1 || rank > 7 {
                return;
            }

            let jaeger_addr = match std::env::var("BAGUA_NET_JAEGER_ADDRESS") {
                Ok(jaeger_addr) => {
                    tracing::info!("detected auto tuning server, connecting");
                    jaeger_addr
                }
                Err(_) => {
                    tracing::warn!("Jaeger server not detected.");
                    return;
                }
            };

            opentelemetry::global::set_text_map_propagator(opentelemetry_jaeger::Propagator::new());
            opentelemetry_jaeger::new_pipeline()
                .with_collector_endpoint(format!("http://{}/api/traces", jaeger_addr))
                .with_service_name("bagua-net")
                .install_batch(opentelemetry::runtime::AsyncStd)
                .unwrap();
        });

        let tracer = opentelemetry::global::tracer("bagua-net");
        let mut span = tracer.start(format!("BaguaNet-{}", rank));
        span.set_attribute(KeyValue::new(
            "socket_devs",
            format!("{:?}", utils::find_interfaces()),
        ));

        let prom_exporter = opentelemetry_prometheus::exporter()
            .with_default_histogram_boundaries(vec![16., 256., 4096., 65536., 1048576., 16777216.])
            .init();
        let meter = opentelemetry::global::meter("bagua-net");
        let state = Arc::new(AppState {
            exporter: prom_exporter.clone(),
            isend_nbytes_gauge: meter
                .u64_value_recorder("isend_nbytes")
                .init()
                .bind(HANDLER_ALL.as_ref()),
            irecv_nbytes_gauge: meter
                .u64_value_recorder("irecv_nbytes")
                .init()
                .bind(HANDLER_ALL.as_ref()),
            uploader: std::thread::spawn(move || {
                let prometheus_addr =
                    std::env::var("BAGUA_NET_PROMETHEUS_ADDRESS").unwrap_or_default();
                let (user, pass, address) = match utils::parse_user_pass_and_addr(&prometheus_addr)
                {
                    Some(ret) => ret,
                    None => return,
                };

                loop {
                    std::thread::sleep(std::time::Duration::from_micros(200));
                    let metric_families = prom_exporter.registry().gather();
                    match prometheus::push_metrics(
                        "BaguaNet",
                        prometheus::labels! { "rank".to_owned() => rank.to_string(), },
                        &address,
                        metric_families,
                        Some(prometheus::BasicAuthentication {
                            username: user.clone(),
                            password: pass.clone(),
                        }),
                    ) {
                        Ok(_) => {}
                        Err(err) => {
                            tracing::warn!("{:?}", err);
                        }
                    }
                }
            }),
        });

        Ok(Self {
            socket_devs: utils::find_interfaces(),
            listen_comm_next_id: 0,
            listen_comm_map: Default::default(),
            send_comm_next_id: 0,
            send_comm_map: Default::default(),
            recv_comm_next_id: 0,
            recv_comm_map: Default::default(),
            socket_request_next_id: 0,
            socket_request_map: Default::default(),
            trace_span_context: opentelemetry::Context::current_with_span(span),
            rank: rank,
            trace_on_flag: rank < 8,
            state: state,
        })
    }

    pub fn devices(&self) -> Result<usize, BaguaNetError> {
        Ok(self.socket_devs.len())
    }

    pub fn get_properties(&self, dev_id: usize) -> Result<NCCLNetProperties, BaguaNetError> {
        let socket_dev = &self.socket_devs[dev_id];

        Ok(NCCLNetProperties {
            name: socket_dev.interface_name.clone(),
            pci_path: socket_dev.pci_path.clone(),
            guid: dev_id as u64,
            ptr_support: NCCL_PTR_HOST,
            speed: utils::get_net_if_speed(&socket_dev.interface_name),
            port: 0,
            max_comms: BaguaNet::DEFAULT_SOCKET_MAX_COMMS,
        })
    }

    pub fn listen(
        &mut self,
        dev_id: usize,
    ) -> Result<(SocketHandle, SocketListenCommID), BaguaNetError> {
        let socket_dev = &self.socket_devs[dev_id];
        let addr = match socket_dev.addr.clone() {
            SockAddr::Inet(inet_addr) => inet_addr,
            others => {
                return Err(BaguaNetError::InnerError(format!(
                    "Got invalid socket address, which is {:?}",
                    others
                )))
            }
        };

        let socket = match Socket::new(
            match addr {
                InetAddr::V4(_) => Domain::IPV4,
                InetAddr::V6(_) => Domain::IPV6,
            },
            Type::STREAM,
            None,
        ) {
            Ok(sock) => sock,
            Err(err) => return Err(BaguaNetError::IOError(format!("{:?}", err))),
        };
        socket.bind(&addr.to_std().into()).unwrap();
        socket.listen(BaguaNet::DEFAULT_LISTEN_BACKLOG).unwrap();

        let listener: net::TcpListener = socket.into();
        let socket_addr = listener.local_addr().unwrap();
        let socket_handle = SocketHandle {
            addr: SockAddr::new_inet(InetAddr::from_std(&socket_addr)),
        };
        let id = self.listen_comm_next_id;
        self.listen_comm_next_id += 1;
        self.listen_comm_map.insert(
            id,
            SocketListenComm {
                tcp_listener: Arc::new(Mutex::new(listener)),
            },
        );

        Ok((socket_handle, id))
    }

    pub fn connect(
        &mut self,
        _dev_id: usize,
        socket_handle: SocketHandle,
    ) -> Result<SocketSendCommID, BaguaNetError> {
        let mut parallel_streams = Vec::new();
        for _ in 0..BaguaNet::NSTREAMS {
            let stream = match net::TcpStream::connect(socket_handle.addr.clone().to_str()) {
                Ok(stream) => stream,
                Err(err) => {
                    tracing::warn!(
                        "net::TcpStream::connect failed, err={:?}, socket_handle={:?}",
                        err,
                        socket_handle
                    );
                    return Err(BaguaNetError::TCPError(format!(
                        "socket_handle={:?}, err={:?}",
                        socket_handle, err
                    )));
                }
            };
            // stream.set_nodelay(true).unwrap();
            // stream.set_nonblocking(true).unwrap();

            parallel_streams.push(stream);
        }

        let (msg_sender, msg_receiver) = flume::unbounded();
        let id = self.send_comm_next_id;
        self.send_comm_next_id += 1;
        let metrics = self.state.clone();
        self.send_comm_map.insert(
            id,
            SocketSendComm {
                msg_sender: msg_sender,
                tcp_sender: Arc::new(std::thread::spawn(move || {
                    let mut stream_id = 0;
                    for (data, state) in msg_receiver.iter() {
                        let send_nbytes = data.len().to_be_bytes();

                        let mut stream = &mut parallel_streams[stream_id];
                        stream_id = (stream_id + 1) % BaguaNet::NSTREAMS;
                        // utils::nonblocking_write_all(&mut stream, &send_nbytes[..]).unwrap();
                        stream.write_all(&send_nbytes[..]).unwrap();
                        // utils::nonblocking_write_all(&mut stream, &data[..]).unwrap();
                        stream.write_all(&data[..]).unwrap();

                        metrics.isend_nbytes_gauge.record(data.len() as u64);
                        (*state.lock().unwrap()) = (true, data.len());
                    }
                })),
            },
        );

        Ok(id)
    }

    pub fn accept(
        &mut self,
        listen_comm_id: SocketListenCommID,
    ) -> Result<SocketRecvCommID, BaguaNetError> {
        let mut parallel_streams = Vec::new();
        for _ in 0..BaguaNet::NSTREAMS {
            let listen_comm = self.listen_comm_map.get(&listen_comm_id).unwrap();
            let (stream, _addr) = match listen_comm.tcp_listener.lock().unwrap().accept() {
                Ok(listen) => listen,
                Err(err) => {
                    return Err(BaguaNetError::TCPError(format!("{:?}", err)));
                }
            };
            // stream.set_nodelay(true).unwrap();
            // stream.set_nonblocking(true).unwrap();

            parallel_streams.push(stream);
        }

        let (msg_sender, msg_receiver) = flume::unbounded();
        let id = self.recv_comm_next_id;
        self.recv_comm_next_id += 1;
        let metrics = self.state.clone();
        self.recv_comm_map.insert(
            id,
            SocketRecvComm {
                msg_sender: msg_sender,
                tcp_sender: Arc::new(std::thread::spawn(move || {
                    let mut stream_id = 0;
                    for (data, state) in msg_receiver.iter() {
                        let mut stream = &mut parallel_streams[stream_id];
                        stream_id = (stream_id + 1) % BaguaNet::NSTREAMS;

                        let mut target_nbytes = data.len().to_be_bytes();
                        stream.read_exact(&mut target_nbytes[..]).unwrap();
                        // utils::nonblocking_read_exact(&mut stream, &mut target_nbytes[..]).unwrap();
                        let target_nbytes = usize::from_be_bytes(target_nbytes);
                        println!("target_nbytes={}", target_nbytes);
                        stream.read_exact(&mut data[..target_nbytes]).unwrap();
                        // utils::nonblocking_read_exact(&mut stream, &mut data[..target_nbytes])
                        //     .unwrap();
                        metrics.irecv_nbytes_gauge.record(target_nbytes as u64);
                        (*state.lock().unwrap()) = (true, target_nbytes);
                    }
                })),
            },
        );

        Ok(id)
    }

    pub fn isend(
        &mut self,
        send_comm_id: SocketSendCommID,
        data: &'static [u8],
    ) -> Result<SocketRequestID, BaguaNetError> {
        let tracer = opentelemetry::global::tracer("bagua-net");
        let mut span = tracer
            .span_builder(format!("isend-{}", send_comm_id))
            .with_parent_context(self.trace_span_context.clone())
            .start(&tracer);
        let send_comm = self.send_comm_map.get(&send_comm_id).unwrap();
        let id = self.socket_request_next_id;

        span.set_attribute(KeyValue::new("id", id as i64));
        span.set_attribute(KeyValue::new("nbytes", data.len() as i64));

        self.socket_request_next_id += 1;
        let task_state = Arc::new(Mutex::new((false, 0)));
        self.socket_request_map.insert(
            id,
            SocketRequest::SendRequest(SocketSendRequest {
                state: task_state.clone(),
                trace_span: span,
            }),
        );

        send_comm
            .msg_sender
            .send((Bytes::from_static(data), task_state))
            .unwrap();

        Ok(id)
    }

    pub fn irecv(
        &mut self,
        recv_comm_id: SocketRecvCommID,
        data: &'static mut [u8],
    ) -> Result<SocketRequestID, BaguaNetError> {
        let tracer = opentelemetry::global::tracer("bagua-net");
        let mut span = tracer
            .span_builder(format!("irecv-{}", recv_comm_id))
            .with_parent_context(self.trace_span_context.clone())
            .start(&tracer);
        let recv_comm = self.recv_comm_map.get(&recv_comm_id).unwrap();
        let id = self.socket_request_next_id;

        span.set_attribute(KeyValue::new("id", id as i64));

        self.socket_request_next_id += 1;
        let task_state = Arc::new(Mutex::new((false, 0)));
        self.socket_request_map.insert(
            id,
            SocketRequest::RecvRequest(SocketRecvRequest {
                state: task_state.clone(),
                trace_span: span,
            }),
        );

        recv_comm.msg_sender.send((data, task_state)).unwrap();

        Ok(id)
    }

    pub fn test(&mut self, request_id: SocketRequestID) -> Result<(bool, usize), BaguaNetError> {
        let request = self.socket_request_map.get_mut(&request_id).unwrap();
        let ret = match request {
            SocketRequest::SendRequest(send_req) => Ok(*send_req.state.lock().unwrap()),
            SocketRequest::RecvRequest(recv_req) => Ok(*recv_req.state.lock().unwrap()),
        };

        if let Ok(ret) = ret {
            if ret.0 {
                if self.trace_on_flag {
                    let request = self.socket_request_map.get_mut(&request_id).unwrap();
                    match request {
                        SocketRequest::SendRequest(send_req) => {
                            send_req.trace_span.end();
                        }
                        SocketRequest::RecvRequest(recv_req) => {
                            recv_req.trace_span.end();
                        }
                    };
                }

                self.socket_request_map.remove(&request_id).unwrap();
            }
        }

        ret
    }

    pub fn close_send(&mut self, send_comm_id: SocketSendCommID) -> Result<(), BaguaNetError> {
        self.send_comm_map.remove(&send_comm_id);

        Ok(())
    }

    pub fn close_recv(&mut self, recv_comm_id: SocketRecvCommID) -> Result<(), BaguaNetError> {
        self.recv_comm_map.remove(&recv_comm_id);

        Ok(())
    }

    pub fn close_listen(
        &mut self,
        listen_comm_id: SocketListenCommID,
    ) -> Result<(), BaguaNetError> {
        self.listen_comm_map.remove(&listen_comm_id);

        Ok(())
    }
}

impl Drop for BaguaNet {
    fn drop(&mut self) {
        // TODO: make shutdown global
        self.trace_span_context.span().end();
        opentelemetry::global::shutdown_tracer_provider();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::{InetAddr, IpAddr, SockAddr};

    #[test]
    fn it_works() {
        let bagua_net = BaguaNet::new().unwrap();
        println!("bagua_net.socket_devs={:?}", bagua_net.socket_devs);

        assert_eq!(2 + 2, 4);
    }

    #[test]
    fn test_socket_handle() {
        let addr = InetAddr::new(IpAddr::new_v4(127, 0, 0, 1), 8123);
        let socket_handle = SocketHandle {
            addr: SockAddr::new_inet(addr),
        };

        let addr = unsafe {
            let (c_sockaddr, _) = socket_handle.addr.as_ffi_pair();
            utils::from_libc_sockaddr(c_sockaddr).unwrap()
        };

        println!("socket_handle={:?}", addr.to_str());
    }
}
