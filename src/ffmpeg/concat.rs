use crate::error::VideoEncodeError;
use path_abs::{PathAbs, PathInfo};
use std::fmt::Write as hi;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{error, info, debug, instrument};

#[derive(Debug)]
pub enum OutputError {
    Path(String),
    Fs(String),
    MkvMergeFailed(String),
    Io(std::io::Error),
}

pub fn mkvmerge(
    temp_dir: &Path,
    output: &Path,
    encoder_extension: &str,
    num_tasks: usize,
) -> Result<(), OutputError> {
    #[cfg(windows)]
    fn fix_path<P: AsRef<Path>>(p: P) -> String {
        const UNC_PREFIX: &str = r#"\\?\"#;

        let p = p.as_ref().display().to_string();
        if let Some(path) = p.strip_prefix(UNC_PREFIX) {
            if let Some(p2) = path.strip_prefix("UNC") {
                format!("\\{}", p2)
            } else {
                path.to_string()
            }
        } else {
            p
        }
    }

    #[cfg(not(windows))]
    fn fix_path<P: AsRef<Path>>(p: P) -> String {
        p.as_ref().display().to_string()
    }

    let mut audio_file = PathBuf::from(&temp_dir);
    audio_file.push("audio.mkv");
    let audio_file = PathAbs::new(&audio_file).map_err(|err| OutputError::Path(err.to_string()))?;
    let audio_file = if audio_file.as_path().exists() {
        Some(fix_path(audio_file))
    } else {
        None
    };

    let mut encode_dir = PathBuf::from(temp_dir);
    encode_dir.push("encoded");

    let output = PathAbs::new(output).map_err(|err| OutputError::Path(err.to_string()))?;

    assert!(num_tasks != 0);

    let options_path = PathBuf::from(&temp_dir).join("options.json");
    let options_json_contents = mkvmerge_options_json(
        num_tasks,
        encoder_extension,
        &fix_path(output.to_str().unwrap()),
        audio_file.as_deref(),
    );

    let mut options_json =
        File::create(options_path).map_err(|err| OutputError::Fs(err.to_string()))?;
    options_json
        .write_all(options_json_contents.as_bytes())
        .map_err(|err| OutputError::Fs(err.to_string()))?;

    let mut cmd = Command::new("mkvmerge");
    cmd.current_dir(&encode_dir);
    cmd.arg("@../options.json");

    let out = cmd.output().map_err(|e| OutputError::Io(e))?;

    if !out.status.success() {
        error!(
            "mkvmerge concatenation failed with output: {:#?}\ncommand: {:?}",
            out, cmd
        );
        return Err(OutputError::MkvMergeFailed(
            String::from_utf8_lossy(&out.stderr).into(),
        ));
    }

    Ok(())
}

pub fn mkvmerge_options_json(num: usize, ext: &str, output: &str, audio: Option<&str>) -> String {
    let mut file_string = String::with_capacity(64 + 12 * num);
    write!(file_string, "[\"-o\", {output:?}").unwrap();
    if let Some(audio) = audio {
        write!(file_string, ", {audio:?}").unwrap();
    }
    file_string.push_str(", \"[\"");
    for i in 0..num {
        write!(file_string, ", \"encoded_chunk_{i}.{ext}\"").unwrap();
    }
    file_string.push_str(",\"]\"]");

    file_string
}

fn ffmpeg_mux(concat: String, input: String, output: String) -> Result<(), OutputError> {
    let ffmpeg_args = vec![
        "-f",
        "concat",
        "-safe",
        "0",
        "-i",
        &concat,
        "-i",
        &input,
        "-map",
        "0:v", // map video from concatenated segments
        "-map",
        "1", // map all streams from original input
        "-c",
        "copy",
        &output,
    ];

    debug!("FFmpeg command: ffmpeg {:?}", ffmpeg_args);

    // Execute FFmpeg command
    Command::new("ffmpeg")
        .arg("-hide_banner")
        .args(&ffmpeg_args)
        .status().map_err(|e| OutputError::Io(e))?;
    Ok(())
}

/// Concatenates video segments and adds back non-video streams.
#[instrument(skip(segment_paths))]
pub fn concatenate_videos_and_copy_streams(
    segment_paths: Vec<PathBuf>,
    original_input: &Path,
    output_file: &Path,
    temp_dir: &PathBuf,
    expected_segments: usize,
    concat: &String,
) -> Result<(), VideoEncodeError> {
    // Verify that all segments exist and match the expected count
    if segment_paths.len() != expected_segments {
        return Err(VideoEncodeError::Concatenation(format!(
            "Mismatch in segment count. Expected: {}, Found: {}",
            expected_segments,
            segment_paths.len()
        )));
    }

    for path in segment_paths.iter() {
        if !path.exists() {
            return Err(VideoEncodeError::Concatenation(format!(
                "Segment file not found: {:?}",
                path
            )));
        }
    }

    let temp_file_list = PathBuf::from("file_list.txt");
    let status = if concat == "ffmpeg" {
        // Create a temporary file list for FFmpeg
        // Unfortunately due to current implementation path of the files inside
        // is relative to the file
        let file_list_content: String = segment_paths
            .iter()
            .map(|path| format!("file '{}'\n", path.to_str().unwrap()))
            .collect();
        std::fs::write(&temp_file_list, file_list_content)?;
    
        let temp_st = temp_file_list.to_string_lossy();
        let original_input = original_input.to_string_lossy();
        let output_file = output_file.to_string_lossy();

        ffmpeg_mux(temp_st.into(), original_input.into(), output_file.into())
    } else {
        mkvmerge(&temp_dir, &output_file, "mkv".into(), expected_segments)
    };

    if status.is_err() {
        eprintln!("{status:?}");
        error!("Failed to concatenate videos and copy streams");
        return Err(VideoEncodeError::Concatenation(
            "Failed to concatenate videos and copy streams".to_string(),
        ));
    }

    info!(
        "Successfully concatenated {} video segments and copied all streams to the final video",
        segment_paths.len(),
    );

    // Clean up temporary file
    fs::remove_file(temp_file_list)?;

    Ok(())
}
