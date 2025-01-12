use crate::error::VideoEncodeError;
use crate::ffmpeg::segment::segment_video;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{debug, error, info, instrument};

/// Represents a video chunk for processing
#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct Chunk {
    pub source_path: PathBuf,
    pub encoded_path: Option<PathBuf>,
    pub index: usize,
    pub encoder_parameters: Vec<String>,
}

impl Chunk {
    #[instrument(skip(encoder_parameters))]
    pub fn new(source_path: PathBuf, index: usize, encoder_parameters: Vec<String>) -> Self {
        debug!(
            "Creating new Chunk: index={}, source={:?}",
            index, source_path
        );

        if !source_path.exists() {
            error!("Source path does not exist: {:?}", source_path);
            panic!("Source path does not exist");
        }

        Chunk {
            source_path,
            encoded_path: None,
            index,
            encoder_parameters,
        }
    }

    #[instrument(skip(self))]
    pub fn encode(&self, output_path: PathBuf) -> Result<Chunk, VideoEncodeError> {
        debug!(
            "Encoding chunk {}: source={:?}, output={:?}, encoder_parameters={:?} ",
            self.index, self.source_path, output_path, self.encoder_parameters
        );

        let command = Command::new("ffmpeg")
            .arg("-hide_banner")
            .arg("-i")
            .arg(&self.source_path)
            .args(&self.encoder_parameters)
            .arg(&output_path)
            .output()?;

        if !command.status.success() {
            let error_msg = format!(
                "Failed to encode chunk {}: {:?}",
                self.index,
                String::from_utf8_lossy(&command.stderr)
            );
            error!("{}", error_msg);
            return Err(VideoEncodeError::Encoding(error_msg));
        }

        info!("Successfully encoded chunk {}", self.index);
        Ok(Chunk {
            source_path: self.source_path.clone(),
            encoded_path: Some(output_path),
            index: self.index,
            encoder_parameters: self.encoder_parameters.clone(),
        })
    }
}

#[instrument(skip(segments, encoder_params))]
pub fn convert_files_to_chunks(
    segments: Vec<PathBuf>,
    encoder_params: Vec<String>,
) -> Result<Vec<Chunk>, VideoEncodeError> {
    debug!("Converting {} files to chunks", segments.len());

    let chunks: Vec<Chunk> = segments
        .into_iter()
        .enumerate()
        .map(|(index, path)| {
            if !path.exists() {
                error!("Segment file does not exist: {:?}", path);
                panic!("Segment file does not exist");
            }
            Chunk::new(path, index, encoder_params.clone())
        })
        .collect();

    info!("Converted {} files to chunks", chunks.len());
    Ok(chunks)
}

#[instrument(skip(encoder_params))]
pub fn split_video(
    input_path: &Path,
    segment_duration: f64,
    segment_dir: &Path,
    encoder_params: &[String],
    encode_dir: &Path,
) -> Result<Vec<PathBuf>, VideoEncodeError> {
    debug!(
        "Splitting video: input={:?}, duration={}, segment_dir={:?}, params={:?}, encode_dir={:?}",
        input_path, segment_duration, segment_dir, encoder_params, encode_dir
    );

    let segmented_files = segment_video(input_path, segment_duration, segment_dir)?;

    info!(
        "Video segmentation complete: {} files",
        segmented_files.len()
    );

    Ok(segmented_files)
}

/// Verifies that FFmpeg is installed and accessible
///
/// # Returns
///
/// A Result indicating success or a VideoEncodeError if FFmpeg is not found
#[instrument]
pub fn verify_binaries(concat: &String) -> Result<(), VideoEncodeError> {
    debug!("Verifying FFmpeg installation");
    let mut result = Ok(());
    match which::which("ffmpeg") {
        Ok(path) => {
            info!("FFmpeg found at: {path:?}");
        }
        Err(e) => {
            error!("FFmpeg not found: {}", e);
            result = Err(VideoEncodeError::FfmpegNotFound);
        }
    }
    if concat == "mkvmerge" {
        match which::which("mkvmerge") {
            Ok(path) => {
                info!("MKVMerge found at: {path:?}");
            }
            Err(e) => {
                error!("MKVMerge not found: {e}");
                result = Err(VideoEncodeError::MkvmergeNotFound);
            }
        }
    }
    result
}
