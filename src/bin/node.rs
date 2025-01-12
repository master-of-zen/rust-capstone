use anyhow::Result;
use clap::Parser;
use std::fs;
use std::path::PathBuf;
use tonic::{transport::Server, Request, Response, Status};
use tracing::{debug, error, info, instrument};
use video_encoding::video_encoding_service_server::{
    VideoEncodingService, VideoEncodingServiceServer,
};
use video_encoding::{EncodeChunkRequest, EncodeChunkResponse};
use video_encoding_system::chunk::{verify_binaries, Chunk};

pub mod video_encoding {
    tonic::include_proto!("video_encoding");
}

use video_encoding_system::config::TempConfig;
use video_encoding_system::logging::init_logging;
use video_encoding_system::settings::Settings;

const MAX_MESSAGE_SIZE: usize = 1024 * 1024 * 1024; // 1 GB

/// CLI arguments for the video encoding node
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to the configuration file
    #[arg(short, long)]
    config_file: Option<PathBuf>,

    /// Node address
    #[arg(short, long)]
    node: Option<String>,

    /// Temporary directory for processing
    #[arg(short, long)]
    temp_dir: Option<PathBuf>,
}

/// Represents the video encoding node
#[derive(Debug)]
pub struct VideoEncodingNode {
    config: TempConfig,
}

#[tonic::async_trait]
impl VideoEncodingService for VideoEncodingNode {
    /// Encodes a chunk of video
    ///
    /// # Arguments
    ///
    /// * `request` - The EncodeChunkRequest containing chunk data and metadata
    ///
    /// # Returns
    ///
    /// A Result containing the EncodeChunkResponse or a Status error
    #[instrument(skip(self, request))]
    async fn encode_chunk(
        &self,
        request: Request<EncodeChunkRequest>,
    ) -> Result<Response<EncodeChunkResponse>, Status> {
        let req = request.into_inner();
        info!("Received encode request for chunk {}", req.chunk_index);

        let input_path = self
            .config
            .segment_dir()
            .join(format!("chunk_{}.mkv", req.chunk_index));
        let output_path = self
            .config
            .encode_dir()
            .join(format!("encoded_chunk_{}.mkv", req.chunk_index));

        debug!("Writing chunk data to file: {:?}", input_path);
        fs::write(&input_path, &req.chunk_data).map_err(|e| {
            error!("Failed to write chunk data to file: {}", e);
            Status::internal("Failed to write chunk data to file")
        })?;

        let chunk = Chunk::new(input_path, req.chunk_index as usize, req.encoder_parameters);

        match chunk.encode(output_path.clone()) {
            Ok(encoded_chunk) => {
                debug!(
                    "Reading encoded chunk data: {:?}",
                    encoded_chunk.encoded_path
                );
                let encoded_data = fs::read(encoded_chunk.encoded_path.unwrap()).map_err(|e| {
                    error!("Failed to read encoded chunk: {}", e);
                    Status::internal("Failed to read encoded chunk")
                })?;

                info!(
                    "Successfully encoded chunk {}, size {}B",
                    req.chunk_index,
                    encoded_data.len()
                );

                debug!(
                    "Removing source {:?} and encoded {:?}",
                    chunk.source_path, output_path
                );
                if let Err(e) = fs::remove_file(chunk.source_path) {
                    error!("Failed to remove source file: {}", e);
                }
                if let Err(e) = fs::remove_file(&output_path) {
                    error!("Failed to remove encoded file: {}", e);
                }

                Ok(Response::new(EncodeChunkResponse {
                    encoded_chunk_data: encoded_data,
                    chunk_index: req.chunk_index,
                    success: true,
                    error_message: String::new(),
                }))
            }
            Err(e) => {
                error!("Failed to encode chunk {}: {}", req.chunk_index, e);
                Ok(Response::new(EncodeChunkResponse {
                    encoded_chunk_data: Vec::new(),
                    chunk_index: req.chunk_index,
                    success: false,
                    error_message: e.to_string(),
                }))
            }
        }
    }
}

/// Initializes and runs the video encoding node
#[tokio::main]
#[instrument]
async fn main() -> Result<()> {
    init_logging();

    let cli = Cli::parse();

    info!("Starting video encoding node");
    debug!("CLI arguments: {:?}", cli);

    let settings = load_settings(&cli)?;

    verify_binaries(&"ffmpeg".to_string())?;

    let config = TempConfig::new(
        Some(settings.processing.temp_dir),
        &PathBuf::from("dummy"),
        "dummy",
    );
    let server = VideoEncodingNode { config };

    let service = VideoEncodingServiceServer::new(server)
        .max_encoding_message_size(MAX_MESSAGE_SIZE)
        .max_decoding_message_size(MAX_MESSAGE_SIZE);

    info!(
        "Server configured, starting to serve on {}",
        settings.node.address
    );
    Server::builder()
        .add_service(service)
        .serve(settings.node.address.parse()?)
        .await?;

    Ok(())
}

/// Loads settings from the configuration file or creates default settings
#[instrument(skip(cli))]
fn load_settings(cli: &Cli) -> Result<Settings> {
    let mut settings = if let Some(config_path) = &cli.config_file {
        debug!("Loading settings from file: {:?}", config_path);
        Settings::from_file(config_path)?
    } else {
        debug!("Loading default settings");
        Settings::new()?
    };

    if let Some(address) = &cli.node {
        debug!("Overriding node address with CLI option: {}", address);
        settings.node.address = address.clone();
    }
    if let Some(temp_dir) = &cli.temp_dir {
        debug!("Overriding temp directory with CLI option: {:?}", temp_dir);
        settings.processing.temp_dir = temp_dir.clone();
    }

    Ok(settings)
}
