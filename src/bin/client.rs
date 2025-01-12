use anyhow::{Context, Result};
use clap::Parser;
use ffmpeg::segment::extract_non_video_streams;
use futures::stream::{FuturesUnordered, StreamExt};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tracing::{debug, error, info, instrument, warn};
use video_encoding_system::chunk::{convert_files_to_chunks, verify_binaries};
use video_encoding_system::ffmpeg;

pub mod video_encoding {
    tonic::include_proto!("video_encoding");
}

use video_encoding::video_encoding_service_client::VideoEncodingServiceClient;
use video_encoding::EncodeChunkRequest;
use video_encoding_system::chunk::{split_video, Chunk};
use video_encoding_system::config::create_temp_config;
use video_encoding_system::ffmpeg::concat::concatenate_videos_and_copy_streams;
use video_encoding_system::logging::init_logging;
use video_encoding_system::settings::Settings;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024 * 1024; // 1 GB

/// CLI arguments for the video encoding client
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Input video file path
    #[arg(short, long)]
    input_file: PathBuf,

    /// Output video file path
    #[arg(short, long)]
    output_file: String,

    /// Path to the configuration file
    #[arg(long)]
    config_file: Option<PathBuf>,

    /// List of node addresses
    #[arg(short, long)]
    nodes: Vec<String>,

    /// List of slot numbers corresponding to each node
    #[arg(long)]
    slots: Vec<usize>,

    /// Encoder parameters, that include encoder and parameters for it
    #[arg(long)]
    encoder_params: Option<Vec<String>>,

    /// Determines method used for concatenating encoded chunks and audio into output file
    #[arg(short, long, default_value = "ffmpeg", value_parser(["ffmpeg", "mkvmerge"]))]
    concat: String,

    /// Temporary directory for processing
    #[arg(long)]
    temp_dir: Option<PathBuf>,

    /// Duration of each video segment in seconds
    #[arg(long)]
    segment_duration: Option<f64>,
}

/// Represents a node connection with its processing capacity
#[derive(Clone)]
struct NodeConnection {
    client: VideoEncodingServiceClient<tonic::transport::Channel>,
    address: String,
    semaphore: Arc<Semaphore>,
}

/// Represents the state of the encoding process
struct EncodingState {
    /// Chunks waiting to be encoded
    pending_chunks: Vec<Chunk>,
    /// Chunks that have been successfully encoded
    completed_chunks: Vec<Chunk>,
}

#[tokio::main]
#[instrument]
async fn main() -> Result<()> {
    init_logging();
    info!("Starting video encoding client");

    let cli = Cli::parse();
    debug!("CLI arguments: {:?}", cli);

    let settings = load_settings(&cli)?;
    verify_binaries(&cli.concat)?;

    let config = create_temp_config(&settings, &cli.input_file, &cli.output_file);

    let nodes = initialize_nodes(&settings.client.node_addresses, &cli.slots).await?;

    let segments = split_video(
        &cli.input_file,
        settings.processing.segment_duration,
        &config.segment_dir(),
        &settings.client.encoder_params,
        &config.encode_dir(),
    )?;

    let non_video_streams = extract_non_video_streams(&cli.input_file, &config.temp_dir)?;

    let chunks = convert_files_to_chunks(segments, settings.client.encoder_params)?;

    info!("Created {} chunks from segments", chunks.len());

    // Initializing client state
    let encoding_state = Arc::new(Mutex::new(EncodingState {
        pending_chunks: chunks,
        completed_chunks: Vec::new(),
    }));

    let mut futures = FuturesUnordered::new();

    // Start encoding tasks for each node
    for node in nodes {
        let state_clone = Arc::clone(&encoding_state);
        futures.push(tokio::spawn(encode_chunks_on_node(node, state_clone)));
    }

    // Wait for all encoding tasks to complete
    while let Some(result) = futures.next().await {
        if let Err(e) = result {
            error!("node task failed: {}", e);
        }
    }

    let encoding_state = encoding_state.lock().await;
    let mut encoded_chunks = encoding_state.completed_chunks.clone();
    encoded_chunks.sort_by_key(|chunk| chunk.index);

    if encoded_chunks.len()
        != encoding_state.pending_chunks.len() + encoding_state.completed_chunks.len()
    {
        warn!("Some chunks were not encoded successfully");
    }

    info!("Concatenating encoded chunks");

    let encoded_paths: Vec<PathBuf> = encoded_chunks
        .iter()
        .map(|chunk| chunk.encoded_path.clone().unwrap())
        .collect();

    concatenate_videos_and_copy_streams(
        encoded_paths,
        &non_video_streams,
        &PathBuf::from(&cli.output_file),
        &config.temp_dir,
        encoded_chunks.len(),
        &cli.concat,
    )?;

    info!("Video encoding completed successfully");

    // Remove temp config folder recursively
    config.delete()?;

    Ok(())
}

// Loads settings from the configuration file or creates default settings
#[instrument]
fn load_settings(cli: &Cli) -> Result<Settings> {
    let mut settings = cli
        .config_file
        .as_ref()
        .map(|path| Settings::from_file(path))
        .unwrap_or_else(|| Settings::new())?;

    if !cli.nodes.is_empty() {
        settings.client.node_addresses = cli.nodes.clone();
    }

    // We get Vec of single string from cli, and process it into multiple arguments
    // that will be used later
    if let Some(encoder_params) = &cli.encoder_params {
        // This is ugly but we can pass a lot of encoders and settings this way
        let mut params: Vec<String> = vec![];
        encoder_params.into_iter().for_each(|x| {
            params.extend(
                x.split(' ')
                    .map(|f| f.trim().to_string())
                    .filter(|c| c != "")
                    .collect::<Vec<String>>(),
            )
        });
        // this ensures we don't have issues with overwriting
        params.push("-y".to_string());
        settings.client.encoder_params = params;
    }

    if let Some(temp_dir) = &cli.temp_dir {
        settings.processing.temp_dir = temp_dir.clone();
    }

    if let Some(segment_duration) = cli.segment_duration {
        settings.processing.segment_duration = segment_duration;
    }

    Ok(settings)
}

/// Initialize connections to all provided node addresses with their corresponding slots
#[instrument(skip(addresses, slots))]
async fn initialize_nodes(addresses: &[String], slots: &[usize]) -> Result<Vec<NodeConnection>> {
    let mut nodes = Vec::new();

    if addresses.len() != slots.len() {
        return Err(anyhow::anyhow!(
            "Number of node addresses does not match the number of slot specifications"
        ));
    }

    for (address, &slot_count) in addresses.iter().zip(slots.iter()) {
        let channel = tonic::transport::Channel::from_shared(address.clone())
            .context("Invalid node address")?
            .connect()
            .await
            .context("Failed to connect to node")?;

        let client = VideoEncodingServiceClient::new(channel)
            .max_decoding_message_size(MAX_MESSAGE_SIZE)
            .max_encoding_message_size(MAX_MESSAGE_SIZE);

        nodes.push(NodeConnection {
            client,
            address: address.clone(),
            semaphore: Arc::new(Semaphore::new(slot_count)),
        });
        info!("Connected to node at {} with {} slots", address, slot_count);
    }

    if nodes.is_empty() {
        return Err(anyhow::anyhow!("No nodes available"));
    }

    Ok(nodes)
}

#[instrument(skip(node, encoding_state))]
async fn encode_chunks_on_node(
    node: NodeConnection,
    encoding_state: Arc<Mutex<EncodingState>>,
) -> Result<()> {
    let mut chunk_futures = FuturesUnordered::new();

    loop {
        // Try to acquire a permit
        if let Ok(permit) = node.semaphore.clone().acquire_owned().await {
            let chunk = {
                let mut state = encoding_state.lock().await;
                state.pending_chunks.pop()
            };

            match chunk {
                Some(chunk) => {
                    let client_clone = node.client.clone();
                    let address = node.address.clone();
                    let state_clone = Arc::clone(&encoding_state);

                    chunk_futures.push(tokio::spawn(async move {
                        let result = send_chunk(chunk.clone(), client_clone).await;
                        drop(permit); // Release the permit after processing

                        match result {
                            Ok(encoded_chunk) => {
                                let mut state = state_clone.lock().await;
                                state.completed_chunks.push(encoded_chunk);
                                info!(
                                    "Chunk {} encoded successfully on node {}",
                                    chunk.index, address
                                );
                            }
                            Err(e) => {
                                error!(
                                    "Failed to encode chunk {} on node {}: {}",
                                    chunk.index, address, e
                                );
                                let mut state = state_clone.lock().await;
                                state.pending_chunks.push(chunk);
                            }
                        }
                    }));
                }
                None => {
                    // No more chunks to process
                    drop(permit);
                    break;
                }
            }
        } else {
            // If we can't acquire a permit, wait for some ongoing tasks to complete
            if !chunk_futures.is_empty() {
                chunk_futures.next().await;
            } else {
                // If there are no chunk futures and we can't acquire permits, we're done
                break;
            }
        }
    }

    // Wait for all remaining chunk futures to complete
    while let Some(_) = chunk_futures.next().await {}

    Ok(())
}

#[instrument(skip(client), fields(chunk_index = chunk.index))]
async fn send_chunk(
    chunk: Chunk,
    mut client: VideoEncodingServiceClient<tonic::transport::Channel>,
) -> Result<Chunk> {
    let chunk_data = std::fs::read(&chunk.source_path).context("Failed to read chunk data")?;

    let request = tonic::Request::new(EncodeChunkRequest {
        chunk_data,
        chunk_index: chunk.index as i32,
        encoder_parameters: chunk.encoder_parameters.clone(),
    });

    debug!("Sending encode request for chunk {}", chunk.index);
    let response = client
        .encode_chunk(request)
        .await
        .context("Failed to send encode request")?
        .into_inner();

    if response.success {
        debug!("Successfully encoded chunk {}", chunk.index);

        let encoded_path =
            std::path::PathBuf::from(format!("./temp/encoded/encoded_chunk_{}.mkv", chunk.index));
        std::fs::write(&encoded_path, response.encoded_chunk_data)
            .context("Failed to write encoded chunk data")?;

        Ok(Chunk {
            encoded_path: Some(encoded_path),
            ..chunk
        })
    } else {
        error!(
            "Failed to encode chunk {}: {}",
            chunk.index, response.error_message
        );
        Err(anyhow::anyhow!(
            "Failed to encode chunk {}: {}",
            chunk.index,
            response.error_message
        ))
    }
}
