extern crate aws_lambda_events;
#[macro_use]
extern crate failure;
extern crate graph_descriptions;
extern crate graph_generator_lib;
extern crate lambda_runtime as lambda;
#[macro_use]
extern crate lazy_static;
#[macro_use]
extern crate log;
extern crate regex;
extern crate rusoto_core;
extern crate rusoto_s3;
extern crate rusoto_sqs;
extern crate serde;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate simple_logger;
extern crate sqs_lambda;
extern crate uuid;

use std::collections::HashSet;
use std::io::Cursor;
use std::str::FromStr;

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use aws_lambda_events::event::sqs::SqsEvent;
use failure::Error;
use graph_descriptions::file::FileState;
use graph_descriptions::graph_description::*;
use graph_descriptions::network_connection::NetworkConnectionState;
use graph_descriptions::process::ProcessState;
use graph_descriptions::process_inbound_connection::ProcessInboundConnectionState;
use graph_descriptions::process_outbound_connection::ProcessOutboundConnectionState;
use lambda::Context;
use lambda::error::HandlerError;
use lambda::lambda;
use regex::Regex;
use rusoto_core::Region;
use rusoto_s3::S3Client;
use rusoto_sqs::SqsClient;
use serde::Deserialize;

use sqs_lambda::completion_event_serializer::CompletionEventSerializer;
use sqs_lambda::event_decoder::PayloadDecoder;
use sqs_lambda::event_emitter::S3EventEmitter;
use sqs_lambda::event_handler::{Completion, EventHandler, OutputEvent};
use sqs_lambda::event_processor::{EventProcessor, EventProcessorActor};
use sqs_lambda::event_retriever::S3PayloadRetriever;
use sqs_lambda::redis_cache::RedisCache;
use sqs_lambda::sqs_completion_handler::{CompletionPolicy, SqsCompletionHandler, SqsCompletionHandlerActor};
use sqs_lambda::sqs_consumer::{ConsumePolicy, SqsConsumer, SqsConsumerActor};

use async_trait::async_trait;

use crate::graph_descriptions::node::NodeT;

#[derive(Clone, Debug, Hash)]
pub enum GenericEvent {
    ProcessStart(ProcessStart),
    ProcessStop(ProcessStop),
    FileCreate(FileCreate),
    FileDelete(FileDelete),
    FileRead(FileRead),
    FileWrite(FileWrite),
    ProcessOutboundConnectionLog(ProcessOutboundConnectionLog),
    ProcessInboundConnectionLog(ProcessInboundConnectionLog),
    ProcessPortBindLog(ProcessPortBindLog),
}

use serde::de::Error as SerdeError;
use sqs_lambda::cache::{Cache, CacheResponse};

impl GenericEvent {
    fn from_value(raw_log: serde_json::Value) -> Result<GenericEvent, serde_json::Error> {
        let eventname = match raw_log
            .get("eventname")
            .and_then(|eventname| eventname.as_str())
            {
                Some(eventname) => eventname.to_owned(),
                None => return Err(serde_json::Error::custom("missing event_type")),
            };

        info!("Parsing log of type: {}", eventname);
        match &eventname[..] {
            "PROCESS_START" => {
                Ok(
                    GenericEvent::ProcessStart(serde_json::from_value(raw_log)?)
                )
            },
            "PROCESS_STOP" => {
                Ok(
                    GenericEvent::ProcessStop(serde_json::from_value(raw_log)?)
                )
            }
            "FILE_CREATE" => {
                Ok(
                    GenericEvent::FileCreate(serde_json::from_value(raw_log)?)
                )
            }
            "FILE_DELETE" => {
                Ok(
                    GenericEvent::FileDelete(serde_json::from_value(raw_log)?)
                )
            }
            "FILE_READ" => {
                Ok(
                    GenericEvent::FileRead(serde_json::from_value(raw_log)?)
                )
            }
            "FILE_WRITE" => {
                Ok(
                    GenericEvent::FileWrite(serde_json::from_value(raw_log)?)
                )
            }
            "OUTBOUND_TCP" => {
                Ok(
                    GenericEvent::ProcessOutboundConnectionLog(serde_json::from_value(raw_log)?)
                )
            }
            "INBOUND_TCP" => {
                Ok(
                    GenericEvent::ProcessInboundConnectionLog(serde_json::from_value(raw_log)?)
                )
            }
            e => Err(serde_json::Error::custom(format!("Invalid event type: {}", e))),
        }
    }
}


#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct ProcessStart {
    process_id: u64,
    parent_process_id: u64,
    name: String,
    hostname: String,
    arguments: String,
    timestamp: u64,
    exe: Option<String>,
    eventname: String,
}

#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct ProcessStop {
    process_id: u64,
    name: String,
    hostname: String,
    timestamp: u64,
    eventname: String,
}

#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct FileCreate {
    creator_process_id: u64,
    creator_process_name: Option<String>,
    path: String,
    hostname: String,
    timestamp: u64,
    eventname: String,
}

#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct FileDelete {
    deleter_process_id: u64,
    deleter_process_name: Option<String>,
    path: String,
    hostname: String,
    timestamp: u64,
    eventname: String,
}

#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct FileRead {
    reader_process_id: u64,
    reader_process_name: Option<String>,
    path: String,
    hostname: String,
    timestamp: u64,
    eventname: String,
}

#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct FileWrite {
    writer_pid: u64,
    writer_process_name: Option<String>,
    path: String,
    hostname: String,
    timestamp: u64,
    eventname: String,
}

#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct ProcessOutboundConnectionLog {
    pid: u64,
    protocol: String,
    src_port: u32,
    dst_port: u32,
    src_hostname: String,
    src_ip_addr: String,
    dst_ip_addr: String,
    timestamp: u64,
    eventname: String,
}

// In an inbound connection "src" is where the connection is coming from
#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct ProcessInboundConnectionLog {
    /// The pid of the process receiving the connection
    pid: u64,
    src_ip_addr: String,
    src_port: u32,
    dst_port: u32,
    dst_hostname: String,
    dst_ip_addr: String,
    protocol: String,
    timestamp: u64,
    eventname: String,
}

#[derive(Clone, Debug, Hash, Serialize, Deserialize)]
pub struct ProcessPortBindLog {
    pid: u64,
    bound_port: u64,
    hostname: String,
    timestamp: u64,
    eventname: String,
}

fn is_internal_ip(ip: &str) -> bool {
    lazy_static!(
        static ref RE: Regex = Regex::new(
            r"/(^127\.)|(^192\.168\.)|(^10\.)|(^172\.1[6-9]\.)|(^172\.2[0-9]\.)|(^172\.3[0-1]\.)|(^::1$)|(^[fF][cCdD])/"
        ).expect("is_internal_ip regex");
    );

    RE.is_match(ip)
}

fn handle_outbound_traffic(conn_log: ProcessOutboundConnectionLog) -> Graph {
    let mut graph = Graph::new(conn_log.timestamp);

    let asset = AssetBuilder::default()
        .asset_id(conn_log.src_hostname.clone())
        .hostname(conn_log.src_hostname.clone())
        .build()
        .expect("outbound_traffic.asset");

    // A process creates an outbound connection to dst_port
    let process = ProcessBuilder::default()
        .asset_id(conn_log.src_hostname.clone())
        .state(ProcessState::Existing)
        .process_id(conn_log.pid)
        .last_seen_timestamp(conn_log.timestamp)
        .build()
        .expect("outbound_traffic.process");

    let outbound = ProcessOutboundConnectionBuilder::default()
        .asset_id(conn_log.src_hostname.clone())
        .ip_address(conn_log.src_ip_addr.clone())
        .protocol(conn_log.protocol.clone())
        .state(ProcessOutboundConnectionState::Connected)
        .port(conn_log.src_port)
        .created_timestamp(conn_log.timestamp)
        .build()
        .expect("outbound_traffic.inbound");

    let src_ip = IpAddressBuilder::default()
        .ip_address(conn_log.src_ip_addr.clone())
        .last_seen_timestamp(conn_log.timestamp)
        .build()
        .expect("outbound_traffic.dst_ip");

    let dst_ip = IpAddressBuilder::default()
        .ip_address(conn_log.dst_ip_addr.clone())
        .last_seen_timestamp(conn_log.timestamp)
        .build()
        .expect("outbound_traffic.src_ip");

    let src_port = IpPortBuilder::default()
        .ip_address(conn_log.src_ip_addr.clone())
        .port(conn_log.src_port)
        .protocol(conn_log.protocol.clone())
        .build()
        .expect("outbound_traffic.src_port");

    let dst_port = IpPortBuilder::default()
        .ip_address(conn_log.dst_ip_addr.clone())
        .port(conn_log.dst_port)
        .protocol(conn_log.protocol.clone())
        .build()
        .expect("outbound_traffic.dst_port");

    let network_connection = NetworkConnectionBuilder::default()
        .state(NetworkConnectionState::Created)
        .src_ip_address(conn_log.src_ip_addr)
        .src_port(conn_log.src_port)
        .dst_ip_address(conn_log.dst_ip_addr)
        .dst_port(conn_log.dst_port)
        .protocol(conn_log.protocol)
        .created_timestamp(conn_log.timestamp)
        .build()
        .expect("outbound_traffic.network_connection");

    // An asset is assigned an IP
    graph.add_edge(
        "asset_ip",
        asset.clone_node_key(),
        src_ip.clone_node_key(),
    );

    // A process spawns on an asset
    graph.add_edge(
        "asset_processes",
        asset.clone_node_key(),
        process.clone_node_key(),
    );

    // A process creates a connection
    graph.add_edge(
        "created_connections",
        process.clone_node_key(),
        outbound.clone_node_key(),
    );

    // The connection is over an IP + Port
    graph.add_edge(
        "connected_over",
        outbound.clone_node_key(),
        src_port.clone_node_key(),
    );

    // The connection is to a dst ip + port
    graph.add_edge(
        "connected_to",
        outbound.clone_node_key(),
        dst_port.clone_node_key(),
    );

    // There is a network connection between the src and dst ports
    graph.add_edge(
        "outbound_connection_to",
        src_port.clone_node_key(),
        network_connection.clone_node_key(),
    );

    graph.add_edge(
        "inbound_connection_to",
        network_connection.clone_node_key(),
        dst_port.clone_node_key(),
    );

    graph.add_node(asset);
    graph.add_node(process);
    graph.add_node(outbound);
    graph.add_node(src_ip);
    graph.add_node(dst_ip);
    graph.add_node(src_port);
    graph.add_node(dst_port);
    graph.add_node(network_connection);

    graph
}

fn handle_inbound_traffic(conn_log: ProcessInboundConnectionLog) -> Graph {
    let mut graph = Graph::new(conn_log.timestamp);

    let asset = AssetBuilder::default()
        .asset_id(conn_log.dst_hostname.clone())
        .hostname(conn_log.dst_hostname.clone())
        .build()
        .expect("inbound_traffic.asset");

    // A process creates an outbound connection to dst_port
    let process = ProcessBuilder::default()
        .asset_id(conn_log.dst_hostname.clone())
        .state(ProcessState::Existing)
        .process_id(conn_log.pid)
        .last_seen_timestamp(conn_log.timestamp)
        .build()
        .expect("inbound_traffic.process");

    let inbound = ProcessInboundConnectionBuilder::default()
        .asset_id(conn_log.dst_hostname.clone())
        .state(ProcessInboundConnectionState::Existing)
        .ip_address(conn_log.dst_ip_addr.clone())
        .protocol(conn_log.protocol.clone())
        .port(conn_log.dst_port)
        .created_timestamp(conn_log.timestamp)
        .build()
        .expect("inbound_traffic.inbound");

    let dst_ip = IpAddressBuilder::default()
        .ip_address(conn_log.dst_ip_addr.clone())
        .last_seen_timestamp(conn_log.timestamp)
        .build()
        .expect("inbound_traffic.dst_ip");

    let src_ip = IpAddressBuilder::default()
        .ip_address(conn_log.src_ip_addr.clone())
        .last_seen_timestamp(conn_log.timestamp)
        .build()
        .expect("inbound_traffic.src_ip");

    let src_port = IpPortBuilder::default()
        .ip_address(conn_log.src_ip_addr.clone())
        .port(conn_log.src_port)
        .build()
        .expect("inbound_traffic.src_port");

    let dst_port = IpPortBuilder::default()
        .ip_address(conn_log.dst_ip_addr.clone())
        .port(conn_log.dst_port)
        .build()
        .expect("inbound_traffic.dst_port");

    let network_connection = NetworkConnectionBuilder::default()
        .state(NetworkConnectionState::Created)
        .src_ip_address(conn_log.src_ip_addr)
        .src_port(conn_log.src_port)
        .dst_ip_address(conn_log.dst_ip_addr)
        .dst_port(conn_log.dst_port)
        .protocol(conn_log.protocol)
        .created_timestamp(conn_log.timestamp)
        .build()
        .expect("inbound_traffic.network_connection");

    // An asset is assigned an IP
    graph.add_edge(
        "asset_ip",
        asset.clone_node_key(),
        dst_ip.clone_node_key(),
    );

    // A process spawns on an asset
    graph.add_edge(
        "asset_processes",
        asset.clone_node_key(),
        process.clone_node_key(),
    );

    // A process creates a connection
    graph.add_edge(
        "received_connection",
        process.clone_node_key(),
        inbound.clone_node_key(),
    );

    // The connection is over an IP + Port
    graph.add_edge(
        "bound_port",
        inbound.clone_node_key(),
        src_port.clone_node_key(),
    );

    // The connection is to a dst ip + port
    graph.add_edge(
        "connected_to",
        inbound.clone_node_key(),
        dst_port.clone_node_key(),
    );

    // There is a network connection between the src and dst ports
    graph.add_edge(
        "outbound_connection_to",
        src_port.clone_node_key(),
        network_connection.clone_node_key(),
    );

    graph.add_edge(
        "inbound_connection_to",
        network_connection.clone_node_key(),
        dst_port.clone_node_key(),
    );

    graph.add_node(asset);
    graph.add_node(process);
    graph.add_node(inbound);
    graph.add_node(dst_ip);
    graph.add_node(src_ip);
    graph.add_node(src_port);
    graph.add_node(dst_port);
    graph.add_node(network_connection);

    graph
}

fn handle_process_start(process_start: ProcessStart) -> Graph {
    let mut graph = Graph::new(process_start.timestamp);

    let asset = AssetBuilder::default()
        .asset_id(process_start.hostname.clone())
        .hostname(process_start.hostname.clone())
        .build()
        .unwrap();

    let parent = ProcessBuilder::default()
        .hostname(process_start.hostname.clone())
        .state(ProcessState::Existing)
        .process_id(process_start.parent_process_id)
        .last_seen_timestamp(process_start.timestamp)
        .build()
        .unwrap();

    let child = ProcessBuilder::default()
        .hostname(process_start.hostname.clone())
        .process_name(process_start.name)
        .state(ProcessState::Created)
        .process_id(process_start.process_id)
        .created_timestamp(process_start.timestamp)
        .build()
        .unwrap();

    if let Some(exe_path) = process_start.exe {
        let child_exe = FileBuilder::default()
            .hostname(process_start.hostname)
            .state(FileState::Existing)
            .last_seen_timestamp(process_start.timestamp)
            .file_path(exe_path)
            .build()
            .unwrap();

        graph.add_edge("bin_file", child.clone_node_key(), child_exe.clone_node_key());
        info!("child_exe: {}", child_exe.clone().into_json());
        graph.add_node(child_exe);
    }

    graph.add_edge(
        "asset_processes",
        asset.clone_node_key(),
        parent.clone_node_key(),
    );

    graph.add_edge(
        "asset_processes",
        asset.clone_node_key(),
        child.clone_node_key(),
    );

    graph.add_edge("children", parent.clone_node_key(), child.clone_node_key());
    graph.add_node(parent);
    graph.add_node(child);

    graph
}


fn handle_process_stop(process_stop: ProcessStop) -> Graph {
    let terminated_process = ProcessBuilder::default()
        .process_name(process_stop.name)
        .hostname(process_stop.hostname)
        .state(ProcessState::Terminated)
        .process_id(process_stop.process_id)
        .terminated_timestamp(process_stop.timestamp)
        .build()
        .unwrap();

    let mut graph = Graph::new(process_stop.timestamp);
    graph.add_node(terminated_process);

    graph
}


fn handle_file_delete(file_delete: FileDelete) -> Graph {
    let deleter = ProcessBuilder::default()
        .hostname(file_delete.hostname.clone())
        .state(ProcessState::Existing)
        .process_name(file_delete.deleter_process_name.unwrap_or_default())
        .process_id(file_delete.deleter_process_id)
        .last_seen_timestamp(file_delete.timestamp)
        .build()
        .unwrap();

    let file = FileBuilder::default()
        .hostname(file_delete.hostname)
        .state(FileState::Deleted)
        .deleted_timestamp(file_delete.timestamp)
        .file_path(file_delete.path)
        .build()
        .unwrap();

    let mut graph = Graph::new(file_delete.timestamp);

    graph.add_edge("deleted", deleter.clone_node_key(), file.clone_node_key());
    graph.add_node(deleter);
    graph.add_node(file);

    graph
}


fn handle_file_create(file_creator: FileCreate) -> Graph {
    let creator = ProcessBuilder::default()
        .hostname(file_creator.hostname.clone())
        .process_name(file_creator.creator_process_name.unwrap_or_default())
        .state(ProcessState::Existing)
        .process_id(file_creator.creator_process_id)
        .last_seen_timestamp(file_creator.timestamp)
        .build()
        .unwrap();

    let file = FileBuilder::default()
        .hostname(file_creator.hostname)
        .state(FileState::Created)
        .created_timestamp(file_creator.timestamp)
        .file_path(file_creator.path)
        .build()
        .unwrap();

    info!("file {}", file.clone().into_json());

    let mut graph = Graph::new(file_creator.timestamp);

    graph.add_edge("created_files", creator.clone_node_key(), file.clone_node_key());
    graph.add_node(creator);
    graph.add_node(file);

    graph
}

fn handle_file_write(file_write: FileWrite) -> Graph {
    let deleter = ProcessBuilder::default()
        .process_name(file_write.writer_process_name.unwrap_or_default())
        .hostname(file_write.hostname.clone())
        .state(ProcessState::Existing)
        .process_id(file_write.writer_pid)
        .last_seen_timestamp(file_write.timestamp)
        .build()
        .unwrap();

    let file = FileBuilder::default()
        .hostname(file_write.hostname)
        .state(FileState::Existing)
        .last_seen_timestamp(file_write.timestamp)
        .file_path(file_write.path)
        .build()
        .unwrap();

    let mut graph = Graph::new(file_write.timestamp);

    graph.add_edge("wrote_files", deleter.clone_node_key(), file.clone_node_key());
    graph.add_node(deleter);
    graph.add_node(file);

    graph
}

fn handle_file_read(file_read: FileRead) -> Graph {
    let deleter = ProcessBuilder::default()
        .process_name(file_read.reader_process_name.unwrap_or_default())
        .hostname(file_read.hostname.clone())
        .state(ProcessState::Existing)
        .process_id(file_read.reader_process_id)
        .last_seen_timestamp(file_read.timestamp)
        .build()
        .unwrap();

    let file = FileBuilder::default()
        .hostname(file_read.hostname)
        .state(FileState::Existing)
        .last_seen_timestamp(file_read.timestamp)
        .file_path(file_read.path)
        .build()
        .unwrap();

    let mut graph = Graph::new(file_read.timestamp);

    graph.add_edge("read_files", deleter.clone_node_key(), file.clone_node_key());
    graph.add_node(deleter);
    graph.add_node(file);

    graph
}

fn handle_log(generic_event: GenericEvent) -> Result<Graph, Error> {
    match generic_event {
        GenericEvent::ProcessStart(event) => {
            Ok(handle_process_start(event))
        }
        GenericEvent::ProcessStop(event) => {
            Ok(handle_process_stop(event))
        }
        GenericEvent::FileCreate(event) => {
            Ok(handle_file_create(event))
        }
        GenericEvent::FileDelete(event) => {
            Ok(handle_file_delete(event))
        }
        GenericEvent::FileRead(event) => {
            Ok(handle_file_read(event))
        }
        GenericEvent::FileWrite(event) => {
            Ok(handle_file_write(event))
        }
        GenericEvent::ProcessOutboundConnectionLog(event) => {
            Ok(handle_outbound_traffic(event))
        }
        GenericEvent::ProcessInboundConnectionLog(event) => {
            Ok(handle_inbound_traffic(event))
        }
        GenericEvent::ProcessPortBindLog(_event) => unimplemented!()
    }
}


#[derive(Clone, Debug, Default)]
pub struct SubgraphSerializer {
    proto: Vec<u8>,
}

impl CompletionEventSerializer for SubgraphSerializer {
    type CompletedEvent = GeneratedSubgraphs;
    type Output = Vec<u8>;
    type Error = failure::Error;

    fn serialize_completed_events(
        &mut self,
        completed_events: &[Self::CompletedEvent],
    ) -> Result<Vec<Self::Output>, Self::Error> {
        let mut subgraph = Graph::new(
            0
        );

        let mut pre_nodes = 0;
        let mut pre_edges = 0;
        for completed_event in completed_events {
            for sg in completed_event.subgraphs.iter() {
                pre_nodes += sg.nodes.len();
                pre_edges += sg.edges.len();
                subgraph.merge(sg);
            }
        }

        if subgraph.is_empty() {
            warn!(
                concat!(
                "Output subgraph is empty. Serializing to empty vector.",
                "pre_nodes: {} pre_edges: {}"
                ),
                pre_nodes,
                pre_edges,
            );
            return Ok(vec![]);
        }

        info!(
            "Serializing {} nodes {} edges. Down from {} nodes {} edges.",
            subgraph.nodes.len(),
            subgraph.edges.len(),
            pre_nodes,
            pre_edges,
        );

        let subgraphs = GeneratedSubgraphs { subgraphs: vec![subgraph] };

        self.proto.clear();

        prost::Message::encode(&subgraphs, &mut self.proto)?;


        let mut compressed = Vec::with_capacity(self.proto.len());
        let mut proto = Cursor::new(&self.proto);
        zstd::stream::copy_encode(&mut proto, &mut compressed, 4)?;

        Ok(vec![compressed])
    }
}

fn time_based_key_fn(_event: &[u8]) -> String {
    info!("event length {}", _event.len());
    let cur_ms = match SystemTime::now().duration_since(UNIX_EPOCH) {
        Ok(n) => n.as_millis(),
        Err(_) => panic!("SystemTime before UNIX EPOCH!"),
    };

    let cur_day = cur_ms - (cur_ms % 86400);

    format!(
        "{}/{}-{}",
        cur_day, cur_ms, uuid::Uuid::new_v4()
    )
}

fn map_sqs_message(event: aws_lambda_events::event::sqs::SqsMessage) -> rusoto_sqs::Message {
    rusoto_sqs::Message {
        attributes: Some(event.attributes),
        body: event.body,
        md5_of_body: event.md5_of_body,
        md5_of_message_attributes: event.md5_of_message_attributes,
        message_attributes: None,
        message_id: event.message_id,
        receipt_handle: event.receipt_handle,
    }
}

#[derive(Debug, Clone, Default)]
pub struct ZstdJsonDecoder;

impl<D> PayloadDecoder<D> for ZstdJsonDecoder
    where for<'a> D: Deserialize<'a>
{
    fn decode(&mut self, body: Vec<u8>) -> Result<D, Box<dyn std::error::Error>>
    {
        let mut decompressed = Vec::new();

        let mut body = Cursor::new(&body);

        zstd::stream::copy_decode(&mut body, &mut decompressed)?;

        Ok(serde_json::from_slice(&decompressed)?)
    }
}


#[derive(Clone)]
struct GenericSubgraphGenerator {
    cache: RedisCache,
}


#[async_trait]
impl EventHandler for GenericSubgraphGenerator
{
    type InputEvent = Vec<serde_json::Value>;
    type OutputEvent = GeneratedSubgraphs;
    type Error = Arc<failure::Error>;

    async fn handle_event(&mut self, events: Vec<serde_json::Value>) -> OutputEvent<Self::OutputEvent, Self::Error> {
        let mut failed: Option<failure::Error> = None;
        let mut final_subgraph = Graph::new(0);
        let mut identities = Vec::with_capacity(events.len());

        for event in events {
            let event = match GenericEvent::from_value(event) {
                Ok(event) => event,
                Err(e) => {
                    error!("Failed to generate subgraph with: {}", e);
                    failed = Some(e.into());
                    continue;
                }
            };

            let identity = event.clone();

            if let Ok(CacheResponse::Hit) = self.cache.get(identity.clone()).await {
                 continue
            }

            let res = handle_log(event);
            let subgraph = match res {
                Ok(subgraph) => subgraph,
                Err(e) => {
                    error!("Failed to generate subgraph with: {}", e);
                    failed = Some(e);
                    continue;
                }
            };
            identities.push(identity);
            final_subgraph.merge(&subgraph);
        }


        let mut completed = if let Some(e) = failed {
            OutputEvent::new(
                Completion::Partial(
                    (
                        GeneratedSubgraphs::new(vec![final_subgraph]),
                        Arc::new(
                            e
                        )
                    )
                )
            )
        } else {
            OutputEvent::new(Completion::Total(GeneratedSubgraphs::new(vec![final_subgraph])))
        };

        identities.into_iter().for_each(|identity| completed.add_identity(identity));

        completed
    }
}

fn handler(event: SqsEvent, ctx: Context) -> Result<(), HandlerError> {
    info!("Handling event");

    let mut initial_events: HashSet<String> = event.records
        .iter()
        .map(|event| event.message_id.clone().unwrap())
        .collect();

    info!("Initial Events {:?}", initial_events);

    let (tx, rx) = std::sync::mpsc::sync_channel(10);


    std::thread::spawn(move || {
        tokio_compat::run_std(
            async move {
                let queue_url = std::env::var("QUEUE_URL").expect("QUEUE_URL");
                info!("Queue Url: {}", queue_url);
                let bucket_prefix = std::env::var("BUCKET_PREFIX").expect("BUCKET_PREFIX");
                let cache_address = {
                    let generic_event_cache_addr = std::env::var("GENERIC_EVENT_CACHE_ADDR").expect("GENERIC_EVENT_CACHE_ADDR");
                    let generic_event_cache_port = std::env::var("GENERIC_EVENT_CACHE_PORT").expect("GENERIC_EVENT_CACHE_PORT");

                    format!(
                        "{}:{}",
                        generic_event_cache_addr,
                        generic_event_cache_port,
                    )
                };

                let bucket = bucket_prefix + "-unid-subgraphs-generated-bucket";
                info!("Output events to: {}", bucket);
                let region = {
                    let region_str = std::env::var("AWS_REGION").expect("AWS_REGION");
                    Region::from_str(&region_str).expect("Region error")
                };

                let cache = RedisCache::new(cache_address.to_owned()).await.expect("Could not create redis client");
                let node_identifier = GenericSubgraphGenerator { cache: cache.clone() };

                info!("SqsCompletionHandler");

                let finished_tx = tx.clone();
                let sqs_completion_handler = SqsCompletionHandlerActor::new(
                    SqsCompletionHandler::new(
                        SqsClient::new(region.clone()),
                        queue_url.to_string(),
                        SubgraphSerializer { proto: Vec::with_capacity(1024) },
                        S3EventEmitter::new(
                            S3Client::new(region.clone()),
                            bucket.to_owned(),
                            time_based_key_fn,
                        ),
                        CompletionPolicy::new(
                            1000, // Buffer up to 1000 messages
                            Duration::from_secs(120), // Buffer for up to 30 seconds
                        ),
                        move |_self_actor, result: Result<String, String>| {
                            match result {
                                Ok(worked) => {
                                    info!("Handled an event, which was successfully deleted: {}", &worked);
                                    tx.send(worked).unwrap();
                                }
                                Err(worked) => {
                                    info!("Handled an initial_event, though we failed to delete it: {}", &worked);
                                    tx.send(worked).unwrap();
                                }
                            }
                        },
                    )
                );


                info!("Defining consume policy");
                let consume_policy = ConsumePolicy::new(
                    ctx, // Use the Context.deadline from the lambda_runtime
                    Duration::from_secs(10), // Stop consuming when there's 2 seconds left in the runtime
                    3, // If we get 3 empty receives in a row, stop consuming
                );

                info!("Defining consume policy");
                let (shutdown_tx, shutdown_notify) = tokio::sync::oneshot::channel();

                info!("SqsConsumer");
                let sqs_consumer = SqsConsumerActor::new(
                    SqsConsumer::new(
                        SqsClient::new(region.clone()),
                        queue_url.clone(),
                        consume_policy,
                        sqs_completion_handler.clone(),
                        shutdown_tx,
                    )
                );

                info!("EventProcessors");
                let event_processors: Vec<_> = (0..10)
                    .map(|_| {
                        EventProcessorActor::new(EventProcessor::new(
                            sqs_consumer.clone(),
                            sqs_completion_handler.clone(),
                            node_identifier.clone(),
                            S3PayloadRetriever::new(S3Client::new(region.clone()), ZstdJsonDecoder {}),
                            cache.clone(),
                        ))
                    })
                    .collect();

                info!("Start Processing");

                futures::future::join_all(event_processors.iter().map(|ep| ep.start_processing())).await;

                let mut proc_iter = event_processors.iter().cycle();
                for event in event.records {
                    let next_proc = proc_iter.next().unwrap();
                    next_proc.process_event(
                        map_sqs_message(event)
                    ).await;
                }

                info!("Waiting for shutdown notification");

                // Wait for the consumers to shutdown
                let _ = shutdown_notify.await;
                info!("Consumer shutdown");
                finished_tx.send("Completed".to_owned()).unwrap();
            });
    });

    info!("Checking acks");
    for r in &rx {
        info!("Acking event: {}", &r);
        initial_events.remove(&r);
        if r == "Completed" {
            let r = rx.recv_timeout(Duration::from_millis(100));
            if let Ok(r) = r {
                initial_events.remove(&r);
            }
            // If we're done go ahead and try to clear out any remaining
            while let Ok(r) = rx.try_recv() {
                initial_events.remove(&r);
            }
            break;
        }
    }

    info!("Completed execution");

    if initial_events.is_empty() {
        info!("Successfully acked all initial events");
        Ok(())
    } else {
        Err(lambda::error::HandlerError::from("Failed to ack all initial events"))
    }
}


//fn my_handler(event: SqsEvent, ctx: Context) -> Result<(), HandlerError> {
//    let region = {
//        let region_str = std::env::var("AWS_REGION").expect("AWS_REGION");
//        Region::from_str(&region_str).expect("Invalid Region")
//    };
//    info!("Creating sqs_client");
//    let sqs_client = Arc::new(SqsClient::new(region.clone()));
//
//    info!("Creating s3_client");
//    let s3_client = Arc::new(S3Client::new(region.clone()));
//
//    info!("Creating retriever");
//    let retriever = S3EventRetriever::new(
//        s3_client.clone(),
//        |d| {
//            info!("Parsing: {:?}", d);
//            events_from_s3_sns_sqs(d)
//        },
//        ZstdJsonDecoder {
//            buffer: Vec::with_capacity(1 << 8),
//        },
//    );
//
//    let queue_url = std::env::var("QUEUE_URL").expect("QUEUE_URL");
//
//    info!("Creating sqs_completion_handler");
//    let sqs_completion_handler = BlockingSqsCompletionHandler::new(sqs_client, queue_url);
//
//    let handler = GenericSubgraphGenerator { s3: s3_client };
//
//    let mut sqs_service = SqsService::new(retriever, handler, sqs_completion_handler);
//
//    info!("Handing off event");
//    sqs_service.run(event, ctx)?;
//
//    Ok(())
//}

fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();
    lambda!(handler);
}
