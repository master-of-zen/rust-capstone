use crate::error::VideoEncodeError;
/// This module is responsible for segmenting input file
/// into multiple independent files which are ready for processing
/// TODO: separating audio before segmenting
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use tracing::{debug, error, info, instrument};

use crate::chunk::verify_binaries;

/// Due to the nature of method -segment_time
/// Getting expected number of segments is not
/// guaranteed as splitting can only be done at keyframes
/// Which means that split time will be inconsistent
/// and exact on file keyframe structure.
#[instrument]
pub fn segment_video(
    input_path: &Path,
    segment_duration: f64,
    segment_dir: &Path,
) -> Result<Vec<PathBuf>, VideoEncodeError> {
    debug!(
        "Starting video segmentation: input={:?}, duration={}, segment_dir={:?}",
        input_path, segment_duration, segment_dir
    );

    verify_binaries(&"ffmpeg".to_string())?;
    debug!("FFmpeg verification successful");

    std::fs::create_dir_all(segment_dir)?;
    debug!("Created segment directory: {:?}", segment_dir);

    let output_pattern = segment_dir.join("chunk_%04d.mp4");
    debug!("Output pattern: {:?}", output_pattern);

    let binding = segment_duration.to_string();
    let inp = input_path.to_string_lossy();
    let output_pattern = output_pattern.to_string_lossy();

    let ffmpeg_args = vec![
        "-hide_banner",
        "-i",
        &inp,
        "-y",
        "-an", // don't copy audio
        "-sn", // don't copy subtitles
        "-dn", // don't copy other data
        "-c",
        "copy",
        "-map",
        "0",
        "-segment_time",
        &binding,
        "-f",
        "segment",
        "-reset_timestamps",
        "1",
        &output_pattern,
    ];

    debug!("FFmpeg command: ffmpeg {:?}", ffmpeg_args);

    // Free line to place your prayers that source don't have bidirectional keyframes
    //
    let status = Command::new("ffmpeg")
        .arg("-hide_banner")
        .args(&ffmpeg_args)
        .status()?;

    if !status.success() {
        error!("Failed to split video. FFmpeg exit status: {}", status);
        return Err(VideoEncodeError::Encoding(
            "Failed to split video".to_string(),
        ));
    }

    debug!("Video segmentation completed successfully");

    let mut segmented_files: Vec<PathBuf> = std::fs::read_dir(segment_dir)?
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().and_then(|s| s.to_str()) == Some("mp4"))
        .map(|entry| {
            let path = entry.path();
            path
        })
        .collect();
    segmented_files.sort();

    debug!(
        "Segmented files: count={}, files={:?}",
        segmented_files.len(),
        segmented_files
    );

    info!("Video split into {} segments", segmented_files.len());

    Ok(segmented_files)
}

/// Extracts audio and other non-video streams from the input file.
/// Returns paths to the extracted files.
#[instrument]
pub fn extract_non_video_streams(
    input_path: &Path,
    temp_dir: &Path,
) -> Result<PathBuf, VideoEncodeError> {
    debug!("Extracting non-video streams from: {:?}", input_path);

    std::fs::create_dir_all(temp_dir)?;

    // Extract audio
    let steams_path = temp_dir.join("audio.mkv");
    let status = Command::new("ffmpeg")
        .arg("-hide_banner")
        .args(&[
            "-i",
            input_path.to_str().unwrap(),
            "-y",
            "-vn",
            "-c", // copy all streams that is not video
            "copy",
            steams_path.to_str().unwrap(),
        ])
        .status()?;

    if !status.success() {
        error!("Failed to extract audio");
        return Err(VideoEncodeError::Encoding(
            "Failed to extract audio".to_string(),
        ));
    }

    Ok(steams_path)
}
