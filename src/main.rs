#[macro_use]
extern crate rocket;

use blake3::Hasher;
use fast_image_resize::{FilterType, ResizeAlg, ResizeOptions, Resizer};
use image::{DynamicImage, ExtendedColorType, ImageEncoder, codecs::avif::AvifEncoder};
use jpegxl_rs::encode::EncoderSpeed;
use jpegxl_rs::encoder_builder;
use libheif_rs::{
    Channel, ColorSpace, CompressionFormat, EncoderQuality, HeifContext, LibHeif, RgbChroma,
};
use rapid_qoi::{Colors, Qoi};
use reqwest::Client;
use rocket::{
    data::{Data, ToByteUnit},
    http::{Status, uri::Origin},
    request::Request,
    response::{Responder, Response},
    serde::json::{Value as SerdeJsonValue, json},
    tokio::task,
};
use serde::Deserialize;
use std::{
    fmt,
    fs,
    io::{Cursor, Write}, // Import the `Write` trait
    path::{Path, PathBuf},
    time::Instant,
};
use tabled::{Table, Tabled};

#[derive(Tabled)]
struct PerformanceMetrics {
    step: &'static str,
    duration_ms: f64,
}

#[derive(Debug, PartialEq)]
pub struct ResizedDimensions {
    pub width: u32,
    pub height: u32,
}

pub trait ImageDimensions {
    fn width(&self) -> u32;
    fn height(&self) -> u32;
}

impl ImageDimensions for DynamicImage {
    fn width(&self) -> u32 {
        self.width()
    }
    fn height(&self) -> u32 {
        self.height()
    }
}

pub fn calculate_resized_dimensions<T: ImageDimensions>(
    image: &T,
    tallest_side: u32,
) -> ResizedDimensions {
    let (original_width, original_height) = (image.width(), image.height());

    if original_width == 0 || original_height == 0 || tallest_side == 0 {
        return ResizedDimensions {
            width: 0,
            height: 0,
        };
    }

    let aspect_ratio = original_width as f64 / original_height as f64;

    let (new_width, new_height) = if original_width >= original_height {
        (
            tallest_side,
            (tallest_side as f64 / aspect_ratio).round() as u32,
        )
    } else {
        (
            (tallest_side as f64 * aspect_ratio).round() as u32,
            tallest_side,
        )
    };

    ResizedDimensions {
        width: new_width,
        height: new_height,
    }
}

#[derive(Deserialize, Clone)]
struct ResizeRequest {
    tallestSide: u32,
    format: Option<String>,
}

fn calculate_hash(input: &str) -> String {
    let mut hasher = Hasher::new();
    hasher.update(input.as_bytes());
    hasher.finalize().to_hex().to_string()
}

#[post("/<_..>", format = "json", data = "<data>")]
async fn resize_handler(url: &Origin<'_>, data: Data<'_>) -> Result<SerdeJsonValue, CustomError> {
    let source = format!("{}", url).strip_prefix("/").unwrap().to_string();

    let request: ResizeRequest = serde_json::from_str(
        &String::from_utf8(
            data.open(1024.bytes())
                .into_bytes()
                .await
                .map_err(|_| CustomError::BadRequest)?
                .into_inner(),
        )
        .map_err(|_| CustomError::BadRequest)?,
    )?;

    let format_str = request
        .format
        .as_ref()
        .map_or(String::new(), |s| s.to_lowercase());

    if !["avif", "heic", "jxl", "qoi"].contains(&format_str.as_str()) {
        return Err(CustomError::UnsupportedFormat);
    }

    let hash_str = calculate_hash(&source);
    let resized_filename = format!("{hash_str}_{}.{}", request.tallestSide, format_str.as_str());
    let images_dir = Path::new("images");
    fs::create_dir_all(images_dir).map_err(|_| CustomError::DirectoryCreationError)?;
    let resized_image_path = images_dir.join(&resized_filename);

    if resized_image_path.exists() {
        return Ok(json!({
            "status": "ALREADY_TRANSFORMED",
            "hash": &hash_str,
            "filename": resized_filename,
        }));
    }

    let mut download_duration = 0.0;
    let downloaded_image_path = images_dir.join(&hash_str);
    let image_bytes: Vec<u8> = match fs::read(&downloaded_image_path) {
        Ok(bytes) => {
            log::info!("CACHE HIT: Reading image {} from file.", &hash_str);
            if bytes.is_empty() {
                return Err(CustomError::FileCorruptError);
            }
            bytes
        }
        Err(e) => {
            if e.kind() == std::io::ErrorKind::NotFound {
                log::info!("CACHE MISS: Downloading image for source: {}", &source);
                let download_start = Instant::now();
                let downloaded_image_bytes = Client::new()
                    .get(&source)
                    .send()
                    .await
                    .map_err(|_| CustomError::DownloadError)?
                    .bytes()
                    .await
                    .map_err(|_| CustomError::DownloadError)?
                    .to_vec();
                download_duration = download_start.elapsed().as_secs_f64() * 1000.0;

                // Create the file first, handling creation errors separately.
                let mut file = fs::File::create(&downloaded_image_path)
                    .map_err(|_| CustomError::FileCreationError)?;
                // Then write all bytes to it, handling write errors.
                file.write_all(&downloaded_image_bytes)
                    .map_err(|_| CustomError::FileWriteError)?;

                log::info!("CACHE WRITE: Saved image {} to file.", &hash_str);
                downloaded_image_bytes
            } else {
                return Err(CustomError::FileReadError);
            }
        }
    };

    if image_bytes.is_empty() {
        return Err(CustomError::BadRequest);
    }

    let resized_image_path_for_task = resized_image_path.clone();

    let (encoded_data, mut metrics) = task::spawn_blocking(move || {
        process_image(
            image_bytes,
            request,
            format_str,
            resized_image_path_for_task,
        )
    })
    .await
    .map_err(|e| CustomError::ProcessingError(e.to_string()))?
    .map_err(|e| e)?;

    metrics.insert(
        0,
        PerformanceMetrics {
            step: "Downloading",
            duration_ms: download_duration,
        },
    );

    let save_start = Instant::now();
    if !encoded_data.is_empty() {
        // Use the same create-then-write pattern for the transformed image.
        let mut file =
            fs::File::create(&resized_image_path).map_err(|_| CustomError::FileCreationError)?;
        file.write_all(&encoded_data)
            .map_err(|_| CustomError::FileWriteError)?;
    }
    let save_duration = save_start.elapsed().as_secs_f64() * 1000.0;

    metrics.push(PerformanceMetrics {
        step: "Saving",
        duration_ms: save_duration,
    });

    let table = Table::new(metrics).to_string();
    log::info!("Processing complete for {}:\n{}", resized_filename, table);

    Ok(json!({
        "status": "TRANSFORMED",
        "hash": hash_str,
        "filename": resized_filename,
    }))
}

fn process_image(
    image_bytes: Vec<u8>,
    request: ResizeRequest,
    format_str: String,
    resized_image_path: PathBuf,
) -> Result<(Vec<u8>, Vec<PerformanceMetrics>), CustomError> {
    let mut metrics = Vec::new();

    let load_start = Instant::now();
    let img = image::load_from_memory(&image_bytes).map_err(|_| CustomError::ImageDecodeError)?;
    metrics.push(PerformanceMetrics {
        step: "Decoding",
        duration_ms: load_start.elapsed().as_secs_f64() * 1000.0,
    });

    let resize_start = Instant::now();
    let resized_dims = calculate_resized_dimensions(&img, request.tallestSide);
    let mut dst_image = DynamicImage::new(resized_dims.width, resized_dims.height, img.color());
    let mut resizer = Resizer::new();
    resizer
        .resize(
            &img,
            &mut dst_image,
            &ResizeOptions::new()
                .resize_alg(ResizeAlg::Convolution(FilterType::CatmullRom))
                .use_alpha(false),
        )
        .map_err(|_| CustomError::ResizeError)?;
    metrics.push(PerformanceMetrics {
        step: "Resizing",
        duration_ms: resize_start.elapsed().as_secs_f64() * 1000.0,
    });

    let encode_start = Instant::now();
    let encoded_data = match format_str.as_str() {
        "avif" => encode_avif(&dst_image),
        "heic" => {
            encode_heic(&resized_image_path, &dst_image)?;
            Ok(Vec::new())
        }
        "jxl" => encode_jxl(&dst_image),
        "qoi" => encode_qoi(&dst_image),
        _ => unreachable!(),
    }?;
    metrics.push(PerformanceMetrics {
        step: "Encoding",
        duration_ms: encode_start.elapsed().as_secs_f64() * 1000.0,
    });

    Ok((encoded_data, metrics))
}
fn encode_avif(dst_image: &DynamicImage) -> Result<Vec<u8>, CustomError> {
    let mut buf = Cursor::new(Vec::new());
    AvifEncoder::new_with_speed_quality(&mut buf, 8, 85)
        .write_image(
            dst_image.as_bytes(),
            dst_image.width(),
            dst_image.height(),
            ExtendedColorType::from(dst_image.color()),
        )
        .map_err(|_| CustomError::ImageEncodeError)?;
    Ok(buf.into_inner())
}

fn encode_heic(output_file: &Path, dst_image: &DynamicImage) -> Result<(), CustomError> {
    let width = dst_image.width();
    let height = dst_image.height();
    let rgb_buf = dst_image.to_rgb8().into_raw();

    let mut heic_image = libheif_rs::Image::new(width, height, ColorSpace::Rgb(RgbChroma::Rgb))
        .map_err(|_| CustomError::ImageEncodeError)?;
    heic_image
        .create_plane(Channel::Interleaved, width, height, 24)
        .map_err(|_| CustomError::ImageEncodeError)?;

    let planes = heic_image.planes_mut();
    let plane_interleaved = planes.interleaved.unwrap();
    let destination_stride = plane_interleaved.stride;
    let destination_data = plane_interleaved.data;
    let source_row_length = (width * 3) as usize;

    if destination_stride == source_row_length {
        destination_data.copy_from_slice(&rgb_buf);
    } else {
        destination_data
            .chunks_mut(destination_stride)
            .zip(rgb_buf.chunks(source_row_length))
            .for_each(|(destination_row, source_row)| {
                destination_row[..source_row_length].copy_from_slice(source_row);
            });
    }

    let lib_heif = LibHeif::new();
    let mut context = HeifContext::new().map_err(|_| CustomError::ImageEncodeError)?;
    let mut encoder = lib_heif
        .encoder_for_format(CompressionFormat::Hevc)
        .map_err(|_| CustomError::ImageEncodeError)?;
    encoder
        .set_quality(EncoderQuality::Lossy(85))
        .map_err(|_| CustomError::ImageEncodeError)?;
    context
        .encode_image(&heic_image, &mut encoder, None)
        .map_err(|_| CustomError::ImageEncodeError)?;

    // The libheif library handles file writing in a single step.
    // We'll map this to FileCreationError as its primary purpose is creating the output file.
    context
        .write_to_file(output_file.to_str().unwrap())
        .map_err(|_| CustomError::FileCreationError)?;

    Ok(())
}

fn encode_jxl(dst_image: &DynamicImage) -> Result<Vec<u8>, CustomError> {
    let width = dst_image.width();
    let height = dst_image.height();
    let rgb_buf = dst_image.to_rgb8().into_raw();

    let mut encoder = encoder_builder()
        .lossless(false)
        .speed(EncoderSpeed::Falcon)
        .quality(1.0)
        .build()
        .map_err(|_| CustomError::ImageEncodeError)?;

    let encoded_data = encoder
        .encode::<u8, u8>(&rgb_buf, width, height)
        .map_err(|_| CustomError::ImageEncodeError)?;

    Ok(encoded_data.data)
}

fn encode_qoi(dst_image: &DynamicImage) -> Result<Vec<u8>, CustomError> {
    let width = dst_image.width();
    let height = dst_image.height();

    let (pixels, colors) = match dst_image.color().has_alpha() {
        true => (dst_image.to_rgba8().into_raw(), Colors::Rgba),
        false => (dst_image.to_rgb8().into_raw(), Colors::Rgb),
    };

    let qoi = Qoi {
        width,
        height,
        colors,
    };

    qoi.encode_alloc(&pixels)
        .map_err(|_| CustomError::ImageEncodeError)
}

#[derive(Debug)]
enum CustomError {
    UnsupportedFormat,
    DownloadError,
    ImageDecodeError,
    ResizeError,
    FileCreationError,
    ImageEncodeError,
    DirectoryCreationError,
    FileWriteError,
    FileReadError,
    FileCorruptError,
    BadRequest,
    ProcessingError(String),
    JsonDeserializeError(String),
}

impl fmt::Display for CustomError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            CustomError::UnsupportedFormat => {
                write!(
                    f,
                    "Unsupported format. Use 'avif', 'heic', 'jxl', or 'qoi'."
                )
            }
            CustomError::DownloadError => write!(f, "Failed to download image from URL."),
            CustomError::ImageDecodeError => {
                write!(f, "Failed to decode image. May be corrupt or unsupported.")
            }
            CustomError::ResizeError => write!(f, "Failed to resize image."),
            CustomError::FileCreationError => write!(f, "Failed to create output file."),
            CustomError::ImageEncodeError => write!(f, "Failed to encode image."),
            CustomError::DirectoryCreationError => write!(f, "Failed to create directories."),
            CustomError::FileWriteError => write!(f, "Failed to write image data to file."),
            CustomError::FileReadError => write!(f, "Failed to read image data from file."),
            CustomError::FileCorruptError => write!(f, "File was empty or corrupt."),
            CustomError::BadRequest => write!(f, "Bad request: invalid request format."),
            CustomError::ProcessingError(details) => {
                write!(f, "Internal processing error: {}", details)
            }
            CustomError::JsonDeserializeError(details) => {
                write!(f, "Bad request: invalid JSON - {}", details)
            }
        }
    }
}

impl From<CustomError> for SerdeJsonValue {
    fn from(error: CustomError) -> Self {
        json!({ "status": "ERROR", "reason": error.to_string() })
    }
}

impl<'r> Responder<'r, 'static> for CustomError {
    fn respond_to(self, request: &'r Request) -> Result<Response<'static>, Status> {
        SerdeJsonValue::from(self).respond_to(request)
    }
}

impl From<serde_json::Error> for CustomError {
    fn from(error: serde_json::Error) -> Self {
        CustomError::JsonDeserializeError(error.to_string())
    }
}

#[launch]
fn rocket() -> _ {
    env_logger::init();
    rocket::build().mount("/", routes![resize_handler])
}
